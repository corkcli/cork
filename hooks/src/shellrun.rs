//! What a shell command changed: proven, not guessed by the clock.
//!
//! A script names none of the files it writes, so the report credited a
//! shell command with every dirty file whose mtime fell in its last 20
//! seconds. In a checkout several sessions share, that was someone else's
//! work as often as not: one session's `npm test` took another's fresh
//! CLAUDE.md paragraph, the commit guard then blocked the real author, and
//! the server told her about "her" imports in files she never wrote
//! (Acme, hooks v88). A runner the idiom list did not know (`tsx`) wrote
//! files nobody was credited with at all.
//!
//! Now the gate, just before a shell command that may write, notes every
//! dirty file's size and mtime and keeps a copy of each (by content, so an
//! unchanged file is copied once). The report, after, takes only what
//! changed in between, with that copy as the exact before. A file another
//! session wrote meanwhile (an edit of its own, or a command of its own
//! that ran alongside) is nobody's, unless only this command names it:
//! unattributed lines never block a commit, and a wrong owner does.
//!
//! Layout, beside the journal: `shell/runs/<session>-<call>.json`,
//! `shell/blobs/<hash>`, `shell/stat.json` (each dirty path's last hash, by
//! size and mtime). A run is kept an hour after it ends, so a later report
//! can see what overlapped it. Fail open throughout: no git, a timeout or a
//! huge tree means no snapshot, and the report falls back to what the
//! command names.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::journal::{self, now_s, path_key, session_tag, sha16};

/// Dirty files a snapshot covers; a tree with more gets none.
const MAX_DIRTY: usize = 5000;
/// New bytes one snapshot may copy; past it a file is noted, not copied.
const MAX_COPY_BYTES: u64 = 16 * 1024 * 1024;
/// Files one command is credited with.
pub const MAX_PROVEN: usize = 200;
const LIST_TIMEOUT: Duration = Duration::from_millis(400);
/// A run that never heard its end (an interrupted call, a background
/// command) stops counting as running after this.
pub const OPEN_S: f64 = 15.0 * 60.0;
/// How long a finished run is kept for the overlap check.
const KEEP_S: f64 = 3600.0;
const GC_EVERY_S: f64 = 600.0;
/// A file whose mtime lands this close to the run's start may have changed
/// without its mtime moving (a coarse clock): its content decides.
const RACY_S: f64 = 2.0;

fn shell_dir(root: &Path, env: &Env) -> PathBuf {
    journal::journal_dir(root, env).join("shell")
}

/// A call's key: the harness's id for it, else its command.
pub fn call_key(tool_use_id: &str, command: &str) -> String {
    sha16(if tool_use_id.is_empty() { command.as_bytes() } else { tool_use_id.as_bytes() })
}

fn run_path(root: &Path, session: &str, call: &str, env: &Env) -> PathBuf {
    shell_dir(root, env).join("runs").join(format!("{}-{call}.json", session_tag(session)))
}

/// (mtime in ns, size) of a regular file.
fn stat_of(path: &Path) -> Option<(u128, u64)> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    Some((mtime, meta.len()))
}

/// The repo's dirty files (modified, or new and not ignored), repo-relative.
/// `None` when git is slow, absent, or the list is too long to be useful.
pub fn dirty(root: &Path) -> Option<Vec<String>> {
    let out = journal::git_bytes(root, &["ls-files", "-m", "-o", "--exclude-standard", "-z"], None, &[], LIST_TIMEOUT)?;
    let mut seen: HashSet<String> = HashSet::new();
    let rels: Vec<String> = String::from_utf8_lossy(&out)
        .split('\0')
        .filter(|r| !r.is_empty() && seen.insert(r.to_string()))
        .map(str::to_string)
        .collect();
    (rels.len() <= MAX_DIRTY).then_some(rels)
}

fn load(path: &Path) -> Value {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

/// Written whole or not at all: a reader never sees half a file.
fn save(path: &Path, value: &Value) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, value.to_string()).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn stat_entry(v: &Value) -> Option<(u128, u64, String)> {
    let a = v.as_array()?;
    let mtime = a.first()?.as_str()?.parse().ok()?;
    let size = a.get(1)?.as_u64()?;
    Some((mtime, size, a.get(2).and_then(Value::as_str).unwrap_or("").to_string()))
}

fn stat_json(mtime: u128, size: u64, hash: &str) -> Value {
    json!([mtime.to_string(), size, hash])
}

/// The gate, just before a shell command runs: what every dirty file is
/// now. Silent and bounded; any failure leaves no snapshot.
pub fn begin(root: &Path, session: &str, call: &str, command: &str, cwd: &Path, background: bool, env: &Env) {
    if session_tag(session).is_empty() {
        return;
    }
    let dir = shell_dir(root, env);
    gc(&dir);
    let Some(rels) = dirty(root) else { return };
    let blobs = dir.join("blobs");
    if std::fs::create_dir_all(&blobs).is_err() {
        return;
    }
    let cache_path = dir.join("stat.json");
    let cache = load(&cache_path);
    let mut next_cache = serde_json::Map::new();
    let mut files = serde_json::Map::new();
    let mut copied: u64 = 0;
    let now = now_s();
    for rel in rels {
        let Some((mtime, size)) = stat_of(&root.join(&rel)) else { continue };
        // a file written moments ago may change again without its mtime
        // moving: its copy is taken afresh (git's racy-clean rule)
        let settled = (mtime as f64) / 1e9 < now - RACY_S;
        let known = cache.get(&rel).and_then(stat_entry)
            .filter(|(m, s, h)| settled && *m == mtime && *s == size && !h.is_empty() && blobs.join(h).is_file())
            .map(|(_, _, h)| h);
        let hash = match known {
            Some(hash) => hash,
            None if size as usize <= journal::MAX_BYTES && copied + size <= MAX_COPY_BYTES => {
                match std::fs::read(root.join(&rel)) {
                    Ok(bytes) => {
                        let hash = sha16(&bytes);
                        let blob = blobs.join(&hash);
                        if !blob.is_file() {
                            copied += size;
                            let tmp = blob.with_extension(format!("tmp{}", std::process::id()));
                            if std::fs::write(&tmp, &bytes).is_err() || std::fs::rename(&tmp, &blob).is_err() {
                                let _ = std::fs::remove_file(&tmp);
                                String::new()
                            } else {
                                hash
                            }
                        } else {
                            hash
                        }
                    }
                    Err(_) => String::new(),
                }
            }
            None => String::new(),
        };
        if !hash.is_empty() {
            next_cache.insert(rel.clone(), stat_json(mtime, size, &hash));
        }
        files.insert(rel, stat_json(mtime, size, &hash));
    }
    save(&cache_path, &Value::Object(next_cache));
    save(&run_path(root, session, call, env), &json!({
        "s": session, "start": now_s(), "end": Value::Null, "bg": background,
        "cmd": command.chars().take(4000).collect::<String>(), "cwd": cwd.to_string_lossy(), "files": files,
    }));
}

/// A run another session made, as the overlap check needs it.
struct Other {
    start: f64,
    end: Option<f64>,
    command: String,
    cwd: PathBuf,
}

fn runs_of_others(dir: &Path, session: &str) -> Vec<Other> {
    let mine = format!("{}-", session_tag(session));
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir.join("runs")).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&mine) || !name.ends_with(".json") {
            continue;
        }
        let run = load(&entry.path());
        let Some(start) = run.get("start").and_then(Value::as_f64) else { continue };
        if journal::same_session(run.get("s").and_then(Value::as_str).unwrap_or(""), session) {
            continue;
        }
        let end = run.get("end").and_then(Value::as_f64).filter(|_| !run.get("bg").and_then(Value::as_bool).unwrap_or(false));
        out.push(Other {
            start,
            end,
            command: run.get("cmd").and_then(Value::as_str).unwrap_or("").to_string(),
            cwd: PathBuf::from(run.get("cwd").and_then(Value::as_str).unwrap_or("")),
        });
    }
    out
}

/// Whether a run of another session was going at some point in `[from, to]`.
fn overlaps(other: &Other, from: f64, to: f64) -> bool {
    let end = other.end.unwrap_or_else(|| (other.start + OPEN_S).min(now_s()));
    other.start <= to && end >= from
}

/// Another session wrote `rel` at or after `since` through an edit tool or
/// a command of its own: its journal says so, or its gate's copy of the
/// file (taken just before its write) is that new.
fn written_by_other(root: &Path, rel: &str, session: &str, since: f64, entries: &[journal::Entry], env: &Env) -> bool {
    if entries.iter().any(|e| e.path == rel && e.t >= since && !journal::same_session(&e.session, session)) {
        return true;
    }
    let mine = format!("{}-", session_tag(session));
    let key = path_key(rel);
    let pre = journal::journal_dir(root, env).join("pre");
    std::fs::read_dir(pre).into_iter().flatten().flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stem = name.strip_suffix(".none").unwrap_or(&name);
        !name.starts_with(&mine)
            && stem.ends_with(&format!("-{key}"))
            && entry.metadata().and_then(|m| m.modified()).ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .is_some_and(|t| t.as_secs_f64() >= since)
    })
}

/// What one shell command changed, as the report journals and reports it.
#[derive(Debug, Default)]
pub struct Proven {
    /// (repo path, the file just before the command: `None` when no copy
    /// could be kept, and the journal finds its own before)
    pub files: Vec<(String, Option<Vec<u8>>)>,
    /// changed while it ran, but another session may have written it
    pub contested: Vec<String>,
}

/// The report, after a shell command: what it changed, from the gate's
/// snapshot. `None` when the gate took none (the caller falls back).
pub fn finish(root: &Path, session: &str, call: &str, env: &Env) -> Option<Proven> {
    let path = run_path(root, session, call, env);
    let mut run = load(&path);
    let start = run.get("start").and_then(Value::as_f64)?;
    let now = now_s();
    // a command that keeps running in the background is still at work
    if !run.get("bg").and_then(Value::as_bool).unwrap_or(false) {
        run["end"] = json!(now);
        save(&path, &run);
    }
    let rels = dirty(root)?;
    let dir = shell_dir(root, env);
    let blobs = dir.join("blobs");
    let command = run.get("cmd").and_then(Value::as_str).unwrap_or("").to_string();
    let cwd = PathBuf::from(run.get("cwd").and_then(Value::as_str).unwrap_or(""));
    let before: BTreeMap<String, (u128, u64, String)> = run.get("files").and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), stat_entry(v)?))).collect())
        .unwrap_or_default();
    let mut changed: Vec<(String, Option<Vec<u8>>)> = Vec::new();
    for rel in rels {
        let now_stat = stat_of(&root.join(&rel));
        let prior = match before.get(&rel) {
            // dirty only now: the command made it so, from HEAD's copy
            None => match now_stat {
                Some(_) => journal::head_bytes(root, &rel, Duration::from_millis(300)),
                None => continue,
            },
            Some((mtime, size, hash)) => {
                let copy = (!hash.is_empty()).then(|| std::fs::read(blobs.join(hash)).ok()).flatten();
                let same_stat = now_stat == Some((*mtime, *size));
                let racy = (*mtime as f64) / 1e9 >= start - RACY_S;
                let same = same_stat && !(racy && copy.as_ref().is_some_and(|c| std::fs::read(root.join(&rel)).ok().as_ref() != Some(c)));
                if same {
                    continue;
                }
                copy
            }
        };
        changed.push((rel, prior));
    }
    if changed.is_empty() {
        return Some(Proven::default());
    }
    let entries = journal::load(root, env);
    let others: Vec<Other> = runs_of_others(&dir, session).into_iter().filter(|o| overlaps(o, start, now)).collect();
    let mut proven = Proven::default();
    for (rel, prior) in changed {
        let contested = written_by_other(root, &rel, session, start, &entries, env)
            || (!others.is_empty()
                && (!crate::shell::names_path(&command, &cwd, root, &rel)
                    || others.iter().any(|o| crate::shell::names_path(&o.command, &o.cwd, root, &rel))));
        if contested {
            proven.contested.push(rel);
        } else {
            proven.files.push((rel, prior));
        }
    }
    // what the command names first: those are surely its own
    proven.files.sort_by_key(|(rel, _)| (!crate::shell::names_path(&command, &cwd, root, rel), rel.clone()));
    proven.files.truncate(MAX_PROVEN);
    Some(proven)
}

/// Whether another session in this checkout wrote anything, or ran a
/// command, since `since`: the clock's guess about an unnamed file is then
/// no evidence of whose it is.
pub fn others_at_work(root: &Path, session: &str, since: f64, env: &Env) -> bool {
    let dir = shell_dir(root, env);
    if runs_of_others(&dir, session).iter().any(|o| overlaps(o, since, now_s())) {
        return true;
    }
    journal::load(root, env).iter().any(|e| e.t >= since && !journal::same_session(&e.session, session))
}

/// `others_at_work`'s per-file twin for the fallback: another session
/// wrote this file since `since`.
pub fn other_wrote(root: &Path, rel: &str, session: &str, since: f64, env: &Env) -> bool {
    written_by_other(root, rel, session, since, &journal::load(root, env), env)
}

fn age_s(path: &Path) -> f64 {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
        .and_then(|t| t.elapsed().ok())
        .map_or(f64::MAX, |d| d.as_secs_f64())
}

/// Every few minutes: runs past their keep, and copies nothing refers to.
fn gc(dir: &Path) {
    let marker = dir.join("gc");
    if age_s(&marker) < GC_EVERY_S {
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&marker, b"");
    let now = now_s();
    let mut keep: HashSet<String> = HashSet::new();
    let mut note = |files: Option<&serde_json::Map<String, Value>>| {
        for v in files.into_iter().flat_map(|m| m.values()) {
            if let Some((_, _, h)) = stat_entry(v) {
                keep.insert(h);
            }
        }
    };
    for entry in std::fs::read_dir(dir.join("runs")).into_iter().flatten().flatten() {
        let run = load(&entry.path());
        let start = run.get("start").and_then(Value::as_f64).unwrap_or(0.0);
        let last = run.get("end").and_then(Value::as_f64).unwrap_or(start + OPEN_S);
        if now - last > KEEP_S || age_s(&entry.path()) > KEEP_S + OPEN_S {
            let _ = std::fs::remove_file(entry.path());
        } else {
            note(run.get("files").and_then(Value::as_object));
        }
    }
    note(load(&dir.join("stat.json")).as_object());
    for entry in std::fs::read_dir(dir.join("blobs")).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name) && age_s(&entry.path()) > GC_EVERY_S {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The repo a shell call works in, found the way the report finds it (so
/// both name the same run), and only where Cork is on.
pub fn root_for(cwd: &Path, env: &Env) -> Option<PathBuf> {
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project_dir.is_empty()).then(|| PathBuf::from(&project_dir));
    let root = config::find_repo_root(&[Some(cwd.to_path_buf()), project.clone()])
        .unwrap_or_else(|| project.unwrap_or_else(|| cwd.to_path_buf()));
    config::config(Some(&root), env).usable().then_some(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn repo(name: &str) -> (PathBuf, Env) {
        let base = std::env::temp_dir().join(format!("cork-shellrun-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("app");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(base.join("home")).unwrap();
        let git = |args: &[&str]| {
            assert!(Command::new("git").args(args).current_dir(&root).stdout(Stdio::null()).stderr(Stdio::null())
                .status().unwrap().success());
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        git(&["add", "-A"]);
        git(&["-c", "user.email=t@e", "-c", "user.name=t", "commit", "-qm", "init"]);
        let env: Env = [("HOME".to_string(), base.join("home").to_string_lossy().into_owned())].into_iter().collect();
        (root, env)
    }

    /// A rewrite that keeps the size and lands on the same mtime (a coarse
    /// clock, or a tool that restores it) is still seen: near the run's start
    /// the content decides.
    #[test]
    fn a_change_that_keeps_size_and_mtime_is_still_seen() {
        let (root, env) = repo("racy");
        let file = root.join("a.txt");
        std::fs::write(&file, "one\nTWO\n").unwrap();
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        begin(&root, "sess-aaaa-1", "c1", "tsx fix.ts", &root, false, &env);
        std::fs::write(&file, "ONE\nTWO\n").unwrap();
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        let proven = finish(&root, "sess-aaaa-1", "c1", &env).expect("a snapshot");
        assert_eq!(proven.files.len(), 1);
        assert_eq!(proven.files[0].0, "a.txt");
        assert_eq!(proven.files[0].1.as_deref(), Some(&b"one\nTWO\n"[..]), "the before is the copy, not HEAD");
        // and an untouched dirty file is not the command's
        begin(&root, "sess-aaaa-1", "c2", "npm test", &root, false, &env);
        let proven = finish(&root, "sess-aaaa-1", "c2", &env).unwrap();
        assert!(proven.files.is_empty() && proven.contested.is_empty());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// A file made dirty by the command is credited against HEAD; no
    /// snapshot means no answer, so the caller falls back.
    #[test]
    fn a_newly_dirty_file_starts_from_head_and_no_snapshot_is_no_answer() {
        let (root, env) = repo("head");
        begin(&root, "sess-bbbb-2", "c1", "make", &root, false, &env);
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(root.join("new.txt"), "fresh\n").unwrap();
        let proven = finish(&root, "sess-bbbb-2", "c1", &env).unwrap();
        let got: BTreeMap<String, Option<Vec<u8>>> = proven.files.into_iter().collect();
        assert_eq!(got.get("a.txt").cloned().flatten().as_deref(), Some(&b"one\ntwo\n"[..]));
        assert_eq!(got.get("new.txt").cloned().flatten().as_deref(), Some(&b""[..]));
        assert!(finish(&root, "sess-bbbb-2", "never-gated", &env).is_none());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// Runs long over are swept, with the copies only they kept; a fresh
    /// run and its copies stay.
    #[test]
    fn old_runs_and_their_copies_are_swept() {
        let (root, env) = repo("gc");
        std::fs::write(root.join("a.txt"), "dirty\n").unwrap();
        begin(&root, "sess-cccc-3", "c1", "make", &root, false, &env);
        let dir = shell_dir(&root, &env);
        let old = dir.join("runs").join("old0-x.json");
        save(&old, &json!({"s": "old0", "start": now_s() - 3.0 * KEEP_S, "end": now_s() - 2.0 * KEEP_S, "files": {"b.txt": ["1", 1, "deadbeefdeadbeef"]}}));
        let orphan = dir.join("blobs").join("deadbeefdeadbeef");
        std::fs::write(&orphan, "x").unwrap();
        let past = std::time::SystemTime::now() - Duration::from_secs(2 * GC_EVERY_S as u64);
        std::fs::File::options().write(true).open(&orphan).unwrap().set_modified(past).unwrap();
        std::fs::File::options().write(true).open(dir.join("gc")).unwrap().set_modified(past).unwrap();
        gc(&dir);
        assert!(!old.exists(), "the old run is gone");
        assert!(!orphan.exists(), "and the copy only it kept");
        let live: Vec<_> = std::fs::read_dir(dir.join("blobs")).unwrap().flatten().collect();
        assert_eq!(live.len(), 1, "the fresh run's copy stays");
        assert!(dir.join("runs").join(format!("{}-c1.json", session_tag("sess-cccc-3"))).exists());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }
}
