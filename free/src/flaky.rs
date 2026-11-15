//! The repo's flaky tests, as every machine on the team has seen them.
//!
//! A machine's check learns a test is flaky when its result changes while
//! the code does not (it fails, then passes on a rerun of the same tree)
//! and reports the test's NAME with two counts: runs on unchanged code it
//! was seen in, and how many of those it failed (cork-hooks flaky.rs).
//! No output, no code. This keeps them per repo, summed across machines,
//! and hands the list back on `/flaky` and `/report`, so every machine's
//! check treats those tests as flaky too and a failure on one of them alone
//! reads "not your change".
//!
//! Evidence decays: counts halve every two weeks without new reports, and a
//! test whose failures have decayed away is forgotten. A test that was
//! fixed stops being excused on its own.

use std::sync::Mutex;

use serde_json::{json, Map, Value};

use crate::store::Store;

pub const BUCKET: &str = "flaky_tests";
/// Counts halve after this long.
pub const HALF_LIFE_S: f64 = 14.0 * 86_400.0;
/// Below this many (decayed) failures a test is no longer listed.
const FORGET_BELOW: f64 = 0.5;
const MAX_ID_CHARS: usize = 200;
const MAX_PER_REPORT: usize = 50;
/// One report's counts are capped: a rerun pair is 2 runs.
const MAX_RUNS_PER_REPORT: f64 = 100.0;
const MAX_TESTS_PER_REPO: usize = 200;

/// Read-modify-write on one repo's record is serialized here: two
/// machines reporting at once must both count.
static WRITE: Mutex<()> = Mutex::new(());

fn decayed(entry: &Value, now: f64) -> (f64, f64) {
    let runs = entry.get("runs").and_then(Value::as_f64).unwrap_or(0.0);
    let fails = entry.get("fails").and_then(Value::as_f64).unwrap_or(0.0);
    let ts = entry.get("ts").and_then(Value::as_f64).unwrap_or(now);
    let factor = 0.5f64.powf(((now - ts).max(0.0)) / HALF_LIFE_S);
    (runs * factor, fails * factor)
}

/// A test id as it may be kept: one line of printable text, capped.
fn clean_id(id: &str) -> String {
    let line = id.lines().next().unwrap_or("").trim();
    if line == "check" {
        return String::new();
    }
    line.chars().filter(|c| !c.is_control()).take(MAX_ID_CHARS).collect()
}

/// The registry as listed: `[{id, runs, fails, reporters, last_s}]`, the
/// most failures first; only tests that failed AND passed on unchanged code.
pub fn registry(store: &Store, scope: &str, now: f64) -> Option<Vec<Value>> {
    let record = store.kv_get(BUCKET, scope)?;
    Some(list(&record, now))
}

fn list(record: &Value, now: f64) -> Vec<Value> {
    let mut rows: Vec<(f64, String, Value)> = Vec::new();
    if let Some(tests) = record.get("tests").and_then(Value::as_object) {
        for (id, entry) in tests {
            let (runs, fails) = decayed(entry, now);
            if fails < FORGET_BELOW || runs - fails < FORGET_BELOW {
                continue;
            }
            rows.push((
                fails,
                id.clone(),
                json!({
                    "id": id,
                    "runs": crate::compat::python_round(runs, 1),
                    "fails": crate::compat::python_round(fails, 1),
                    "reporters": entry.get("reporters").and_then(Value::as_array).map(|r| r.len()).unwrap_or(0),
                    "last_s": entry.get("ts").and_then(Value::as_f64).unwrap_or(now),
                }),
            ));
        }
    }
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.cmp(&b.1)));
    rows.into_iter().map(|(_, _, row)| row).collect()
}

/// Fold one machine's report (`tests: [{id, runs, fails}]`) into the repo's
/// registry; the registry after it. `reporter` is the caller's user id,
/// kept so the dashboard can say how many people saw it.
pub fn record(store: &Store, scope: &str, reporter: &str, tests: &[Value], now: f64) -> Vec<Value> {
    let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut record = store.kv_get(BUCKET, scope).unwrap_or_else(|| json!({"tests": {}}));
    let mut changed = false;
    {
        if !record.get("tests").map(Value::is_object).unwrap_or(false) {
            record["tests"] = json!({});
        }
        let map: &mut Map<String, Value> = record["tests"].as_object_mut().expect("an object, just set");
        for test in tests.iter().take(MAX_PER_REPORT) {
            let id = clean_id(test.get("id").and_then(Value::as_str).unwrap_or(""));
            if id.is_empty() {
                continue;
            }
            let runs = test.get("runs").and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, MAX_RUNS_PER_REPORT);
            let fails = test.get("fails").and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, runs);
            if runs <= 0.0 {
                continue;
            }
            let entry = map.entry(id).or_insert_with(|| json!({"runs": 0.0, "fails": 0.0, "first_s": now, "reporters": []}));
            let (old_runs, old_fails) = decayed(entry, now);
            entry["runs"] = json!(old_runs + runs);
            entry["fails"] = json!(old_fails + fails);
            entry["ts"] = json!(now);
            if !reporter.is_empty() {
                let reporters = entry.get_mut("reporters").and_then(Value::as_array_mut);
                match reporters {
                    Some(list) if !list.iter().any(|r| r.as_str() == Some(reporter)) && list.len() < 20 => list.push(json!(reporter)),
                    Some(_) => {}
                    None => entry["reporters"] = json!([reporter]),
                }
            }
            changed = true;
        }
        // what has decayed away is forgotten; a runaway repo keeps its worst
        map.retain(|_, entry| decayed(entry, now).1 >= FORGET_BELOW / 4.0);
        if map.len() > MAX_TESTS_PER_REPO {
            let mut by_fails: Vec<(String, f64)> = map.iter().map(|(k, e)| (k.clone(), decayed(e, now).1)).collect();
            by_fails.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            for (id, _) in by_fails.into_iter().skip(MAX_TESTS_PER_REPO) {
                map.remove(&id);
            }
        }
    }
    if changed {
        let _ = store.kv_put(BUCKET, scope, &record, now);
    }
    list(&record, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("store")
    }

    #[test]
    fn reports_from_two_machines_add_up_and_are_listed_worst_first() {
        let store = store();
        let now = 1_000_000.0;
        record(&store, "ws:r", "alice", &[json!({"id": "t::x", "runs": 2, "fails": 1})], now);
        record(&store, "ws:r", "alice", &[json!({"id": "t::y", "runs": 2, "fails": 1})], now);
        let listed = record(&store, "ws:r", "bob", &[json!({"id": "t::x", "runs": 18, "fails": 3})], now);
        assert_eq!(listed[0]["id"], json!("t::x"));
        assert_eq!(listed[0]["runs"], json!(20.0));
        assert_eq!(listed[0]["fails"], json!(4.0));
        assert_eq!(listed[0]["reporters"], json!(2));
        assert_eq!(listed[1]["id"], json!("t::y"));
        assert!(registry(&store, "ws:other", now).is_none(), "per repo");
    }

    #[test]
    fn evidence_decays_and_a_fixed_test_is_forgotten() {
        let store = store();
        let now = 1_000_000.0;
        record(&store, "ws:r", "alice", &[json!({"id": "t::x", "runs": 8, "fails": 4})], now);
        let later = registry(&store, "ws:r", now + HALF_LIFE_S).unwrap();
        assert_eq!(later[0]["fails"], json!(2.0), "halved after a half-life");
        assert_eq!(later[0]["runs"], json!(4.0));
        assert!(registry(&store, "ws:r", now + 4.0 * HALF_LIFE_S).unwrap().is_empty(), "decayed away");
    }

    #[test]
    fn only_names_and_counts_are_kept_and_nonsense_is_dropped() {
        let store = store();
        let now = 1_000_000.0;
        let listed = record(&store, "ws:r", "alice", &[
            json!({"id": "t::x\nTraceback (most recent call last)", "runs": 2, "fails": 1, "output": "secret"}),
            json!({"id": "check", "runs": 2, "fails": 1}),
            json!({"id": "t::always", "runs": 3, "fails": 3}),
            json!({"id": "t::huge", "runs": 1e9, "fails": 1e9}),
        ], now);
        let ids: Vec<&str> = listed.iter().filter_map(|r| r["id"].as_str()).collect();
        assert_eq!(ids, vec!["t::x"], "a test that never passed is broken, not flaky: {listed:?}");
        let kept = store.kv_get(BUCKET, "ws:r").unwrap();
        assert!(!kept.to_string().contains("secret") && !kept.to_string().contains("Traceback"));
    }
}
