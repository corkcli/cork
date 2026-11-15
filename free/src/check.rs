//! The repo check, run by the hook after each batch of writes.
//!
//! Two small services behind it. `/verified` stamps the caller's latest
//! write records with the command that ran and whether it passed, so the
//! briefing and `get_symbol` can say "verified by …" (or "check failed")
//! for every module, not only those written through the typed-op path.
//! `/attribute` answers the two questions a failure raises that grep
//! cannot: who last wrote the files the traceback names (another agent's
//! write a minute ago is not the caller's bug to fix), and what the symbols
//! it names actually look like (the exact signature and docstring, so the
//! moment the agent learns it did not know enough is the moment it gets
//! the fact). Hook-only, like `/observe` and `/brief`.

use serde_json::{json, Map, Value};

use crate::store::Store;

const ATTRIBUTE_WINDOW_S: f64 = 86_400.0;
const MAX_PATHS: usize = 40;
/// A passing baseline stamps the whole indexed tree.
const MAX_STAMP_PATHS: usize = 600;
const MAX_SYMBOLS: usize = 20;

/// Stamp `provenance.verified` on the caller's latest write record for
/// each path; a ledger row records the run for the dashboard.
pub fn stamp_verified(
    store: &Store, scope: &str, user: &str, agent: &str, paths: &[String], command: &str, ok: bool,
    now: f64,
) -> Value {
    let mark = json!({"command": command, "ok": ok, "ts": now});
    let mut stamped = 0usize;
    for path in paths.iter().take(MAX_STAMP_PATHS) {
        let Some(mut record) = store.get_file(scope, user, path) else {
            // indexed from the tree, never written by this user: the mark goes
            // on the graph record, which get_symbol serves as "observed"
            let key = format!("{scope}:{}", crate::codegraph::path_key(path));
            let Some(mut graph_record) = store.kv_get(crate::codegraph::GRAPH_BUCKET, &key) else { continue };
            let Some(map) = graph_record.as_object_mut() else { continue };
            map.insert("verified".into(), mark.clone());
            if store.kv_put(crate::codegraph::GRAPH_BUCKET, &key, &graph_record, now).is_ok() {
                stamped += 1;
            }
            continue;
        };
        let Some(map) = record.as_object_mut() else { continue };
        let prov = map.entry("provenance").or_insert_with(|| json!({}));
        if let Some(prov) = prov.as_object_mut() {
            prov.insert("verified".into(), mark.clone());
        }
        if store.put_file(scope, user, path, &record, now).is_ok() {
            stamped += 1;
        }
    }
    let _ = store.ledger_append(
        scope,
        "check_run",
        &json!({"user": user, "agent": agent, "paths": paths.iter().take(MAX_PATHS).collect::<Vec<_>>(), "count": paths.len(), "command": command, "ok": ok}),
        now,
    );
    json!({"ok": true, "stamped": stamped})
}

/// A file's language, for telling one name's definitions apart: its
/// extension, with the extensions of one language as one.
fn language(path: &str) -> &str {
    let ext = path.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match ext {
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => "js",
        "py" | "pyi" => "py",
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "c",
        other => other,
    }
}

/// How many of these files the session touched most lately to weigh.
const RECENT_FILES: usize = 12;

/// Which of a name's definitions (`paths`) the session means, by what it
/// worked on lately (`recent`, newest first): the language it touched most,
/// then the top folder it touched most. None when that does not tell one
/// apart (or nothing was touched): naming the wrong one confidently sent
/// an agent on Rust's `AppState::emit` to a script's `emit`.
pub fn pick_definition(paths: &[&str], recent: &[String]) -> Option<usize> {
    if paths.len() == 1 {
        return Some(0);
    }
    let recent: Vec<&str> = recent.iter().take(RECENT_FILES).map(String::as_str).collect();
    if paths.is_empty() || recent.is_empty() {
        return None;
    }
    let top = |path: &str| path.split_once('/').map(|(dir, _)| dir.to_string()).unwrap_or_default();
    let score = |path: &str| -> (usize, usize) {
        let same_language = recent.iter().filter(|r| language(r) == language(path)).count();
        let same_folder = recent.iter().filter(|r| top(r) == top(path)).count();
        (same_language, same_folder)
    };
    let mut scored: Vec<(usize, (usize, usize))> = paths.iter().enumerate().map(|(i, p)| (i, score(p))).collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    (scored[0].1 > scored[1].1 && scored[0].1 != (0, 0)).then_some(scored[0].0)
}

/// Who last wrote each path (from the ledger, newest first) and the exact
/// facts for each symbol (from the graph records).
pub fn attribute(
    store: &Store, scope: &str, caller: &str, session: &str, paths: &[String], symbols: &[String], now: f64,
) -> Value {
    let mut files = Map::new();
    if !paths.is_empty() {
        let wanted: std::collections::HashSet<&str> =
            paths.iter().take(MAX_PATHS).map(String::as_str).collect();
        let mut rows = store.ledger_since_kinds(scope, now - ATTRIBUTE_WINDOW_S, &["edit_reported"]);
        rows.sort_by(|a, b| b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal));
        for row in rows {
            let path = row.payload.get("path").and_then(Value::as_str).unwrap_or("");
            if !wanted.contains(path) || files.contains_key(path) {
                continue;
            }
            let user = row.payload.get("user").and_then(Value::as_str).unwrap_or("");
            let row_session = row.payload.get("session").and_then(Value::as_str).unwrap_or("");
            // four agents on one credential are four agents: a write from
            // another session is not the caller's, even under the same user,
            // nor one by another agent of its session (`session` names the
            // caller as presence::agent_session does)
            let same_user = user == caller;
            let mine = same_user && (session.is_empty() || row_session.is_empty() || crate::deltas::row_agent(&row.payload) == session);
            files.insert(
                path.to_string(),
                json!({
                    "user": user,
                    "agent": row.payload.get("agent").and_then(Value::as_str).unwrap_or(""),
                    "age_s": crate::compat::python_round((now - row.ts).max(0.0), 1),
                    "mine": mine,
                    "same_user": same_user,
                }),
            );
            if files.len() == wanted.len() {
                break;
            }
        }
        // each author as the caller knows them (`nickname (email)`), and the
        // harness they work in, never the hook that reported the edit
        let users: std::collections::BTreeSet<String> = files.values()
            .filter_map(|f| f.get("user").and_then(Value::as_str).map(str::to_string))
            .filter(|u| !u.is_empty() && u != caller)
            .collect();
        let labels = crate::activity::identity_labels(store, caller, &users);
        for entry in files.values_mut() {
            let user = entry.get("user").and_then(Value::as_str).unwrap_or("").to_string();
            let agent = crate::presence::agent_name(entry.get("agent").and_then(Value::as_str).unwrap_or(""));
            if let Some(map) = entry.as_object_mut() {
                map.insert("agent".into(), json!(agent));
                if let Some(label) = labels.get(&user).and_then(|l| l.get("label")).and_then(Value::as_str) {
                    map.insert("who".into(), json!(label));
                }
            }
        }
    }
    let mut facts = Map::new();
    if !symbols.is_empty() {
        let graph = crate::graphview::snapshot(store, scope);
        let wanted: Vec<&str> = symbols.iter().take(MAX_SYMBOLS).map(String::as_str).collect();
        // every file that defines each name: one name can be two symbols in
        // two languages (Rust's `AppState::emit` and a script's `emit`)
        let mut found: std::collections::BTreeMap<&str, Vec<(String, Value)>> = std::collections::BTreeMap::new();
        for (_key, record) in store.kv_list(crate::codegraph::GRAPH_BUCKET, &format!("{scope}:")) {
            let path = record.get("path").and_then(Value::as_str).unwrap_or("");
            let Some(entries) = record.get("symbols").and_then(Value::as_object) else { continue };
            for name in &wanted {
                if let Some(entry) = entries.get(*name) {
                    found.entry(name).or_default().push((path.to_string(), entry.clone()));
                }
            }
        }
        // what this session works on decides between them, read only when
        // a name is ambiguous
        let recent = if found.values().any(|c| c.len() > 1) {
            crate::freshness::seen_paths(store, scope, caller, session)
        } else {
            Vec::new()
        };
        for (name, candidates) in found {
            let paths: Vec<&str> = candidates.iter().map(|(path, _)| path.as_str()).collect();
            let Some(chosen) = pick_definition(&paths, &recent) else {
                // nothing tells them apart: both, never the wrong one named
                facts.insert(name.to_string(), json!({"ambiguous": paths.iter().take(4).collect::<Vec<_>>()}));
                continue;
            };
            let (path, entry) = &candidates[chosen];
            // callers from the resolved snapshot, so aliased imports count
            let callers: Vec<String> = crate::graphview::blast_radius(&graph, &format!("{path}::{name}"), 1)
                .into_iter()
                .flatten()
                .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_string))
                .take(12)
                .collect();
            let mut fact = json!({
                "path": path,
                "callers": callers,
                "kind": entry.get("kind").cloned().unwrap_or(json!("")),
                "signature": entry.get("signature").cloned().unwrap_or(json!("")),
                "params": entry.get("params").cloned().unwrap_or(json!([])),
                "doc": entry.get("doc").cloned().unwrap_or(json!("")),
                // where it is: the answer to a grep is a range to Read, not a file
                "span": entry.get("span").cloned().unwrap_or(json!([0, 0])),
            });
            let elsewhere: Vec<&str> = paths.iter().filter(|p| **p != path.as_str()).take(3).copied().collect();
            if !elsewhere.is_empty() {
                fact["elsewhere"] = json!(elsewhere);
            }
            facts.insert(name.to_string(), fact);
        }
    }
    json!({"ok": true, "files": files, "facts": facts})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn define(store: &Store, scope: &str, path: &str, name: &str, signature: &str) {
        let record = json!({"path": path, "language": "", "imports": [],
            "symbols": {name: {"kind": "function", "signature": signature}}});
        let key = format!("{scope}:{}", crate::codegraph::path_key(path));
        store.kv_put(crate::codegraph::GRAPH_BUCKET, &key, &record, crate::store::now()).unwrap();
    }

    /// The field report (v103): after a shell call, the map named a script's
    /// `emit` in static/js/store.js while the agent worked on Rust's
    /// `AppState::emit`. The session's own files decide which one it means;
    /// when they do not, both are named and neither as the answer.
    #[test]
    fn a_name_in_two_languages_is_the_one_the_session_works_in_or_both() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        define(&db, scope, "static/js/store.js", "emit", "function emit(event)");
        define(&db, scope, "src/state.rs", "emit", "pub fn emit(&self, event: Event)");
        let ask = |session: &str| attribute(&db, scope, "kevin@x.com", session, &[], &["emit".to_string()], crate::store::now());
        // a session on the Rust side
        crate::freshness::saw(&db, scope, "kevin@x.com", "rust", "src/api.rs", "h1", true);
        crate::freshness::saw(&db, scope, "kevin@x.com", "rust", "src/main.rs", "h2", false);
        let fact = &ask("rust")["facts"]["emit"];
        assert_eq!(fact["path"], json!("src/state.rs"), "{fact}");
        assert_eq!(fact["elsewhere"], json!(["static/js/store.js"]));
        // a session on the front end
        crate::freshness::saw(&db, scope, "kevin@x.com", "web", "static/js/board.js", "h3", false);
        assert_eq!(ask("web")["facts"]["emit"]["path"], json!("static/js/store.js"));
        // one that touched nothing yet: both, and no path to read as the answer
        let fact = &ask("new")["facts"]["emit"];
        assert!(fact.get("path").is_none(), "{fact}");
        assert_eq!(fact["ambiguous"].as_array().map(Vec::len), Some(2), "{fact}");
        // a name with one definition is answered as always
        define(&db, scope, "src/state.rs", "emit_all", "pub fn emit_all(&self)");
        let once = attribute(&db, scope, "kevin@x.com", "new", &[], &["emit_all".to_string()], crate::store::now());
        assert_eq!(once["facts"]["emit_all"]["path"], json!("src/state.rs"), "{once}");
        assert!(once["facts"]["emit_all"].get("elsewhere").is_none(), "{once}");
    }

    #[test]
    fn the_language_touched_most_lately_then_the_folder_decides() {
        let recent = |paths: &[&str]| paths.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        let both = ["src/state.rs", "web/src/store.ts"];
        assert_eq!(pick_definition(&both, &recent(&["web/src/app.tsx", "src/lib.rs", "web/src/x.js"])), Some(1));
        assert_eq!(pick_definition(&both, &recent(&["src/lib.rs", "web/src/app.tsx"])), None, "one of each: no telling");
        // one language, two folders: the session's folder
        let two = ["server/src/emit.rs", "tools/src/emit.rs"];
        assert_eq!(pick_definition(&two, &recent(&["tools/src/main.rs"])), Some(1));
        assert_eq!(pick_definition(&two, &recent(&["README.md"])), None);
    }
}
