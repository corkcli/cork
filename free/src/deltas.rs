//! Live deltas: what other agents changed in the region this session is
//! working in, since its last step, as a few compressed lines.
//!
//! The working set is what the session has read, written, or been briefed
//! on (ephemeral `ws:{scope}:{session}:{path_key}` rows, two hours), and a
//! row remembers whether the session WROTE the file. What is relevant
//! depends on that: in a file the session wrote, every change by someone
//! else is (the two edits overlap); in a file it only read or was briefed
//! on, and through a dependent, only a change to an interface is — a
//! rename, a removal, a new signature. A new function or a body edit there
//! changes nothing the reader relies on, and telling it anyway made agents
//! pull and re-read for nothing (Study 3: the agents at the edges spent
//! 0.3–0.6M tokens more each keeping up with changes that did not touch
//! them). Lines about one file fold into one. The hook asks on
//! every event and prepends the answer to that event's own output, so an
//! agent learns of a colliding edit inside the step it was already taking —
//! never by rereading the graph, never the whole graph.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::store::{now, Store};

const WS_TTL_S: f64 = 7_200.0;

fn text_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The agent a ledger row is by: its session, or `session/worker` when a
/// helper of the session wrote it (presence::agent_session).
pub(crate) fn row_agent(payload: &Value) -> String {
    crate::presence::agent_session(&text_of(payload, "session"), &text_of(payload, "worker"))
}
const DEFAULT_BUDGET: usize = 1_500;
const MAX_BUDGET: usize = 6_000;
const MAX_EVENTS_PER_ROW: usize = 4;
const MAX_SYMBOLS_PER_ROW: usize = 10;
const MAX_LOOKBACK_S: f64 = 3_600.0;

fn ws_key(scope: &str, session: &str, path: &str) -> String {
    format!("ws:{scope}:{session}:{}", crate::codegraph::path_key(path))
}

/// A path joins the session's working set. `wrote` marks a file the session
/// wrote; a later read or briefing never takes that back.
pub fn touch(store: &Store, scope: &str, session: &str, paths: &[String], stamp: f64) {
    touch_as(store, scope, session, paths, false, stamp);
}

pub fn touch_as(store: &Store, scope: &str, session: &str, paths: &[String], wrote: bool, stamp: f64) {
    if session.is_empty() {
        return;
    }
    // a task's overlap with another agent's work, said once it opens the code
    crate::overlap::touched(store, scope, session, paths);
    for path in paths {
        if path.is_empty() {
            continue;
        }
        let key = ws_key(scope, session, path);
        let kept = store.eph_get(&key);
        let had = kept.as_ref().and_then(|v| v.get("wrote").and_then(Value::as_bool)).unwrap_or(false);
        // when the session first had the file, and first wrote it: what others
        // changed between the two is not in what it read (`deltas_inner`)
        let first = kept.as_ref().and_then(|v| v.get("first").and_then(Value::as_f64)).unwrap_or(stamp);
        let mut row = json!({"path": path, "ts": stamp, "first": first, "wrote": wrote || had});
        match kept.as_ref().and_then(|v| v.get("wrote_at").and_then(Value::as_f64)) {
            Some(at) => row["wrote_at"] = json!(at),
            None if wrote => row["wrote_at"] = json!(stamp),
            None => {}
        }
        let _ = store.eph_set(&key, &row, Some(WS_TTL_S));
    }
}

/// Paths a briefing named to this session. They join the working set the
/// briefing reads back after a compaction, but they are not code the session
/// touches: a change there reaches it only once it reads, writes or calls
/// into that code. A prompt briefing names a dozen modules, and Study 8's
/// agents each heard the same `Δ fmt signature …` line through them,
/// whether or not they went near `fmt`. A path already read or written
/// stays as it was.
pub fn touch_briefed(store: &Store, scope: &str, session: &str, paths: &[String], stamp: f64) {
    if session.is_empty() {
        return;
    }
    for path in paths {
        if path.is_empty() {
            continue;
        }
        let key = ws_key(scope, session, path);
        match store.eph_get(&key) {
            Some(row) if row.get("briefed").and_then(Value::as_bool) != Some(true) => continue,
            _ => {
                let _ = store.eph_set(&key, &json!({"path": path, "ts": stamp, "wrote": false, "briefed": true}), Some(WS_TTL_S));
            }
        }
    }
}

/// The files this session itself read or wrote (not only was briefed on):
/// where a teammate's change is news to it.
fn touched_set(store: &Store, scope: &str, session: &str) -> BTreeSet<String> {
    store
        .eph_scan(&format!("ws:{scope}:{session}:"))
        .into_iter()
        .filter(|(_, v)| v.get("briefed").and_then(Value::as_bool) != Some(true))
        .filter_map(|(_, v)| v.get("path").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// The sessions a message is news for, by the code it names: those whose
/// working set (what they read or wrote, not what a briefing named) holds
/// a file the message names, a file declaring a name it names, or a file
/// that calls one. `None` when the message names no code the map knows:
/// then it is for everyone at work. The delta's own targeting, applied to a
/// broadcast (Study 8, second study: every agent read every agent's plan).
pub fn sessions_named_by(store: &Store, scope: &str, message: &str) -> Option<BTreeSet<String>> {
    static WORD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let word = WORD.get_or_init(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_./]*[A-Za-z0-9_]").expect("word pattern"));
    // identifier-like words only: `create_booking`, `api/router.py`,
    // `Space.cap`, `listBookings`; plain English names no code
    let codey = |w: &str| {
        w.contains('_') || w.contains('/') || w.contains('.')
            || w.chars().skip(1).any(|c| c.is_ascii_uppercase()) && w.chars().any(|c| c.is_ascii_lowercase())
    };
    // or written as code: `fmt`, fmt(...)
    let as_code = |m: &regex::Match| {
        let before = message[..m.start()].chars().last();
        let after = message[m.end()..].chars().next();
        before == Some('`') || matches!(after, Some('`') | Some('('))
    };
    let words: BTreeSet<String> = word.find_iter(message)
        .filter(|m| m.as_str().len() >= 3 && (codey(m.as_str()) || as_code(m)))
        .map(|m| m.as_str().trim_end_matches('.').to_string())
        .collect();
    if words.is_empty() {
        return None;
    }
    let graph = crate::graphview::snapshot(store, scope);
    let mut files: BTreeSet<String> = BTreeSet::new();
    for (nid, node) in &graph.nodes {
        let named = words.iter().any(|w| {
            *w == node.path || node.path.ends_with(&format!("/{w}"))
                || (!node.symbol.is_empty() && (*w == node.symbol || w.ends_with(&format!(".{}", node.symbol))))
        });
        if !named {
            continue;
        }
        files.insert(node.path.clone());
        // and who calls it
        if !node.symbol.is_empty() {
            for level in crate::graphview::blast_radius(&graph, nid, 1) {
                for dep in level {
                    if let Some(p) = dep.get("path").and_then(Value::as_str) {
                        files.insert(p.to_string());
                    }
                }
            }
        }
    }
    if files.is_empty() {
        return None;
    }
    let prefix = format!("ws:{scope}:");
    let mut sessions: BTreeSet<String> = BTreeSet::new();
    for (key, row) in store.eph_scan(&prefix) {
        if row.get("briefed").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let path = row.get("path").and_then(Value::as_str).unwrap_or("");
        if !files.contains(path) {
            continue;
        }
        // ws:{scope}:{session}:{path_key}
        let rest = &key[prefix.len()..];
        if let Some((session, _)) = rest.rsplit_once(':') {
            // a helper's working set is its own; the message reaches its session
            sessions.insert(crate::presence::split_agent(session).0.to_string());
        }
    }
    Some(sessions)
}

/// Each change is told to a session once, whichever call carries it (the
/// hook's told list covers what it said itself; this covers what these
/// deltas said, across hooks that keep no list).
fn told_key(scope: &str, session: &str, fact: &str) -> String {
    format!("deltold:{scope}:{session}:{}", crate::hashing::sha256_hex(fact).get(..16).unwrap_or(""))
}

/// The names this agent (presence::agent_session) changed itself since
/// `since`, per file, from its own reported edits: what its next write is
/// likely to touch again.
pub(crate) fn own_names(store: &Store, scope: &str, user: &str, session: &str, since: f64) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if session.is_empty() {
        return out;
    }
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    for row in store.ledger_since_kinds(scope, since, &["edit_reported"]) {
        let p = &row.payload;
        if text(p, "user") != user || row_agent(p) != session {
            continue;
        }
        let names = out.entry(text(p, "path")).or_default();
        for event in p.get("events").and_then(Value::as_array).into_iter().flatten() {
            for key in ["symbol", "new_name"] {
                let name = text(event, key);
                if !name.is_empty() {
                    names.insert(name);
                }
            }
        }
        if let Some(changed) = p.get("symbols_changed").and_then(Value::as_object) {
            names.extend(changed.keys().cloned());
        }
    }
    out
}

/// The files this session wrote for the first time after `since`, each
/// with when it first had the file (read it, or wrote it).
fn first_written_since(store: &Store, scope: &str, session: &str, since: f64) -> BTreeMap<String, f64> {
    store
        .eph_scan(&format!("ws:{scope}:{session}:"))
        .into_iter()
        .filter(|(_, v)| v.get("wrote_at").and_then(Value::as_f64).is_some_and(|at| at > since))
        .filter_map(|(_, v)| {
            let path = v.get("path").and_then(Value::as_str)?.to_string();
            let first = v.get("first").or_else(|| v.get("ts")).and_then(Value::as_f64)?;
            Some((path, first))
        })
        .collect()
}

/// The files of the working set this session wrote.
pub(crate) fn written_set(store: &Store, scope: &str, session: &str) -> BTreeSet<String> {
    store
        .eph_scan(&format!("ws:{scope}:{session}:"))
        .into_iter()
        .filter(|(_, v)| v.get("wrote").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|(_, v)| v.get("path").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// A change another agent's code could break on: a rename, a removal, a new
/// signature. A body edit or a new symbol breaks no caller.
fn is_interface_change(kind: &str, signature_before: &str, signature_after: &str) -> bool {
    matches!(kind, "renamed" | "removed")
        || (kind != "added" && !signature_after.is_empty() && signature_after != signature_before)
}

pub(crate) fn working_set(store: &Store, scope: &str, session: &str) -> BTreeSet<String> {
    store
        .eph_scan(&format!("ws:{scope}:{session}:"))
        .into_iter()
        .filter_map(|(_, v)| v.get("path").and_then(Value::as_str).map(str::to_string))
        .collect()
}

pub(crate) fn age_text(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 120.0 {
        "just now".to_string()
    } else if s < 7_200.0 {
        format!("{}m ago", (s / 60.0) as i64)
    } else {
        format!("{}h ago", (s / 3_600.0) as i64)
    }
}

/// The id of an interface change as every channel names it, so a session
/// hears it once whichever way it arrives (the hook's pre-write gate, the
/// protocol's push, these deltas): `path::symbol|sig|<new signature, no
/// whitespace>`, `|ren|<new name>` or `|del|`. The hook keeps the ones it
/// said; these deltas leave those out and name the ones they say.
pub fn fact_id(path: &str, symbol: &str, kind: &str, signature_before: &str, signature_after: &str, new_name: &str) -> Option<String> {
    let compact = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let (short, new) = match kind {
        "renamed" => ("ren", new_name.to_string()),
        "removed" => ("del", String::new()),
        "added" => return None,
        _ if !signature_after.is_empty() && signature_after != signature_before => ("sig", compact(signature_after)),
        _ => return None,
    };
    Some(format!("{path}::{symbol}|{short}|{new}"))
}

/// What other agents changed in this session's working set, plus any held
/// plan groups that came free for this person: the release lines ride on
/// every answer, deltas or not, because a planner waiting on them may have
/// nothing else changing around it. The interface changes in `told` (fact
/// ids the session already heard) are left out; the answer's `facts` names
/// the ones it says.
/// `session` is the reader as an agent (presence::agent_session): a helper
/// hears its siblings' and its lead's changes as a teammate's, and its own
/// never; a session's held plan groups are its lead's.
pub fn deltas_told(store: &Store, scope: &str, user: &str, session: &str, since_ts: f64, budget: usize, told: &BTreeSet<String>) -> Value {
    let lead = crate::presence::split_agent(session).0;
    let mut releases = crate::traffic::release_lines(store, scope, user, lead);
    // a module a teammate is writing that this session imports: once each
    releases.extend(crate::inflight::take_notices(store, scope, session));
    let mut answer = deltas_inner(store, scope, user, session, since_ts, budget, told);
    if !releases.is_empty() {
        if let Some(map) = answer.as_object_mut() {
            let mut lines: Vec<Value> = map.get("lines").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut sent: Vec<Value> = map.get("sent").and_then(Value::as_array).cloned().unwrap_or_default();
            sent.resize(lines.len(), Value::Null);
            let stamp = now();
            sent.extend(releases.iter().map(|_| json!(stamp)));
            lines.extend(releases.into_iter().map(Value::String));
            map.insert("lines".into(), json!(lines));
            map.insert("sent".into(), json!(sent));
        }
    }
    answer
}

fn deltas_inner(store: &Store, scope: &str, user: &str, session: &str, since_ts: f64, budget: usize, told: &BTreeSet<String>) -> Value {
    let stamp = now();
    if session.is_empty() {
        return json!({"ok": true, "ts": stamp, "lines": [], "count": 0});
    }
    let budget = if budget == 0 { DEFAULT_BUDGET } else { budget.min(MAX_BUDGET) };
    let ws = touched_set(store, scope, session);
    let written = written_set(store, scope, session);
    let lands = crate::freshness::lands(store, scope, user, session);
    let my_branch = crate::branches::of(store, scope, user, session);
    if ws.is_empty() {
        return json!({"ok": true, "ts": stamp, "lines": [], "count": 0});
    }
    let since = if since_ts > 0.0 { since_ts.max(stamp - MAX_LOOKBACK_S) } else { stamp - 120.0 };
    // A change made before the last step, to a file this session had read
    // then and has just written for the first time, is news once now: when
    // it happened the session only read the file, and a change there was not
    // news yet. Two sessions on one file (v104's test): the one that wrote
    // first heard every change of the other's, the other heard none of the
    // first's, and found them by reading the file again.
    let newly = first_written_since(store, scope, session, since);
    let caught_up = |path: &str, ts: f64| newly.get(path).is_some_and(|first| ts > *first);
    let back = newly.values().fold(since, |a, b| a.min(*b)).max(stamp - MAX_LOOKBACK_S);
    let mut rows = store.ledger_since_kinds(scope, back, &["edit_reported", "rationale"]);
    // the reasons authors gave after the fact, keyed by (path, user, session)
    let reasons: BTreeMap<(String, String, String), String> = rows
        .iter()
        .filter(|row| row.kind == "rationale")
        .map(|row| {
            let p = &row.payload;
            let key = (
                p.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
                p.get("user").and_then(Value::as_str).unwrap_or("").to_string(),
                p.get("session").and_then(Value::as_str).unwrap_or("").to_string(),
            );
            (key, p.get("why").and_then(Value::as_str).unwrap_or("").to_string())
        })
        .collect();
    // A change on another branch is not caught up on: it meets this work only
    // when the branches merge, and the gate said so once per file and branch
    // when this session first wrote there (v110 said it a second time).
    let same_branch = |row: &crate::store::LedgerRow| {
        crate::branches::apart(store, scope, &my_branch, (&text_of(&row.payload, "user"), &row_agent(&row.payload))).is_none()
    };
    rows.retain(|row| {
        let path = row.payload.get("path").and_then(Value::as_str).unwrap_or("");
        row.kind == "edit_reported" && (row.ts > since || (caught_up(path, row.ts) && same_branch(row)))
    });
    if rows.is_empty() {
        return json!({"ok": true, "ts": stamp, "lines": [], "count": 0});
    }
    let graph = crate::graphview::snapshot(store, scope);
    // the names this session changed itself, per file
    let mine = own_names(store, scope, user, session, stamp - WS_TTL_S);
    // the authors, named the way the reader knows them
    let authors: std::collections::BTreeSet<String> = rows
        .iter()
        .filter_map(|row| row.payload.get("user").and_then(Value::as_str).map(str::to_string))
        .filter(|author| author != user)
        .collect();
    let labels = crate::activity::identity_labels(store, user, &authors);
    // latest change per (path, symbol, kind) wins
    let mut seen: BTreeMap<(String, String, String), Value> = BTreeMap::new();
    let mut order: Vec<(String, String, String)> = Vec::new();
    for row in rows {
        let path = row.payload.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        if path.is_empty() {
            continue;
        }
        let row_user = row.payload.get("user").and_then(Value::as_str).unwrap_or("");
        let row_session = row.payload.get("session").and_then(Value::as_str).unwrap_or("");
        // by agent: a helper's write is not its lead's, nor a sibling's
        let author = row_agent(&row.payload);
        let same_user = row_user == user;
        if same_user && (row_session.is_empty() || author == session) {
            continue; // my own step
        }
        let sibling = if same_user { crate::presence::sibling_label(store, scope, user, &author, session) } else { String::new() };
        let who = if !sibling.is_empty() {
            sibling
        } else if same_user {
            // by its title and its Claude Code name, either of which a
            // message `--to` it takes
            let facts = crate::freshness::session_facts(store, scope, row_user, row_session);
            let named = crate::freshness::quoted_work_label(
                &crate::freshness::title_of(store, scope, row_user, row_session), &facts.name, &facts.task);
            if named.is_empty() { "another session of yours".to_string() } else { format!("another session of yours ({named})") }
        } else {
            labels.get(row_user).and_then(|l| l.get("label")).and_then(Value::as_str).unwrap_or(row_user).to_string()
        };
        // another branch: their change meets this one when the branches merge (branches.rs)
        let elsewhere = crate::branches::apart(store, scope, &my_branch, (row_user, &author)).unwrap_or_default();
        let who = if elsewhere.is_empty() { who } else { format!("{who} on branch {elsewhere}") };
        let events: Vec<Value> = row.payload.get("events").and_then(Value::as_array).cloned().unwrap_or_default();
        let changed: Vec<String> = row
            .payload
            .get("symbols_changed")
            .and_then(Value::as_object)
            .map(|m| m.keys().take(MAX_SYMBOLS_PER_ROW).cloned().collect())
            .unwrap_or_default();
        // relevance: a change is news only to code this session's next write
        // edits or calls. In a file it wrote, a change to a name it changed
        // itself or an interface change; anywhere, an interface change to a
        // name its files call. A
        // teammate's new function, a body it never touched, a test added
        // beside its own: none of that is news (Study 8, second rerun: such
        // deltas preceded a third of the agents' manual pulls, and test-file
        // deltas doubled).
        let overlaps = written.contains(&path);
        let is_test = crate::graphview::is_test_path(&path);
        let my_names: BTreeSet<String> = mine.get(&path).cloned().unwrap_or_default();
        let mut via: Option<String> = None;
        if !is_test {
            // the changed names, their new names, and (for a rename or a
            // removal, whose callers point at a name no longer there) the file
            let mut nodes: Vec<String> = Vec::new();
            for symbol in &changed {
                nodes.extend(crate::graphview::find_node(&graph, &path, symbol));
            }
            for event in &events {
                let new_name = event.get("new_name").and_then(Value::as_str).unwrap_or("");
                if !new_name.is_empty() {
                    nodes.extend(crate::graphview::find_node(&graph, &path, new_name));
                }
                if matches!(event.get("kind").and_then(Value::as_str), Some("renamed" | "removed")) {
                    nodes.extend(crate::graphview::find_node(&graph, &path, ""));
                }
            }
            'outer: for nid in &nodes {
                for level in crate::graphview::blast_radius(&graph, nid, 1) {
                    for node in level {
                        if let Some(dep) = node.get("path").and_then(Value::as_str) {
                            // a caller in another file it read or wrote, or in
                            // this one when it wrote it
                            if ws.contains(dep) && (dep != path || overlaps) {
                                via = Some(dep.to_string());
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
        // in a source file it wrote, an interface change anywhere in it: its
        // next write there may call the changed name
        let same_file = overlaps && !is_test;
        if via.is_none() && my_names.is_empty() && !same_file {
            continue;
        }

        let age = age_text(stamp - row.ts);
        // why, when the author said: on the row, or in a rationale row that followed it
        let because = row
            .payload
            .get("why")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|w| !w.is_empty())
            .or_else(|| reasons.get(&(path.clone(), row_user.to_string(), row_session.to_string())).cloned())
            .unwrap_or_default();
        // (symbol, kind, text, fact, once, whether this session changed that name too)
        let mut lines_for_row: Vec<(String, String, String, String, String, bool)> = Vec::new();
        for event in events.iter().take(MAX_EVENTS_PER_ROW) {
            let symbol = event.get("symbol").and_then(Value::as_str).unwrap_or("").to_string();
            let kind = event.get("kind").and_then(Value::as_str).unwrap_or("").to_string();
            let before = event.get("signature_before").and_then(Value::as_str).unwrap_or("");
            let after = event.get("signature_after").and_then(Value::as_str).unwrap_or("");
            // a name it changed itself (any change), or one its code calls (an
            // interface change)
            let edits_it = my_names.contains(&symbol);
            if !edits_it && !((via.is_some() || same_file) && is_interface_change(&kind, before, after)) {
                continue;
            }
            // diffed against another checkout: not a change anyone can rely on
            if kind == "modified" && event.get("confidence").and_then(Value::as_str) == Some(crate::semantics::UNCONFIRMED) {
                continue;
            }
            let new_name = event.get("new_name").and_then(Value::as_str).unwrap_or("");
            // said already (the pre-write gate, the protocol's push): not again
            let fact = fact_id(&path, &symbol, &kind, before, after, new_name).unwrap_or_default();
            if !fact.is_empty() && told.contains(&fact) {
                continue;
            }
            // and said by these deltas before: once per session
            let once = if fact.is_empty() { format!("{path}::{symbol}|{kind}|{row_user}|{author}") } else { fact.clone() };
            if store.eph_get(&told_key(scope, session, &once)).is_some() {
                continue;
            }
            // a ledger row keeps kind "modified" for a signature change and
            // carries the new signature alongside; that is the change that matters
            let what = match kind.as_str() {
                "renamed" => format!("renamed → {new_name}"),
                "removed" => "removed".to_string(),
                "added" => if after.is_empty() { "added".to_string() } else { format!("added: {after}") },
                // an unchanged signature is a body edit, not a new interface;
                // a new one says what callers were written against
                _ if !after.is_empty() && after != before && before.is_empty() => format!("signature → {after}"),
                _ if !after.is_empty() && after != before => format!("signature {before} → {after}"),
                _ => "body changed".to_string(),
            };
            lines_for_row.push((symbol, kind, what, fact, once, edits_it));
        }
        if lines_for_row.is_empty() {
            // a change with no event of its own says only that the file moved:
            // worth a line only when it names code this session changed too
            let both: Vec<String> = changed.iter().filter(|s| my_names.contains(*s)).cloned().collect();
            if both.is_empty() {
                continue;
            }
            let what = format!("changed: {}", both.join(", "));
            let once = format!("{path}|{what}|{row_user}|{author}");
            if store.eph_get(&told_key(scope, session, &once)).is_some() {
                continue;
            }
            lines_for_row.push((String::new(), "changed".to_string(), what, String::new(), once, true));
        }
        for (symbol, kind, what, fact, once, edits_it) in lines_for_row {
            let key = (path.clone(), symbol.clone(), kind.clone());
            if !seen.contains_key(&key) {
                order.push(key.clone());
            }
            // what the hook needs to name the caller's own lines (fixlines.rs)
            let event = events.iter().find(|e| e.get("symbol").and_then(Value::as_str) == Some(symbol.as_str()) && e.get("kind").and_then(Value::as_str) == Some(kind.as_str()));
            let ev = |k: &str| event.and_then(|e| e.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
            let fix_in = via.clone().unwrap_or_else(|| if same_file { path.clone() } else { String::new() });
            seen.insert(key, json!({"symbol": symbol, "what": what, "who": who, "user": row_user, "age": age, "ts": row.ts,
                                    "via": via.clone().unwrap_or_default(), "because": because, "fact": fact, "once": once,
                                    "branch": elsewhere, "kind": kind, "new_name": ev("new_name"), "mine": edits_it,
                                    "before": ev("signature_before"), "after": ev("signature_after"), "fix_in": fix_in}));
        }
    }
    // one line per file, newest file first: its changes joined, one tail
    let mut files: Vec<String> = Vec::new();
    let mut by_file: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for key in order.iter().rev() {
        let Some(entry) = seen.get(key) else { continue };
        if !by_file.contains_key(&key.0) {
            files.push(key.0.clone());
        }
        by_file.entry(key.0.clone()).or_default().push(entry.clone());
    }
    let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut dropped = 0usize;
    let mut count = 0usize;
    let mut facts: Vec<String> = Vec::new();
    // whose changes the lines carry (user ids): the endpoint notes them on
    // the step they reached and takes them off the answer
    let mut authors: Vec<String> = Vec::new();
    // lines whose changes are all on another branch: landing never rebases
    // onto those, they meet this work only when the branches merge
    let mut apart_lines = 0usize;
    // when each line's newest change was reported: its heads-up's "sent"
    let mut sent: Vec<f64> = Vec::new();
    // renames, removals and new signatures, with the caller's file: the hook
    // names the lines of it each one makes wrong (hooks v78, fixlines.rs)
    let mut fixes: Vec<Value> = Vec::new();
    for path in files {
        let entries = &by_file[&path];
        let first = &entries[0];
        let target_of = |e: &Value| {
            let symbol = field(e, "symbol");
            if symbol.is_empty() { path.clone() } else { format!("{path}::{symbol}") }
        };
        let head = if entries.len() == 1 {
            format!("Δ {} {}", target_of(first), field(first, "what"))
        } else {
            let parts: Vec<String> = entries
                .iter()
                .map(|e| {
                    let symbol = field(e, "symbol");
                    if symbol.is_empty() { field(e, "what") } else { format!("{symbol} {}", field(e, "what")) }
                })
                .collect();
            format!("Δ {path}: {}", parts.join("; "))
        };
        let mut who: Vec<String> = Vec::new();
        for e in entries {
            let w = field(e, "who");
            if !who.contains(&w) {
                who.push(w);
            }
        }
        let mut text = format!("{head} — {}, {}", who.join(", "), field(first, "age"));
        // why this agent hears it: its code calls it, or it changed that code too
        if let Some(dep) = entries.iter().map(|e| field(e, "via")).find(|v| !v.is_empty()) {
            text.push_str(&format!(" · you use it from {dep}"));
        } else if entries.iter().any(|e| e.get("mine").and_then(Value::as_bool) == Some(true)) {
            text.push_str(" · you changed it too");
        } else {
            text.push_str(" · in a file you edited");
        }
        text.push_str(&format!(" · {}", next_step(entries)));
        if let Some(why) = entries.iter().map(|e| field(e, "because")).find(|v| !v.is_empty()) {
            text.push_str(&format!(" · because: {why}"));
        }
        // their change is in their checkout; whether it can be in yours yet
        let apart = entries.iter().all(|e| !field(e, "branch").is_empty());
        if apart {
            text.push_str(" · conflicts only when the branches merge");
        } else if let Some(b) = crate::freshness::behind(store, scope, user, session, &path) {
            text.push_str(if !b.pushed {
                " · still only in their checkout (not pushed yet)"
            } else if lands {
                // a pull now buys nothing: the push rebases onto it
                " · pushed; landing will rebase onto it"
            } else {
                " · pushed, not in your copy yet: pull before you edit it"
            });
        }
        if used + text.len() > budget {
            dropped += entries.len();
            continue;
        }
        used += text.len() + 1;
        count += entries.len();
        apart_lines += usize::from(apart);
        lines.push(text);
        sent.push(entries.iter().filter_map(|e| e.get("ts").and_then(Value::as_f64)).fold(0.0, f64::max));
        for once in entries.iter().map(|e| field(e, "once")).filter(|o| !o.is_empty()) {
            let _ = store.eph_set(&told_key(scope, session, &once), &json!(stamp), Some(WS_TTL_S));
        }
        facts.extend(entries.iter().map(|e| field(e, "fact")).filter(|f| !f.is_empty()));
        for author in entries.iter().map(|e| field(e, "user")) {
            if !author.is_empty() && !authors.contains(&author) {
                authors.push(author);
            }
        }
        for e in entries {
            let (kind, before, after) = (field(e, "kind"), field(e, "before"), field(e, "after"));
            let interface = kind == "renamed" || kind == "removed" || (!after.is_empty() && after != before);
            if interface && !field(e, "fix_in").is_empty() && !field(e, "symbol").is_empty() {
                fixes.push(json!({"symbol": field(e, "symbol"), "kind": if kind == "renamed" || kind == "removed" { kind } else { "signature".to_string() },
                                  "new_name": field(e, "new_name"), "signature_after": after, "in": field(e, "fix_in")}));
            }
        }
    }
    let count = count + dropped;
    if lines.is_empty() {
        return json!({"ok": true, "ts": stamp, "lines": [], "count": 0});
    }
    // with landing on, nothing here is a reason to pull: "since your last
    // step" and a list of paths read as "your copy is behind" and sent agents
    // to `git pull` mid-task (Study 8, second rerun)
    let mut out = vec![if apart_lines == lines.len() {
        // all on other branches: nothing to pull and nothing landing rebases onto
        format!(
            "Cork: {count} teammate change(s) on other branches to code you edit or call. They meet yours only \
when the branches merge: carry on. Say so to the user when one changes what you do:"
        )
    } else if lands {
        format!(
            "Cork: {count} teammate change(s) to code you edit or call. Write against them; landing rebases onto them \
when you push: carry on. Say so to the user when one changes what you do:"
        )
    } else {
        format!(
            "Cork deltas since your last step — {count} change(s) by other agents in what you are working on. \
Say so to the user when one changes what you do:"
        )
    }];
    out.extend(lines);
    // aligned with `out`: the heading and the "more" are no heads-up of their own
    let mut sent: Vec<Value> = std::iter::once(Value::Null).chain(sent.into_iter().map(|t| json!(t))).collect();
    if dropped > 0 {
        out.push(format!("  … {dropped} more"));
        sent.push(Value::Null);
    }
    let mut answer = json!({"ok": true, "ts": stamp, "lines": out, "count": count, "sent": sent});
    if !facts.is_empty() {
        answer["facts"] = json!(facts);
    }
    if !authors.is_empty() {
        answer["from"] = json!(authors);
    }
    if !fixes.is_empty() {
        answer["fixes"] = json!(fixes);
    }
    answer
}

/// What to do about one line's changes, the interface ones first: what a
/// caller writes now, else how to edit beside them.
fn next_step(entries: &[Value]) -> &'static str {
    let kind = |k: &str| entries.iter().any(|e| e.get("kind").and_then(Value::as_str) == Some(k));
    let resigned = entries.iter().any(|e| {
        let field = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("");
        field("kind") != "added" && !field("after").is_empty() && field("after") != field("before")
    });
    if kind("renamed") {
        "next: write the new name"
    } else if kind("removed") {
        "next: add no new use of it"
    } else if resigned {
        "next: call it with the new signature"
    } else {
        "next: keep their lines when you edit it"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The map: `path` declares `names`, and `caller` imports and calls each.
    fn callers(db: &Store, scope: &str, path: &str, names: &[&str], caller: &str) {
        let module = path.trim_end_matches(".py").replace('/', ".");
        let symbols: serde_json::Map<String, Value> = names.iter()
            .map(|n| (n.to_string(), json!({"kind": "function", "edges": [], "span": [1, 2], "params": ["x"], "sites": []})))
            .collect();
        crate::codegraph::update_file(db, scope, path, json!({"path": path, "language": "python", "symbols": symbols, "imports": [], "ts": 1.0}), 1.0);
        let edges: Vec<Value> = names.iter().map(|n| json!([n, "", "calls"])).collect();
        let imports: Vec<Value> = names.iter().map(|n| json!({"module": module, "names": [[n, n]]})).collect();
        crate::codegraph::update_file(db, scope, caller, json!({"path": caller, "language": "python",
            "symbols": {"tile": {"kind": "function", "edges": edges, "span": [1, 2], "params": ["x"], "sites": []}},
            "imports": imports, "ts": 1.0}), 1.0);
    }

    /// One agent of sixteen announces a change to `fmt` with no recipient:
    /// it reaches the sessions on that code (reading or writing a file that
    /// calls it, or the file itself), not the other fourteen. A message that
    /// names no code is for everyone.
    #[test]
    fn a_broadcast_reaches_only_the_sessions_on_the_code_it_names() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        let stamp = now();
        callers(&db, scope, "money.py", &["fmt"], "app.py");
        crate::codegraph::update_file(&db, scope, "orders.py", json!({"path": "orders.py", "language": "python",
            "symbols": {"orders": {"kind": "function", "edges": [], "span": [1, 2], "params": [], "sites": []}}, "imports": [], "ts": 1.0}), 1.0);
        for i in 0..16 {
            let file = match i { 1 => "app.py", 2 => "money.py", _ => "orders.py" };
            touch(&db, scope, &format!("s{i}"), &[file.to_string()], stamp);
        }
        touch_briefed(&db, scope, "s15", &["app.py".to_string()], stamp);
        let on = sessions_named_by(&db, scope, "done: `fmt` now takes currency, fmt(x, currency='USD')").expect("names code");
        assert_eq!(on, ["s1".to_string(), "s2".to_string()].into_iter().collect::<BTreeSet<_>>());
        assert!(sessions_named_by(&db, scope, "tests are red on main, I'm on it").is_none(), "names no code: everyone");
    }

    /// Study 8, second rerun: a third of the agents' manual pulls followed a
    /// "deltas since your last step" notice, and test-file deltas doubled. A
    /// teammate's change reaches a session only when it touches a name the
    /// session changed itself or calls; a teammate's new tests in a shared
    /// test file reach no one; and with landing on the header never says
    /// "pull" or "since your last step".
    #[test]
    fn only_code_this_session_edits_or_calls_is_news_and_never_a_pull() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        let stamp = now();
        let own = |path: &str, symbol: &str| {
            db.ledger_append(scope, "edit_reported", &json!({"path": path, "user": "u", "session": "me",
                "events": [{"symbol": symbol, "kind": "added", "signature_after": format!("def {symbol}()")}]}), stamp - 5.0).unwrap();
        };
        // this session wrote its own test and its own route in shared files
        touch_as(&db, scope, "me", &["tests/test_api.py".to_string(), "api/router.py".to_string()], true, stamp - 5.0);
        own("tests/test_api.py", "test_mine");
        own("api/router.py", "handle_mine");
        crate::freshness::note_lands(&db, scope, "u", "me", Some(true));
        let theirs = |path: &str, events: Value| {
            db.ledger_append(scope, "edit_reported", &json!({"path": path, "user": "u", "session": "them", "events": events}), stamp + 0.5).unwrap();
        };
        // a teammate adds tests beside it, adds a route, and edits a body of theirs
        theirs("tests/test_api.py", json!([{"symbol": "test_theirs", "kind": "added", "signature_after": "def test_theirs()"}]));
        theirs("api/router.py", json!([{"symbol": "handle_theirs", "kind": "added", "signature_after": "def handle_theirs()"},
                                       {"symbol": "handle", "kind": "modified", "signature_before": "def handle(r)", "signature_after": "def handle(r)"}]));
        assert_eq!(deltas_told(&db, scope, "u", "me", stamp, 0, &BTreeSet::new())["count"], json!(0), "none of it is news");
        // they change the route this session wrote: news, with no pull in it
        theirs("api/router.py", json!([{"symbol": "handle_mine", "kind": "modified", "signature_before": "def handle_mine()",
                                        "signature_after": "def handle_mine(request)"}]));
        let said = deltas_told(&db, scope, "u", "me", stamp, 0, &BTreeSet::new());
        assert_eq!(said["count"], json!(1), "{said}");
        let text = said["lines"].as_array().unwrap().iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
        assert!(text.contains("handle_mine") && text.contains("landing rebases onto them when you push"), "{text}");
        // why it reaches this agent, and what to do about it
        assert!(text.contains(" · you changed it too · next: call it with the new signature"), "{text}");
        assert!(!text.to_lowercase().contains("pull") && !text.contains("since your last step"), "{text}");
    }

    /// Alice re-signatures compute_tax; Bob read its caller. The delta says
    /// old -> new, names the fact, and leaves it out once Bob's hook says it
    /// already told him (its pre-write gate or the protocol's push did).
    #[test]
    fn an_interface_change_reaches_a_caller_once_whichever_way_it_came() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        let stamp = now();
        callers(&db, scope, "tax.py", &["compute_tax", "rate_for"], "app.py");
        touch(&db, scope, "b1", &["app.py".to_string()], stamp);
        let row = json!({"path": "tax.py", "user": "alice", "session": "a1",
            "events": [{"symbol": "compute_tax", "kind": "modified", "signature_before": "def compute_tax(amount)",
                        "signature_after": "def compute_tax(amount, region)"},
                       {"symbol": "rate_for", "kind": "renamed", "new_name": "rate_of"}],
            "symbols_changed": {"compute_tax": 1, "rate_for": 1}});
        db.ledger_append(scope, "edit_reported", &row, stamp + 0.5).unwrap();
        let fresh = deltas_told(&db, scope, "bob", "b1", stamp, 0, &BTreeSet::new());
        let text = fresh["lines"].as_array().unwrap().iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
        assert!(text.contains("compute_tax signature def compute_tax(amount) → def compute_tax(amount, region)"), "{text}");
        assert!(text.contains(" · you use it from app.py · next: write the new name"), "{text}");
        let facts: Vec<&str> = fresh["facts"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert_eq!(facts.len(), 2);
        assert!(facts.contains(&"tax.py::compute_tax|sig|defcompute_tax(amount,region)"), "{facts:?}");
        assert!(facts.contains(&"tax.py::rate_for|ren|rate_of"), "{facts:?}");
        // the same fact ids the hook keys its told set by (cork-hooks deps.rs)
        assert_eq!(
            fact_id("tax.py", "compute_tax", "signature", "def compute_tax(amount)", "def compute_tax(amount, region)", "").unwrap(),
            "tax.py::compute_tax|sig|defcompute_tax(amount,region)"
        );
        // said once: the same call again says nothing, whatever the hook's list
        assert_eq!(deltas_told(&db, scope, "bob", "b1", stamp, 0, &BTreeSet::new())["count"], json!(0));
        // another session the hook already told (its pre-write gate or the
        // protocol's push did): only what is new is said
        touch(&db, scope, "b2", &["app.py".to_string()], stamp);
        let told: BTreeSet<String> = ["tax.py::compute_tax|sig|defcompute_tax(amount,region)".to_string()].into();
        let again = deltas_told(&db, scope, "bob", "b2", stamp, 0, &told);
        let text = again["lines"].as_array().unwrap().iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
        assert!(!text.contains("compute_tax") && text.contains("rate_for renamed → rate_of"), "{text}");
        touch(&db, scope, "b3", &["app.py".to_string()], stamp);
        let all: BTreeSet<String> = facts.iter().map(|f| f.to_string()).collect();
        assert_eq!(deltas_told(&db, scope, "bob", "b3", stamp, 0, &all)["count"], json!(0));
        // a body edit names no fact
        assert!(fact_id("tax.py", "f", "modified", "def f(a)", "def f(a)", "").is_none());
    }

    /// The field report (v104): sessions A and B in one checkout, on one
    /// file. A wrote first; it heard every change B made there, and B heard
    /// none of A's (A's `kind` field on `Issue`), because when A made them B
    /// had only read the file. When B writes the file, it hears what A
    /// changed in it since B read it: both hear each other, each once.
    #[test]
    fn two_sessions_on_one_file_hear_each_other() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        let stamp = now();
        let edit = |session: &str, events: Value, at: f64| {
            db.ledger_append(scope, "edit_reported", &json!({"path": "src/db.rs", "user": "kevin@x.com", "session": session,
                "events": events}), at).unwrap();
            touch_as(&db, scope, session, &["src/db.rs".to_string()], true, at);
        };
        let lines = |session: &str, since: f64| -> Vec<String> {
            deltas_told(&db, scope, "kevin@x.com", session, since, 0, &BTreeSet::new())["lines"]
                .as_array().unwrap().iter().filter_map(Value::as_str).filter(|l| l.starts_with('Δ')).map(str::to_string).collect()
        };
        // both read the file
        touch(&db, scope, "A", &["src/db.rs".to_string()], stamp - 40.0);
        touch(&db, scope, "B", &["src/db.rs".to_string()], stamp - 40.0);
        // A adds its field to Issue and a migration helper
        edit("A", json!([{"symbol": "Issue", "kind": "modified", "signature_before": "pub struct Issue", "signature_after": "pub struct Issue"},
                         {"symbol": "add_column", "kind": "added", "signature_after": "fn add_column(conn, table, column, decl)"}]), stamp - 30.0);
        // B, still only reading the file, is not told of a body edit there
        assert!(lines("B", stamp - 35.0).is_empty());
        // B adds its own field to Issue
        edit("B", json!([{"symbol": "Issue", "kind": "modified", "signature_before": "pub struct Issue", "signature_after": "pub struct Issue"}]),
             stamp - 10.0);
        // A hears B's change to the code it changed
        let a = lines("A", stamp - 25.0);
        assert!(a.len() == 1 && a[0].starts_with("Δ src/db.rs::Issue body changed — another session of yours"), "{a:?}");
        // and B, now writing the file, hears A's: made after B read it, before B's last step
        let b = lines("B", stamp - 20.0);
        assert!(b.len() == 1 && b[0].starts_with("Δ src/db.rs::Issue body changed — another session of yours") && b[0].contains("you changed it too"), "{b:?}");
        // each once
        assert!(lines("A", stamp - 25.0).is_empty() && lines("B", stamp - 20.0).is_empty());
    }

    /// The catch-up on a file a session writes for the first time leaves out
    /// a change made on another branch: it meets this work only when the
    /// branches merge, and the gate said so once per file and branch (v110
    /// said it again at the next edit, and the release's hook tests failed).
    #[test]
    fn the_catch_up_leaves_out_changes_on_another_branch() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        let stamp = now();
        crate::branches::note(&db, scope, "alice", "a1", "feat/x", "");
        crate::branches::note(&db, scope, "carol", "c1", "main", "");
        crate::branches::note(&db, scope, "bob", "b1", "main", "");
        touch(&db, scope, "b1", &["total.py".to_string()], stamp - 40.0);
        let edit = |user: &str, session: &str, at: f64| {
            db.ledger_append(scope, "edit_reported", &json!({"path": "total.py", "user": user, "session": session,
                "events": [{"symbol": "amount_due", "kind": "modified", "signature_before": "def amount_due(lines)",
                            "signature_after": "def amount_due(lines)"}]}), at).unwrap();
        };
        edit("alice", "a1", stamp - 30.0);
        edit("carol", "c1", stamp - 25.0);
        // bob changes the same function, for the first time
        edit("bob", "b1", stamp - 10.0);
        touch_as(&db, scope, "b1", &["total.py".to_string()], true, stamp - 10.0);
        let said = deltas_told(&db, scope, "bob", "b1", stamp - 20.0, 0, &BTreeSet::new());
        let lines = said["lines"].as_array().unwrap().iter().filter_map(Value::as_str).filter(|l| l.starts_with('Δ')).collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("carol") && !lines[0].contains("alice") && !lines[0].contains("feat/x"), "{lines:?}");
    }

    /// Study 8: the same `Δ fmt signature …` line reached most of sixteen
    /// agents, through every module a briefing had named to them. A change
    /// reaches a session that read or wrote the changed file, or calls into
    /// it from a file it read or wrote; a session only briefed on the file
    /// hears nothing; and nobody hears it twice.
    #[test]
    fn a_delta_reaches_only_the_sessions_that_touch_the_code_and_only_once() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        let stamp = now();
        callers(&db, scope, "money.py", &["fmt"], "app.py");
        touch(&db, scope, "reader", &["app.py".to_string()], stamp);
        touch(&db, scope, "file_reader", &["money.py".to_string()], stamp);
        touch_briefed(&db, scope, "briefed", &["app.py".to_string()], stamp);
        touch(&db, scope, "elsewhere", &["orders.py".to_string()], stamp);
        let row = json!({"path": "money.py", "user": "alice", "session": "a1",
            "events": [{"symbol": "fmt", "kind": "modified", "signature_before": "def fmt(x)", "signature_after": "def fmt(x, currency)"}],
            "symbols_changed": {"fmt": 1}});
        db.ledger_append(scope, "edit_reported", &row, stamp + 0.5).unwrap();
        let said = |session: &str| deltas_told(&db, scope, "u", session, stamp, 0, &BTreeSet::new())["count"].as_u64().unwrap();
        assert_eq!(said("reader"), 1);
        assert_eq!(said("reader"), 0, "once");
        assert_eq!(said("briefed"), 0, "a briefing is not a touch");
        assert_eq!(said("elsewhere"), 0);
        assert_eq!(said("file_reader"), 0, "reading the file it lives in is not calling it");
        // once the briefed session reads the file it is on that code
        touch(&db, scope, "briefed", &["app.py".to_string()], stamp);
        assert_eq!(said("briefed"), 1);
        // a briefing never takes a read back
        touch_briefed(&db, scope, "reader", &["app.py".to_string()], stamp);
        assert!(touched_set(&db, scope, "reader").contains("app.py"));
    }

    /// Kevin's call (2026-11-14): every agent on cork-cloud that day was a
    /// helper of one Claude Code session, and the deltas skipped everything
    /// by the same user and session, so 25 agents heard none of each other's
    /// changes. Each helper is its own agent: a sibling's change is news to
    /// it once, and its own never; the lead hears its helpers, and they hear
    /// the lead.
    #[test]
    fn helpers_of_one_session_hear_each_other_once_and_never_themselves() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        let stamp = now();
        crate::freshness::note_session(&db, scope, "u", "s", "cork-cloud-19", "claude", "m");
        crate::presence::note_worker(&db, scope, "u", "s", "wA", "general-purpose", "repo-names");
        crate::presence::note_worker(&db, scope, "u", "s", "wB", "general-purpose", "dash-nav");
        let (lead, a, b) = ("s".to_string(), crate::presence::agent_session("s", "wA"), crate::presence::agent_session("s", "wB"));
        let wrote = |worker: &str, symbol: &str, before: &str, after: &str, at: f64| {
            db.ledger_append(scope, "edit_reported", &json!({"path": "api/router.py", "user": "u", "session": "s", "worker": worker,
                "events": [{"symbol": symbol, "kind": if before.is_empty() { "added" } else { "modified" },
                            "signature_before": before, "signature_after": after}]}), at).unwrap();
        };
        // all three wrote the router, each its own route
        for (agent, worker, symbol) in [(&lead, "", "handle_lead"), (&a, "wA", "handle_a"), (&b, "wB", "handle_b")] {
            touch_as(&db, scope, agent, &["api/router.py".to_string()], true, stamp - 5.0);
            wrote(worker, symbol, "", &format!("def {symbol}()"), stamp - 5.0);
        }
        let said = |agent: &str| deltas_told(&db, scope, "u", agent, stamp, 0, &BTreeSet::new());
        let lines = |v: &Value| v["lines"].as_array().unwrap().iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
        // helper A changes the route helper B wrote
        wrote("wA", "handle_b", "def handle_b()", "def handle_b(request)", stamp + 0.5);
        let heard = said(&b);
        assert_eq!(heard["count"], json!(1), "{heard}");
        assert!(lines(&heard).contains("cork-cloud-19's helper repo-names"), "{heard}");
        assert_eq!(said(&b)["count"], json!(0), "once");
        assert_eq!(said(&a)["count"], json!(0), "never its own");
        // the lead wrote the file: its helper's new signature there is news to it
        let heard = said(&lead);
        assert_eq!(heard["count"], json!(1), "{heard}");
        assert!(lines(&heard).contains("cork-cloud-19's helper repo-names"), "{heard}");
        // and the lead's change reaches its helpers, named as their lead
        wrote("", "handle_a", "def handle_a()", "def handle_a(request)", stamp + 1.0);
        let heard = said(&a);
        assert_eq!(heard["count"], json!(1), "{heard}");
        assert!(lines(&heard).contains("cork-cloud-19 (your lead)"), "{heard}");
        assert_eq!(said(&lead)["count"], json!(0), "the lead never hears itself");
        // a session with no helpers: its own rows are its own, as before
        db.ledger_append(scope, "edit_reported", &json!({"path": "api/router.py", "user": "u", "session": "solo",
            "events": [{"symbol": "handle_solo", "kind": "added", "signature_after": "def handle_solo()"}]}), stamp - 5.0).unwrap();
        touch_as(&db, scope, "solo", &["api/router.py".to_string()], true, stamp - 5.0);
        db.ledger_append(scope, "edit_reported", &json!({"path": "api/router.py", "user": "u", "session": "solo",
            "events": [{"symbol": "handle_solo", "kind": "modified", "signature_before": "def handle_solo()",
                        "signature_after": "def handle_solo(x)"}]}), stamp + 2.0).unwrap();
        let solo = said("solo");
        assert!(!lines(&solo).contains("handle_solo"), "{solo}");
    }

    /// Helpers in worktrees of their own, on other branches, stay as quiet as
    /// two sessions there are: the first-write catch-up leaves a sibling's
    /// earlier change on another branch out, and keeps one on the same.
    #[test]
    fn a_helper_on_another_branch_is_left_out_of_the_catch_up() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        let stamp = now();
        let (c, d, e) = (crate::presence::agent_session("s", "wC"), crate::presence::agent_session("s", "wD"), crate::presence::agent_session("s", "wE"));
        crate::branches::note(&db, scope, "u", &c, "feat/c", "");
        crate::branches::note(&db, scope, "u", &d, "feat/d", "");
        crate::branches::note(&db, scope, "u", &e, "feat/d", "");
        // D read the file first; C and E each renamed a function in it
        touch(&db, scope, &d, &["shop/cart.py".to_string()], stamp - 100.0);
        touch(&db, scope, &d, &["shop/calls.py".to_string()], stamp - 100.0);
        for (worker, from, to) in [("wC", "total", "cart_total"), ("wE", "fees", "cart_fees")] {
            db.ledger_append(scope, "edit_reported", &json!({"path": "shop/cart.py", "user": "u", "session": "s", "worker": worker,
                "events": [{"symbol": from, "kind": "renamed", "new_name": to}]}), stamp - 50.0).unwrap();
        }
        // then wrote it: a teammate's earlier change on its own branch is news
        // now; one on another branch meets it only when the branches merge
        touch_as(&db, scope, &d, &["shop/cart.py".to_string()], true, stamp - 1.0);
        let said = deltas_told(&db, scope, "u", &d, stamp - 10.0, 0, &BTreeSet::new());
        let text = said["lines"].as_array().unwrap().iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
        assert!(text.contains("cart_fees") && !text.contains("cart_total"), "{text}");
    }
}
