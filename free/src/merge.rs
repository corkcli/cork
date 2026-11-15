//! Offline imagination: what would happen if this landed?
//!
//! Both answers here are counterfactual. `simulate_merge` asks which symbols
//! would disagree if several workspaces landed together right now, before any
//! real merge exists. `blind_spots` asks the harder question — where is this
//! map unreliable — and answers it about itself.
//!
//! Precision over recall in both. A one-sided add is normal in-flight work,
//! not a conflict, and reporting it as one would teach agents to ignore the
//! output. Only the same symbol, held by two workspaces, at two different
//! hashes, counts.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::compat::python_round;
use crate::semantics;
use crate::store::{now, Store};

pub fn simulate_merge(store: &Store, scope: &str, repo_id: &str, users: &[String]) -> Value {
    let stamp = now();
    let all_users: Vec<String> = store
        .list_workspaces(scope)
        .into_iter()
        .filter_map(|workspace| {
            workspace.get("user").and_then(Value::as_str).map(str::to_string)
        })
        .collect();
    let targets: Vec<String> = if users.is_empty() {
        all_users.clone()
    } else {
        users.iter().filter(|user| all_users.contains(user)).cloned().collect()
    };

    // user -> path -> symbols, from each workspace's freshest servable parse
    let mut tables: BTreeMap<String, BTreeMap<String, serde_json::Map<String, Value>>> =
        BTreeMap::new();
    for user in &targets {
        let mut per_path = BTreeMap::new();
        for (path, record) in store.list_files(scope, user) {
            if let Some(served) = semantics::serve(Some(&record), stamp) {
                per_path.insert(path, served.symbols);
            }
        }
        tables.insert(user.clone(), per_path);
    }

    let paths: BTreeSet<&String> = tables.values().flat_map(|table| table.keys()).collect();
    let mut conflicts: Vec<Value> = Vec::new();
    for path in &paths {
        let holders: Vec<(&String, &serde_json::Map<String, Value>)> = tables
            .iter()
            .filter_map(|(user, table)| table.get(*path).map(|symbols| (user, symbols)))
            .collect();
        if holders.len() < 2 {
            continue; // only one workspace has this file: nothing to disagree about
        }
        let names: BTreeSet<&String> =
            holders.iter().flat_map(|(_user, symbols)| symbols.keys()).collect();
        for symbol in names {
            let versions: Vec<(&String, &Value)> = holders
                .iter()
                .filter_map(|(user, symbols)| symbols.get(symbol).map(|entry| (*user, entry)))
                .collect();
            if versions.len() < 2 {
                continue; // a one-sided add is in-flight work, not a conflict
            }
            let hashes: BTreeSet<&str> = versions
                .iter()
                .map(|(_user, entry)| entry.get("hash").and_then(Value::as_str).unwrap_or(""))
                .collect();
            if hashes.len() < 2 {
                continue;
            }
            let signatures: serde_json::Map<String, Value> = versions
                .iter()
                .map(|(user, entry)| {
                    (
                        (*user).clone(),
                        json!(entry.get("signature").and_then(Value::as_str).unwrap_or("")),
                    )
                })
                .collect();
            let distinct: BTreeSet<&str> =
                signatures.values().filter_map(Value::as_str).collect();
            conflicts.push(json!({
                "path": path,
                "symbol": symbol,
                // a differing signature is a contract break; a differing body
                // at the same signature is only an implementation clash
                "kind": if distinct.len() > 1 { "signature" } else { "implementation" },
                "signatures": signatures,
            }));
        }
    }

    // Counterfactuals over MEMORY, not just code. If these workspaces landed,
    // which anchored notes go stale, and which settled rationales would be
    // re-litigated? The second is the more valuable half: it surfaces a
    // decision before someone unknowingly reverses it.
    let mut notes_affected: Vec<Value> = Vec::new();
    let mut rationales_triggered: Vec<Value> = Vec::new();
    if !conflicts.is_empty() {
        let keys: BTreeSet<(String, String)> = conflicts
            .iter()
            .map(|conflict| {
                (
                    conflict.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
                    conflict.get("symbol").and_then(Value::as_str).unwrap_or("").to_string(),
                )
            })
            .collect();
        let conflict_paths: BTreeSet<String> = keys.iter().map(|(path, _)| path.clone()).collect();

        for (_key, memory) in store.kv_list("memory", &format!("{scope}:")) {
            let Some(anchor) = memory.get("anchor").filter(|a| !a.is_null()) else { continue };
            if crate::compat::truthy(memory.get("superseded_by")) {
                continue;
            }
            let kind = anchor.get("kind").and_then(Value::as_str).unwrap_or("");
            let path = anchor.get("path").and_then(Value::as_str).unwrap_or("").to_string();
            let symbol = anchor.get("symbol").and_then(Value::as_str).unwrap_or("").to_string();
            let hit = (kind == "symbol" && keys.contains(&(path.clone(), symbol)))
                || (kind == "file" && conflict_paths.contains(&path));
            if !hit {
                continue;
            }
            let auto = memory.get("auto").and_then(Value::as_str).unwrap_or("");
            let entry = json!({
                "memory_id": memory.get("id").cloned().unwrap_or(Value::Null),
                "fact": memory.get("fact").and_then(Value::as_str).unwrap_or(""),
                "anchor": anchor.clone(),
                "auto": auto,
            });
            if auto == "rationale" {
                rationales_triggered.push(entry);
            } else {
                notes_affected.push(entry);
            }
        }
    }

    json!({
        "repo_id": repo_id,
        "users": targets,
        "paths_compared": paths.len(),
        "conflicts": conflicts,
        "clean": conflicts.is_empty(),
        "notes_affected": notes_affected,
        "rationales_triggered": rationales_triggered,
    })
}

const BLIND_SPOT_CAP: usize = 50;

/// Metacognition: where this map is unreliable.
///
/// Files that never parsed cleanly, files serving state old enough to be
/// wrong, workspaces that have gone quiet. A system that answers confidently
/// everywhere is lying somewhere, and the honest move is to name the places
/// rather than let an agent discover them by being wrong.
pub fn blind_spots(
    store: &Store, scope: &str, repo_id: &str, idle_after_s: f64, hot_ttl_s: f64,
) -> Value {
    let stamp = now();
    let mut never_clean: Vec<Value> = Vec::new();
    let mut stale: Vec<Value> = Vec::new();
    let mut quiet: Vec<Value> = Vec::new();

    for workspace in store.list_workspaces(scope) {
        let user = workspace.get("user").and_then(Value::as_str).unwrap_or("").to_string();
        let silent_s = stamp - workspace.get("updated").and_then(Value::as_f64).unwrap_or(stamp);
        if silent_s > idle_after_s {
            quiet.push(json!({"user": user, "silent_s": python_round(silent_s, 1)}));
        }
        for (path, record) in store.list_files(scope, &user) {
            let Some(last_clean) = record.get("last_clean").filter(|v| !v.is_null()) else {
                never_clean.push(json!({"user": user, "path": path}));
                continue;
            };
            let ts = last_clean.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            if record.get("status").and_then(Value::as_str) == Some("partial")
                && stamp - ts > hot_ttl_s
            {
                stale.push(json!({
                    "user": user, "path": path,
                    "age_s": python_round(stamp - ts, 1),
                }));
            }
        }
    }

    // Neither listing orders its rows, and both of these are truncated — so
    // unsorted, the same repo reported not just a different order but a
    // different SET of blind spots each call. Sorted, the cap always keeps
    // the same fifty.
    let by_user_path = |entry: &Value| {
        (
            entry.get("user").and_then(Value::as_str).unwrap_or("").to_string(),
            entry.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
        )
    };
    never_clean.sort_by_key(by_user_path);
    stale.sort_by_key(by_user_path);
    quiet.sort_by_key(|entry| {
        entry.get("user").and_then(Value::as_str).unwrap_or("").to_string()
    });
    never_clean.truncate(BLIND_SPOT_CAP);
    stale.truncate(BLIND_SPOT_CAP);

    json!({
        "repo_id": repo_id,
        "never_clean": never_clean,
        "stale": stale,
        "quiet_workspaces": quiet,
        "note": "Cork is silent about these; treat its answers there as unreliable.",
    })
}

// ------------------------------------------------------- differential check

/// The narrow differential oracle.
///
/// The question is never "is this code correct" — nothing here could answer
/// that, because the server keeps no source and cannot run a typechecker or a
/// test. The question it CAN answer is narrower and still worth asking: does
/// my diff preserve the assumptions the other workspace's code relies on,
/// judged at the interface level from two trees and the parsed reference
/// graph.
///
/// The verdicts are deliberately unbalanced. FAIL needs a signature that
/// diverges on a symbol someone actually references. Implementation-only
/// divergence is not a failure of THIS oracle and is reported as nothing at
/// all rather than as a pass it did not earn. Everything else — a signature
/// nobody is known to reference, a symbol present on one side only — is
/// UNKNOWN, because reliance outside the parsed fragment is unknowable from
/// here. The response says exactly what was and was not checked and never
/// claims past it.
pub fn differential_check(store: &Store, scope: &str, mine_user: &str, other_user: &str) -> Value {
    let stamp = now();
    let table_of = |user: &str| -> BTreeMap<String, serde_json::Map<String, Value>> {
        let mut per_path = BTreeMap::new();
        for (path, record) in store.list_files(scope, user) {
            if let Some(served) = semantics::serve(Some(&record), stamp) {
                per_path.insert(path, served.symbols);
            }
        }
        per_path
    };
    let mine = table_of(mine_user);
    let theirs = table_of(other_user);
    if theirs.is_empty() {
        return json!({
            "ok": false,
            "error": format!("no workspace state for {}", crate::compat::python_repr(other_user)),
        });
    }

    /// Everything in `table` that names `symbol` in its extracted references.
    /// A symbol never counts as referencing itself.
    fn dependents_of(
        table: &BTreeMap<String, serde_json::Map<String, Value>>, name: &str,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (path, symbols) in table {
            for (symbol, entry) in symbols {
                if symbol == name {
                    continue;
                }
                let refs = entry.get("refs").and_then(Value::as_array);
                let names = refs.map(|refs| {
                    refs.iter().any(|reference| reference.as_str() == Some(name))
                });
                if names.unwrap_or(false) {
                    out.push(format!("{path}::{symbol}"));
                }
            }
        }
        out.sort();
        out
    }

    let empty = serde_json::Map::new();
    let paths: BTreeSet<&String> = mine.keys().chain(theirs.keys()).collect();
    let mut findings: Vec<Value> = Vec::new();

    for path in paths {
        let my_syms = mine.get(path).unwrap_or(&empty);
        let their_syms = theirs.get(path).unwrap_or(&empty);
        let names: BTreeSet<&String> = my_syms.keys().chain(their_syms.keys()).collect();

        for symbol in names {
            let my_version = my_syms.get(symbol);
            let their_version = their_syms.get(symbol);

            let (Some(my_version), Some(their_version)) = (my_version, their_version) else {
                // one side only: a removal this oracle cannot confirm. It is
                // only worth reporting when the OTHER side's code still names
                // it, which is the shape that would actually break.
                let (present, absent) = if my_version.is_none() {
                    ("theirs", "mine")
                } else {
                    ("mine", "theirs")
                };
                let refs = dependents_of(if my_version.is_none() { &theirs } else { &mine }, symbol);
                if !refs.is_empty() {
                    findings.push(json!({
                        "verdict": "UNKNOWN", "path": path, "symbol": symbol,
                        "reason": format!(
                            "present in {present} only; removal unconfirmed, \
                             but referenced by {absent}'s code"),
                        "referenced_by": refs,
                    }));
                }
                continue;
            };

            if my_version.get("hash") == their_version.get("hash") {
                continue;
            }
            let sig_mine = my_version.get("signature").and_then(Value::as_str).unwrap_or("");
            let sig_theirs = their_version.get("signature").and_then(Value::as_str).unwrap_or("");
            if sig_mine == sig_theirs {
                continue; // implementation-only: this oracle has no claim
            }

            let their_deps = dependents_of(&theirs, symbol);
            let my_deps = dependents_of(&mine, symbol);
            if !their_deps.is_empty() || !my_deps.is_empty() {
                findings.push(json!({
                    "verdict": "FAIL", "path": path, "symbol": symbol,
                    "reason": "signature diverges on a symbol the other side's code references",
                    "signature_mine": sig_mine, "signature_theirs": sig_theirs,
                    "referenced_by_theirs": their_deps, "referenced_by_mine": my_deps,
                }));
            } else {
                findings.push(json!({
                    "verdict": "UNKNOWN", "path": path, "symbol": symbol,
                    "reason": "signature diverges but no parsed code references it; \
                               reliance outside the fragment is unknowable here",
                    "signature_mine": sig_mine, "signature_theirs": sig_theirs,
                }));
            }
        }
    }

    let verdicts: BTreeSet<&str> =
        findings.iter().filter_map(|f| f.get("verdict").and_then(Value::as_str)).collect();
    let result = if verdicts.contains("FAIL") {
        "FAIL"
    } else if verdicts.contains("UNKNOWN") {
        "UNKNOWN"
    } else {
        "PASS"
    };

    json!({
        "ok": true,
        "result": result,
        "oracle": "interface",
        "checked": "signature preservation of symbols referenced by the other workspace's parsed code",
        "not_checked": ["behavior", "types beyond signatures", "tests",
                        "references from unparsed or unreported files"],
        "users": [mine_user, other_user],
        "findings": findings,
    })
}

/// How far back a live session's unlanded work is looked for.
const PREVIEW_LOOKBACK_S: f64 = 7_200.0;
const PREVIEW_MAX_SYMBOLS: usize = 200;
const PREVIEW_MAX_CONFLICTS: usize = 5;

/// `simulate_merge` for one landing: the symbols this session is about to
/// push (`changed`: `{path, symbol, kind}`, names only, from its hook's diff
/// of its own commits), against the work other live sessions on the same
/// branch have written and not landed. The same rule as `simulate_merge`,
/// at session grain: only a symbol both sides changed counts; a one-sided
/// add is in-flight work, not a conflict. Work already landed is skipped:
/// the landing rebases onto it, and git says so if it conflicts. Work on
/// another branch is skipped too: it meets this one when the branches merge
/// (branches.rs). Advice only; nothing here ever stops a landing.
pub fn preview_landing(store: &Store, scope: &str, user: &str, session: &str, changed: &[Value]) -> Value {
    let stamp = now();
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    // path -> (symbol -> my kind)
    let mut mine: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for entry in changed.iter().take(PREVIEW_MAX_SYMBOLS) {
        let (path, symbol) = (text(entry, "path"), text(entry, "symbol"));
        if !path.is_empty() && !symbol.is_empty() {
            mine.entry(path).or_default().insert(symbol, text(entry, "kind"));
        }
    }
    if mine.is_empty() || session.is_empty() {
        return json!({"ok": true, "conflicts": []});
    }
    let branch = crate::branches::of(store, scope, user, session);
    let since = stamp - PREVIEW_LOOKBACK_S;
    // (user, session, path) -> when they last wrote it
    let mut writers: BTreeMap<(String, String, String), f64> = BTreeMap::new();
    for (_, copy) in store.eph_scan(&format!("copyhash:{scope}:")) {
        let (u, s, p) = (text(&copy, "user"), text(&copy, "session"), text(&copy, "path"));
        let ts = copy.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        if copy.get("wrote").and_then(Value::as_bool) == Some(true) && !s.is_empty() && mine.contains_key(&p) && ts >= since {
            writers.insert((u, s, p), ts);
        }
    }
    // the certain changes, with what they changed to (hot markers)
    let mut details: BTreeMap<(String, String, String, String), Value> = BTreeMap::new();
    for path in mine.keys() {
        for (_, marker) in store.eph_scan(&format!("hot:{scope}:{}:", crate::repo::path_key(path))) {
            let (u, s) = (text(&marker, "user"), text(&marker, "session"));
            if text(&marker, "path") != *path || s.is_empty() {
                continue;
            }
            let ts = marker.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            let seen = writers.entry((u.clone(), s.clone(), path.clone())).or_insert(ts);
            *seen = seen.max(ts);
            for event in marker.get("events").and_then(Value::as_array).into_iter().flatten() {
                details.insert((u.clone(), s.clone(), path.clone(), text(event, "symbol")), event.clone());
            }
        }
    }
    writers.retain(|(u, s, _), _| {
        !(u == user && s == session)
            && store.eph_get(&format!("sessended:{scope}:{s}")).is_none()
            && crate::branches::apart(store, scope, &branch, (u, s)).is_none()
    });
    if writers.is_empty() {
        return json!({"ok": true, "conflicts": []});
    }
    // their reported edits and landings, read once
    let rows = store.ledger_since_kinds(scope, since, &["edit_reported", "landed"]);
    let mut conflicts: Vec<Value> = Vec::new();
    let mut more = 0usize;
    for ((u, s, path), last) in &writers {
        let landed = rows.iter().any(|row| {
            let by = text(&row.payload, "session");
            row.kind == "landed" && row.ts >= *last && text(&row.payload, "user") == *u && (by.is_empty() || by == *s)
        });
        if landed {
            continue;
        }
        let (mut added, mut touched): (BTreeSet<String>, BTreeSet<String>) = Default::default();
        for row in rows.iter().filter(|r| r.kind == "edit_reported") {
            let p = &row.payload;
            if text(p, "user") != *u || text(p, "session") != *s || text(p, "path") != *path {
                continue;
            }
            for event in p.get("events").and_then(Value::as_array).into_iter().flatten() {
                let symbol = text(event, "symbol");
                match text(event, "kind").as_str() {
                    "added" => {
                        added.insert(symbol);
                    }
                    // the other copy's change seen through this writer's diff
                    "modified" if text(event, "confidence") == semantics::UNCONFIRMED => {}
                    _ => {
                        touched.insert(symbol);
                    }
                }
            }
        }
        for (u2, s2, p2, symbol) in details.keys() {
            if u2 == u && s2 == s && p2 == path {
                touched.insert(symbol.clone());
            }
        }
        let who = crate::inflight::author_label(store, user, u);
        for (symbol, my_kind) in &mine[path] {
            let both_add = my_kind == "added" && added.contains(symbol);
            if !touched.contains(symbol) && !both_add {
                continue;
            }
            if conflicts.len() >= PREVIEW_MAX_CONFLICTS {
                more += 1;
                continue;
            }
            let event = details.get(&(u.clone(), s.clone(), path.clone(), symbol.clone()));
            let kind = match event.map(|e| text(e, "kind")) {
                Some(k) if !k.is_empty() => k,
                _ if both_add => "added".to_string(),
                _ => "body".to_string(),
            };
            let to = event
                .map(|e| {
                    let new_name = text(e, "new_name");
                    if new_name.is_empty() { text(e, "signature_after") } else { new_name }
                })
                .unwrap_or_default();
            conflicts.push(json!({"path": path, "symbol": symbol, "who": who, "kind": kind, "now": to, "yours": my_kind}));
        }
    }
    json!({"ok": true, "conflicts": conflicts, "more": more})
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn a_landing_hears_which_of_its_symbols_a_live_session_is_also_changing() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        let path = "booking.py";
        let h = crate::freshness::fingerprint_content("x\n");
        for (u, s, b) in [("kevin@x.com", "k1", "main"), ("bob@x.com", "b1", "main"), ("alice@x.com", "a1", "feat/x")] {
            crate::branches::note(&db, scope, u, s, b, "");
            crate::freshness::saw(&db, scope, u, s, path, &h, true);
        }
        // bob re-signatures create_booking and has not landed; alice does the
        // same on another branch
        for (u, s) in [("bob@x.com", "b1"), ("alice@x.com", "a1")] {
            let event = json!({"symbol": "create_booking", "kind": "signature", "confidence": "certain",
                               "signature_after": "def create_booking(slot, user)"});
            db.ledger_append(scope, "edit_reported", &json!({"user": u, "session": s, "path": path,
                "events": [{"symbol": "create_booking", "kind": "modified", "confidence": "certain"}]}), now()).unwrap();
            let _ = db.eph_set(&format!("hot:{scope}:{}:{u}#{s}", crate::repo::path_key(path)),
                &json!({"path": path, "user": u, "session": s, "ts": now(), "events": [event]}), Some(600.0));
        }
        let changed = vec![json!({"path": path, "symbol": "create_booking", "kind": "body"}),
                           json!({"path": path, "symbol": "cancel_booking", "kind": "body"})];
        let answer = preview_landing(&db, scope, "kevin@x.com", "k1", &changed);
        let conflicts = answer["conflicts"].as_array().unwrap();
        assert_eq!(conflicts.len(), 1, "bob only, and only the symbol both change: {answer}");
        assert_eq!(conflicts[0]["symbol"], "create_booking");
        assert_eq!(conflicts[0]["who"], "bob@x.com");
        assert_eq!(conflicts[0]["kind"], "signature");
        assert_eq!(conflicts[0]["now"], "def create_booking(slot, user)");
        // once bob lands, it is the landing's rebase to meet, not a prediction
        db.ledger_append(scope, "landed", &json!({"user": "bob@x.com", "session": "b1"}), now() + 1.0).unwrap();
        assert!(preview_landing(&db, scope, "kevin@x.com", "k1", &changed)["conflicts"].as_array().unwrap().is_empty());
    }
}
