//! Briefings that learn, per repo, from what agents did with them.
//!
//! The hook settles every briefed path once (`briefstat.rs`): written without
//! a read (hit), read and then written (needed), read and never written
//! (miss), never opened at all (unused); and at a session's end it names the
//! files the session wrote that no briefing showed (sought). Three rules turn
//! those outcomes into the next briefing, each bounded and each said in the
//! answer's `learned` field so the change is never silent:
//!
//! * a module agents routinely leave unopened is rendered short (its header
//!   and what changed), or, as a dependent nobody named, only named in the
//!   coverage tail;
//! * a module agents keep reading after being briefed gets its exact facts in
//!   full: every symbol, whole docstrings, and the line span of each, so the
//!   read that remains is the few lines that matter;
//! * a file agents went looking for after a briefing that did not show it is
//!   added the next time a prompt names what those sessions' prompts named.
//!
//! Every count halves every HALF_LIFE_S, so the repo's habits are what the
//! briefing follows, not its history; nothing acts on fewer than MIN_EVIDENCE
//! sessions' worth; and a module the prompt names outright is never dropped.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::store::{now, Store};

pub const BUCKET: &str = "brieflearn";
pub const SOUGHT_BUCKET: &str = "briefsought";
/// How fast an outcome is forgotten: half its weight every two weeks.
const HALF_LIFE_S: f64 = 14.0 * 86_400.0;
/// Decayed outcomes a path needs before any rule acts on it.
const MIN_EVIDENCE: f64 = 3.0;
/// Shares of a path's outcomes that make it ignored / re-read.
const IGNORED_SHARE: f64 = 0.7;
const REREAD_SHARE: f64 = 0.5;
/// Decayed sessions that must have sought a file under a name before a
/// prompt naming it is briefed that file.
const SOUGHT_MIN: f64 = 1.5;
/// Bounds: files remembered per name, names per session, files added per briefing.
const SOUGHT_PER_NAME: usize = 8;
const NAMES_PER_SESSION: usize = 20;
pub const MAX_ADDED: usize = 2;
/// The names a session's prompts used, kept this long for its outcome report.
const NAMES_TTL_S: f64 = 12.0 * 3600.0;

const KINDS: [&str; 4] = ["hit", "needed", "miss", "unused"];

/// What the outcomes say about one path's place in a briefing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// No rule applies: rendered as always.
    Usual,
    /// Routinely unopened: header and changes only.
    Short,
    /// Routinely read after the briefing: every fact, with line spans.
    Full,
}

fn decay(value: f64, age_s: f64) -> f64 {
    if age_s <= 0.0 {
        return value;
    }
    value * 0.5f64.powf(age_s / HALF_LIFE_S)
}

fn key(scope: &str, path: &str) -> String {
    format!("{scope}:{}", crate::codegraph::path_key(path))
}

fn names_key(scope: &str, session: &str) -> String {
    format!("briefnames:{scope}:{session}")
}

/// One path's counts, decayed to `at`.
fn counts(row: &Value, at: f64) -> BTreeMap<&'static str, f64> {
    let age = at - row.get("ts").and_then(Value::as_f64).unwrap_or(at);
    KINDS.iter().map(|k| (*k, decay(row.get(*k).and_then(Value::as_f64).unwrap_or(0.0), age))).collect()
}

/// Fold one session's outcome report in: `paths` is the hook's
/// `{"hit": [...], "needed": [...], "miss": [...], "unused": [...], "sought": [...]}`.
pub fn record(store: &Store, scope: &str, session: &str, paths: &Value) {
    let Some(by_kind) = paths.as_object() else { return };
    let stamp = now();
    for kind in KINDS {
        for path in by_kind.get(kind).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).take(100) {
            if path.is_empty() || path.len() > 512 {
                continue;
            }
            let k = key(scope, path);
            let row = store.kv_get(BUCKET, &k).unwrap_or_else(|| json!({}));
            let mut decayed = counts(&row, stamp);
            *decayed.entry(kind).or_insert(0.0) += 1.0;
            let mut out = Map::new();
            out.insert("path".into(), json!(path));
            for (k2, v) in decayed {
                out.insert(k2.into(), json!((v * 1000.0).round() / 1000.0));
            }
            out.insert("ts".into(), json!(stamp));
            let _ = store.kv_put(BUCKET, &k, &Value::Object(out), stamp);
        }
    }
    // a file the session wrote that no briefing showed: linked to the names
    // its prompts used, so the next prompt naming them is briefed it
    let sought: Vec<String> = by_kind.get("sought").and_then(Value::as_array).into_iter().flatten()
        .filter_map(Value::as_str).filter(|p| !p.is_empty() && p.len() <= 512).take(20).map(str::to_string).collect();
    if sought.is_empty() || session.is_empty() {
        return;
    }
    let names: Vec<String> = store.eph_get(&names_key(scope, session))
        .and_then(|v| v.as_array().cloned()).unwrap_or_default()
        .iter().filter_map(Value::as_str).map(str::to_string).collect();
    for name in names {
        let k = format!("{scope}:{name}");
        let row = store.kv_get(SOUGHT_BUCKET, &k).unwrap_or_else(|| json!({}));
        let age = stamp - row.get("ts").and_then(Value::as_f64).unwrap_or(stamp);
        let mut weights: Vec<(String, f64)> = row.get("paths").and_then(Value::as_object)
            .map(|m| m.iter().map(|(p, w)| (p.clone(), decay(w.as_f64().unwrap_or(0.0), age))).collect())
            .unwrap_or_default();
        for path in &sought {
            match weights.iter_mut().find(|(p, _)| p == path) {
                Some(slot) => slot.1 += 1.0,
                None => weights.push((path.clone(), 1.0)),
            }
        }
        weights.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        weights.truncate(SOUGHT_PER_NAME);
        let map: Map<String, Value> = weights.into_iter().map(|(p, w)| (p, json!((w * 1000.0).round() / 1000.0))).collect();
        let _ = store.kv_put(SOUGHT_BUCKET, &k, &json!({"paths": map, "ts": stamp}), stamp);
    }
}

/// The names this session's prompt used, kept for its outcome report: code
/// names and paths the hook found in the prompt, never its prose.
pub fn note_names(store: &Store, scope: &str, session: &str, identifiers: &[String]) {
    if session.is_empty() || identifiers.is_empty() {
        return;
    }
    let k = names_key(scope, session);
    let mut names: Vec<String> = store.eph_get(&k).and_then(|v| v.as_array().cloned()).unwrap_or_default()
        .iter().filter_map(Value::as_str).map(str::to_string).collect();
    for ident in identifiers {
        let name = ident.trim().to_lowercase();
        if name.len() < 3 || name.len() > 200 || names.contains(&name) {
            continue;
        }
        if names.len() >= NAMES_PER_SESSION {
            break;
        }
        names.push(name);
    }
    let _ = store.eph_set(&k, &json!(names), Some(NAMES_TTL_S));
}

/// How `path` should be rendered, by its decayed outcomes.
pub fn shape(store: &Store, scope: &str, path: &str, at: f64) -> Shape {
    let Some(row) = store.kv_get(BUCKET, &key(scope, path)) else { return Shape::Usual };
    shape_of(&counts(&row, at))
}

fn shape_of(c: &BTreeMap<&'static str, f64>) -> Shape {
    let total: f64 = c.values().sum();
    // (a hair under: three sessions a second apart have decayed a little)
    if total < MIN_EVIDENCE - 0.01 {
        return Shape::Usual;
    }
    let get = |k: &str| c.get(k).copied().unwrap_or(0.0);
    if get("unused") / total >= IGNORED_SHARE {
        return Shape::Short;
    }
    // read after being briefed: whether it was then written or not
    if (get("needed") + get("miss")) / total >= REREAD_SHARE {
        return Shape::Full;
    }
    Shape::Usual
}

/// Files earlier sessions went looking for under these names after a
/// briefing that did not show them, most sought first, with the name each
/// was sought under; at most `limit`.
pub fn sought_for(store: &Store, scope: &str, identifiers: &[String], limit: usize) -> Vec<(String, String)> {
    let stamp = now();
    let mut best: BTreeMap<String, (f64, String)> = BTreeMap::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for ident in identifiers {
        let name = ident.trim().to_lowercase();
        if name.len() < 3 || !seen.insert(name.clone()) {
            continue;
        }
        let Some(row) = store.kv_get(SOUGHT_BUCKET, &format!("{scope}:{name}")) else { continue };
        let age = stamp - row.get("ts").and_then(Value::as_f64).unwrap_or(stamp);
        for (path, weight) in row.get("paths").and_then(Value::as_object).into_iter().flatten() {
            let w = decay(weight.as_f64().unwrap_or(0.0), age);
            if w < SOUGHT_MIN - 0.01 {
                continue;
            }
            let slot = best.entry(path.clone()).or_insert((0.0, ident.clone()));
            if w > slot.0 {
                *slot = (w, ident.clone());
            }
        }
    }
    let mut ranked: Vec<(String, f64, String)> = best.into_iter().map(|(p, (w, n))| (p, w, n)).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    ranked.into_iter().take(limit).map(|(p, _, n)| (p, n)).collect()
}

/// What one path's outcomes are, for a dashboard or a test: decayed counts
/// and the shape they give.
pub fn view(store: &Store, scope: &str, path: &str) -> Value {
    let stamp = now();
    let row = store.kv_get(BUCKET, &key(scope, path)).unwrap_or_else(|| json!({}));
    let c = counts(&row, stamp);
    let shape = match shape_of(&c) {
        Shape::Usual => "usual",
        Shape::Short => "short",
        Shape::Full => "full",
    };
    let round = |x: f64| (x * 100.0).round() / 100.0;
    json!({"path": path, "hit": round(c["hit"]), "needed": round(c["needed"]), "miss": round(c["miss"]),
           "unused": round(c["unused"]), "shape": shape})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn outcomes_decide_the_shape_and_need_evidence() {
        let db = store();
        let unused = json!({"unused": ["a.py"]});
        record(&db, "w:r", "s1", &unused);
        record(&db, "w:r", "s2", &unused);
        assert_eq!(shape(&db, "w:r", "a.py", now()), Shape::Usual, "two sessions are not a habit");
        record(&db, "w:r", "s3", &unused);
        assert_eq!(shape(&db, "w:r", "a.py", now()), Shape::Short);
        for s in ["s4", "s5", "s6"] {
            record(&db, "w:r", s, &json!({"needed": ["b.py"]}));
        }
        assert_eq!(shape(&db, "w:r", "b.py", now()), Shape::Full);
        // hits keep a module as it is
        for s in ["s7", "s8", "s9"] {
            record(&db, "w:r", s, &json!({"hit": ["c.py"]}));
        }
        assert_eq!(shape(&db, "w:r", "c.py", now()), Shape::Usual);
    }

    #[test]
    fn outcomes_fade() {
        let db = store();
        for s in ["s1", "s2", "s3", "s4"] {
            record(&db, "w:r", s, &json!({"unused": ["a.py"]}));
        }
        assert_eq!(shape(&db, "w:r", "a.py", now()), Shape::Short);
        // a month later the habit is a quarter of what it was: below the evidence floor
        assert_eq!(shape(&db, "w:r", "a.py", now() + 2.0 * HALF_LIFE_S), Shape::Usual);
        assert!(decay(4.0, HALF_LIFE_S) - 2.0 < 1e-9);
    }

    #[test]
    fn a_file_sought_under_a_name_is_offered_to_the_next_prompt_naming_it() {
        let db = store();
        for s in ["s1", "s2"] {
            note_names(&db, "w:r", s, &["refund_total".to_string(), "x".to_string()]);
            record(&db, "w:r", s, &json!({"sought": ["billing/refunds.py"]}));
        }
        let found = sought_for(&db, "w:r", &["Refund_Total".to_string()], MAX_ADDED);
        assert_eq!(found, vec![("billing/refunds.py".to_string(), "Refund_Total".to_string())]);
        // one session is not enough; another name finds nothing
        note_names(&db, "w:r", "s3", &["other_name".to_string()]);
        record(&db, "w:r", "s3", &json!({"sought": ["misc.py"]}));
        assert!(sought_for(&db, "w:r", &["other_name".to_string()], MAX_ADDED).is_empty());
        assert!(sought_for(&db, "w:r", &["unrelated".to_string()], MAX_ADDED).is_empty());
    }
}
