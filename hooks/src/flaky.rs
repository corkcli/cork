//! Flaky tests: learned on one machine, known to the whole team.
//!
//! A test is flaky when its result changes while the code does not: it
//! fails and then passes on a rerun with no write in between, or two checks
//! on the same tree disagree about it. The check (check.rs) watches for
//! both. What it learns goes to the server as the test's NAME and two
//! counts (`runs`, `fails`), never the output and never any code, through
//! `http::post` like every other request, so it shows in `cork audit`.
//!
//! The server keeps a per-repo registry with the evidence and a decay
//! (cork-rs flaky.rs) and hands it back on every `/flaky` and `/report`
//! answer; this machine keeps the last one it heard, so every machine's
//! check treats the team's flaky tests as flaky too, and a failure on one
//! of them alone is "not your change".
//!
//! The cache lives beside the credentials (`~/.collide/flaky/<digest>.json`):
//! `team` (the registry as last heard, `team_at`), `local` (what this
//! machine saw itself) and `unsent` (counts not yet acknowledged).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::config::{self, Config, Env};
use crate::report::{save_json, sha256_hex, user_agent, MAP_EXTS};

/// A failing check is run once more, on the same tree, when it is this
/// quick: the rerun is what tells a flaky test from a broken one.
pub const RERUN_MAX_S: f64 = 20.0;
/// The registry as last heard is trusted this long before a check asks again.
const TEAM_TTL_S: f64 = 900.0;
/// A name, not a log line.
pub const MAX_ID_CHARS: usize = 200;
const MAX_TESTS: usize = 50;
/// A tree too big to fingerprint by walking falls back to git.
const MAX_FINGERPRINT_FILES: usize = 20_000;
const SKIP_DIRS: [&str; 9] =
    ["node_modules", "target", "__pycache__", "venv", "dist", "build", "vendor", "site-packages", "coverage"];

/// What is known about one flaky test: how many runs on unchanged code it
/// was seen in, and how many of those it failed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Evidence {
    pub runs: f64,
    pub fails: f64,
}

impl Evidence {
    fn add(&mut self, other: Evidence) {
        self.runs += other.runs;
        self.fails += other.fails;
    }

    /// "4/20": whole runs, never a failure count of zero for a flaky test.
    pub fn ratio(&self) -> String {
        let fails = self.fails.round().max(1.0) as u64;
        let runs = (self.runs.round() as u64).max(fails + 1);
        format!("{fails}/{runs}")
    }
}

/// A test id as it may leave this machine: a single trimmed line, capped;
/// "" for what is not a name at all.
pub fn clean_id(id: &str) -> String {
    let line = id.lines().next().unwrap_or("").trim();
    if line.is_empty() || line == "check" {
        return String::new();
    }
    line.chars().filter(|c| !c.is_control()).take(MAX_ID_CHARS).collect()
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn cache_path(env: &Env, server: &str, repo_id: &str) -> PathBuf {
    let digest = sha256_hex(format!("{server}\n{repo_id}").as_bytes());
    let base = config::credentials_path(env).parent().map(Path::to_path_buf).unwrap_or_default();
    base.join("flaky").join(format!("{}.json", &digest[..16]))
}

fn load(env: &Env, cfg: &Config) -> Map<String, Value> {
    config::load_json(&cache_path(env, &cfg.server, &cfg.repo_id)).as_object().cloned().unwrap_or_default()
}

fn evidence_map(value: Option<&Value>) -> BTreeMap<String, Evidence> {
    let mut out = BTreeMap::new();
    if let Some(map) = value.and_then(Value::as_object) {
        for (id, e) in map {
            let runs = e.get("runs").and_then(Value::as_f64).unwrap_or(0.0);
            let fails = e.get("fails").and_then(Value::as_f64).unwrap_or(0.0);
            out.insert(id.clone(), Evidence { runs, fails });
        }
    }
    out
}

fn evidence_json(map: &BTreeMap<String, Evidence>) -> Value {
    let mut out = Map::new();
    for (id, e) in map {
        out.insert(id.clone(), json!({"runs": e.runs, "fails": e.fails}));
    }
    Value::Object(out)
}

/// The registry in an answer (`flaky: [{id, runs, fails}]`), by id.
fn registry_of(answer: &Value) -> Option<BTreeMap<String, Evidence>> {
    let list = answer.get("flaky")?.as_array()?;
    let mut out = BTreeMap::new();
    for row in list {
        let id = row.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let runs = row.get("runs").and_then(Value::as_f64).unwrap_or(0.0);
        let fails = row.get("fails").and_then(Value::as_f64).unwrap_or(0.0);
        out.insert(id.to_string(), Evidence { runs, fails });
    }
    Some(out)
}

/// An answer carried the repo's registry (`/report`, `/flaky`): kept for
/// the next check. Called from `http::post` for every answer.
pub fn learn(server: &str, sent: &Value, answer: &Value) {
    let Some(team) = registry_of(answer) else { return };
    let repo_id = sent.get("repo_id").and_then(Value::as_str).unwrap_or("");
    if server.is_empty() || repo_id.is_empty() {
        return;
    }
    let env = config::env_map();
    let path = cache_path(&env, server, repo_id);
    let mut cache = config::load_json(&path).as_object().cloned().unwrap_or_default();
    cache.insert("team".into(), evidence_json(&team));
    cache.insert("team_at".into(), json!(now_s()));
    save_json(&path, &Value::Object(cache));
}

/// Every test known to be flaky in this repo, with its evidence: the
/// team's registry (asked again when the copy here is stale) and what this
/// machine saw itself; the larger count wins.
pub fn known(env: &Env, cfg: &Config) -> BTreeMap<String, Evidence> {
    if cfg.server.is_empty() || cfg.repo_id.is_empty() {
        return BTreeMap::new();
    }
    let mut cache = load(env, cfg);
    let fresh = cache.get("team_at").and_then(Value::as_f64).map(|t| now_s() - t < TEAM_TTL_S).unwrap_or(false);
    if !fresh && cfg.usable() {
        // a pull is a report of nothing; the answer lands through `learn`
        if crate::http::post(&cfg.server, "/flaky", &cfg.token, &user_agent(),
            &json!({"repo_id": cfg.repo_id, "tests": []}), Duration::from_secs(3)).is_ok()
        {
            cache = load(env, cfg);
        }
    }
    let mut out = evidence_map(cache.get("team"));
    for (id, mine) in evidence_map(cache.get("local")) {
        let slot = out.entry(id).or_default();
        if mine.runs > slot.runs {
            *slot = mine;
        }
    }
    out
}

/// What two runs on the same tree say about each test: a test whose result
/// changed is newly flaky (both runs count); one already known counts the
/// later run. Pure, so it is tested on its own.
pub fn observe(
    before: &[String], after: &[String], known: &BTreeSet<String>,
) -> BTreeMap<String, Evidence> {
    let failed_before: BTreeSet<&str> = before.iter().map(String::as_str).collect();
    let failed_after: BTreeSet<&str> = after.iter().map(String::as_str).collect();
    let mut ids: BTreeSet<String> = known.clone();
    ids.extend(before.iter().cloned());
    ids.extend(after.iter().cloned());
    let mut out = BTreeMap::new();
    for id in ids {
        if clean_id(&id) != id {
            continue; // the catch-all "check", or not a name we would send
        }
        let b = failed_before.contains(id.as_str());
        let a = failed_after.contains(id.as_str());
        let fail = |x: bool| if x { 1.0 } else { 0.0 };
        if known.contains(&id) {
            out.insert(id, Evidence { runs: 1.0, fails: fail(a) });
        } else if a != b {
            out.insert(id, Evidence { runs: 2.0, fails: fail(a) + fail(b) });
        }
    }
    out
}

/// Fold what was observed into this machine's own record and the counts
/// still to send; true when a test became flaky here for the first time.
pub fn record(env: &Env, cfg: &Config, seen: &BTreeMap<String, Evidence>) -> bool {
    if seen.is_empty() || cfg.server.is_empty() || cfg.repo_id.is_empty() {
        return false;
    }
    let path = cache_path(env, &cfg.server, &cfg.repo_id);
    let mut cache = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let mut local = evidence_map(cache.get("local"));
    let mut unsent = evidence_map(cache.get("unsent"));
    let mut new = false;
    for (id, e) in seen {
        new |= !local.contains_key(id);
        local.entry(id.clone()).or_default().add(*e);
        unsent.entry(id.clone()).or_default().add(*e);
    }
    cache.insert("local".into(), evidence_json(&local));
    cache.insert("unsent".into(), evidence_json(&unsent));
    save_json(&path, &Value::Object(cache));
    new
}

/// Send the counts not yet acknowledged: names and numbers, nothing else.
/// The answer is the team's registry, kept through `learn`.
pub fn flush(env: &Env, cfg: &Config) {
    if !cfg.usable() {
        return;
    }
    let path = cache_path(env, &cfg.server, &cfg.repo_id);
    let cache = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let unsent = evidence_map(cache.get("unsent"));
    if unsent.is_empty() {
        return;
    }
    let tests: Vec<Value> = unsent
        .iter()
        .take(MAX_TESTS)
        .map(|(id, e)| json!({"id": id, "runs": e.runs.round() as u64, "fails": e.fails.round() as u64}))
        .collect();
    let sent: BTreeSet<String> = unsent.keys().take(MAX_TESTS).cloned().collect();
    if crate::http::post(&cfg.server, "/flaky", &cfg.token, &user_agent(),
        &json!({"repo_id": cfg.repo_id, "tests": tests}), Duration::from_secs(5)).is_err()
    {
        return; // kept for the next check
    }
    let mut cache = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let mut left = evidence_map(cache.get("unsent"));
    left.retain(|id, _| !sent.contains(id));
    cache.insert("unsent".into(), evidence_json(&left));
    save_json(&path, &Value::Object(cache));
}

/// The tree's state, so two runs can be known to have seen the same code:
/// every source file's path, size and modification time. `None` when the
/// tree is too big to walk and git cannot say either.
pub fn fingerprint(root: &Path) -> Option<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(entry.path());
                }
                continue;
            }
            let lower = name.to_ascii_lowercase();
            if !kind.is_file() || !MAP_EXTS.iter().any(|ext| lower.ends_with(ext)) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
            let rel = entry.path().strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            rows.push(format!("{rel}\t{}\t{mtime}", meta.len()));
            if rows.len() > MAX_FINGERPRINT_FILES {
                return git_fingerprint(root);
            }
        }
    }
    rows.sort();
    Some(sha256_hex(rows.join("\n").as_bytes())[..16].to_string())
}

fn git_fingerprint(root: &Path) -> Option<String> {
    let head = crate::git::git(root, &["rev-parse", "HEAD"], Duration::from_secs(5))?;
    let status = crate::git::git(root, &["status", "--porcelain=v1", "-uall"], Duration::from_secs(10))?;
    let diff = crate::git::git(root, &["diff", "HEAD"], Duration::from_secs(10)).unwrap_or_default();
    Some(sha256_hex(format!("{head}\n{status}\n{diff}").as_bytes())[..16].to_string())
}

/// The verdict's words for flaky failures: "`test_x` failed 4/20 runs" or,
/// for one test, "failed 4/20 runs" alone.
pub fn evidence_text(ids: &[String], known: &BTreeMap<String, Evidence>) -> String {
    let ratio = |id: &String| known.get(id).map(Evidence::ratio).unwrap_or_else(|| "1/2".to_string());
    if ids.len() == 1 {
        return format!("failed {} runs on unchanged code", ratio(&ids[0]));
    }
    let parts: Vec<String> = ids.iter().map(|id| format!("`{id}` failed {}", ratio(id))).collect();
    format!("{} runs on unchanged code", parts.join(", "))
}

pub fn backticked(ids: &[String]) -> String {
    let quoted: Vec<String> = ids.iter().map(|id| format!("`{id}`")).collect();
    match quoted.len() {
        0 => String::new(),
        1 => quoted[0].clone(),
        n => format!("{} and {}", quoted[..n - 1].join(", "), quoted[n - 1]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_test_that_flips_on_the_same_tree_is_flaky_and_both_runs_count() {
        let seen = observe(&ids(&["t::x", "t::real"]), &ids(&["t::real"]), &BTreeSet::new());
        assert_eq!(seen.get("t::x"), Some(&Evidence { runs: 2.0, fails: 1.0 }));
        assert!(!seen.contains_key("t::real"), "failing both times is broken, not flaky");
    }

    #[test]
    fn a_known_flaky_test_counts_every_later_run_pass_or_fail() {
        let known: BTreeSet<String> = ["t::x".to_string()].into_iter().collect();
        assert_eq!(observe(&[], &[], &known).get("t::x"), Some(&Evidence { runs: 1.0, fails: 0.0 }));
        assert_eq!(observe(&[], &ids(&["t::x"]), &known).get("t::x"), Some(&Evidence { runs: 1.0, fails: 1.0 }));
    }

    #[test]
    fn the_catch_all_and_non_names_are_never_learned() {
        let seen = observe(&ids(&["check"]), &[], &BTreeSet::new());
        assert!(seen.is_empty());
        assert_eq!(clean_id("  t::x - AssertionError\nboom"), "t::x - AssertionError");
        assert_eq!(clean_id(&"a".repeat(500)).len(), MAX_ID_CHARS);
        assert_eq!(clean_id("check"), "");
    }

    #[test]
    fn the_ratio_is_whole_runs_and_never_zero_failures() {
        assert_eq!(Evidence { runs: 20.2, fails: 3.7 }.ratio(), "4/20");
        assert_eq!(Evidence { runs: 0.0, fails: 0.2 }.ratio(), "1/2");
    }

    #[test]
    fn the_fingerprint_changes_with_a_source_file_and_ignores_the_rest() {
        let dir = std::env::temp_dir().join(format!("collide-flaky-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".pytest_cache")).unwrap();
        std::fs::write(dir.join("src/a.py"), "x = 1\n").unwrap();
        let first = fingerprint(&dir).unwrap();
        std::fs::write(dir.join(".pytest_cache/state.py"), "noise").unwrap();
        std::fs::write(dir.join("counter.txt"), "1").unwrap();
        assert_eq!(fingerprint(&dir).unwrap(), first, "caches and non-source files are not the code");
        std::fs::write(dir.join("src/a.py"), "x = 22\n").unwrap();
        assert_ne!(fingerprint(&dir).unwrap(), first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_words_name_each_test_and_its_evidence() {
        let mut known = BTreeMap::new();
        known.insert("a".to_string(), Evidence { runs: 20.0, fails: 4.0 });
        known.insert("b".to_string(), Evidence { runs: 9.0, fails: 2.0 });
        assert_eq!(evidence_text(&ids(&["a"]), &known), "failed 4/20 runs on unchanged code");
        assert_eq!(evidence_text(&ids(&["a", "b"]), &known), "`a` failed 4/20, `b` failed 2/9 runs on unchanged code");
        assert_eq!(backticked(&ids(&["a", "b", "c"])), "`a`, `b` and `c`");
    }

    #[test]
    fn learned_counts_are_kept_until_sent_and_the_registry_is_heard_from_answers() {
        let home = std::env::temp_dir().join(format!("collide-flaky-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), home.to_string_lossy().into_owned());
        env.insert("HOME".into(), home.to_string_lossy().into_owned());
        let cfg = Config { server: "http://127.0.0.1:9".into(), token: "t".into(), repo_id: "r".into(),
            verify: String::new(), local_parse: true, machine_local: false };
        let mut seen = BTreeMap::new();
        seen.insert("t::x".to_string(), Evidence { runs: 2.0, fails: 1.0 });
        assert!(record(&env, &cfg, &seen), "first time this machine saw it");
        assert!(!record(&env, &cfg, &seen));
        let cache = load(&env, &cfg);
        assert_eq!(evidence_map(cache.get("unsent")).get("t::x"), Some(&Evidence { runs: 4.0, fails: 2.0 }));
        // nothing answers on port 9: the counts wait for the next check
        flush(&env, &cfg);
        assert_eq!(evidence_map(load(&env, &cfg).get("unsent")).len(), 1);
        let answer = json!({"flaky": [{"id": "t::y", "runs": 20, "fails": 4}]});
        assert_eq!(registry_of(&answer).unwrap().get("t::y"), Some(&Evidence { runs: 20.0, fails: 4.0 }));
        let _ = std::fs::remove_dir_all(&home);
    }
}
