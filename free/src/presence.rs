//! Presence: where an AGENT is looking.
//!
//! The read-side hook fires on every Read, Grep and Bash, so this has to be
//! featherweight — ephemeral markers only, no ledger row, no tree change,
//! gone in minutes. What it buys is that teammates can see where someone is
//! working before they write, which is the whole point of reporting reads at
//! all.
//!
//! Two markers, with deliberately different lifetimes. `focus:` is short (3
//! minutes) because it drives the live "reading now" line and the online dot,
//! and a stale one would lie. `lastact:` lasts a week, so someone who only
//! read files an hour ago is still listed as idle with what they last
//! touched, instead of vanishing when the focus marker expires.
//!
//! The unit is the agent, not the person: `{user}#{session}`, the identity
//! the hot markers already chose (see `report::report_edit`) and the gate
//! already honours. Four agents on one credential were four agents whose
//! markers overwrote each other; now each has its own. And the unit of the
//! ROOM is the workspace, not the repo — presence keys are
//! `focus:{workspace}:{agent_id}` with the repo carried in the record — so
//! the roster of any repo can show everyone in the workspace, filtered
//! through the member's per-repo access at read time.

use serde_json::{json, Map, Value};

use crate::store::{now, Store};

pub const FOCUS_TTL_S: f64 = 180.0;
const LAST_ACTION_TTL_S: f64 = 7.0 * 86400.0;
const AGENT_TOKENS_TTL_S: f64 = 7.0 * 86400.0;
/// A paused agent stays paused on the Now panel for up to six hours, or
/// until it acts again — the harness's reset times are hours away, and a
/// 3-minute marker would flip a limited agent back to "idle" while its
/// quota is still gone.
const LIMIT_TTL_S: f64 = 6.0 * 3600.0;
const MAX_PATH: usize = 300;
const MAX_NOTE: usize = 200;
/// A closing message keeps its first two sentences (`first_sentences`), up
/// to this many characters for a run-on one.
pub const MAX_DONE: usize = 600;
/// The sentences of a closing message the Now line keeps.
const DONE_SENTENCES: usize = 2;

pub(crate) fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The longest effort name kept: the harnesses name it in a word.
const MAX_EFFORT: usize = 24;

/// The reasoning effort a turn ran at, as the harness names it ("low",
/// "medium", "high", "xhigh", "max"), and whether it ran in Claude Code's
/// ultracode mode. It rides beside the model wherever the model is kept, so
/// an opus at medium and the same opus at max are told apart on the roster
/// and in the costs.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Effort {
    /// "" when the harness did not say
    pub effort: String,
    pub ultracode: bool,
}

impl Effort {
    /// Off a hook post, a usage entry or a stored record: the name trimmed,
    /// lowercased and capped, and ultracode only when it is a true.
    pub fn of(value: &Value) -> Self {
        let named = value.get("effort").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
        Effort {
            effort: clip(&named, MAX_EFFORT).trim_end().to_string(),
            ultracode: value.get("ultracode").and_then(Value::as_bool) == Some(true),
        }
    }

    /// Onto a record beside its model. What is unknown is left off, and
    /// taken off a record that had it: an effort stands for the model it
    /// was sent with, never for the next one.
    pub fn tag(&self, map: &mut Map<String, Value>) {
        if self.effort.is_empty() {
            map.remove("effort");
        } else {
            map.insert("effort".into(), json!(self.effort));
        }
        if self.ultracode {
            map.insert("ultracode".into(), json!(true));
        } else {
            map.remove("ultracode");
        }
    }

    /// The same, onto a JSON object.
    pub fn tag_value(&self, value: &mut Value) {
        if let Some(map) = value.as_object_mut() {
            self.tag(map);
        }
    }
}

/// `{user}#{session}`, or the bare user when no session was reported —
/// exactly the identity the hot markers use, so an MCP-only agent that never
/// names a session is one agent under the person's own name rather than a
/// row that can never be matched.
/// An agent field as a notice may name it: the harness or model (`claude`,
/// `codex`), or "" when the field holds only the reporting program
/// (`cork-report-hook/89`, `collide-report-hook/75` from a hook released
/// before the rename, `collide-hook land`, `collide-autofix`). Agents read
/// "another session of yours (collide-report-hook)" in Study 8: the hook's
/// own user agent standing where a teammate's name belongs.
pub fn agent_name(agent: &str) -> String {
    let agent = agent.trim();
    let lowered = agent.to_ascii_lowercase();
    if lowered.starts_with("cork") || lowered.starts_with("collide") || lowered.contains("hook") || lowered.contains('/') {
        return String::new();
    }
    agent.to_string()
}

pub fn agent_id(user: &str, session: &str) -> String {
    if session.is_empty() { user.to_string() } else { format!("{user}#{session}") }
}

/// The agent a hook call speaks for, as the state kept per working copy
/// names it: the session (its lead), or `session/worker` for one of its
/// helpers. A helper's hooks carry its session's id and its own `worker`;
/// keyed by the session alone, two helpers of one session were one agent
/// to each other: neither heard the other's change, and either could
/// overwrite the other's lines unseen. Ledger rows keep the session and
/// `worker` apart; this is for copies, branches, working sets and what each
/// agent was told.
pub fn agent_session(session: &str, worker: &str) -> String {
    if session.is_empty() || worker.is_empty() { session.to_string() } else { format!("{session}/{worker}") }
}

/// `(session, worker)` of an [`agent_session`]; worker "" for a lead.
pub fn split_agent(agent: &str) -> (&str, &str) {
    agent.split_once('/').unwrap_or((agent, ""))
}

/// How one agent of a session is named to another of the same session: a
/// helper as its lead's helper (`cork-cloud-19's helper repo-names`), the
/// lead by its Claude Code name (`cork-cloud-19 (your lead)`). "" when the
/// two are not of one session.
pub fn sibling_label(store: &Store, scope: &str, user: &str, author: &str, reader: &str) -> String {
    let (session, worker) = split_agent(author);
    if session.is_empty() || session != split_agent(reader).0 || author == reader {
        return String::new();
    }
    let lead = crate::freshness::session_facts(store, scope, user, session).name;
    if worker.is_empty() {
        return if lead.is_empty() { "your lead".to_string() } else { format!("{lead} (your lead)") };
    }
    let name = [helper_key(scope, user, session, worker), worker_key(scope, user, session, worker)]
        .iter()
        .filter_map(|key| store.eph_get(key))
        .map(|record| text(&record, "name"))
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| worker.chars().take(24).collect());
    if lead.is_empty() { format!("your session's helper {name}") } else { format!("{lead}'s helper {name}") }
}

/// `("<workspace>", "<repo>")`. The repo half may itself carry colons, so the
/// split is on the FIRST one only, everywhere a scope is taken apart.
pub fn split_scope(scope: &str) -> (&str, &str) {
    scope.split_once(':').unwrap_or((scope, ""))
}

pub fn focus_key(scope: &str, user: &str, session: &str) -> String {
    format!("focus:{}:{}", split_scope(scope).0, agent_id(user, session))
}

pub fn last_action_key(scope: &str, user: &str, session: &str) -> String {
    format!("lastact:{}:{}", split_scope(scope).0, agent_id(user, session))
}

/// The step a session took last in ONE repo, kept when it moves on to
/// another: `lastin:{ws}:{repo}:{agent}`. The marker above is per workspace
/// and the history is the newest [`HISTORY_LEN`] steps, so a session that
/// wrote into a sibling checkout and then worked on in its own repo pushed
/// that step out of reach within a few minutes of shell commands, and the
/// sibling repo's page lost it (activity.rs reads this instead).
pub fn last_in_key(scope: &str, user: &str, session: &str) -> String {
    format!("lastin:{scope}:{}", agent_id(user, session))
}

/// How long a repo keeps a session's last step there once it has moved on.
const LAST_IN_TTL_S: f64 = 86400.0;

/// A subagent working inside a session: `worker:{ws}:{agent_id}:{worker}`,
/// alive for as long as a focus marker. Counted per session by the roster.
pub const WORKER_TTL_S: f64 = 180.0;

pub fn worker_key(scope: &str, user: &str, session: &str, worker: &str) -> String {
    format!("worker:{}:{}:{}", split_scope(scope).0, agent_id(user, session), worker)
}

/// A hook post named the subagent it came from: refresh that worker's marker,
/// with the name it was started with when the hook knows it (hooks v72+).
/// Any post from the worker is a completion: a command it had started has
/// ended, so its marker goes back to the short lifetime.
pub fn note_worker(store: &Store, scope: &str, user: &str, session: &str, worker: &str, worker_type: &str, name: &str) {
    note_worker_activity(store, scope, user, session, worker, worker_type, name, "", "", false);
}

/// The worker's marker, refreshed with what it is doing now. `action` empty
/// keeps the one it had. A marker for a command that has `started` stands
/// for the command's length (up to `STARTED_TTL_S`), as the session's own
/// focus does: a long test suite or wait loop sends nothing until it ends,
/// and the worker is live all that time. The type and name a later post
/// leaves out are kept from the marker.
#[allow(clippy::too_many_arguments)]
pub fn note_worker_activity(
    store: &Store, scope: &str, user: &str, session: &str, worker: &str, worker_type: &str, name: &str,
    action: &str, path: &str, started: bool,
) {
    if worker.is_empty() || user.is_empty() {
        return;
    }
    let key = worker_key(scope, user, session, worker);
    // a marker that ran out while the helper thought keeps its name on the
    // helper's own record
    let listed = store.eph_get(&helper_key(scope, user, session, worker));
    let existing = store.eph_get(&key).or_else(|| listed.clone()).unwrap_or(Value::Null);
    let mut record = identity(scope, user, session);
    record.insert("worker".into(), json!(worker));
    let kind = if worker_type.is_empty() { text(&existing, "type") } else { worker_type.to_string() };
    record.insert("type".into(), json!(kind));
    let named = if name.is_empty() { text(&existing, "name") } else { clip(name, MAX_WORKER_NAME) };
    if !named.is_empty() {
        record.insert("name".into(), json!(named));
    }
    let (action, path) = if action.is_empty() {
        (text(&existing, "action"), text(&existing, "path"))
    } else {
        (action.to_string(), clip(path, MAX_PATH))
    };
    if !action.is_empty() {
        record.insert("action".into(), json!(action));
        record.insert("path".into(), json!(path));
    }
    if started {
        record.insert("started".into(), json!(true));
    }
    let stamp = now();
    record.insert("ts".into(), json!(stamp));
    let ttl = if started { STARTED_TTL_S } else { WORKER_TTL_S };
    // on its session's list of helpers: written when it is new or renamed,
    // renewed every few minutes, not on every step
    let fresh = listed.as_ref().is_some_and(|l| {
        text(l, "type") == kind && text(l, "name") == named && stamp - l.get("ts").and_then(Value::as_f64).unwrap_or(0.0) < HELPER_RENEW_S
    });
    if !fresh {
        let mut helper = identity(scope, user, session);
        helper.insert("worker".into(), json!(worker));
        helper.insert("type".into(), json!(kind));
        helper.insert("name".into(), json!(named));
        helper.insert("since".into(), listed.as_ref().and_then(|l| l.get("since").cloned()).unwrap_or_else(|| json!(stamp)));
        helper.insert("ts".into(), json!(stamp));
        let _ = store.eph_set(&helper_key(scope, user, session, worker), &Value::Object(helper), Some(HELPER_TTL_S));
    }
    let _ = store.eph_set(&key, &Value::Object(record), Some(ttl));
}

/// A helper on its session's list of the agents a message can reach:
/// `helper:{ws}:{agent_id}:{worker}`, from its first hook call until its
/// SubagentStop or its session's end. The worker marker is the roster's and
/// goes three minutes after the helper's last step: an in-process teammate
/// that thought for minutes between tool calls fell off it, and `cork
/// message --to` its name answered "no agent called ..." (v99).
pub const HELPER_TTL_S: f64 = 12.0 * 3600.0;
const HELPER_RENEW_S: f64 = 300.0;

pub fn helper_key(scope: &str, user: &str, session: &str, worker: &str) -> String {
    format!("helper:{}:{}:{}", split_scope(scope).0, agent_id(user, session), worker)
}

/// The helpers of one session in this repo that have not ended:
/// `{worker, type, name, since, ts}` each, oldest first.
pub fn session_helpers(store: &Store, scope: &str, user: &str, session: &str) -> Vec<Value> {
    if user.is_empty() || session.is_empty() {
        return Vec::new();
    }
    let repo_id = split_scope(scope).1;
    let mut helpers: Vec<Value> = store
        .eph_scan(&helper_key(scope, user, session, ""))
        .into_iter()
        .map(|(_, record)| record)
        .filter(|record| !text(record, "worker").is_empty() && text(record, "repo_id") == repo_id)
        .collect();
    let since = |r: &Value| r.get("since").and_then(Value::as_f64).unwrap_or(0.0);
    helpers.sort_by(|a, b| since(a).partial_cmp(&since(b)).unwrap_or(std::cmp::Ordering::Equal));
    helpers
}

/// The session ended: none of its helpers is at work any more.
pub fn forget_helpers(store: &Store, scope: &str, user: &str, session: &str) {
    if user.is_empty() || session.is_empty() {
        return;
    }
    for (key, _) in store.eph_scan(&helper_key(scope, user, session, "")) {
        store.eph_delete(&key);
    }
}

/// What a stream entry says about the subagent that made it: the worker id,
/// its type and the name it was started with (from its marker, which every
/// worker post refreshes first). Nothing for the session's own entries.
fn worker_tag(store: &Store, scope: &str, user: &str, session: &str, worker: &str) -> Map<String, Value> {
    let mut tag = Map::new();
    if worker.is_empty() {
        return tag;
    }
    tag.insert("worker".into(), json!(worker));
    if let Some(marker) = store.eph_get(&worker_key(scope, user, session, worker)) {
        for (from, to) in [("type", "worker_type"), ("name", "worker_name")] {
            let value = text(&marker, from);
            if !value.is_empty() {
                tag.insert(to.into(), json!(value));
            }
        }
    }
    tag
}

const MAX_WORKER_NAME: usize = 120;

/// The subagent finished (the hook's SubagentStop): the roster stops
/// counting it now rather than when its marker runs out.
pub fn forget_worker(store: &Store, scope: &str, user: &str, session: &str, worker: &str) {
    if !worker.is_empty() && !user.is_empty() {
        store.eph_delete(&worker_key(scope, user, session, worker));
        store.eph_delete(&helper_key(scope, user, session, worker));
    }
}

pub fn tokens_key(scope: &str, user: &str, session: &str) -> String {
    format!("agenttokens:{}:{}", split_scope(scope).0, agent_id(user, session))
}

/// The identity every presence record carries. `repo_id` is what tells a
/// per-agent record from one written before this change — never the key's
/// arity, because a scope can hold more than one colon.
fn identity(scope: &str, user: &str, session: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("user".into(), json!(user));
    map.insert("session".into(), json!(session));
    map.insert("agent_id".into(), json!(agent_id(user, session)));
    map.insert("repo_id".into(), json!(split_scope(scope).1));
    map
}

/// Cumulative per-agent token counter, deduped by turn: many tool calls share
/// one assistant turn, so only the FIRST report carrying a new turn id adds
/// that turn's tokens.
///
/// The record also keeps the context the agent was last carrying (`context`,
/// with `model` and the effort it ran at): the size a message it did not
/// send would have replayed, which is what a saving reported without its own
/// usage is priced against.
#[allow(clippy::too_many_arguments)]
pub fn bump_tokens(
    store: &Store, scope: &str, user: &str, session: &str, tokens: i64, turn_id: &str, context: i64, model: &str,
    effort: &Effort,
) -> i64 {
    let key = tokens_key(scope, user, session);
    let mut record = store
        .eph_get(&key)
        .unwrap_or_else(|| json!({"total": 0, "last_turn": ""}));
    let last_turn = record.get("last_turn").and_then(Value::as_str).unwrap_or("").to_string();
    let total = record.get("total").and_then(Value::as_i64).unwrap_or(0);
    let mut result = total;
    let mut changed = false;
    if let Some(map) = record.as_object_mut() {
        if tokens > 0 && !turn_id.is_empty() && turn_id != last_turn {
            result = total + tokens;
            map.insert("total".into(), json!(result));
            map.insert("last_turn".into(), json!(turn_id));
            changed = true;
        }
        if context > 0 {
            map.insert("context".into(), json!(context));
            if !model.is_empty() {
                map.insert("model".into(), json!(model));
                effort.tag(map);
            }
            changed = true;
        }
        if changed {
            map.extend(identity(scope, user, session));
        }
    }
    if changed {
        let _ = store.eph_set(&key, &record, Some(AGENT_TOKENS_TTL_S));
    }
    result
}

/// The `via` tag as stored — `{host, path, mcp}` — from the
/// "host path" string the routes build (`via_tag`): the first host of a comma list, lowercased; the path
/// normalised to one leading slash ("/" when empty); `mcp` when the path is
/// the root or ends in "/mcp". A bare word with no host ("mcp", the MCP
/// server's tag) is a path. The dashboard's `viaLabel` reads the object and
/// shows nothing for a string, so this is what makes the chip render.
pub fn via_view(via: &str) -> Value {
    let via = via.trim();
    if via.is_empty() {
        return Value::Null;
    }
    let (host, path) = match via.split_once(' ') {
        Some((host, path)) => (host, path),
        None if via.starts_with('/') => ("", via),
        None if via.contains('.') || via.contains(':') => (via, ""),
        None => ("", via),
    };
    let host = host.split(',').next().unwrap_or("").trim().to_lowercase();
    let trimmed = path.trim().trim_matches('/');
    let path = if trimmed.is_empty() { "/".to_string() } else { format!("/{trimmed}") };
    let mcp = path == "/" || path.trim_end_matches('/').ends_with("/mcp");
    json!({"host": host, "path": path, "mcp": mcp})
}

/// The one line the Now panel shows under an agent: the last thing it did.
#[allow(clippy::too_many_arguments)]
pub fn note_last_action(
    store: &Store, scope: &str, user: &str, session: &str, kind: &str, path: &str,
    agent: &str, model: &str, effort: &Effort, branch: &str, note: &str, via: &str, worker: &str,
) {
    let mut record = identity(scope, user, session);
    // a subagent's step is labelled with it, so the stream shows it as the
    // subagent's and not as the session's own
    record.extend(worker_tag(store, scope, user, session, worker));
    record.insert("kind".into(), json!(kind));
    record.insert("path".into(), json!(clip(path, MAX_PATH)));
    record.insert("ts".into(), json!(now()));
    record.insert("agent".into(), json!(agent));
    record.insert("model".into(), json!(model));
    effort.tag(&mut record);
    record.insert("branch".into(), json!(branch));
    // a closing message was trimmed to its first sentences already; the
    // margin is for the ellipsis and the code it closes
    record.insert("note".into(), json!(clip(note, if kind == "done" { MAX_DONE + 8 } else { MAX_NOTE })));
    record.insert("via".into(), via_view(via));
    // the line it replaces joins the agent's recent history (the dashboard
    // shows it above the current bubble), unless it said the same thing
    let key = last_action_key(scope, user, session);
    if let Some(previous) = store.eph_get(&key) {
        // the session is leaving a repo: its last step there is kept, for
        // that repo's page, however many steps it takes elsewhere after
        let left = text(&previous, "repo_id");
        if !left.is_empty() && previous.get("repo_id") != record.get("repo_id") {
            let left_scope = format!("{}:{left}", split_scope(scope).0);
            let _ = store.eph_set(&last_in_key(&left_scope, user, session), &previous, Some(LAST_IN_TTL_S));
        }
        let same = previous.get("kind") == record.get("kind") && previous.get("path") == record.get("path")
            && previous.get("worker") == record.get("worker");
        if !same {
            push_history(store, scope, user, session, &previous);
        } else if kind == "edit" && note.is_empty() {
            // the same file again: its edit summary stands until the
            // report's diff writes a fresh one (annotate_last_edit)
            if let Some(kept) = previous.get("note").filter(|n| n.as_str().is_some_and(|n| !n.is_empty())) {
                record.insert("note".into(), kept.clone());
            }
        }
    }
    let _ = store.eph_set(&key, &Value::Object(record), Some(LAST_ACTION_TTL_S));
}

/// What an edit changed, in a line (`+9 −3 · charge(user, amount, region) ·
/// old_total → subtotal`), onto the agent's last action when that is still
/// the edit of `path`: the chat's "edited" bubble shows it under the path.
pub fn annotate_last_edit(store: &Store, scope: &str, user: &str, session: &str, path: &str, note: &str) {
    if note.is_empty() {
        return;
    }
    let key = last_action_key(scope, user, session);
    let Some(mut record) = store.eph_get(&key) else { return };
    if record.get("kind").and_then(Value::as_str) != Some("edit") || record.get("path").and_then(Value::as_str) != Some(&clip(path, MAX_PATH)) {
        return;
    }
    record["note"] = json!(clip(note, MAX_NOTE));
    let _ = store.eph_set(&key, &record, Some(LAST_ACTION_TTL_S));
}

/// How long a "running" that has started may stand with nothing after it:
/// long enough for a slow test suite or build, not forever for a killed one.
pub const STARTED_TTL_S: f64 = 1800.0;

/// A shell command has STARTED (PreToolUse; the hook hands it off so the
/// command is not held): the agent is running it now. Only the focus marker
/// moves, flagged `started`; the command's end (PostToolUse) overwrites it
/// and writes the last action, which is when the dashboard says "ran".
#[allow(clippy::too_many_arguments)]
pub fn note_started(
    store: &Store, scope: &str, user: &str, session: &str, command: &str, branch: &str, model: &str, effort: &Effort,
) {
    let mut marker = identity(scope, user, session);
    if !model.is_empty() {
        marker.insert("model".into(), json!(model));
        effort.tag(&mut marker);
    }
    marker.insert("path".into(), json!(clip(command, MAX_PATH)));
    marker.insert("action".into(), json!("running"));
    marker.insert("started".into(), json!(true));
    marker.insert("branch".into(), json!(branch));
    marker.insert("ts".into(), json!(now()));
    let _ = store.eph_set(&focus_key(scope, user, session), &Value::Object(marker), Some(STARTED_TTL_S));
}

/// A shell command a SUBAGENT started, when the hook named it (the started
/// post carries `worker`): the worker's marker says "running" and stands
/// for the command's length, capped at `STARTED_TTL_S`; the session's own
/// focus is left alone. Its end (the worker's next post) completes it.
pub fn note_worker_started(
    store: &Store, scope: &str, user: &str, session: &str, worker: &str, worker_type: &str, name: &str, command: &str,
) {
    note_worker_activity(store, scope, user, session, worker, worker_type, name, "running", command, true);
}

/// A subagent's command has ended. A hook that does not name the worker on
/// its "started" post put that command on the SESSION's focus; the worker's
/// end report, which does name it, is the completion, so a started focus
/// for the same command is not left standing for half an hour.
fn complete_worker_command(store: &Store, scope: &str, user: &str, session: &str, shown: &str) {
    let key = focus_key(scope, user, session);
    let Some(focus) = store.eph_get(&key) else { return };
    if focus.get("started").and_then(Value::as_bool) == Some(true) && text(&focus, "path") == shown {
        store.eph_delete(&key);
    }
}

/// How many earlier lines an agent keeps IN THE CACHE (the roster row's
/// `earlier_actions`, one read per poll). A session and its subagents
/// share one history: four builders at work write dozens of steps each,
/// and at thirty the parent's own trailing steps had pushed theirs out,
/// so their lanes on the timeline vanished the moment they finished.
pub const HISTORY_LEN: usize = 150;

/// Every step a session took, for good: the cache above is the first page
/// of it, this is the rest. Kept in the durable store as chunks of
/// [`STEP_CHUNK`] steps per session (`steplog:{ws}:{agent}:{n}`, with
/// `steplog:{ws}:{agent}` naming the newest chunk), so a step is never
/// dropped for being the 151st and the feed's "earlier" reaches the first
/// thing the session did. Read back a chunk at a time, newest first.
pub const STEP_CHUNK: usize = 100;
pub const STEPLOG_BUCKET: &str = "steplog";

fn steplog_index_key(scope: &str, user: &str, session: &str) -> String {
    format!("{}:{}", split_scope(scope).0, agent_id(user, session))
}

fn steplog_chunk_key(scope: &str, user: &str, session: &str, chunk: i64) -> String {
    format!("{}:{}", steplog_index_key(scope, user, session), chunk)
}

/// A step the cache is about to drop or has just pushed: appended to the
/// session's log, in the newest chunk, a new chunk every STEP_CHUNK.
fn append_step(store: &Store, scope: &str, user: &str, session: &str, item: &Value) {
    let index_key = steplog_index_key(scope, user, session);
    let stamp = now();
    let index = store.kv_get(STEPLOG_BUCKET, &index_key).unwrap_or_else(|| json!({"head": 0, "total": 0}));
    let mut head = index.get("head").and_then(Value::as_i64).unwrap_or(0);
    let total = index.get("total").and_then(Value::as_i64).unwrap_or(0);
    let chunk_key = steplog_chunk_key(scope, user, session, head);
    let mut items: Vec<Value> = store.kv_get(STEPLOG_BUCKET, &chunk_key).and_then(|c| c.get("items").and_then(Value::as_array).cloned()).unwrap_or_default();
    if items.len() >= STEP_CHUNK {
        head += 1;
        items = Vec::new();
    }
    items.push(item.clone());
    let _ = store.kv_put(STEPLOG_BUCKET, &steplog_chunk_key(scope, user, session, head), &json!({"items": items}), stamp);
    let _ = store.kv_put(STEPLOG_BUCKET, &index_key, &json!({"head": head, "total": total + 1, "updated": stamp}), stamp);
}

/// A chunk of a session's step log, oldest first, newest chunk when
/// `before` is None, else the chunk before `before`: (items, this chunk's
/// number, the number of the one before it or None at the start, the
/// log's total). Steps older than `oldest_ts` (the plan's window) are
/// left out.
pub fn steps(store: &Store, scope: &str, user: &str, session: &str, before: Option<i64>, oldest_ts: f64) -> (Vec<Value>, i64, Option<i64>, i64) {
    let index = store.kv_get(STEPLOG_BUCKET, &steplog_index_key(scope, user, session)).unwrap_or_else(|| json!({"head": 0, "total": 0}));
    let head = index.get("head").and_then(Value::as_i64).unwrap_or(0);
    let total = index.get("total").and_then(Value::as_i64).unwrap_or(0);
    let chunk = match before {
        Some(b) => b - 1,
        None => head,
    };
    if chunk < 0 {
        return (Vec::new(), 0, None, total);
    }
    let items: Vec<Value> = store
        .kv_get(STEPLOG_BUCKET, &steplog_chunk_key(scope, user, session, chunk))
        .and_then(|c| c.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|item| item.get("ts").and_then(Value::as_f64).unwrap_or(0.0) >= oldest_ts)
        .collect();
    let next = (chunk > 0).then_some(chunk);
    (items, chunk, next, total)
}

pub fn history_key(scope: &str, user: &str, session: &str) -> String {
    format!("lasthist:{}:{}", split_scope(scope).0, agent_id(user, session))
}

fn push_history(store: &Store, scope: &str, user: &str, session: &str, previous: &Value) {
    let key = history_key(scope, user, session);
    let mut items: Vec<Value> = store.eph_get(&key).and_then(|v| v.get("items").and_then(Value::as_array).cloned()).unwrap_or_default();
    let pick = |k: &str| previous.get(k).cloned().unwrap_or(Value::Null);
    // the repo the step was in: a session that works across two checkouts
    // is shown on both repos' pages from its history (activity.rs)
    let mut item = json!({"kind": pick("kind"), "path": pick("path"), "note": pick("note"), "ts": pick("ts"), "repo_id": pick("repo_id")});
    // what Cork and the agent exchanged on that step goes with it
    if let Some(x) = previous.get("x").filter(|x| x.is_object()) {
        item["x"] = x.clone();
    }
    // a subagent's step keeps its label in the history too, and every step
    // the branch it was taken on
    for field in ["worker", "worker_type", "worker_name", "branch"] {
        if let Some(value) = previous.get(field).filter(|v| v.as_str().is_some_and(|v| !v.is_empty())) {
            item[field] = value.clone();
        }
    }
    // the cache keeps the newest HISTORY_LEN; the log keeps them all
    append_step(store, scope, user, session, &item);
    items.insert(0, item);
    items.truncate(HISTORY_LEN);
    let _ = store.eph_set(&key, &json!({"items": items}), Some(LAST_ACTION_TTL_S));
}

/// The agent's earlier lines, newest first (the current one not among them).
pub fn history(store: &Store, scope: &str, user: &str, session: &str) -> Vec<Value> {
    store.eph_get(&history_key(scope, user, session))
        .and_then(|v| v.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// Writing supersedes reading: `report_edit` calls this so the Now line stops
/// saying "reading X" once THIS agent has moved on — another agent of the
/// same person keeps its own line.
pub fn clear_focus(store: &Store, scope: &str, user: &str, session: &str) {
    store.eph_delete(&focus_key(scope, user, session));
}

/// A roster is workspace-wide but a live feed is per repo, so a presence
/// change in one repo has to reach the dashboards watching every other repo
/// of the workspace. What crosses is a content-free nudge — no user, no path,
/// not even which repo — because the subscriber's per-repo access is unknown
/// here; the dashboard answers it by re-reading the activity endpoint, which
/// filters. Live subscribers only: the nudge never lands in a repo's event
/// ring, where it would be mistaken for history.
pub fn nudge_workspace(store: &Store, scope: &str) {
    nudge(store, scope, false);
}

/// This repo's own feed normally hears the event itself; `include_self` is
/// for a change that publishes none.
fn nudge(store: &Store, scope: &str, include_self: bool) {
    let (workspace, _repo) = split_scope(scope);
    let nudge = json!({"kind": "presence_nudge", "workspace": workspace, "ts": now()});
    for other in store.list_scopes(&format!("{workspace}:")) {
        if include_self || other != scope {
            store.broadcast(&other, &nudge);
        }
    }
}

/// Removing a repo from a workspace takes its agents' presence with it. The
/// keys no longer carry the repo, so this reads the record's `repo_id` —
/// the same field the roster reads — rather than a prefix.
pub fn forget_scope(store: &Store, scope: &str) {
    let (workspace, repo) = split_scope(scope);
    for bucket in ["focus", "lastact", "agenttokens"] {
        for (key, record) in store.eph_scan(&format!("{bucket}:{workspace}:")) {
            if record.get("repo_id").and_then(Value::as_str) == Some(repo) {
                store.eph_delete(&key);
            }
        }
    }
}

/// How long since a hook session's last step it still counts as open, for
/// saying when a waiting message will land (`reach_of`).
pub const OPEN_SESSION_S: f64 = 2.0 * 3600.0;

/// How a message still waiting for `user` will reach them, from their
/// agents' last steps in this workspace (the week-long markers):
///
/// - `hooks`: a session with Cork's hooks is open; it hears the message
///   at its next step;
/// - `mcp`: only MCP clients without hooks (a chat app on the connector,
///   say) have worked here lately; one hears it on its next Cork call;
/// - `none`: no agent of theirs is open.
///
/// `{kind, agent, last_seen}`, `agent` the newest one's name as it gave it.
pub fn reach_of(store: &Store, workspace: &str, user: &str) -> Value {
    let prefix = format!("lastact:{workspace}:{user}");
    let stamp = now();
    let (mut hooks, mut mcp): (Option<(f64, String)>, Option<(f64, String)>) = (None, None);
    for (key, marker) in store.eph_scan(&prefix) {
        let rest = &key[prefix.len()..];
        // `user` and `user#session`, never another user whose label starts the same
        if !(rest.is_empty() || rest.starts_with('#')) {
            continue;
        }
        let session = rest.trim_start_matches('#');
        let ts = marker.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        let agent = text(&marker, "agent");
        let via_mcp = marker.get("via").and_then(|v| v.get("mcp")).and_then(Value::as_bool) == Some(true);
        let scope = format!("{workspace}:{}", text(&marker, "repo_id"));
        let ended = !session.is_empty() && store.eph_get(&format!("sessended:{scope}:{session}")).is_some();
        let slot = if via_mcp { &mut mcp } else { &mut hooks };
        if via_mcp || (!session.is_empty() && !ended && stamp - ts <= OPEN_SESSION_S) {
            if slot.as_ref().is_none_or(|(seen, _)| ts > *seen) {
                *slot = Some((ts, agent));
            }
        }
    }
    let (kind, newest) = match (hooks, mcp) {
        (Some(open), _) => ("hooks", Some(open)),
        (None, Some(app)) => ("mcp", Some(app)),
        (None, None) => ("none", None),
    };
    let (last_seen, agent) = newest.map(|(ts, agent)| (json!(ts), agent_name(&agent))).unwrap_or((Value::Null, String::new()));
    json!({"kind": kind, "agent": agent, "last_seen": last_seen})
}

/// Presence from any protocol call — a briefing, an intent, a check — so an
/// agent appears on the dashboard the moment it does anything. Never clobbers
/// a richer, fresher read or edit marker from the hooks.
pub fn touch_presence(store: &Store, scope: &str, user: &str, session: &str, agent: &str, via: &str) {
    let key = focus_key(scope, user, session);
    let stamp = now();
    if let Some(existing) = store.eph_get(&key) {
        if stamp - existing.get("ts").and_then(Value::as_f64).unwrap_or(0.0) < 60.0 {
            return;
        }
    }
    let mut marker = identity(scope, user, session);
    marker.insert("path".into(), json!(""));
    marker.insert("action".into(), json!("working"));
    marker.insert("agent".into(), json!(agent));
    marker.insert("ts".into(), json!(stamp));
    let _ = store.eph_set(&key, &Value::Object(marker), Some(FOCUS_TTL_S));
    // the week-long row too: an
    // agent that only ever called the protocol still has a last action, and
    // without this it vanished from the Now panel after three minutes
    note_last_action(store, scope, user, session, "working", "", agent, "", &Effort::default(), "", "", via, "");
    nudge_workspace(store, scope);
}

pub struct PresenceInput<'a> {
    pub scope: &'a str,
    pub user_id: &'a str,
    /// Which conversation is looking. The hook has always sent it; the
    /// server used to drop it on the floor and fold every agent of a person
    /// into one marker.
    pub session: &'a str,
    pub path: &'a str,
    pub action: &'a str,
    pub agent: &'a str,
    pub model: &'a str,
    /// the effort the model ran at, and ultracode (`Effort::of` the post)
    pub effort: Effort,
    pub branch: &'a str,
    pub tokens: i64,
    pub turn_id: &'a str,
    /// input + cache read + cache creation of the turn: what a message replays
    pub context: i64,
    pub via: &'a str,
    /// the subagent the post came from ("" for the session itself): its
    /// step moves its worker marker and is labelled with it in the stream
    pub worker: &'a str,
}

/// The agent's harness hit a usage limit (the Stop hook read it off the
/// transcript); `path` carries the harness's own words. The Now panel shows the agent PAUSED with
/// that note for up to six hours or until it acts again — replacing whatever
/// read marker was standing, so a "reading …" pill never floats over a paused
/// agent; the ledger keeps a row; the live bus and since_your_last_call carry
/// it to teammates' agents, so nobody waits on a collaborator that silently
/// went dark. One row per distinct note: repeating the same note refreshes
/// the marker and says so, and writes nothing durable.
fn report_limit(store: &Store, input: &PresenceInput) -> Value {
    let stamp = now();
    let note = clip(if input.path.is_empty() { "usage limit reached" } else { input.path }, MAX_NOTE);
    let key = focus_key(input.scope, input.user_id, input.session);
    let existing = store.eph_get(&key).unwrap_or_else(|| json!({}));
    // the Stop hook has no transcript tail to read a model out of, so the
    // marker keeps the model the agent's last read reported, and its effort
    let (model, effort) = if input.model.is_empty() {
        (text(&existing, "model"), Effort::of(&existing))
    } else {
        (input.model.to_string(), input.effort.clone())
    };
    let mut marker = identity(input.scope, input.user_id, input.session);
    marker.insert("path".into(), json!(note));
    marker.insert("action".into(), json!("limit"));
    marker.insert("agent".into(), json!(input.agent));
    marker.insert("model".into(), json!(model));
    effort.tag(&mut marker);
    marker.insert("branch".into(), json!(input.branch));
    marker.insert("ts".into(), json!(stamp));
    let _ = store.eph_set(&key, &Value::Object(marker), Some(LIMIT_TTL_S));
    // the note travels in `note`, not `path`: the last-action line reads
    // "paused · <note>", and there is no file to name
    note_last_action(
        store, input.scope, input.user_id, input.session, "limit", "",
        input.agent, input.model, &input.effort, input.branch, &note, input.via, "",
    );
    if text(&existing, "action") == "limit" && text(&existing, "path") == note {
        return json!({"ok": true, "noted": "limit", "repeat": true});
    }
    let mut row = json!({"user": input.user_id, "agent": input.agent, "note": note});
    // the caller's address, stamped on the one row this path writes
    if !input.via.is_empty() {
        row["via"] = via_view(input.via);
    }
    let _ = store.ledger_append(input.scope, "agent_limit", &row, stamp);
    crate::events::publish(
        store,
        input.scope,
        json!({"kind": "agent_limit", "user": input.user_id, "agent": input.agent, "note": note}),
        input.via,
    );
    json!({"ok": true, "noted": "limit"})
}

/// The session is over — the hook's SessionEnd. The focus marker goes at
/// once (a "reading x" pill on an agent that has exited would lie) and the
/// last-action line becomes "ended", which the roster reports as `ended` so
/// the dashboard stops counting the agent while its spend still adds up:
/// the token counter is left alone, what it spent is spent. No ledger row
/// and nothing on the event ring — an exit is not history anyone replays —
/// but every dashboard of the workspace, this repo's included, is nudged to
/// re-read the roster.
fn report_ended(store: &Store, input: &PresenceInput) -> Value {
    store.eph_delete(&focus_key(input.scope, input.user_id, input.session));
    forget_helpers(store, input.scope, input.user_id, input.session);
    note_last_action(
        store, input.scope, input.user_id, input.session, "ended", "",
        input.agent, input.model, &input.effort, input.branch, "", input.via, "",
    );
    nudge(store, input.scope, true);
    json!({"ok": true, "noted": "ended"})
}

/// A turn's end — the hook's Stop, carrying the agent's closing message.
/// The agent is waiting on its person now, not still running whatever it
/// ran last, so the focus marker goes and the last-action line becomes
/// "done" with the first two sentences of that message as its note, its
/// line breaks and Markdown kept. The model (with its effort), agent and
/// branch stay as the line before had them: Stop knows none of them. With the workspace's
/// `closing_messages` switch off the text arrives empty (privacy.rs) and
/// the note is empty too.
pub fn report_done(store: &Store, scope: &str, user: &str, session: &str, reply: &str) {
    if session.is_empty() {
        return;
    }
    store.eph_delete(&focus_key(scope, user, session));
    let previous = store.eph_get(&last_action_key(scope, user, session)).unwrap_or(Value::Null);
    let said = first_sentences(reply, DONE_SENTENCES, MAX_DONE);
    note_last_action(
        store, scope, user, session, "done", "",
        &text(&previous, "agent"), &text(&previous, "model"), &Effort::of(&previous), &text(&previous, "branch"), &said, "", "",
    );
    nudge(store, scope, true);
}

/// Words a period follows without ending a sentence ("e.g. the gate").
const ABBREVIATIONS: [&str; 18] = [
    "e.g", "i.e", "eg", "ie", "vs", "cf", "approx", "incl", "esp", "viz", "al", "a.k.a", "aka", "resp", "fig", "mr", "mrs", "dr",
];

/// Where a scan of Markdown stands on code: inside an inline code span,
/// inside a fenced block.
#[derive(Default, Clone, Copy)]
struct CodeState {
    inline: bool,
    fence: bool,
}

impl CodeState {
    /// Step over the backticks starting at `chars[i]`; how many there were.
    fn backticks(&mut self, chars: &[char], i: usize) -> usize {
        let run = chars[i..].iter().take_while(|c| **c == '`').count();
        if run >= 3 {
            if !self.inline {
                self.fence = !self.fence;
            }
        } else if !self.fence {
            self.inline = !self.inline;
        }
        run
    }

    fn of(chars: &[char]) -> CodeState {
        let mut state = CodeState::default();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '`' => i += state.backticks(chars, i),
                '\n' => {
                    // a stray backtick never swallows the next line
                    state.inline = false;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        state
    }
}

/// Is the period at `chars[at]` part of a word rather than a sentence's
/// end: an abbreviation (`e.g.`, `i.e.`, `vs.`), or a numbered list's
/// marker (`1.` at the start of a line)?
fn period_in_word(chars: &[char], at: usize) -> bool {
    let mut start = at;
    while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '.') {
        start -= 1;
    }
    let word: String = chars[start..at].iter().collect::<String>().to_lowercase();
    if ABBREVIATIONS.contains(&word.as_str()) {
        return true;
    }
    let line_start = chars[..start].iter().rev().take_while(|c| **c != '\n').all(|c| c.is_whitespace());
    !word.is_empty() && word.chars().all(|c| c.is_ascii_digit()) && line_start
}

/// The first `count` sentences of `text`, its line breaks and Markdown
/// kept. A sentence ends at `.`, `!` or `?` followed by whitespace or the
/// end — so `3.14`, `v1.2` and `chat.tsx` never end one, nor does `e.g.`,
/// a list's `1.`, or anything inside backticks. A run-on sentence is cut at
/// `cap` characters, on a word, with an ellipsis and any code it opened
/// closed again. The dashboard trims rows stored before this the same way
/// (dashboard/sentences.ts).
pub fn first_sentences(text: &str, count: usize, cap: usize) -> String {
    let chars: Vec<char> = text.trim().chars().collect();
    let mut state = CodeState::default();
    let mut found = 0usize;
    let mut end = chars.len();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            i += state.backticks(&chars, i);
            continue;
        }
        if c == '\n' {
            state.inline = false;
        }
        if !state.inline && !state.fence && matches!(c, '.' | '!' | '?') {
            // a run of marks, then what closes around them (`done.)`, `**done.**`)
            let mut j = i + 1;
            while j < chars.len() && matches!(chars[j], '.' | '!' | '?') {
                j += 1;
            }
            while j < chars.len() && matches!(chars[j], ')' | ']' | '"' | '\'' | '*' | '_' | '\u{201d}' | '\u{2019}') {
                j += 1;
            }
            let at_break = j == chars.len() || chars[j].is_whitespace();
            if at_break && !(c == '.' && j == i + 1 && period_in_word(&chars, i)) {
                found += 1;
                if found >= count {
                    end = j;
                    break;
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    let mut kept: Vec<char> = chars[..end].to_vec();
    while kept.last().is_some_and(|c| c.is_whitespace()) {
        kept.pop();
    }
    if kept.len() <= cap {
        return kept.iter().collect();
    }
    // a run-on: cut on a word, never past the cap
    let mut cut = cap;
    if let Some(space) = kept[..cap].iter().rposition(|c| c.is_whitespace()) {
        if space > cap / 2 {
            cut = space;
        }
    }
    let mut head: Vec<char> = kept[..cut].to_vec();
    while head.last().is_some_and(|c| c.is_whitespace()) {
        head.pop();
    }
    let open = CodeState::of(&head);
    let mut out: String = head.iter().collect();
    out.push('\u{2026}');
    if open.fence {
        out.push_str("\n```");
    } else if open.inline {
        out.push('`');
    }
    out
}

/// What Cork and the agent exchanged on one step, kept on the step's own
/// record as `x` (the last action, then its history item: the same
/// lifetimes): an edit's hunk ranges, what changed by name, its callers,
/// the gate's verdict, lint findings and the dependents' tests; a prompt's
/// briefing by counts and names; the deltas and messages a tool result
/// carried. Structure and counts only, never prompt text, file text or a
/// command. Bounded: lists capped at `MAX_X_NAMES`, strings at
/// `MAX_X_TEXT`, the whole record at `MAX_X_BYTES`, the least telling
/// fields shed first.
pub const MAX_X_NAMES: usize = 8;
const MAX_X_TEXT: usize = 120;
pub const MAX_X_BYTES: usize = 1200;
/// Counts that add up across the posts of one step.
const X_SUMMED: [&str; 2] = ["deltas", "msgs"];
/// Fields shed, in this order, when a step's exchange runs over its bytes.
const X_SHED: [&str; 11] = [
    "caller_names", "doc", "lint_names", "deltas_from", "msgs_from", "module_names", "symbol_names", "sig", "hunks", "ren", "gate",
];

/// Which step an exchange belongs to: whatever the agent is on now, or its
/// newest step of this kind on this path (a test verdict arrives after the
/// agent has moved on).
pub enum Step<'a> {
    Current,
    Latest { kind: &'a str, path: &'a str },
}

/// What one step cost, for the dashboard's line and nothing else: the
/// usage of the assistant message that made the tool call (hooks v82+ send
/// it as `step`: the message's id and its four counters). Several tool calls
/// of one message carry the same message, so the dashboard shows it once,
/// on that message's last step. The bill is the transcript's at Stop
/// (`bill_session`): this never touches the ledger or a counter.
pub fn step_usage(body: &Value) -> Option<Value> {
    let step = body.get("step").and_then(Value::as_object)?;
    let n = |k: &str| step.get(k).and_then(Value::as_i64).unwrap_or(0).max(0);
    let counts = crate::pricing::Counts {
        input: n("input"), output: n("output"), cache_read: n("cache_read"), cache_write: n("cache_write"), cache_write_1h: 0,
    };
    let tokens = counts.input + counts.output + counts.cache_read + counts.cache_write;
    if tokens <= 0 {
        return None;
    }
    let mut x = json!({"stok": tokens});
    let message = clip(step.get("msg").and_then(Value::as_str).unwrap_or(""), 16);
    if !message.is_empty() {
        x["smsg"] = json!(message);
    }
    let model = text(body, "model");
    if !model.is_empty() {
        let usd = crate::pricing::usd(&model, &counts, &crate::pricing::Serving::default());
        if usd > 0.0 {
            x["susd"] = json!(crate::compat::python_round(usd, 4));
        }
    }
    Some(x)
}

/// A step of this agent's: its last action or a history item, the newest
/// of `kind` on `path` (clash.rs reads the other side's edit off it).
pub fn find_step(store: &Store, scope: &str, user: &str, session: &str, kind: &str, path: &str) -> Option<Value> {
    let shown = clip(path, MAX_PATH);
    let hit = |record: &Value| text(record, "kind") == kind && text(record, "path") == shown;
    if let Some(last) = store.eph_get(&last_action_key(scope, user, session)).filter(hit) {
        return Some(last);
    }
    history(store, scope, user, session).into_iter().find(hit)
}

/// Rewrite the records of one agent's steps in place (clash.rs settles
/// an outcome this way): `edit` gets each step and says whether it changed it.
pub fn edit_steps(store: &Store, scope: &str, user: &str, session: &str, mut edit: impl FnMut(&mut Value) -> bool) {
    let key = last_action_key(scope, user, session);
    if let Some(mut last) = store.eph_get(&key) {
        if edit(&mut last) {
            let _ = store.eph_set(&key, &last, Some(LAST_ACTION_TTL_S));
        }
    }
    let key = history_key(scope, user, session);
    if let Some(mut items) = store.eph_get(&key) {
        let mut changed = false;
        if let Some(list) = items.get_mut("items").and_then(Value::as_array_mut) {
            for item in list.iter_mut() {
                changed |= edit(item);
            }
        }
        if changed {
            let _ = store.eph_set(&key, &items, Some(LAST_ACTION_TTL_S));
        }
    }
}

fn tidy_exchange(value: &Value) -> Value {
    match value {
        Value::String(s) => json!(clip(s, MAX_X_TEXT)),
        Value::Array(items) => Value::Array(items.iter().take(MAX_X_NAMES).map(tidy_exchange).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), tidy_exchange(v))).collect()),
        other => other.clone(),
    }
}

/// An exchange held to its bounds (see `MAX_X_BYTES`).
pub fn bound_exchange(value: &Value) -> Value {
    let size = |x: &Value| serde_json::to_string(x).map(|s| s.len()).unwrap_or(0);
    let Value::Object(mut map) = tidy_exchange(value) else { return json!({}) };
    for key in X_SHED {
        if size(&Value::Object(map.clone())) <= MAX_X_BYTES {
            break;
        }
        map.remove(key);
    }
    // still over: the largest field goes, until it fits; a collision (held
    // to its own bound, clash.rs) goes last
    while size(&Value::Object(map.clone())) > MAX_X_BYTES {
        let largest = map.iter().filter(|(k, _)| k.as_str() != "c").max_by_key(|(_, v)| size(v)).map(|(k, _)| k.clone())
            .or_else(|| map.keys().next().cloned());
        let Some(largest) = largest else { break };
        map.remove(&largest);
    }
    Value::Object(map)
}

fn merge_exchange(into: &mut Value, patch: &Map<String, Value>) {
    let mut x = into.as_object().cloned().unwrap_or_default();
    for (key, value) in patch {
        let merged = match (x.get(key), value) {
            (Some(Value::Number(a)), Value::Number(b)) if X_SUMMED.contains(&key.as_str()) => {
                json!(a.as_i64().unwrap_or(0) + b.as_i64().unwrap_or(0))
            }
            (Some(Value::Array(a)), Value::Array(b)) if key.ends_with("_from") => {
                let mut all = a.clone();
                for v in b {
                    if !all.contains(v) {
                        all.push(v.clone());
                    }
                }
                Value::Array(all)
            }
            _ => value.clone(),
        };
        x.insert(key.clone(), merged);
    }
    *into = bound_exchange(&Value::Object(x));
}

/// Fold `patch` into the exchange of one of the agent's steps (see `Step`).
/// Nothing when the step is gone (aged out, or past the history's length).
pub fn note_exchange(store: &Store, scope: &str, user: &str, session: &str, step: Step, patch: &Value) {
    let Some(patch) = patch.as_object().filter(|p| !p.is_empty()) else { return };
    let matches = |record: &Value| match &step {
        Step::Current => true,
        Step::Latest { kind, path } => text(record, "kind") == *kind && text(record, "path") == clip(path, MAX_PATH),
    };
    let key = last_action_key(scope, user, session);
    let Some(mut record) = store.eph_get(&key) else { return };
    if matches(&record) {
        merge_exchange(&mut record["x"], patch);
        let _ = store.eph_set(&key, &record, Some(LAST_ACTION_TTL_S));
        return;
    }
    if matches!(step, Step::Current) {
        return;
    }
    let history = history_key(scope, user, session);
    let Some(mut items) = store.eph_get(&history) else { return };
    let Some(item) = items.get_mut("items").and_then(Value::as_array_mut).and_then(|items| items.iter_mut().find(|item| matches(item))) else {
        return;
    };
    merge_exchange(&mut item["x"], patch);
    let _ = store.eph_set(&history, &items, Some(LAST_ACTION_TTL_S));
}

/// The messages a hook answer handed this agent, on the step it is on:
/// how many, and from whom (a user id, or "your other session").
pub fn note_messages(store: &Store, scope: &str, user: &str, session: &str, count: usize, from: &[String]) {
    if count == 0 {
        return;
    }
    note_exchange(store, scope, user, session, Step::Current, &json!({"msgs": count, "msgs_from": from}));
}

pub fn report_presence(store: &Store, input: &PresenceInput) -> Value {
    // "limit" is a different thing entirely — the agent stopped because it
    // ran out of quota — with its own marker lifetime and a ledger row
    if input.action == "limit" {
        return report_limit(store, input);
    }
    if input.action == "ended" {
        return report_ended(store, input);
    }
    let action = match input.action {
        "reading" | "searching" | "running" | "thinking" => input.action,
        _ => "reading",
    };
    // a subagent's context is its own, not the session's
    let context = if input.worker.is_empty() { input.context } else { 0 };
    bump_tokens(store, input.scope, input.user_id, input.session, input.tokens, input.turn_id, context, input.model, &input.effort);

    let shown = clip(input.path, MAX_PATH);
    if input.worker.is_empty() {
        let mut marker = identity(input.scope, input.user_id, input.session);
        marker.insert("path".into(), json!(shown));
        marker.insert("action".into(), json!(action));
        marker.insert("agent".into(), json!(input.agent));
        marker.insert("model".into(), json!(input.model));
        input.effort.tag(&mut marker);
        marker.insert("branch".into(), json!(input.branch));
        marker.insert("ts".into(), json!(now()));
        let _ = store.eph_set(
            &focus_key(input.scope, input.user_id, input.session),
            &Value::Object(marker),
            Some(FOCUS_TTL_S),
        );
    } else {
        // a subagent's step moves its own marker, never the session's focus:
        // the session's own command in flight stands until it ends
        note_worker_activity(store, input.scope, input.user_id, input.session, input.worker, "", "", action, input.path, false);
        complete_worker_command(store, input.scope, input.user_id, input.session, &shown);
    }
    note_last_action(
        store, input.scope, input.user_id, input.session, action, input.path,
        input.agent, input.model, &input.effort, input.branch, "", input.via, input.worker,
    );

    let mut event = json!({
        "kind": "focus", "user": input.user_id, "session": input.session,
        "agent_id": agent_id(input.user_id, input.session),
        "path": shown, "action": action, "agent": input.agent,
    });
    if let Some(map) = event.as_object_mut() {
        if !input.worker.is_empty() {
            map.insert("worker".into(), json!(input.worker));
        }
        if input.tokens > 0 {
            map.insert("tokens".into(), json!(input.tokens));
        }
        if !input.model.is_empty() {
            map.insert("model".into(), json!(input.model));
            input.effort.tag(map);
        }
        if !input.branch.is_empty() {
            map.insert("branch".into(), json!(input.branch));
        }
    }
    crate::events::publish(store, input.scope, event, input.via);
    nudge_workspace(store, input.scope);
    json!({"ok": true})
}

#[cfg(test)]
mod effort_tests {
    use super::*;

    #[test]
    fn an_effort_is_kept_as_sent_trimmed_lowercased_and_capped_and_ultracode_only_when_true() {
        let read = |body: Value| Effort::of(&body);
        assert_eq!(read(json!({"effort": " XHigh ", "ultracode": true})), Effort { effort: "xhigh".into(), ultracode: true });
        assert_eq!(read(json!({"effort": "", "ultracode": "true"})), Effort::default());
        assert_eq!(read(json!({"model": "opus"})), Effort::default());
        assert_eq!(read(json!({"effort": "a".repeat(100)})).effort.len(), MAX_EFFORT);
        // onto a record: unknown is left off, and taken off a record that had it
        let mut record = json!({"model": "opus", "effort": "max", "ultracode": true});
        Effort::default().tag_value(&mut record);
        assert_eq!(record, json!({"model": "opus"}));
        Effort { effort: "low".into(), ultracode: false }.tag_value(&mut record);
        assert_eq!(record, json!({"model": "opus", "effort": "low"}));
    }

    #[test]
    fn the_token_record_keeps_the_effort_beside_its_model() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let (scope, user, session) = ("w:github.com/acme/a", "k@x.io", "s1");
        bump_tokens(&store, scope, user, session, 10, "t1", 5_000, "opus", &Effort { effort: "max".into(), ultracode: true });
        let record = store.eph_get(&tokens_key(scope, user, session)).unwrap();
        assert_eq!((record["effort"].clone(), record["ultracode"].clone()), (json!("max"), json!(true)));
        // a later turn on a model that names none drops what it no longer describes
        bump_tokens(&store, scope, user, session, 10, "t2", 6_000, "sonnet", &Effort::default());
        let record = store.eph_get(&tokens_key(scope, user, session)).unwrap();
        assert_eq!(record["model"], json!("sonnet"));
        assert!(record.get("effort").is_none() && record.get("ultracode").is_none());
    }
}

#[cfg(test)]
mod steplog_tests {
    use super::*;

    #[test]
    fn every_step_is_kept_in_the_log_in_chunks_and_read_back_newest_chunk_first() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let (scope, user, session) = ("w:github.com/acme/a", "k@x.io", "s1");
        // 230 distinct steps: the cache keeps HISTORY_LEN, the log all of them
        for i in 0..230 {
            note_last_action(&store, scope, user, session, "reading", &format!("src/f{i}.rs"), "claude-code", "", &crate::presence::Effort::default(), "dev", "", "", "");
        }
        // the newest line is still the current one, not in the history yet
        assert_eq!(history(&store, scope, user, session).len(), HISTORY_LEN);
        let (newest, chunk, next, total) = steps(&store, scope, user, session, None, 0.0);
        assert_eq!(total, 229, "229 replaced lines were logged");
        assert_eq!(chunk, 2);
        assert_eq!(newest.len(), 29);
        assert_eq!(newest.last().unwrap()["path"], json!("src/f228.rs"), "oldest first within a chunk");
        assert_eq!(newest[0]["repo_id"], json!("github.com/acme/a"), "each step says its repo");
        let (middle, chunk, next2, _) = steps(&store, scope, user, session, next, 0.0);
        assert_eq!((chunk, middle.len()), (1, STEP_CHUNK));
        assert_eq!(middle[0]["path"], json!("src/f100.rs"));
        let (first, chunk, none, _) = steps(&store, scope, user, session, next2, 0.0);
        assert_eq!((chunk, first.len(), none), (0, STEP_CHUNK, None));
        assert_eq!(first[0]["path"], json!("src/f0.rs"), "back to the session's first step");
        // the plan's window leaves older steps out
        let (windowed, _, _, _) = steps(&store, scope, user, session, None, now() + 1.0);
        assert!(windowed.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        first_sentences, focus_key, reach_of, history, last_action_key, note_exchange, note_last_action, note_messages, report_done, via_view, Step,
        MAX_DONE, MAX_X_BYTES, MAX_X_NAMES,
    };
    use crate::store::{now, Store};
    use serde_json::{json, Value};

    #[test]
    fn a_turns_end_reads_done_with_what_it_said() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let (scope, user, session) = ("ws:repo", "u1", "s1");
        note_last_action(&store, scope, user, session, "running", "cargo test", "claude-code", "opus", &crate::presence::Effort::default(), "dev", "", "", "");
        let _ = store.eph_set(&focus_key(scope, user, session), &json!({"action": "running"}), Some(60.0));
        report_done(&store, scope, user, session, "All green.\n\nThe   **fix** is in. Then a third sentence nobody needs.");
        assert!(store.eph_get(&focus_key(scope, user, session)).is_none(), "nothing is running any more");
        let last = store.eph_get(&last_action_key(scope, user, session)).expect("last action");
        assert_eq!(last["kind"], "done");
        // the first two sentences, line breaks and Markdown kept
        assert_eq!(last["note"], "All green.\n\nThe   **fix** is in.");
        // Stop knows no model: the line before's is kept
        assert_eq!(last["model"], "opus");
        assert_eq!(last["branch"], "dev");
        // with closing_messages off the text arrives empty (privacy.rs): no note
        report_done(&store, scope, user, session, "");
        let last = store.eph_get(&last_action_key(scope, user, session)).expect("last action");
        assert_eq!(last["note"], "");
    }

    #[test]
    fn a_closing_message_keeps_its_first_two_sentences() {
        let two = |text: &str| first_sentences(text, 2, MAX_DONE);
        // decimals, versions and file names never end a sentence
        assert_eq!(two("Pi is 3.14 here. Bumped to v1.2.3 now. Extra."), "Pi is 3.14 here. Bumped to v1.2.3 now.");
        assert_eq!(two("Edited chat.tsx and presence.rs. Tests pass! More to say."), "Edited chat.tsx and presence.rs. Tests pass!");
        // abbreviations and a list's numbers neither
        assert_eq!(two("Kept e.g. the gate and i.e. the lint. Done? Yes."), "Kept e.g. the gate and i.e. the lint. Done?");
        assert_eq!(two("Steps:\n1. Trim it\n2. Ship it. Then more. And more."), "Steps:\n1. Trim it\n2. Ship it. Then more.");
        // nor a period inside backticks, inline or fenced
        assert_eq!(two("Ran `cargo test. --release` fine. Second. Third."), "Ran `cargo test. --release` fine. Second.");
        assert_eq!(two("Look:\n```\nfn a() { x. y }\n```\nIt works. Next. Last."), "Look:\n```\nfn a() { x. y }\n```\nIt works. Next.");
        // closers stay with their sentence
        assert_eq!(two("It **landed.** Pushed (to dev.) Third."), "It **landed.** Pushed (to dev.)");
        // fewer sentences than asked: all of it
        assert_eq!(two("  Only one, no stop  "), "Only one, no stop");
        // a run-on is cut at the cap, on a word, with an ellipsis
        let long = "word ".repeat(400);
        let cut = first_sentences(&long, 2, MAX_DONE);
        assert!(cut.chars().count() <= MAX_DONE + 1, "{}", cut.len());
        assert!(cut.ends_with("word\u{2026}"), "{cut}");
        // and code it opened is closed again
        let open = format!("See `{}", "x ".repeat(400));
        assert!(first_sentences(&open, 2, 50).ends_with("\u{2026}`"));
        let fenced = format!("```\n{}", "y ".repeat(400));
        assert!(first_sentences(&fenced, 2, 50).ends_with("\u{2026}\n```"));
    }

    #[test]
    fn a_steps_exchange_is_bounded_and_moves_into_its_history() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let (scope, user, session) = ("ws:repo", "u1", "s1");
        note_last_action(&store, scope, user, session, "edit", "pkg/a.py", "claude-code", "opus", &crate::presence::Effort::default(), "dev", "", "", "");
        // far too much: forty long names, a long string
        let names: Vec<String> = (0..40).map(|i| format!("{}{i}", "n".repeat(300))).collect();
        let patch = json!({"add": names, "caller_names": names, "sig": [["f", "x".repeat(500), "y".repeat(500)]], "tok": 1200});
        note_exchange(&store, scope, user, session, Step::Current, &patch);
        let x = store.eph_get(&last_action_key(scope, user, session)).unwrap()["x"].clone();
        assert!(serde_json::to_string(&x).unwrap().len() <= MAX_X_BYTES, "{x}");
        assert!(x["add"].as_array().is_none_or(|a| a.len() <= MAX_X_NAMES), "{x}");
        assert_eq!(x["tok"], 1200, "counts survive the shedding");
        // deltas and messages add up across the posts of one step
        note_exchange(&store, scope, user, session, Step::Current, &json!({"deltas": 2, "deltas_from": ["bob@x.com"]}));
        note_messages(&store, scope, user, session, 1, &["ann@x.com".to_string()]);
        note_exchange(&store, scope, user, session, Step::Current, &json!({"deltas": 1, "deltas_from": ["bob@x.com", "cy@x.com"]}));
        let x = store.eph_get(&last_action_key(scope, user, session)).unwrap()["x"].clone();
        assert_eq!((x["deltas"].clone(), x["deltas_from"].clone(), x["msgs"].clone()), (json!(3), json!(["bob@x.com", "cy@x.com"]), json!(1)));
        // the agent moves on: the step and its exchange go to the history,
        // where a late verdict still finds it
        note_last_action(&store, scope, user, session, "reading", "pkg/b.py", "claude-code", "opus", &crate::presence::Effort::default(), "dev", "", "", "");
        note_exchange(&store, scope, user, session, Step::Latest { kind: "edit", path: "pkg/a.py" }, &json!({"tests": "failing", "failed": 2}));
        let earlier = history(&store, scope, user, session);
        assert_eq!(earlier[0]["path"], "pkg/a.py");
        assert_eq!((earlier[0]["x"]["tests"].clone(), earlier[0]["x"]["deltas"].clone()), (json!("failing"), json!(3)));
        // a step that is not there takes nothing
        note_exchange(&store, scope, user, session, Step::Latest { kind: "edit", path: "pkg/zzz.py" }, &json!({"tests": "clear"}));
        assert!(store.eph_get(&last_action_key(scope, user, session)).unwrap().get("x").is_none());
    }

    #[test]
    fn via_view_builds_the_object_python_stores() {
        // the routes' "host path" string becomes {host, path, mcp}
        assert_eq!(
            via_view("api.corkcli.com /presence"),
            json!({"host": "api.corkcli.com", "path": "/presence", "mcp": false})
        );
        assert_eq!(
            via_view("MCP.corkcli.com,proxy /"),
            json!({"host": "mcp.corkcli.com", "path": "/", "mcp": true})
        );
        assert_eq!(
            via_view("api.corkcli.com mcp/"),
            json!({"host": "api.corkcli.com", "path": "/mcp", "mcp": true})
        );
        // the MCP server's bare tag is a path with no host
        assert_eq!(via_view("mcp"), json!({"host": "", "path": "/mcp", "mcp": true}));
        // unknown stays unknown (null)
        assert_eq!(via_view(""), Value::Null);
    }

    #[test]
    fn a_waiting_message_knows_whether_an_open_session_a_chat_app_or_no_one_will_take_it() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        assert_eq!(reach_of(&store, "ws", "carol@x.dev")["kind"], "none");
        // a chat app on the connector: sessionless MCP calls
        note_last_action(&store, "ws:github.com/a/b", "carol@x.dev", "", "working", "", "rmcp 3.4.0", "", &crate::presence::Effort::default(), "", "", "mcp.corkcli.com /", "");
        let reach = reach_of(&store, "ws", "carol@x.dev");
        assert_eq!(reach["kind"], "mcp");
        assert_eq!(reach["agent"], "rmcp 3.4.0");
        // another person whose label starts the same is not them
        assert_eq!(reach_of(&store, "ws", "carol@x.de")["kind"], "none");
        // a Claude Code session with hooks, working now: it hears it at its next step
        note_last_action(&store, "ws:github.com/a/b", "carol@x.dev", "s1", "edit", "a.rs", "claude", "", &crate::presence::Effort::default(), "main", "", "/report", "");
        assert_eq!(reach_of(&store, "ws", "carol@x.dev")["kind"], "hooks");
        // that session ended: back to the chat app
        store.eph_set("sessended:ws:github.com/a/b:s1", &json!({"ts": now()}), Some(60.0)).unwrap();
        assert_eq!(reach_of(&store, "ws", "carol@x.dev")["kind"], "mcp");
    }
}
