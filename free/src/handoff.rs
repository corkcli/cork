//! A session's handoff, for the next session on the same code or task.
//!
//! An agent that stops mid-task leaves its successor to rediscover where it
//! got to: which files it changed, whether the tests passed, what it meant to
//! do next. When a session ends (the hook's SessionEnd, or a turn's end the
//! harness marks as the session's last), Cork keeps a short structured
//! handoff, built from what it already holds rather than from a new store:
//!
//! * done: the files and symbols the session changed (its `edit_reported`
//!   rows), each with when it last changed it and what git says of it
//!   (uncommitted, committed, pushed: the hook's word at the session's end),
//!   and the last test run the hook saw (`examples`' `extests`), with when.
//!   A session that sat idle overnight and ended this morning hands off
//!   last night's edits as last night's, never as fresh work;
//! * left: the sentences of the agent's last closing message that say what
//!   remains (only while the workspace keeps closing messages: with the
//!   `closing_messages` switch off the hook sends none and the handoff is the
//!   structural part alone), the work it deferred (`defer_intent`'s
//!   tripwires, by reference, rendered from the tripwire itself), and
//!   failing tests;
//! * where: anchored to the files it touched, and to its task as a vector
//!   (never the prompt's words).
//!
//! The turn's own note (`intents::settle_turn`) stays the module's note; the
//! handoff does not repeat it. A handoff is kept when something was left,
//! and offered to another agent only when the record says so (deferred work,
//! failing tests), never on the closing message's words alone, which are
//! the agent's reply to its person and stay with them (`cork today`). It is
//! offered once per receiving session, in the prompt
//! briefing of a session whose prompt names its files or symbols, whose own
//! work is in them, or whose task means the same; it expires after TTL_S,
//! and a later session that ends having changed the same files retires it.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::store::{now, Store};

pub const BUCKET: &str = "handoff";
const TTL_S: f64 = 3.0 * 86_400.0;
/// How far back a session's writes are gathered.
const WINDOW_S: f64 = 24.0 * 3600.0;
const MAX_FILES: usize = 6;
const MAX_SYMBOLS: usize = 8;
const MAX_LEFT: usize = 3;
const MAX_LEFT_CHARS: usize = 160;
const MAX_DEFERRED: usize = 2;
/// At most this many handoffs in one briefing.
const MAX_SHOWN: usize = 2;
/// A task this close in meaning is the same task (examples.rs's floor for
/// "the same kind of task", bge-small, process sentences removed).
const TASK_FLOOR: f32 = 0.80;
const LEFT_TTL_S: f64 = 86_400.0;

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn strings(v: &Value, k: &str) -> Vec<String> {
    v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

fn left_key(scope: &str, session: &str) -> String {
    format!("handoffleft:{scope}:{session}")
}

/// What remains, in the agent's words: the sentences and list items of a
/// closing message that say so ("Still to do: wire the fee into total_due").
pub fn left_items(message: &str) -> Vec<String> {
    const MARKERS: [&str; 18] = [
        "left to", "remaining", "remains", "todo", "to do", "still need", "still to", "not yet", "next step",
        "next:", "unfinished", "didn't get to", "did not get to", "follow-up", "follow up", "haven't", "have not",
        "pending",
    ];
    let mut out: Vec<String> = Vec::new();
    for raw in message.split(['\n']).flat_map(|line| line.split_inclusive(['.', '!', '?'])) {
        // the agent's own words only: not a line of code it quoted, nor a
        // Cork notice it passed on (v104: `pub const STATUSES = ["todo", …]`
        // and "nothing to do unless it changes your work" were handed to the
        // next session as work left over)
        if quoted_not_said(raw) {
            continue;
        }
        let plain: String = raw.replace("**", "").replace('`', "").trim().trim_start_matches(['-', '*', '#', ' ']).trim().to_string();
        if plain.len() < 8 {
            continue;
        }
        // a marker in the words, not inside a quoted string or code span
        let lower = without_quotes(&raw.to_lowercase());
        if !MARKERS.iter().any(|m| lower.contains(m)) || NOT_LEFT.iter().any(|m| lower.contains(m)) {
            continue;
        }
        let clipped: String = if plain.chars().count() > MAX_LEFT_CHARS {
            format!("{}…", plain.chars().take(MAX_LEFT_CHARS - 1).collect::<String>().trim_end())
        } else {
            plain
        };
        if !out.contains(&clipped) {
            out.push(clipped);
        }
        if out.len() >= MAX_LEFT {
            break;
        }
    }
    out
}

/// Phrases with a marker in them that say nothing is left.
const NOT_LEFT: [&str; 5] = ["nothing to do", "nothing left", "nothing remains", "nothing remaining", "no follow-up"];

/// A line of code, or a line of Cork's own output the agent passed on.
fn quoted_not_said(line: &str) -> bool {
    let t = line.trim().trim_start_matches(['-', '*', '>', ' ']);
    let code = ["pub ", "fn ", "const ", "let ", "def ", "function ", "export ", "import ", "use ", "class ", "struct ", "return "]
        .iter()
        .any(|k| t.starts_with(k))
        || t.ends_with(';')
        || t.ends_with('{')
        || t.contains("[&str")
        || t.contains(" => ")
        || (t.contains(" = ") && (t.contains('[') || t.contains('{') || t.contains('(')));
    // a briefing's symbol line ends in its line range: `fn f() [7-9]`
    let ranged = t.trim_end_matches(['.', ';']).ends_with(']')
        && t.rsplit('[').next().is_some_and(|r| r.trim_end_matches([']', '.', ';']).split('-').all(|n| n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty()));
    let cork = ["cork:", "cork briefing", "for your information", "(not your user", "Δ "].iter().any(|k| t.to_lowercase().starts_with(k) || t.contains("(not your user"));
    code || ranged || cork
}

/// A line with its quoted strings and code spans taken out.
fn without_quotes(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut inside: Option<char> = None;
    for c in line.chars() {
        match inside {
            Some(q) if c == q => inside = None,
            Some(_) => {}
            None if c == '"' || c == '`' => inside = Some(c),
            None => out.push(c),
        }
    }
    out
}

/// A turn ended with this closing message: what it says remains is kept for
/// the session's handoff (the newest turn's word wins). An empty message (the
/// privacy switch is off, or the agent said nothing) clears nothing.
pub fn note_closing(store: &Store, scope: &str, session: &str, message: &str) {
    if session.is_empty() || message.trim().is_empty() {
        return;
    }
    let left = left_items(message);
    let _ = store.eph_set(&left_key(scope, session), &json!({"left": left, "ts": now()}), Some(LEFT_TTL_S));
}

/// The session is over: keep its handoff. None when it changed nothing and
/// left nothing.
pub fn save(store: &Store, scope: &str, user: &str, session: &str) -> Option<Value> {
    save_with(store, scope, user, session, None)
}

/// What git says of one file the session changed, from the hook's report
/// at its end (`{uncommitted, unpushed, upstream}`), else from a landing
/// of this session's since the file's last change; "" when unknown.
fn git_state(store: &Store, scope: &str, user: &str, session: &str, path: &str, changed_at: f64, git: Option<&Value>) -> &'static str {
    if let Some(git) = git.filter(|g| g.is_object()) {
        let has = |key: &str| strings(git, key).iter().any(|p| p == path);
        return if has("uncommitted") {
            "uncommitted"
        } else if has("unpushed") {
            "committed, not pushed"
        } else if git.get("upstream").and_then(Value::as_bool) == Some(true) {
            "pushed"
        } else {
            "committed"
        };
    }
    let landed = store
        .ledger_since_kinds(scope, changed_at, &["landed"])
        .iter()
        .any(|row| text(&row.payload, "user") == user && text(&row.payload, "session") == session);
    if landed { "pushed" } else { "" }
}

/// `save`, with what git says of the session's files (`git_state`).
pub fn save_with(store: &Store, scope: &str, user: &str, session: &str, git: Option<&Value>) -> Option<Value> {
    if session.is_empty() {
        return None;
    }
    let stamp = now();
    let mut files: Vec<String> = Vec::new();
    // when each file last changed: the session's end can be hours after it
    let mut changed_at: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut symbols: Vec<String> = Vec::new();
    let mut first_write = f64::MAX;
    let mut last_write = 0.0f64;
    for row in store.ledger_since_kinds(scope, stamp - WINDOW_S, &["edit_reported"]) {
        let p = &row.payload;
        if text(p, "session") != session || text(p, "user") != user {
            continue;
        }
        let path = text(p, "path");
        if path.is_empty() || crate::codegraph::is_collide_artifact(&path) {
            continue;
        }
        first_write = first_write.min(row.ts);
        last_write = last_write.max(row.ts);
        if !files.contains(&path) {
            files.push(path.clone());
        }
        let newest = changed_at.get(&path).and_then(Value::as_f64).unwrap_or(0.0).max(row.ts);
        changed_at.insert(path.clone(), json!(newest));
        let mut names: Vec<String> = p.get("symbols_changed").and_then(Value::as_object).map(|m| m.keys().cloned().collect()).unwrap_or_default();
        for event in p.get("events").and_then(Value::as_array).into_iter().flatten() {
            names.push(text(event, "symbol"));
        }
        for name in names.into_iter().filter(|n| !n.is_empty()) {
            if !symbols.contains(&name) {
                symbols.push(name);
            }
        }
    }
    // the last test run the hook saw in this session's shell
    let tests = store.eph_get(&format!("extests:{scope}:{session}")).map(|t| {
        let ts = t.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        json!({"ok": t.get("ok").and_then(Value::as_bool).unwrap_or(false), "command": text(&t, "command"),
               "after_last_write": last_write > 0.0 && ts >= last_write, "ts": ts})
    });
    let left: Vec<String> = store.eph_get(&left_key(scope, session)).map(|l| strings(&l, "left")).unwrap_or_default();
    // the work it deferred, by reference: the tripwire is the record
    let since = if first_write < f64::MAX { first_write - 2.0 * 3600.0 } else { stamp - 6.0 * 3600.0 };
    // the work this session deferred: a tripwire it set, or one of its
    // person's that names no session and watches only its own files and
    // names. Another session's leftovers are that session's to hand off
    let own_ground = |t: &Value| {
        let paths = strings(t, "paths");
        let names = strings(t, "symbols");
        (!paths.is_empty() || !names.is_empty())
            && paths.iter().all(|p| files.contains(p))
            && names.iter().all(|n| symbols.contains(n))
    };
    let mut deferred: Vec<(f64, Value)> = store
        .kv_list("tripwire", &format!("{scope}:"))
        .into_iter()
        .filter(|(_, t)| text(t, "owner") == user && t.get("fired").map(Value::is_null).unwrap_or(true))
        .filter(|(_, t)| match text(t, "session").as_str() {
            "" => own_ground(t),
            theirs => theirs == session,
        })
        .filter_map(|(_, t)| {
            let created = t.get("created").and_then(Value::as_f64).unwrap_or(0.0);
            (created >= since).then_some((created, t))
        })
        .collect();
    deferred.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    deferred.truncate(MAX_DEFERRED);
    // where the deferred work is: the handoff is anchored there too
    let mut anchors: Vec<String> = Vec::new();
    for (_, t) in &deferred {
        for p in strings(t, "paths") {
            if !files.contains(&p) && !anchors.contains(&p) {
                anchors.push(p);
            }
        }
        for name in strings(t, "symbols") {
            if !symbols.contains(&name) {
                symbols.push(name);
            }
        }
    }
    let deferred: Vec<String> = deferred.into_iter().map(|(_, t)| text(&t, "id")).collect();
    if files.is_empty() && left.is_empty() && deferred.is_empty() {
        return None;
    }
    let task = store.eph_get(&format!("extask:{scope}:{session}"));
    let kept: Vec<&String> = files.iter().take(MAX_FILES).collect();
    let git_of: serde_json::Map<String, Value> = kept
        .iter()
        .map(|path| {
            let at = changed_at.get(path.as_str()).and_then(Value::as_f64).unwrap_or(0.0);
            ((*path).clone(), json!(git_state(store, scope, user, session, path, at, git)))
        })
        .collect();
    let changed_at: serde_json::Map<String, Value> =
        kept.iter().filter_map(|path| changed_at.get(path.as_str()).map(|at| ((*path).clone(), at.clone()))).collect();
    let record = json!({
        "user": user, "session": session, "ended": stamp,
        "files": kept,
        "changed_at": changed_at, "git": git_of,
        "anchors": anchors,
        "symbols": symbols.iter().take(MAX_SYMBOLS).collect::<Vec<_>>(),
        "tests": tests.unwrap_or(Value::Null),
        "left": left, "deferred": deferred,
        "q": task.as_ref().map(|t| text(t, "q")).unwrap_or_default(),
        "title": crate::freshness::title_of(store, scope, user, session),
        "task": crate::freshness::session_facts(store, scope, user, session).task,
    });
    // a later handoff over the same files is the newer word on them
    let mine: BTreeSet<&String> = files.iter().collect();
    for (key, older) in store.kv_list(BUCKET, &format!("{scope}:")) {
        let theirs = strings(&older, "files");
        if text(&older, "session") != session && !theirs.is_empty() && theirs.iter().all(|f| mine.contains(f)) {
            let _ = store.kv_delete(BUCKET, &key);
        }
    }
    // a session that finished its work retires the handoffs it took over,
    // and leaves none of its own: only what is left is handed off
    if !unfinished(&record) {
        let _ = store.kv_delete(BUCKET, &format!("{scope}:{session}"));
        return Some(record);
    }
    let _ = store.kv_put(BUCKET, &format!("{scope}:{session}"), &record, stamp);
    let _ = store.ledger_append_later(scope, "handoff_saved", &json!({"user": user, "session": session,
        "files": record["files"].as_array().map(Vec::len).unwrap_or(0)}), stamp);
    Some(record)
}

/// Something was left: words that say so, deferred work, or failing tests.
fn unfinished(h: &Value) -> bool {
    !strings(h, "left").is_empty() || left_in_fact(h)
}

/// Left by the record, not by what the agent said: deferred work (its
/// defer_intent tripwires) or failing tests. Only this reaches another
/// session's briefing; the closing message's "still to do" is the agent's
/// reply to its person (shown to them in `cork today`), and quoted to
/// another agent it was noise ("I haven't messaged the other session").
fn left_in_fact(h: &Value) -> bool {
    !strings(h, "deferred").is_empty() || h.pointer("/tests/ok").and_then(Value::as_bool) == Some(false)
}

pub struct Receiver<'a> {
    pub scope: &'a str,
    pub viewer: &'a str,
    pub session: &'a str,
    /// What this briefing points at, and what the session has read or written.
    pub paths: &'a [String],
    /// The names the prompt used.
    pub identifiers: &'a [String],
    /// The prompt's task vector; empty without embeddings.
    pub q: &'a [f32],
    pub share_knowledge: bool,
}

/// The handoffs this session should hear, each once, as briefing lines.
pub fn brief_lines(store: &Store, r: &Receiver) -> Vec<String> {
    if r.session.is_empty() {
        return Vec::new();
    }
    let stamp = now();
    let paths: BTreeSet<&str> = r.paths.iter().map(String::as_str).collect();
    let names: BTreeSet<String> = r.identifiers.iter().map(|i| i.trim().to_lowercase()).collect();
    let mut matched: Vec<(f64, Value)> = Vec::new();
    for (key, h) in store.kv_list(BUCKET, &format!("{}:", r.scope)) {
        let ended = h.get("ended").and_then(Value::as_f64).unwrap_or(0.0);
        if ended < stamp - TTL_S {
            let _ = store.kv_delete(BUCKET, &key);
            continue;
        }
        if text(&h, "session") == r.session || !left_in_fact(&h) {
            continue;
        }
        if !r.share_knowledge && text(&h, "user") != r.viewer {
            continue;
        }
        let mut files = strings(&h, "files");
        files.extend(strings(&h, "anchors"));
        let on_files = files.iter().any(|f| {
            paths.contains(f.as_str())
                || names.iter().any(|n| (n.contains('/') || n.contains('.')) && (f.to_lowercase() == *n || f.to_lowercase().ends_with(&format!("/{n}"))))
        });
        let on_symbols = strings(&h, "symbols").iter().any(|s| names.contains(&s.to_lowercase()));
        let q = crate::embed::decode_vec(&text(&h, "q"));
        let on_task = !r.q.is_empty() && !q.is_empty() && crate::embed::cosine(r.q, &q) >= TASK_FLOOR;
        if !(on_files || on_symbols || on_task) {
            continue;
        }
        let told = format!("handofftold:{}:{}:{}", r.scope, r.session, text(&h, "session"));
        if store.eph_get(&told).is_some() {
            continue;
        }
        matched.push((ended, h));
    }
    matched.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::new();
    for (_, h) in matched.into_iter().take(MAX_SHOWN) {
        let told = format!("handofftold:{}:{}:{}", r.scope, r.session, text(&h, "session"));
        let _ = store.eph_set(&told, &json!({"ts": stamp}), Some(TTL_S));
        let _ = store.ledger_append_later(r.scope, "handoff_shown", &json!({"user": r.viewer, "session": r.session,
            "from": text(&h, "session")}), stamp);
        out.push(render(store, r.scope, r.viewer, &h, stamp));
    }
    out
}

/// "Handoff from becca's session (ended 2h ago): changed billing/pricing.py
/// (14h ago, uncommitted): price_for; tests FAILING 14h ago (`pytest -q`).
/// Left: wire the fee into total_due."
pub fn render(store: &Store, scope: &str, viewer: &str, h: &Value, stamp: f64) -> String {
    let user = text(h, "user");
    let who = if user == viewer { "your earlier session".to_string() } else { format!("{}'s session", crate::inflight::author_label(store, viewer, &user)) };
    let age = crate::deltas::age_text(stamp - h.get("ended").and_then(Value::as_f64).unwrap_or(stamp));
    // what it was doing, else what the harness called it
    let task = text(h, "task");
    let title = if task.is_empty() { text(h, "title") } else { task };
    let on = if title.is_empty() { String::new() } else { format!(" on \"{}\"", title.chars().take(80).collect::<String>()) };
    let mut parts: Vec<String> = Vec::new();
    let files = strings(h, "files");
    if !files.is_empty() {
        let symbols = strings(h, "symbols");
        let named = if symbols.is_empty() { String::new() } else { format!(": {}", symbols.join(", ")) };
        parts.push(format!("changed {}{named}", changed_files(h, &files, stamp)));
    }
    if let Some(tests) = h.get("tests").filter(|t| t.is_object()) {
        let command = text(tests, "command");
        let how = if command.is_empty() { String::new() } else { format!(" (`{}`)", command.chars().take(80).collect::<String>()) };
        let ok = tests.get("ok").and_then(Value::as_bool).unwrap_or(false);
        // when they ran, as the files say when they changed
        let ran = tests.get("ts").and_then(Value::as_f64).map(|ts| format!(" {}", crate::deltas::age_text(stamp - ts))).unwrap_or_default();
        let when = if tests.get("after_last_write").and_then(Value::as_bool).unwrap_or(false) { "" } else { " before its last write" };
        parts.push(if ok { format!("tests passed{ran}{when}{how}") } else { format!("tests FAILING{ran}{when}{how}") });
    }
    let mut line = format!("Handoff from {who}{on} (ended {age}): {}.", parts.join("; "));
    let deferred: Vec<String> = strings(h, "deferred")
        .iter()
        .filter_map(|id| store.kv_get("tripwire", &format!("{scope}:{id}")))
        .filter(|t| t.get("fired").map(Value::is_null).unwrap_or(true))
        .map(|t| {
            let at: Vec<String> = strings(&t, "paths").into_iter().chain(strings(&t, "symbols")).take(2).collect();
            format!("\"{}\" ({})", text(&t, "note").chars().take(MAX_LEFT_CHARS).collect::<String>(), at.join(", "))
        })
        .collect();
    if !deferred.is_empty() {
        line.push_str(&format!(" Deferred: {}.", deferred.join("; ")));
    }
    line
}

/// The files a handoff changed, each with when it last changed and what git
/// says of it, the ones alike said together: "API.md, Cargo.toml (14h ago,
/// committed, not pushed), src/api.rs (14h ago, uncommitted)". A handoff
/// kept before v103 knows neither, and names the files alone.
fn changed_files(h: &Value, files: &[String], stamp: f64) -> String {
    let changed_at = h.get("changed_at").and_then(Value::as_object);
    let git = h.get("git").and_then(Value::as_object);
    let mut groups: Vec<(String, Vec<&str>)> = Vec::new();
    for path in files {
        let mut said: Vec<String> = Vec::new();
        if let Some(at) = changed_at.and_then(|m| m.get(path)).and_then(Value::as_f64) {
            said.push(crate::deltas::age_text(stamp - at));
        }
        if let Some(state) = git.and_then(|m| m.get(path)).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            said.push(state.to_string());
        }
        let said = said.join(", ");
        match groups.last_mut() {
            Some((last, paths)) if *last == said => paths.push(path),
            _ => groups.push((said, vec![path])),
        }
    }
    groups
        .into_iter()
        .map(|(said, paths)| if said.is_empty() { paths.join(", ") } else { format!("{} ({said})", paths.join(", ")) })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn receiver<'a>(session: &'a str, paths: &'a [String], identifiers: &'a [String]) -> Receiver<'a> {
        Receiver { scope: "w:r", viewer: "kevin@x.com", session, paths, identifiers, q: &[], share_knowledge: true }
    }

    #[test]
    fn what_is_left_is_read_from_the_closing_message() {
        let message = "I added `card_fee` and wired it into the invoice.\n\n**Still to do:** apply the fee in total_due. \
The peak multiplier is not yet used anywhere. Tests pass.";
        assert_eq!(left_items(message), vec!["Still to do: apply the fee in total_due.", "The peak multiplier is not yet used anywhere."]);
        assert!(left_items("All done, tests pass.").is_empty());
    }

    #[test]
    fn a_finished_session_leaves_nothing_to_hand_off_and_an_unfinished_one_does() {
        let db = store();
        let edit = json!({"user": "becca@x.com", "session": "A", "path": "billing/pricing.py",
                          "events": [{"symbol": "card_fee", "kind": "added"}]});
        db.ledger_append("w:r", "edit_reported", &edit, now()).unwrap();
        // the tests passed and the agent said nothing was left: nothing to tell
        crate::examples::note_tests(&db, "w:r", "becca@x.com", "A", true, "pytest -q");
        save(&db, "w:r", "becca@x.com", "A").unwrap();
        let paths = vec!["billing/pricing.py".to_string()];
        assert!(brief_lines(&db, &receiver("B", &paths, &[])).is_empty());
        // a closing message that says work is left is the agent's reply to
        // its person: kept for them (`cork today`), never quoted to another agent
        note_closing(&db, "w:r", "A", "Added card_fee. Still to do: apply it in total_due.");
        let kept = save(&db, "w:r", "becca@x.com", "A").unwrap();
        assert_eq!(kept["left"], json!(["Still to do: apply it in total_due."]));
        assert!(brief_lines(&db, &receiver("B", &paths, &[])).is_empty());
        // the record leaving work does reach it: the tests now fail
        crate::examples::note_tests(&db, "w:r", "becca@x.com", "A", false, "pytest -q");
        save(&db, "w:r", "becca@x.com", "A").unwrap();
        let lines = brief_lines(&db, &receiver("B", &paths, &[]));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0], "Handoff from becca@x.com's session (ended just now): changed billing/pricing.py (just now): card_fee; tests FAILING just now (`pytest -q`).");
        // once per session
        assert!(brief_lines(&db, &receiver("B", &paths, &[])).is_empty());
        // and never to the session that left it
        assert!(brief_lines(&db, &receiver("A", &paths, &[])).is_empty());
    }

    #[test]
    fn a_session_that_finishes_the_work_retires_the_handoff() {
        let db = store();
        for (session, user) in [("A", "becca@x.com"), ("B", "dan@x.com")] {
            let edit = json!({"user": user, "session": session, "path": "billing/pricing.py", "events": [{"symbol": "card_fee"}]});
            db.ledger_append("w:r", "edit_reported", &edit, now()).unwrap();
        }
        note_closing(&db, "w:r", "A", "Still to do: apply the fee in total_due.");
        save(&db, "w:r", "becca@x.com", "A").unwrap();
        // B took it over and finished, tests passing
        crate::examples::note_tests(&db, "w:r", "dan@x.com", "B", true, "pytest -q");
        save(&db, "w:r", "dan@x.com", "B").unwrap();
        let paths = vec!["billing/pricing.py".to_string()];
        assert!(brief_lines(&db, &receiver("C", &paths, &[])).is_empty());
        assert!(db.kv_list(BUCKET, "w:r:").is_empty());
    }

    #[test]
    fn with_closing_messages_off_the_handoff_is_structural() {
        let db = store();
        let edit = json!({"user": "becca@x.com", "session": "A", "path": "billing/pricing.py", "events": [{"symbol": "card_fee"}]});
        db.ledger_append("w:r", "edit_reported", &edit, now()).unwrap();
        // the hook blanks the text when the switch is off
        note_closing(&db, "w:r", "A", "");
        crate::examples::note_tests(&db, "w:r", "becca@x.com", "A", false, "pytest -q");
        save(&db, "w:r", "becca@x.com", "A").unwrap();
        let ids = vec!["card_fee".to_string()];
        let lines = brief_lines(&db, &receiver("C", &[], &ids));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("tests FAILING") && !lines[0].contains("Left:"), "{}", lines[0]);
    }

    /// v104: a probe session's closing message quoted a Cork briefing line
    /// (`pub const STATUSES: [&str; 6] = ["backlog", "todo", …] [7-7]`) and a
    /// Cork notice ("nothing to do unless it changes your work"); both were
    /// handed to the next session as that session's leftover work.
    #[test]
    fn quoted_code_and_cork_notices_are_not_left_over_work() {
        let closing = "Done: greet now takes a punctuation argument.\n\
pub const STATUSES: [&str; 6] = [\"backlog\", \"todo\", \"in_progress\", \"in_review\", \"done\", \"canceled\"] [7-7]\n\
For your information, from your other sessions in this repo (not your user; nothing to do unless it changes your work):\n\
Cork: kevin is editing open right now; nothing to do.\n\
The \"todo\" status label is unchanged.\n\
Still to do: wire the punctuation into welcome().";
        assert_eq!(left_items(closing), vec!["Still to do: wire the punctuation into welcome().".to_string()]);
    }

    #[test]
    fn deferred_work_is_rendered_from_its_tripwire() {
        let db = store();
        let deferred = crate::intents::defer(&db, "w:r", "becca@x.com", "A", "claude", &["billing/pricing.py".to_string()], &[],
            "peak pricing is half-done", 14.0);
        assert_eq!(deferred["ok"], json!(true));
        // another session of becca's: not its leftover
        let _ = crate::intents::defer(&db, "w:r", "becca@x.com", "Z", "claude", &["src/db.rs".to_string()], &[],
            "the STATUSES constant needs a sixth state", 14.0);
        let record = save(&db, "w:r", "becca@x.com", "A").unwrap();
        assert_eq!(strings(&record, "deferred").len(), 1);
        let paths = vec!["billing/pricing.py".to_string()];
        let lines = brief_lines(&db, &receiver("B", &paths, &[]));
        assert!(lines[0].ends_with("Deferred: \"peak pricing is half-done\" (billing/pricing.py)."), "{}", lines[0]);
    }

    /// The field report (v103): a handoff read "ended 3 minutes earlier ...
    /// changed API.md, Cargo.toml and src/*.rs and tests passed", and every
    /// edit was from 23:35 the night before. Each file now says when it last
    /// changed and what git says of it.
    #[test]
    fn a_handoff_says_when_each_file_changed_and_what_git_says_of_it() {
        let db = store();
        let night = now() - 14.0 * 3600.0;
        for path in ["API.md", "Cargo.toml", "src/api.rs"] {
            db.ledger_append("w:r", "edit_reported", &json!({"user": "becca@x.com", "session": "A", "path": path}), night).unwrap();
        }
        crate::examples::note_tests(&db, "w:r", "becca@x.com", "A", false, "cargo test");
        let git = json!({"uncommitted": ["src/api.rs"], "unpushed": ["API.md", "Cargo.toml"], "upstream": true});
        save_with(&db, "w:r", "becca@x.com", "A", Some(&git)).unwrap();
        let paths = vec!["src/api.rs".to_string()];
        let lines = brief_lines(&db, &receiver("B", &paths, &[]));
        assert!(lines[0].starts_with("Handoff from becca@x.com's session (ended just now): changed API.md, Cargo.toml \
(14h ago, committed, not pushed), src/api.rs (14h ago, uncommitted); tests FAILING"), "{}", lines[0]);
        // without the hook's word, a landing of the session's since says pushed
        db.ledger_append("w:r", "edit_reported", &json!({"user": "dan@x.com", "session": "C", "path": "web/board.js"}), night).unwrap();
        db.ledger_append("w:r", "landed", &json!({"user": "dan@x.com", "session": "C"}), now() - 3600.0).unwrap();
        crate::examples::note_tests(&db, "w:r", "dan@x.com", "C", false, "npm test");
        save(&db, "w:r", "dan@x.com", "C").unwrap();
        let paths = vec!["web/board.js".to_string()];
        let lines = brief_lines(&db, &receiver("B", &paths, &[]));
        assert!(lines[0].contains("changed web/board.js (14h ago, pushed); tests FAILING"), "{}", lines[0]);
    }

    #[test]
    fn a_handoff_reaches_only_code_or_tasks_it_is_about() {
        let db = store();
        let edit = json!({"user": "becca@x.com", "session": "A", "path": "billing/pricing.py", "events": [{"symbol": "card_fee"}]});
        db.ledger_append("w:r", "edit_reported", &edit, now()).unwrap();
        crate::examples::note_tests(&db, "w:r", "becca@x.com", "A", false, "pytest -q");
        save(&db, "w:r", "becca@x.com", "A").unwrap();
        let elsewhere = vec!["api/router.py".to_string()];
        let ids = vec!["handle".to_string()];
        assert!(brief_lines(&db, &receiver("B", &elsewhere, &ids)).is_empty());
        // named by its path's tail
        let named = vec!["pricing.py".to_string()];
        assert_eq!(brief_lines(&db, &receiver("B", &[], &named)).len(), 1);
    }
}
