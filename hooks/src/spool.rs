//! Reports the server never got, sent again.
//!
//! A write's report can fail: the network drops, the server is between
//! deploys, a request times out. The agent must never wait on it, so the
//! hook moves on — and before this, that edit was simply missing from the
//! map, the teammates' deltas and the ledger. Now the path is remembered
//! here, and the next hook run in the repo sends it again first.
//!
//! What is sent again is the file as it is NOW, read from the working copy
//! that wrote it (a subagent's worktree is not the parent's checkout), not
//! the content that failed: a stale copy arriving after a newer one would
//! roll the map back. A path is remembered once, however many times it
//! failed, so the spool stays small. A file that is gone by then is dropped.
//!
//! A resend is history, not presence: it goes out marked `resent`, and the
//! server records the file without refreshing any marker, so a subagent
//! that finished hours ago is never drawn live again by its parent's posts.
//! And it is bounded every way. A timed-out post was usually slow, not lost
//! (the server recorded it; only the answer missed the hook's budget), so
//! an entry is tried a few times at most and never stands in front of the
//! rest; an hour old, it is dropped; a subagent's entries go with its
//! SubagentStop; and the file keeps a couple of hundred paths at most.
//!
//! A path this very run reports live is owed no longer: the live report
//! carries the file as it is, and a resend of it going out first made the
//! live one look unchanged (no step on the agent's line).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::config::{self, Env};

/// At most this many paths are kept; the oldest go first.
const MAX_PATHS: usize = 200;
/// A spool file larger than this is not read: it is dropped whole.
const MAX_FILE_BYTES: u64 = 512 * 1024;
/// A path older than this is dropped rather than sent: the edit is history
/// by then, and the next session's index covers the file.
const MAX_AGE_S: f64 = 3_600.0;
/// Sends per entry before it is dropped. A post that timed out was usually
/// slow, not lost: the server recorded it and only its answer came late.
const MAX_TRIES: u64 = 3;
/// At most this many resends per hook run, and at most this share of the
/// run's budget: the agent's own report comes first.
const MAX_RESENDS: usize = 20;

fn spool_path(env: &Env, server: &str, repo_id: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{server}\n{repo_id}").as_bytes());
    let tag: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    crate::config::data_dir(env).join("spool").join(format!("{tag}.json"))
}

/// The subagents seen ending (SubagentStop), beside the spool: their
/// entries are never sent again, nor spooled anew.
fn ended_path(env: &Env, server: &str, repo_id: &str) -> PathBuf {
    spool_path(env, server, repo_id).with_extension("ended.json")
}

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn load(path: &Path) -> Map<String, Value> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
        let _ = std::fs::remove_file(path);
        return Map::new();
    }
    config::load_json(path).as_object().cloned().unwrap_or_default()
}

fn save(path: &Path, entries: &Map<String, Value>) {
    if entries.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    crate::report::save_json(path, &Value::Object(entries.clone()));
}

/// An entry's key: the file's absolute path, so a subagent's worktree and
/// the parent's checkout never share one.
fn key(root: &Path, rel: &str) -> String {
    root.join(rel).to_string_lossy().into_owned()
}

fn ts_of(entry: &Value) -> f64 {
    entry.get("ts").and_then(Value::as_f64).unwrap_or(0.0)
}

fn tries_of(entry: &Value) -> u64 {
    entry.get("tries").and_then(Value::as_u64).unwrap_or(0)
}

fn worker_of(entry: &Value) -> &str {
    entry.get("meta").and_then(|m| m.get("worker")).and_then(Value::as_str).unwrap_or("")
}

/// The ended subagents still worth remembering (an hour, as entries are).
fn ended(env: &Env, server: &str, repo_id: &str) -> Map<String, Value> {
    let mut seen = load(&ended_path(env, server, repo_id));
    let cutoff = now() - MAX_AGE_S;
    seen.retain(|_, ts| ts.as_f64().unwrap_or(0.0) >= cutoff);
    seen
}

/// A report for `rel` (under `root`) did not reach the server: remember the
/// path, where it lives and the session fields it was sent with. A subagent
/// already seen ending owes nothing.
pub fn remember(env: &Env, server: &str, repo_id: &str, root: &Path, rel: &str, meta: &Map<String, Value>) {
    let worker = meta.get("worker").and_then(Value::as_str).unwrap_or("");
    if !worker.is_empty() && ended(env, server, repo_id).contains_key(worker) {
        return;
    }
    // what the message cost belongs to the live post that failed, not to a
    // resend later
    let mut meta = meta.clone();
    meta.remove("step");
    let path = spool_path(env, server, repo_id);
    let mut entries = load(&path);
    let cutoff = now() - MAX_AGE_S;
    entries.retain(|_, v| ts_of(v) >= cutoff && v.get("rel").is_some());
    entries.insert(
        key(root, rel),
        json!({"ts": now(), "rel": rel, "root": root.to_string_lossy(), "tries": 0, "meta": meta}),
    );
    if entries.len() > MAX_PATHS {
        let mut by_age: Vec<(String, f64)> = entries.iter().map(|(k, v)| (k.clone(), ts_of(v))).collect();
        by_age.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        for (k, _) in by_age.into_iter().take(entries.len() - MAX_PATHS) {
            entries.remove(&k);
        }
    }
    save(&path, &entries);
}

/// A report for `rel` (under `root`) reached the server: nothing about it
/// is owed.
pub fn forget(env: &Env, server: &str, repo_id: &str, root: &Path, rel: &str) {
    let path = spool_path(env, server, repo_id);
    let mut entries = load(&path);
    if entries.remove(&key(root, rel)).is_some() {
        save(&path, &entries);
    }
}

/// A subagent finished (SubagentStop): whatever it still owed is dropped,
/// and nothing of it is spooled again. What it wrote reached the server
/// already or is history; either way, sending it later would only draw the
/// subagent as live once more.
pub fn worker_ended(env: &Env, server: &str, repo_id: &str, worker: &str) {
    if worker.is_empty() {
        return;
    }
    let mut seen = ended(env, server, repo_id);
    seen.insert(worker.to_string(), json!(now()));
    crate::report::save_json(&ended_path(env, server, repo_id), &Value::Object(seen));
    let path = spool_path(env, server, repo_id);
    let mut entries = load(&path);
    let before = entries.len();
    entries.retain(|_, v| worker_of(v) != worker);
    if entries.len() != before {
        save(&path, &entries);
    }
}

/// What a resend run did to the entries it read, applied to the spool as it
/// is now: another hook run may have remembered or forgotten paths while
/// this one was posting, and a whole-file save would undo that. Only the
/// entry this run read is touched: a newer failure of the same path is a
/// new debt and stays.
fn commit(path: &Path, dropped: &[(String, f64)], tried: &[(String, f64, u64)]) {
    if dropped.is_empty() && tried.is_empty() {
        return;
    }
    let mut entries = load(path);
    let same = |entries: &Map<String, Value>, k: &str, ts: f64| entries.get(k).is_some_and(|v| ts_of(v) == ts);
    for (k, ts) in dropped {
        if same(&entries, k, *ts) {
            entries.remove(k);
        }
    }
    for (k, ts, tries) in tried {
        if same(&entries, k, *ts) {
            entries[k.as_str()]["tries"] = json!(tries);
        }
    }
    save(path, &entries);
}

/// Send what is owed, least-tried and oldest first, within `budget`.
/// `build` makes the payload for a path from its current content and the
/// remembered fields (None when the file is gone or not reportable); each
/// goes out marked `resent`, so the server records it as history and moves
/// no marker. An entry the server answered is drained; one that failed is
/// kept for another try, up to `MAX_TRIES`. Stops at the first failure: the
/// server is slow or unreachable. `covered` are the files (under `root`)
/// this run reports live right after: owed no longer, dropped unsent.
/// Returns how many were sent.
pub fn resend(
    env: &Env, server: &str, repo_id: &str, token: &str, user_agent: &str, budget: Duration,
    root: &Path, covered: &[String], build: &dyn Fn(&str, &str, &Map<String, Value>) -> Option<Value>,
) -> usize {
    resend_with(env, server, repo_id, budget, root, covered, build, &|payload, left| {
        crate::http::post(server, "/report", token, user_agent, payload, left).map(|_| ())
    })
}

fn resend_with(
    env: &Env, server: &str, repo_id: &str, budget: Duration, covered_root: &Path, covered: &[String],
    build: &dyn Fn(&str, &str, &Map<String, Value>) -> Option<Value>,
    post: &dyn Fn(&Value, Duration) -> Result<(), ()>,
) -> usize {
    let path = spool_path(env, server, repo_id);
    let entries = load(&path);
    if entries.is_empty() {
        return 0;
    }
    let started = Instant::now();
    let ended = ended(env, server, repo_id);
    let stamp = now();
    let mut dropped: Vec<(String, f64)> = Vec::new();
    let mut order: Vec<(&String, &Value)> = entries.iter().collect();
    // an entry that keeps failing does not stand in front of the rest
    order.sort_by(|a, b| {
        tries_of(a.1).cmp(&tries_of(b.1)).then(ts_of(a.1).partial_cmp(&ts_of(b.1)).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut sent = 0;
    let mut attempts = 0;
    let mut stopped = false;
    for (k, entry) in order {
        let ts = ts_of(entry);
        let tries = tries_of(entry);
        let rel = entry.get("rel").and_then(Value::as_str).unwrap_or("");
        let root = entry.get("root").and_then(Value::as_str).unwrap_or("");
        let worker = worker_of(entry);
        // an entry from before roots were kept, too old, tried enough, a
        // subagent's that has ended, or a file this run reports live in a
        // moment: owed no longer, and never sent
        if rel.is_empty() || root.is_empty() || stamp - ts > MAX_AGE_S || tries >= MAX_TRIES
            || (!worker.is_empty() && ended.contains_key(worker))
            || covered.iter().any(|c| key(covered_root, c) == *k)
        {
            dropped.push((k.clone(), ts));
            continue;
        }
        let left = budget.saturating_sub(started.elapsed());
        if stopped || attempts >= MAX_RESENDS || left.is_zero() {
            continue; // the rest are still swept for what is owed no longer
        }
        let meta = entry.get("meta").and_then(Value::as_object).cloned().unwrap_or_default();
        // the working copy that wrote it; a removed worktree takes it along
        let Ok(bytes) = std::fs::read(Path::new(root).join(rel)) else {
            dropped.push((k.clone(), ts));
            continue;
        };
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let Some(mut payload) = build(rel, &content, &meta) else {
            dropped.push((k.clone(), ts));
            continue;
        };
        payload["resent"] = json!(true);
        // counted before the post: a run killed mid-send still spends a try
        attempts += 1;
        commit(&path, &[], &[(k.clone(), ts, tries + 1)]);
        match post(&payload, left) {
            Ok(()) => {
                dropped.push((k.clone(), ts));
                sent += 1;
            }
            Err(()) => {
                // its last try: owed no longer, whatever the server made of it
                if tries + 1 >= MAX_TRIES {
                    dropped.push((k.clone(), ts));
                }
                stopped = true;
            }
        }
    }
    commit(&path, &dropped, &[]);
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn env(home: &Path) -> Env {
        let mut e = Env::default();
        e.insert("HOME".into(), home.to_string_lossy().to_string());
        e
    }

    fn scratch(name: &str) -> (PathBuf, PathBuf, Env) {
        let home = std::env::temp_dir().join(format!("spool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let root = home.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let env = env(&home);
        (home, root, env)
    }

    fn meta(worker: &str) -> Map<String, Value> {
        let mut meta = Map::new();
        meta.insert("session".into(), json!("s1"));
        if !worker.is_empty() {
            meta.insert("worker".into(), json!(worker));
        }
        meta.insert("step".into(), json!({"tok": 1}));
        meta
    }

    fn build(rel: &str, content: &str, meta: &Map<String, Value>) -> Option<Value> {
        let mut payload = meta.clone();
        payload.insert("path".into(), json!(rel));
        payload.insert("content".into(), json!(content));
        Some(Value::Object(payload))
    }

    const SERVER: &str = "http://127.0.0.1:9";

    #[test]
    fn a_failed_report_is_sent_again_with_the_files_current_content() {
        let (home, root, env) = scratch("current");
        std::fs::write(root.join("a.py"), "v1").unwrap();
        remember(&env, SERVER, "r", &root, "gone.py", &meta("")); // oldest: visited first
        std::thread::sleep(Duration::from_millis(5));
        remember(&env, SERVER, "r", &root, "a.py", &meta(""));
        remember(&env, SERVER, "r", &root, "a.py", &meta("")); // once, however often it failed
        assert_eq!(load(&spool_path(&env, SERVER, "r")).len(), 2);
        assert!(load(&spool_path(&env, "http://other", "r")).is_empty(), "another server has its own spool");
        // the file moved on before the resend: the NEW content goes
        std::fs::write(root.join("a.py"), "v2").unwrap();
        let seen = RefCell::new(Vec::new());
        let sent = resend_with(&env, SERVER, "r", Duration::from_millis(500), &root, &[], &build, &|p, _| {
            seen.borrow_mut().push(p.clone());
            Err(())
        });
        assert_eq!(sent, 0);
        let first = &seen.borrow()[0];
        assert_eq!((first["path"].clone(), first["content"].clone(), first["session"].clone()), (json!("a.py"), json!("v2"), json!("s1")));
        assert!(first.get("step").is_none(), "a resend carries no step cost: {first}");
        // still owed after the failure; the gone file is not
        let left = load(&spool_path(&env, SERVER, "r"));
        assert!(left.contains_key(&key(&root, "a.py")) && !left.contains_key(&key(&root, "gone.py")), "{left:?}");
        forget(&env, SERVER, "r", &root, "a.py");
        assert!(!spool_path(&env, SERVER, "r").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_entry_the_server_accepts_is_drained_and_goes_out_as_history() {
        let (home, root, env) = scratch("drain");
        std::fs::write(root.join("a.py"), "x").unwrap();
        std::fs::write(root.join("b.py"), "y").unwrap();
        remember(&env, SERVER, "r", &root, "a.py", &meta("w1"));
        remember(&env, SERVER, "r", &root, "b.py", &meta(""));
        let posted = RefCell::new(Vec::new());
        let sent = resend_with(&env, SERVER, "r", Duration::from_millis(500), &root, &[], &build, &|p, _| {
            posted.borrow_mut().push(p.clone());
            Ok(())
        });
        assert_eq!(sent, 2);
        // a resend never refreshes liveness: the server is told it is history
        assert!(posted.borrow().iter().all(|p| p["resent"] == json!(true)), "{:?}", posted.borrow());
        assert!(!spool_path(&env, SERVER, "r").exists(), "accepted: nothing is owed");
        // and a second run sends nothing
        let again = resend_with(&env, SERVER, "r", Duration::from_millis(500), &root, &[], &build, &|_, _| panic!("nothing owed"));
        assert_eq!(again, 0);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_ended_workers_entries_are_dropped_and_never_sent_or_spooled_again() {
        let (home, root, env) = scratch("ended");
        std::fs::write(root.join("a.py"), "x").unwrap();
        std::fs::write(root.join("b.py"), "y").unwrap();
        remember(&env, SERVER, "r", &root, "a.py", &meta("w1"));
        remember(&env, SERVER, "r", &root, "b.py", &meta("")); // the parent's own
        worker_ended(&env, SERVER, "r", "w1");
        let left = load(&spool_path(&env, SERVER, "r"));
        assert_eq!(left.len(), 1, "{left:?}");
        assert!(left.contains_key(&key(&root, "b.py")));
        // a late failure of the ended worker is not spooled
        remember(&env, SERVER, "r", &root, "a.py", &meta("w1"));
        assert_eq!(load(&spool_path(&env, SERVER, "r")).len(), 1);
        // an entry spooled behind the stop's back is dropped unsent
        let mut entries = load(&spool_path(&env, SERVER, "r"));
        entries.insert(key(&root, "a.py"), json!({"ts": now(), "rel": "a.py", "root": root.to_string_lossy(), "tries": 0, "meta": meta("w1")}));
        save(&spool_path(&env, SERVER, "r"), &entries);
        let posted = RefCell::new(Vec::new());
        resend_with(&env, SERVER, "r", Duration::from_millis(500), &root, &[], &build, &|p, _| {
            posted.borrow_mut().push(p["path"].clone());
            Ok(())
        });
        assert_eq!(*posted.borrow(), vec![json!("b.py")], "only the parent's own entry goes");
        assert!(!spool_path(&env, SERVER, "r").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_old_entry_is_dropped_unsent_and_a_slow_one_is_tried_three_times_at_most() {
        let (home, root, env) = scratch("age");
        std::fs::write(root.join("old.py"), "x").unwrap();
        std::fs::write(root.join("slow.py"), "y").unwrap();
        std::fs::write(root.join("next.py"), "z").unwrap();
        let path = spool_path(&env, SERVER, "r");
        remember(&env, SERVER, "r", &root, "slow.py", &meta(""));
        let mut entries = load(&path);
        entries.insert(key(&root, "old.py"), json!({"ts": now() - 2.0 * 3_600.0, "rel": "old.py", "root": root.to_string_lossy(), "tries": 0, "meta": meta("w1")}));
        // an entry from before v83 (no root): which tree it came from is unknown
        entries.insert("legacy.py".into(), json!({"ts": now(), "meta": meta("w1")}));
        save(&path, &entries);
        // the server takes each post but answers too late: the hook sees a failure
        let posted = RefCell::new(Vec::new());
        let slow = |p: &Value, _: Duration| {
            posted.borrow_mut().push(p["path"].as_str().unwrap_or("").to_string());
            Err(())
        };
        resend_with(&env, SERVER, "r", Duration::from_millis(500), &root, &[], &build, &slow);
        let left = load(&path);
        assert!(!left.contains_key(&key(&root, "old.py")) && !left.contains_key("legacy.py"), "{left:?}");
        assert_eq!(*posted.borrow(), vec!["slow.py".to_string()], "the old and the rootless are never sent");
        // a newer failure joins: the one that keeps failing does not stand in front of it
        std::thread::sleep(Duration::from_millis(5));
        remember(&env, SERVER, "r", &root, "next.py", &meta(""));
        resend_with(&env, SERVER, "r", Duration::from_millis(500), &root, &[], &build, &slow);
        assert_eq!(posted.borrow().last().map(String::as_str), Some("next.py"));
        for _ in 0..4 {
            resend_with(&env, SERVER, "r", Duration::from_millis(500), &root, &[], &build, &slow);
        }
        let tries = posted.borrow().iter().filter(|p| *p == "slow.py").count();
        assert_eq!(tries, MAX_TRIES as usize, "{:?}", posted.borrow());
        assert!(!path.exists(), "every entry spent its tries: {:?}", load(&path));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_worktrees_entry_is_read_from_the_worktree_and_dropped_when_it_is_removed() {
        let (home, root, env) = scratch("tree");
        let tree = home.join("worktree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(root.join("a.py"), "main checkout").unwrap();
        std::fs::write(tree.join("a.py"), "worktree").unwrap();
        remember(&env, SERVER, "r", &tree, "a.py", &meta("w1"));
        remember(&env, SERVER, "r", &root, "a.py", &meta(""));
        assert_eq!(load(&spool_path(&env, SERVER, "r")).len(), 2, "one entry per working copy");
        let posted = RefCell::new(Vec::new());
        resend_with(&env, SERVER, "r", Duration::from_millis(500), &root, &[], &build, &|p, _| {
            posted.borrow_mut().push((p.get("worker").cloned(), p["content"].clone()));
            Err(())
        });
        assert_eq!(posted.borrow()[0], (Some(json!("w1")), json!("worktree")));
        std::fs::remove_dir_all(&tree).unwrap();
        let posted = RefCell::new(Vec::new());
        resend_with(&env, SERVER, "r", Duration::from_millis(500), &root, &[], &build, &|p, _| {
            posted.borrow_mut().push(p["content"].clone());
            Ok(())
        });
        assert_eq!(*posted.borrow(), vec![json!("main checkout")], "the removed worktree's entry is not sent");
        assert!(!spool_path(&env, SERVER, "r").exists(), "{:?}", load(&spool_path(&env, SERVER, "r")));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_file_this_run_reports_live_is_owed_no_longer_and_never_resent() {
        let (home, root, env) = scratch("covered");
        std::fs::write(root.join("a.py"), "v1").unwrap();
        std::fs::write(root.join("b.py"), "v1").unwrap();
        remember(&env, SERVER, "r", &root, "a.py", &meta(""));
        remember(&env, SERVER, "r", &root, "b.py", &meta(""));
        // a later session wrote a.py and reports it in this very run: a
        // resend of it, going out first with the file as it is now, made
        // that live report look unchanged (no step on the agent's line)
        let seen = RefCell::new(Vec::new());
        let sent = resend_with(&env, SERVER, "r", Duration::from_millis(500), &root, &["a.py".to_string()], &build, &|p, _| {
            seen.borrow_mut().push(p["path"].clone());
            Ok(())
        });
        assert_eq!((sent, seen.borrow().clone()), (1, vec![json!("b.py")]), "only the uncovered file went");
        assert!(!spool_path(&env, SERVER, "r").exists(), "and nothing is owed any more");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_spool_is_bounded_in_paths_and_bytes() {
        let (home, root, env) = scratch("bound");
        for i in 0..(MAX_PATHS + 10) {
            remember(&env, SERVER, "r", &root, &format!("f{i}.py"), &meta(""));
        }
        let path = spool_path(&env, SERVER, "r");
        assert_eq!(load(&path).len(), MAX_PATHS);
        std::fs::write(&path, vec![b' '; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        assert!(load(&path).is_empty() && !path.exists(), "an oversized spool is dropped whole");
        let _ = std::fs::remove_dir_all(&home);
    }
}
