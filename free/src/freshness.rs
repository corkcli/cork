//! Whose change is in your copy.
//!
//! Every agent works in its own checkout. When a teammate changes a file,
//! the change is in THEIR copy; it reaches yours only after they push and you
//! pull. Three agents on one repo showed what happens without this: the one
//! fixing revenue edited `dashboard_tiles.py` a minute after a teammate had
//! renamed the function it calls, pushed, and moved on; her copy still had
//! the old name, and she paid for it at the rebase (and, without Cork, the
//! merged code was wrong two runs in three).
//!
//! So each copy is fingerprinted: the content hash of every file an agent
//! reads or writes, and the latest teammate write of each file. A write to a
//! file a teammate changed after your copy was taken is a collision about to
//! happen, and the gate says so once, with what to do: pull first when their
//! change is pushed, or work elsewhere while it is still only in their copy.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::codegraph::path_key;
use crate::store::{now, Store};

/// How long a copy fingerprint and a latest write are kept.
const TTL_S: f64 = 7_200.0;
/// A teammate write older than this is history, not a live collision.
const LIVE_S: f64 = 1_800.0;

/// A copy's fingerprint: a hash over its line hashes, which a hook that
/// parses locally sends instead of the text, and the server computes from
/// the text otherwise — the same fingerprint either way.
pub fn fingerprint(line_hashes: &[String]) -> String {
    let digest = Sha256::digest(line_hashes.join(",").as_bytes());
    digest.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

pub fn fingerprint_content(content: &str) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    fingerprint(&crate::diff::line_hashes(&lines))
}

fn copy_key(scope: &str, user: &str, session: &str, path: &str) -> String {
    format!("copyhash:{scope}:{user}:{session}:{}", path_key(path))
}

/// When this session last saw `path` (read it, or wrote it): its copy's time.
pub fn copy_seen_at(store: &Store, scope: &str, user: &str, session: &str, path: &str) -> Option<f64> {
    store.eph_get(&copy_key(scope, user, session, path)).and_then(|c| c.get("ts").and_then(Value::as_f64))
}

/// The files this session read or wrote lately (its copies, kept `TTL_S`),
/// newest first: what it is working on, by language and folder.
pub fn seen_paths(store: &Store, scope: &str, user: &str, session: &str) -> Vec<String> {
    if session.is_empty() {
        return Vec::new();
    }
    let mut seen: Vec<(f64, String)> = store
        .eph_scan(&format!("copyhash:{scope}:{user}:{session}:"))
        .into_iter()
        .filter_map(|(_, c)| {
            let path = c.get("path").and_then(Value::as_str)?.to_string();
            Some((c.get("ts").and_then(Value::as_f64).unwrap_or(0.0), path))
        })
        .collect();
    seen.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    seen.into_iter().map(|(_, path)| path).collect()
}

fn write_key(scope: &str, path: &str) -> String {
    format!("writehash:{scope}:{}", path_key(path))
}

fn checkout_key(scope: &str, user: &str, session: &str) -> String {
    format!("checkout:{scope}:{user}:{session}")
}

/// Which working copy a session is in (the hooks send it with each call).
pub fn note_checkout(store: &Store, scope: &str, user: &str, session: &str, checkout: &str) {
    if session.is_empty() || checkout.is_empty() {
        return;
    }
    let key = checkout_key(scope, user, session);
    if store.eph_get(&key).and_then(|v| v.as_str().map(str::to_string)).as_deref() != Some(checkout) {
        let _ = store.eph_set(&key, &json!(checkout), Some(TTL_S));
    }
}

fn title_key(scope: &str, user: &str, session: &str) -> String {
    format!("title:{scope}:{user}:{session}")
}

/// What the harness calls the session ("Fix spend accounting", or the name
/// a person gave it), as the hook reads it off the transcript (hooks v72+):
/// the roster's name for the agent. A call that names none changes
/// nothing.
pub fn note_title(store: &Store, scope: &str, user: &str, session: &str, title: &str) {
    let title: String = title.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect();
    if session.is_empty() || title.is_empty() {
        return;
    }
    let key = title_key(scope, user, session);
    if store.eph_get(&key).and_then(|v| v.as_str().map(str::to_string)).as_deref() != Some(title.as_str()) {
        let _ = store.eph_set(&key, &json!(title), Some(TITLE_TTL_S));
    }
}

/// As long as the roster keeps an agent's last action: a session that went
/// quiet is still listed by its name.
const TITLE_TTL_S: f64 = 7.0 * 86_400.0;

/// The title `note_title` kept for a session, "" when none.
pub fn title_of(store: &Store, scope: &str, user: &str, session: &str) -> String {
    if session.is_empty() {
        return String::new();
    }
    store.eph_get(&title_key(scope, user, session)).and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

/// Per workspace, not per repo: a session is the same session in every
/// repo it works in.
fn facts_key(scope: &str, user: &str, session: &str) -> String {
    format!("sessfacts:{}:{user}:{session}", crate::presence::split_scope(scope).0)
}

/// What a session is, as its hooks describe it (hooks v103+): the name
/// Claude Code gives it (`cork-linear-test-e3`, what its own ListAgents and
/// SendMessage use), the harness it runs in (`claude`, `codex`, `cursor`)
/// and the machine it runs on (the hooks' machine id).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionFacts {
    pub name: String,
    pub harness: String,
    pub machine: String,
    /// what it is doing now, as its hooks name it (hooks v105+): the item its
    /// todo list has in progress, else what its latest prompt asked
    pub task: String,
}

/// Keep what a hook call says of its session. A field it leaves out stays
/// as it was.
pub fn note_session(store: &Store, scope: &str, user: &str, session: &str, name: &str, harness: &str, machine: &str) {
    let clean = |v: &str, n: usize| -> String { v.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(n).collect() };
    let (name, harness, machine) = (clean(name, 80), clean(harness, 20).to_ascii_lowercase(), clean(machine, 80));
    if session.is_empty() || (name.is_empty() && harness.is_empty() && machine.is_empty()) {
        return;
    }
    let key = facts_key(scope, user, session);
    let kept = store.eph_get(&key).unwrap_or_else(|| json!({}));
    let mut record = kept.clone();
    for (field, value) in [("name", name), ("harness", harness), ("machine", machine)] {
        if !value.is_empty() {
            record[field] = json!(value);
        }
    }
    if record != kept {
        let _ = store.eph_set(&key, &record, Some(TITLE_TTL_S));
    }
}

/// What `note_session` kept for a session; empty fields for what no hook said.
pub fn session_facts(store: &Store, scope: &str, user: &str, session: &str) -> SessionFacts {
    if session.is_empty() {
        return SessionFacts::default();
    }
    let record = store.eph_get(&facts_key(scope, user, session)).unwrap_or_default();
    let field = |k: &str| record.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    SessionFacts { name: field("name"), harness: field("harness"), machine: field("machine"), task: field("task") }
}

/// Keep what a session is doing now (`SessionFacts::task`). A call that
/// names nothing changes nothing.
pub fn note_task(store: &Store, scope: &str, user: &str, session: &str, task: &str) {
    let task: String = task.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect();
    if session.is_empty() || task.is_empty() {
        return;
    }
    let key = facts_key(scope, user, session);
    let mut record = store.eph_get(&key).unwrap_or_else(|| json!({}));
    if record.get("task").and_then(Value::as_str) != Some(task.as_str()) {
        record["task"] = json!(task);
        let _ = store.eph_set(&key, &record, Some(TITLE_TTL_S));
    }
}

/// A session's work as a list labels it: the name Claude Code gives it (the
/// address a message `--to` takes) and what it is doing now,
/// `cork-linear-test-0a (on "add an issue kind field")`. Without a task, by
/// its title and name (`label`): a title Claude Code generated for the
/// session's first task outlives it, and labelled a session's edits
/// "Lattice 3D engine" while it added an issue `kind` field (v104's test).
pub fn work_label(title: &str, name: &str, task: &str) -> String {
    match (name.trim(), task.trim()) {
        (_, "") => label(title, name),
        ("", task) => format!("\"{task}\""),
        (name, task) => format!("{name} (on \"{task}\")"),
    }
}

/// `work_label` for a line that names the session among other words:
/// `cork-linear-test-0a, on "add an issue kind field"`.
pub fn quoted_work_label(title: &str, name: &str, task: &str) -> String {
    match (name.trim(), task.trim()) {
        (_, "") => quoted_label(title, name),
        ("", task) => format!("on \"{task}\""),
        (name, task) => format!("{name}, on \"{task}\""),
    }
}

/// A session as a list names it: its title and its Claude Code name,
/// `Cork version number (cork-linear-test-e3)`, so an agent that knows it
/// by either finds it; either alone when only one is known.
pub fn label(title: &str, name: &str) -> String {
    match (title.trim(), name.trim()) {
        ("", name) => name.to_string(),
        (title, "") => title.to_string(),
        (title, name) if title.eq_ignore_ascii_case(name) => title.to_string(),
        (title, name) => format!("{title} ({name})"),
    }
}

/// `label` with the title quoted, for a line that names the session among
/// other words: `"Cork version number", cork-linear-test-e3`.
pub fn quoted_label(title: &str, name: &str) -> String {
    match (title.trim(), name.trim()) {
        ("", name) => name.to_string(),
        (title, "") => format!("\"{title}\""),
        (title, name) if title.eq_ignore_ascii_case(name) => format!("\"{title}\""),
        (title, name) => format!("\"{title}\", {name}"),
    }
}

/// One session's `work_label`, from what its hooks said; "" when they said nothing.
pub fn session_label(store: &Store, scope: &str, user: &str, session: &str) -> String {
    let facts = session_facts(store, scope, user, session);
    work_label(&title_of(store, scope, user, session), &facts.name, &facts.task)
}

/// The sessions whose facts are kept for one identity: on this machine's
/// own server an agent is its own identity (`agent-3f2a91c0`), and this is
/// how its session is found from it.
pub fn sessions_of(store: &Store, scope: &str, user: &str) -> Vec<String> {
    let prefix = format!("sessfacts:{}:{user}:", crate::presence::split_scope(scope).0);
    store.eph_scan(&prefix).into_iter().filter_map(|(key, _)| key.strip_prefix(&prefix).map(str::to_string)).collect()
}

fn ide_key(scope: &str, user: &str, session: &str) -> String {
    format!("ide:{scope}:{user}:{session}")
}

/// The editor a session works in and its workspace (`vscode`, `Platform`),
/// as the hook names them: the dashboard's breadcrumb. A call that names
/// neither changes nothing.
pub fn note_ide(store: &Store, scope: &str, user: &str, session: &str, ide: &str, workspace: &str) {
    let ide: String = ide.trim().chars().take(40).collect();
    let workspace: String = workspace.trim().chars().take(120).collect();
    if session.is_empty() || (ide.is_empty() && workspace.is_empty()) {
        return;
    }
    let key = ide_key(scope, user, session);
    let record = json!({"ide": ide, "ide_workspace": workspace});
    if store.eph_get(&key).as_ref() != Some(&record) {
        let _ = store.eph_set(&key, &record, Some(TTL_S));
    }
}

/// The (editor, workspace) `note_ide` kept for a session, "" for each unknown.
pub fn ide_of(store: &Store, scope: &str, user: &str, session: &str) -> (String, String) {
    if session.is_empty() {
        return Default::default();
    }
    let record = store.eph_get(&ide_key(scope, user, session)).unwrap_or_default();
    let field = |k: &str| record.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    (field("ide"), field("ide_workspace"))
}

fn same_checkout(store: &Store, scope: &str, a: (&str, &str), b: (&str, &str)) -> bool {
    // a helper's own copy (presence::agent_session), else its session's
    let kept = |u: &str, s: &str| store.eph_get(&checkout_key(scope, u, s)).and_then(|v| v.as_str().map(str::to_string));
    let get = |(u, s): (&str, &str)| {
        let (lead, worker) = crate::presence::split_agent(s);
        kept(u, s).or_else(|| (!worker.is_empty()).then(|| kept(u, lead)).flatten())
    };
    matches!((get(a), get(b)), (Some(x), Some(y)) if x == y)
}

/// Whether two sessions work in one working copy (one folder on one
/// machine, as their hooks report it): what either has changed and not
/// pushed is already on the other's disk. A person's earlier session in the
/// same folder is not a teammate with unpushed work, and its changes are
/// never news "not pushed yet" to the next one.
pub fn same_copy(store: &Store, scope: &str, a: (&str, &str), b: (&str, &str)) -> bool {
    !a.1.is_empty() && !b.1.is_empty() && same_checkout(store, scope, a, b)
}

/// This session's copy of `path` holds `content` (it read it, or wrote it).
pub fn saw(store: &Store, scope: &str, user: &str, session: &str, path: &str, hash: &str, wrote: bool) {
    if session.is_empty() || hash.is_empty() {
        return;
    }
    let stamp = now();
    let _ = store.eph_set(&copy_key(scope, user, session, path),
        &json!({"h": hash, "ts": stamp, "wrote": wrote, "user": user, "session": session, "path": path}), Some(TTL_S));
    if wrote {
        let _ = store.eph_set(&write_key(scope, path), &json!({"h": hash, "ts": stamp, "user": user, "session": session, "path": path}), Some(TTL_S));
    }
}

/// A teammate's change to `path` that this session's copy does not have.
pub struct Behind {
    pub user: String,
    pub session: String,
    pub age_s: f64,
    /// their change is on origin (they landed after writing it): a pull gets it
    pub pushed: bool,
    pub written_at: f64,
    /// when this session last looked at the file
    pub copied_at: f64,
}

/// Whether this session's copy of `path` is behind a teammate's live write:
/// None when it has their change, when the latest write is its own, when it
/// never looked at the file, or when it wrote the file itself since.
pub fn behind(store: &Store, scope: &str, user: &str, session: &str, path: &str) -> Option<Behind> {
    if session.is_empty() {
        return None;
    }
    let write = store.eph_get(&write_key(scope, path))?;
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let (w_user, w_session) = (text(&write, "user"), text(&write, "session"));
    if w_user == user && w_session == session {
        return None;
    }
    // one working copy: their change is already on this disk
    if same_checkout(store, scope, (user, session), (&w_user, &w_session)) {
        return None;
    }
    let written_at = write.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let stamp = now();
    if stamp - written_at > LIVE_S {
        return None;
    }
    let copy = store.eph_get(&copy_key(scope, user, session, path))?;
    if copy.get("h") == write.get("h") {
        return None;
    }
    // its own edit since: the copies have diverged, and landing merges them
    let copied_at = copy.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    if copy.get("wrote").and_then(Value::as_bool).unwrap_or(false) && copied_at > written_at {
        return None;
    }
    // a helper's write lands with its session's landing
    let lander = crate::presence::split_agent(&w_session).0;
    let pushed = store
        .ledger_since_kinds(scope, written_at, &["landed"])
        .iter()
        .any(|row| text(&row.payload, "user") == w_user && text(&row.payload, "session") == lander);
    Some(Behind { user: w_user, session: w_session, age_s: stamp - written_at, pushed, written_at, copied_at })
}

/// What a session's pending write does to a file, as its hook worked it out
/// from the file before and after (hooks v75+): the top-level names it adds,
/// and the existing ones it changes, renames or removes. An older hook sends
/// none, and the gate then judges the file as a whole, as before.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WriteFacts {
    pub adds: std::collections::BTreeSet<String>,
    pub changes: std::collections::BTreeSet<String>,
}

impl WriteFacts {
    /// `{"adds": [...], "changes": [...]}` from the gate's request.
    pub fn from_json(value: Option<&Value>) -> Option<WriteFacts> {
        let value = value.filter(|v| v.is_object())?;
        let names = |key: &str| -> std::collections::BTreeSet<String> {
            value.get(key).and_then(Value::as_array).into_iter().flatten()
                .filter_map(Value::as_str).map(str::to_string).filter(|n| !n.is_empty()).take(200).collect()
        };
        Some(WriteFacts { adds: names("adds"), changes: names("changes") })
    }

    fn all(&self) -> std::collections::BTreeSet<String> {
        self.adds.union(&self.changes).cloned().collect()
    }
}

/// What a teammate's session did to `path` since `since`, from its reported
/// edits: the names it added, and the names it changed any other way (a body
/// or signature edit, a rename, a removal). `None` when there is no reported
/// edit to read it from, or one that names no symbol (a file no parser
/// knows): then nothing is known, and nothing is assumed.
pub fn their_changes(
    store: &Store, scope: &str, user: &str, session: &str, path: &str, since: f64,
) -> Option<(std::collections::BTreeSet<String>, std::collections::BTreeSet<String>)> {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let rows: Vec<Value> = store
        .ledger_since_kinds(scope, since - 1.0, &["edit_reported"])
        .into_iter()
        .map(|row| row.payload)
        .filter(|p| text(p, "user") == user && text(p, "session") == session && text(p, "path") == path)
        .collect();
    if rows.is_empty() {
        return None;
    }
    let (mut added, mut touched) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
    for row in &rows {
        let events = row.get("events").and_then(Value::as_array).cloned().unwrap_or_default();
        if events.is_empty() {
            // a first sight or an unparsed file says only which names moved
            let named: Vec<String> = row.get("symbols_changed").and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect()).unwrap_or_default();
            if named.is_empty() {
                return None;
            }
            touched.extend(named);
            continue;
        }
        for event in &events {
            let symbol = text(event, "symbol");
            match text(event, "kind").as_str() {
                "added" => {
                    added.insert(symbol);
                }
                // the other copy's change seen through this writer's diff: not its own
                "modified" if text(event, "confidence") == crate::semantics::UNCONFIRMED => {}
                _ => {
                    touched.insert(symbol);
                    let new_name = text(event, "new_name");
                    if !new_name.is_empty() {
                        touched.insert(new_name);
                    }
                }
            }
        }
    }
    // a name added and then edited again is still a name only they added
    let added: std::collections::BTreeSet<String> = added.into_iter().filter(|n| !n.is_empty()).collect();
    let touched: std::collections::BTreeSet<String> = touched.into_iter().filter(|n| !n.is_empty() && !added.contains(n)).collect();
    Some((added, touched))
}

/// What the gate says about a write to a file a teammate changed: stop it
/// once, or let it through with a line for the agent's context.
#[derive(Debug, Clone, PartialEq)]
pub enum Say {
    Block(String),
    Notice(String),
}

fn names_text(names: &std::collections::BTreeSet<String>) -> String {
    let shown: Vec<String> = names.iter().take(4).map(|n| format!("`{n}`")).collect();
    let more = if names.len() > 4 { format!(" and {} more", names.len() - 4) } else { String::new() };
    format!("{}{more}", shown.join(", "))
}

fn lands_key(scope: &str, user: &str, session: &str) -> String {
    format!("lands:{scope}:{user}:{session}")
}

/// Whether this session's pushes go through `cork land` (its hook says
/// so with each call): a teammate's push is then something landing rebases
/// onto, not something to pull now.
pub fn note_lands(store: &Store, scope: &str, user: &str, session: &str, lands: Option<bool>) {
    if session.is_empty() {
        return;
    }
    let key = lands_key(scope, user, session);
    match lands {
        Some(true) if store.eph_get(&key).is_none() => {
            let _ = store.eph_set(&key, &json!(true), Some(TTL_S));
        }
        Some(false) => store.eph_delete(&key),
        _ => {}
    }
}

pub fn lands(store: &Store, scope: &str, user: &str, session: &str) -> bool {
    // a helper's own word (presence::agent_session), else its session's
    let lead = crate::presence::split_agent(session).0;
    !session.is_empty()
        && (store.eph_get(&lands_key(scope, user, session)).is_some() || store.eph_get(&lands_key(scope, user, lead)).is_some())
}

/// Whether `user`'s `session` landed (pushed through Cork) after `since`:
/// its changes up to then are on origin. A landing that names no session
/// is the person's, and counts for each of their sessions.
pub fn landed_since(store: &Store, scope: &str, user: &str, session: &str, since: f64) -> bool {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    // a helper's work lands with its session's landing
    let session = crate::presence::split_agent(session).0;
    store.ledger_since_kinds(scope, since, &["landed"]).iter().any(|row| {
        let by = text(&row.payload, "session");
        text(&row.payload, "user") == user && (session.is_empty() || by.is_empty() || by == session)
    })
}

/// The gate's say on a write to `path` whose copy is behind a teammate's
/// write, said once per teammate write:
///
/// - their change and this write touch different names (`write`, from the
///   hook): nothing to stop. Two agents adding their own tests to one file
///   is the common case (Study 8: 589k of the 802k tokens agents spent stalled
///   at the gate at 16 agents); git merges it, and landing rebases onto it.
///   A line says who else is in the file.
/// - the same names, or what either side does is unknown: stopped once, and
///   told to pull only when their change is pushed: a pull can bring in only
///   what is on origin.
///
/// A retry goes through, so nobody is ever stuck on it.
#[allow(clippy::too_many_arguments)]
pub fn gate_note(
    store: &Store, scope: &str, user: &str, session: &str, path: &str, who: &str, write: Option<&WriteFacts>, lands: bool,
) -> Option<Say> {
    let behind = behind(store, scope, user, session, path)?;
    // it has looked at the file since their write: that read was told whose
    // change its copy lacks, and a copy read since may well hold theirs plus
    // its own (the block is for an edit made on a copy read before it)
    if behind.copied_at > behind.written_at {
        return None;
    }
    // once per (session, file, teammate session): a retry, or the same teammate's next
    // write, goes through (Study 8, second study: one agent was held 15 times)
    let told = format!("behindtold:{scope}:{user}:{session}:{}:{}#{}", path_key(path), behind.user, behind.session);
    if store.eph_get(&told).is_some() {
        return None;
    }
    let _ = store.eph_set(&told, &json!({"ts": now()}), Some(TTL_S));
    let age = crate::deltas::age_text(behind.age_s);
    let theirs = their_changes(store, scope, &behind.user, &behind.session, path, behind.copied_at);
    let their_all: Option<std::collections::BTreeSet<String>> = theirs.as_ref().map(|(a, t)| a.union(t).cloned().collect());
    // both sides known, and no name in common: let it through
    if let (Some(mine), Some((_, touched)), Some(their_all)) = (write, &theirs, &their_all) {
        let mine_all = mine.all();
        if !mine_all.is_empty() && mine_all.is_disjoint(their_all) {
            let names = names_text(their_all);
            let additive = mine.changes.is_empty() && touched.is_empty();
            return Some(Say::Notice(if behind.pushed {
                format!(
                    "Cork: {who} pushed a change to {path} ({names}, {age}); your write touches other code. {}",
                    if lands { "Landing will rebase onto it when you push; carry on." } else { "Pull (git pull --rebase) before you push; carry on." }
                )
            } else if additive {
                format!("Cork: {who} is also adding to {path} ({names}, {age}, not pushed yet); landing merges both. Carry on.")
            } else {
                format!(
                    "Cork: {who} is changing {names} in {path} in their own checkout ({age}, not pushed yet); your write \
touches other code, and landing merges both. Carry on."
                )
            }));
        }
    }
    // the names both sides touch are ones the teammate is CREATING, not
    // pushed: no hold, its interface (hooks v78; v76 held, and an agent
    // deleted its copy of the new module and rebuilt it)
    if let (Some(mine), Some((added, _)), Some(their_all), false) = (write, &theirs, &their_all, behind.pushed) {
        let both: std::collections::BTreeSet<String> = mine.all().intersection(their_all).cloned().collect();
        if !both.is_empty() && both.is_subset(added) {
            let symbols = crate::codegraph::symbols_of(store, scope, path);
            let names: Vec<String> = added.iter().cloned().collect();
            let iface: Vec<String> = names
                .iter()
                .take(8)
                .map(|n| symbols.get(n).and_then(|s| s.get("signature")).and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(n).to_string())
                .collect();
            let new_file = their_all == added;
            return Some(Say::Notice(crate::inflight::build_against_line(who, path, &names, new_file, &iface.join("; "))));
        }
    }
    // the same names: which ones both sides change
    let overlap = match (write, &their_all) {
        (Some(mine), Some(their_all)) => {
            let both: std::collections::BTreeSet<String> = mine.all().intersection(their_all).cloned().collect();
            if both.is_empty() { String::new() } else { format!(" (both of you change {})", names_text(&both)) }
        }
        _ => String::new(),
    };
    Some(Say::Block(if behind.pushed && lands {
        format!(
            "{who} changed {path} {age} and pushed it{overlap}; your copy does not have it yet. Make your edit fit theirs \
             (landing will rebase onto their change when you push), then retry the write and it goes through."
        )
    } else if behind.pushed {
        format!(
            "{who} changed {path} {age} and pushed it{overlap}; your copy does not have it yet. Pull first (git pull --rebase), \
             re-read {path}, then make your edit on top of theirs — editing the old copy is a merge conflict waiting to happen."
        )
    } else {
        format!(
            "{who} is changing {path} in their own checkout ({age}){overlap}; it is not pushed, so your copy does not have it. \
             Do not redo their change yourself: make your edit fit theirs, then retry the write and it goes through. \
             When you push, Cork rebases onto their change once it lands."
        )
    }))
}

/// A teammate's work just landed. A session whose copy of a file in it is
/// behind hears so on its next step, unless it lands its own pushes: landing
/// rebases onto the teammate's commits, so "pull now" only made agents stop
/// and pull mid-task (Study 8: the largest reaction cost at 16 agents). A
/// session that lands hears a short line only when it wrote one of those
/// files itself; the gate still stops a write that changes the same code.
/// A session that does not land is told to pull: the last thing it heard was
/// "not pushed yet", and an agent redid the teammate's rename by hand in its
/// own copy before the gate sent it to pull.
pub fn announce_landing(store: &Store, scope: &str, user: &str, session: &str) -> usize {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let written: Vec<(String, String)> = store
        .eph_scan(&format!("writehash:{scope}:"))
        .into_iter()
        // the session's writes and its helpers' (presence::agent_session)
        .filter(|(_, w)| text(w, "user") == user && crate::presence::split_agent(&text(w, "session")).0 == session)
        .map(|(_, w)| (text(&w, "path"), text(&w, "h")))
        .filter(|(p, _)| !p.is_empty())
        .collect();
    if written.is_empty() {
        return 0;
    }
    // reader session -> (reader user, files it is behind on)
    let mut behind_by: std::collections::BTreeMap<String, (String, Vec<String>)> = std::collections::BTreeMap::new();
    // a landing on this branch does not put a checkout on another branch
    // behind: the two meet only when the branches merge (branches.rs)
    let my_branch = crate::branches::of(store, scope, user, session);
    for (_, copy) in store.eph_scan(&format!("copyhash:{scope}:")) {
        let (r_user, r_session, r_path) = (text(&copy, "user"), text(&copy, "session"), text(&copy, "path"));
        if (r_user == user && r_session == session) || r_session.is_empty() {
            continue;
        }
        if crate::branches::apart(store, scope, &my_branch, (&r_user, &r_session)).is_some() {
            continue;
        }
        let Some((_, h)) = written.iter().find(|(p, _)| *p == r_path) else { continue };
        if text(&copy, "h") == *h || same_checkout(store, scope, (&r_user, &r_session), (user, session)) {
            continue;
        }
        let entry = behind_by.entry(r_session).or_insert_with(|| (r_user.clone(), Vec::new()));
        if !entry.1.contains(&r_path) {
            entry.1.push(r_path);
        }
    }
    let mut told = 0;
    let stamp = now();
    for (r_session, (r_user, paths)) in behind_by {
        let sibling = if r_user == user { crate::presence::sibling_label(store, scope, user, session, &r_session) } else { String::new() };
        let who = if !sibling.is_empty() {
            sibling
        } else if r_user == user {
            "Another session of yours".to_string()
        } else {
            crate::inflight::author_label(store, &r_user, user)
        };
        let line = if lands(store, scope, &r_user, &r_session) {
            // only the files it wrote itself, and of a teammate's test file
            // only one where both changed the same tests: adding tests to a
            // shared file is not news, and a reader's copy is landing's to
            // update (Study 8, second rerun: a landing notice preceded 18
            // manual pulls)
            let wrote = crate::deltas::written_set(store, scope, &r_session);
            let mine = crate::deltas::own_names(store, scope, &r_user, &r_session, stamp - TTL_S);
            let shared: Vec<String> = paths
                .iter()
                .filter(|p| wrote.contains(*p))
                .filter(|p| {
                    if !crate::graphview::is_test_path(p) {
                        return true;
                    }
                    let theirs = their_changes(store, scope, user, session, p, stamp - TTL_S)
                        .map(|(a, t)| a.union(&t).cloned().collect::<std::collections::BTreeSet<String>>())
                        .unwrap_or_default();
                    mine.get(*p).is_some_and(|names| !names.is_disjoint(&theirs))
                })
                .cloned()
                .collect();
            if shared.is_empty() {
                continue;
            }
            format!(
                "Cork: {who} landed a change to {}, which you changed too. Your next land rebases onto it; carry on \
(the gate says so if your next edit changes the same code).",
                shared.join(", ")
            )
        } else {
            format!(
                "Cork: {who} just pushed their change to {}; your copy is behind it. Before your next edit to \
those files, pull (git pull --rebase) and re-read them; until then, carry on. Do not redo their change yourself: it is on origin.",
                paths.join(", ")
            )
        };
        let files = paths.join(", ");
        crate::inflight::queue_notice(store, scope, &r_session, &format!("landed|{user}|{session}|{files}"), &line, user, session, &paths);
        told += 1;
    }
    told
}

/// A session ended: it is no longer working anywhere, however recently it
/// read or wrote (the overlap notice told people to split work with an
/// agent that had already gone).
pub fn note_ended(store: &Store, scope: &str, session: &str) {
    if !session.is_empty() {
        let _ = store.eph_set(&format!("sessended:{scope}:{session}"), &json!({"ts": now()}), Some(TTL_S));
    }
}

/// Other live sessions that have read or written any of `paths` in the last
/// `within_s`: (user, session, the files among `paths` they are in).
pub fn working_in(store: &Store, scope: &str, user: &str, session: &str, paths: &[String], within_s: f64) -> Vec<(String, String, Vec<String>)> {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let cutoff = now() - within_s;
    let mut by: std::collections::BTreeMap<(String, String), Vec<String>> = std::collections::BTreeMap::new();
    for (_, copy) in store.eph_scan(&format!("copyhash:{scope}:")) {
        let (u, s, p) = (text(&copy, "user"), text(&copy, "session"), text(&copy, "path"));
        if s.is_empty() || (u == user && s == session) || !paths.contains(&p) {
            continue;
        }
        if copy.get("ts").and_then(Value::as_f64).unwrap_or(0.0) < cutoff
            || store.eph_get(&format!("sessended:{scope}:{s}")).is_some()
        {
            continue;
        }
        let files = by.entry((u, s)).or_default();
        if !files.contains(&p) {
            files.push(p);
        }
    }
    by.into_iter().map(|((u, s), f)| (u, s, f)).collect()
}

/// Who wrote `path` last, and when: (user, session, ts). Read before a
/// write records itself, it is the write this one may have landed on
/// (clash.rs).
pub fn latest_writer(store: &Store, scope: &str, path: &str) -> Option<(String, String, f64)> {
    let w = store.eph_get(&write_key(scope, path))?;
    let text = |k: &str| w.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Some((text("user"), text("session"), w.get("ts").and_then(Value::as_f64)?))
}

/// The latest recorded write of `path`: (when, fingerprint).
pub fn latest_write(store: &Store, scope: &str, path: &str) -> Option<(f64, String)> {
    let w = store.eph_get(&write_key(scope, path))?;
    Some((w.get("ts").and_then(Value::as_f64)?, w.get("h").and_then(Value::as_str).unwrap_or("").to_string()))
}

/// For the read note: is the teammate's recent write in what this session
/// just read, and is it pushed?
pub fn read_view(store: &Store, scope: &str, user: &str, session: &str, path: &str) -> Value {
    match behind(store, scope, user, session, path) {
        None => json!({"in_copy": true}),
        Some(b) => json!({"in_copy": false, "pushed": b.pushed}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_behind_a_teammates_write_is_told_once_and_pushed_or_not() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let (scope, p) = ("w:r", "shop/tiles.py");
        let (v1, v2) = (fingerprint_content("def tiles():\n    return settled_total()\n"), fingerprint_content("def tiles():\n    return paid_total()\n"));
        note_checkout(&db, scope, "alice", "a1", "clone-a");
        note_checkout(&db, scope, "bob", "b1", "clone-b");
        saw(&db, scope, "alice", "a1", p, &v1, false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        saw(&db, scope, "bob", "b1", p, &v2, true);
        // bob's change is only in his checkout
        let b = behind(&db, scope, "alice", "a1", p).expect("behind");
        assert!(!b.pushed);
        let Some(Say::Block(note)) = gate_note(&db, scope, "alice", "a1", p, "Bob", None, false) else { panic!("told") };
        assert!(note.contains("not pushed"), "{note}");
        assert!(gate_note(&db, scope, "alice", "a1", p, "Bob", None, false).is_none(), "a retry goes through");
        // he lands: a pull gets it
        db.ledger_append(scope, "landed", &json!({"user": "bob", "session": "b1", "commit": "c1"}), now()).unwrap();
        assert!(behind(&db, scope, "alice", "a1", p).unwrap().pushed);
        assert_eq!(read_view(&db, scope, "alice", "a1", p)["pushed"], true);
        // the landing itself tells her, once, on her next step
        assert_eq!(announce_landing(&db, scope, "bob", "b1"), 1);
        let notices = crate::inflight::take_notices(&db, scope, "a1");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("just pushed their change to shop/tiles.py") && notices[0].contains("Before your next edit"), "{}", notices[0]);
        assert!(crate::inflight::take_notices(&db, scope, "a1").is_empty());
        // she pulls and re-reads: current
        saw(&db, scope, "alice", "a1", p, &v2, false);
        assert!(behind(&db, scope, "alice", "a1", p).is_none());
        assert_eq!(read_view(&db, scope, "alice", "a1", p)["in_copy"], true);
        // the writer is never behind its own write; another session in the
        // SAME checkout has the change on disk
        assert!(behind(&db, scope, "bob", "b1", p).is_none());
        note_checkout(&db, scope, "bob", "b2", "clone-b");
        saw(&db, scope, "bob", "b2", p, &v1, false);
        assert!(behind(&db, scope, "bob", "b2", p).is_none(), "same checkout");
        // a session that never looked at the file is not held
        assert!(behind(&db, scope, "carol", "c1", p).is_none());
    }

    fn edit_row(db: &Store, scope: &str, user: &str, session: &str, path: &str, events: Value) {
        db.ledger_append(scope, "edit_reported", &json!({"user": user, "session": session, "path": path, "events": events}), now()).unwrap();
    }

    fn adds(names: &[&str]) -> WriteFacts {
        WriteFacts { adds: names.iter().map(|n| n.to_string()).collect(), changes: Default::default() }
    }

    /// Study 8: sixteen agents each adding a test to tests/test_features.py
    /// were stopped at the gate one after another. Two sessions that only add
    /// new names pass, with a line; a write that changes a name the teammate
    /// changed is still stopped; an older hook that says nothing about its
    /// write is judged as before.
    #[test]
    fn two_sessions_adding_their_own_tests_to_one_file_both_pass() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let (scope, p) = ("w:r", "tests/test_features.py");
        let (v1, v2) = (fingerprint_content("def test_a():\n    pass\n"), fingerprint_content("def test_a():\n    pass\n\ndef test_b():\n    pass\n"));
        note_checkout(&db, scope, "u", "a1", "clone-a");
        note_checkout(&db, scope, "u", "b1", "clone-b");
        saw(&db, scope, "u", "a1", p, &v1, false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        // Bob adds test_b and it is reported
        saw(&db, scope, "u", "b1", p, &v2, true);
        edit_row(&db, scope, "u", "b1", p, json!([{"symbol": "test_b", "kind": "added", "confidence": "certain"}]));
        // Alice adds test_c: both only add, no name in common
        let said = gate_note(&db, scope, "u", "a1", p, "Another session of yours", Some(&adds(&["test_c"])), true);
        let Some(Say::Notice(line)) = said else { panic!("{said:?}") };
        assert!(line.contains("is also adding to tests/test_features.py (`test_b`") && line.contains("landing merges both"), "{line}");
        assert!(!line.contains("pull"), "{line}");

        // a write that changes the very test Bob is changing is stopped once
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        note_checkout(&db, scope, "u", "a1", "clone-a");
        note_checkout(&db, scope, "u", "b1", "clone-b");
        saw(&db, scope, "u", "a1", p, &v1, false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        saw(&db, scope, "u", "b1", p, &v2, true);
        edit_row(&db, scope, "u", "b1", p, json!([{"symbol": "test_a", "kind": "modified", "confidence": "certain"}]));
        let mine = WriteFacts { adds: Default::default(), changes: ["test_a".to_string()].into() };
        let said = gate_note(&db, scope, "u", "a1", p, "Bob", Some(&mine), true);
        let Some(Say::Block(reason)) = said else { panic!("{said:?}") };
        assert!(reason.contains("both of you change `test_a`") && reason.contains("not pushed"), "{reason}");

        // an older hook says nothing of its write: the file is judged whole
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        note_checkout(&db, scope, "u", "a1", "clone-a");
        note_checkout(&db, scope, "u", "b1", "clone-b");
        saw(&db, scope, "u", "a1", p, &v1, false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        saw(&db, scope, "u", "b1", p, &v2, true);
        edit_row(&db, scope, "u", "b1", p, json!([{"symbol": "test_b", "kind": "added", "confidence": "certain"}]));
        assert!(matches!(gate_note(&db, scope, "u", "a1", p, "Bob", None, true), Some(Say::Block(_))));
    }

    /// Scaling study 2, v76: b6 was writing a new credits module, b7 wrote
    /// against `balance_for` in it, was told to "wait for them to land it",
    /// deleted its copy and rebuilt it (25 calls). A write to names a
    /// teammate is creating, not pushed, gets the module's interface and no
    /// hold.
    #[test]
    fn a_teammate_creating_a_module_gets_its_interface_never_a_hold() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let (scope, p) = ("w:r", "lanternly/billing/credits.py");
        let theirs = "def balance_for(member_id: int) -> int:\n    return 0\n";
        note_checkout(&db, scope, "u", "b7", "clone-b7");
        note_checkout(&db, scope, "u", "b6", "clone-b6");
        saw(&db, scope, "u", "b7", p, &fingerprint_content("def unused_credits(member_id: int) -> list:\n    return []\n"), false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        // b6 creates the module, and it is reported
        saw(&db, scope, "u", "b6", p, &fingerprint_content(theirs), true);
        crate::report::observe_batch(&db, scope, &[(p, theirs, None)], 0.0);
        edit_row(&db, scope, "u", "b6", p, json!([{"symbol": "balance_for", "kind": "added", "confidence": "certain"}]));
        // b7, about to use it, writes balance_for too
        let said = gate_note(&db, scope, "u", "b7", p, "Another session of yours", Some(&adds(&["balance_for"])), true);
        let Some(Say::Notice(line)) = said else { panic!("no hold, a notice: {said:?}") };
        assert!(line.contains("Build against its interface"), "{line}");
        assert!(line.contains("def balance_for(member_id: int) -> int"), "{line}");
        assert!(!line.to_lowercase().contains("wait") && !line.contains("delete") && !line.contains("rm "), "{line}");
    }

    /// A pushed change to a file this session also writes: with landing on,
    /// a write elsewhere in the file carries on, and the landing itself says
    /// "landing will rebase", never "pull now"; to a reader that only read
    /// the file it says nothing at all.
    #[test]
    fn with_landing_on_a_teammates_push_is_never_a_pull_now() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let (scope, p) = ("w:r", "shop/tiles.py");
        let (v1, v2) = (fingerprint_content("def tiles():\n    return 1\n"), fingerprint_content("def tiles():\n    return 2\n"));
        for (s, c) in [("a1", "clone-a"), ("b1", "clone-b"), ("c1", "clone-c")] {
            note_checkout(&db, scope, "u", s, c);
            note_lands(&db, scope, "u", s, Some(true));
        }
        saw(&db, scope, "u", "a1", p, &v1, true); // Alice wrote the file
        crate::deltas::touch_as(&db, scope, "a1", &[p.to_string()], true, now());
        saw(&db, scope, "u", "c1", p, &v1, false); // Carol only read it
        std::thread::sleep(std::time::Duration::from_millis(5));
        saw(&db, scope, "u", "b1", p, &v2, true);
        edit_row(&db, scope, "u", "b1", p, json!([{"symbol": "tiles", "kind": "modified", "confidence": "certain"}]));
        db.ledger_append(scope, "landed", &json!({"user": "u", "session": "b1"}), now()).unwrap();
        assert_eq!(announce_landing(&db, scope, "u", "b1"), 1, "Alice only");
        let alice = crate::inflight::take_notices(&db, scope, "a1");
        assert!(alice[0].contains("Your next land rebases onto it") && !alice[0].to_lowercase().contains("pull"), "{alice:?}");
        assert!(crate::inflight::take_notices(&db, scope, "c1").is_empty(), "a reader hears nothing");
        // Alice's next write adds a new function: it goes through with a line
        let said = gate_note(&db, scope, "u", "a1", p, "Bob", Some(&adds(&["badge"])), true);
        let Some(Say::Notice(line)) = said else { panic!("{said:?}") };
        assert!(line.contains("pushed a change to shop/tiles.py") && line.contains("Landing will rebase"), "{line}");
        // without landing the reader is still told to pull
        let db2 = Store::open(std::path::Path::new(":memory:")).unwrap();
        note_checkout(&db2, scope, "u", "a1", "clone-a");
        note_checkout(&db2, scope, "u", "b1", "clone-b");
        saw(&db2, scope, "u", "a1", p, &v1, false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        saw(&db2, scope, "u", "b1", p, &v2, true);
        db2.ledger_append(scope, "landed", &json!({"user": "u", "session": "b1"}), now()).unwrap();
        assert_eq!(announce_landing(&db2, scope, "u", "b1"), 1);
        assert!(crate::inflight::take_notices(&db2, scope, "a1")[0].contains("pull (git pull --rebase)"));
    }

    /// A landing on one branch does not put a checkout on another branch
    /// behind: the reader on a feature branch hears nothing (its work meets
    /// the landing only when the branches merge); one on the same branch, or
    /// whose branch is unknown, is told as before.
    #[test]
    fn a_landing_on_another_branch_is_not_announced_as_behind() {
        let (scope, p) = ("w:r", "shop/tiles.py");
        let (v1, v2) = (fingerprint_content("def tiles():\n    return 1\n"), fingerprint_content("def tiles():\n    return 2\n"));
        for (reader_branch, told) in [("feature/badges", 0), ("main", 1), ("", 1)] {
            let db = Store::open(std::path::Path::new(":memory:")).unwrap();
            note_checkout(&db, scope, "u", "a1", "clone-a");
            note_checkout(&db, scope, "u", "b1", "clone-b");
            crate::branches::note(&db, scope, "u", "b1", "main", "");
            crate::branches::note(&db, scope, "u", "a1", reader_branch, "");
            saw(&db, scope, "u", "a1", p, &v1, false);
            std::thread::sleep(std::time::Duration::from_millis(5));
            saw(&db, scope, "u", "b1", p, &v2, true);
            db.ledger_append(scope, "landed", &json!({"user": "u", "session": "b1"}), now()).unwrap();
            assert_eq!(announce_landing(&db, scope, "u", "b1"), told, "reader on {reader_branch:?}");
            assert_eq!(crate::inflight::take_notices(&db, scope, "a1").len(), told);
        }
    }

    #[test]
    fn the_editor_and_its_workspace_are_kept_until_a_call_names_one() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        assert_eq!(ide_of(&db, scope, "alice", "a1"), (String::new(), String::new()));
        note_ide(&db, scope, "alice", "a1", "vscode", "Platform");
        assert_eq!(ide_of(&db, scope, "alice", "a1"), ("vscode".into(), "Platform".into()));
        // a call from an older hook (or a plain terminal) names neither: kept
        note_ide(&db, scope, "alice", "a1", "", "");
        assert_eq!(ide_of(&db, scope, "alice", "a1"), ("vscode".into(), "Platform".into()));
        note_ide(&db, scope, "alice", "a1", "cursor", "");
        assert_eq!(ide_of(&db, scope, "alice", "a1"), ("cursor".into(), String::new()));
        assert_eq!(ide_of(&db, scope, "alice", ""), (String::new(), String::new()));
    }
}
