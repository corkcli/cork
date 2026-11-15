//! What the last agent in a repo learned, carried to the next one.
//!
//! Agent #2 used to start cold: agent #1 had read the files, found where
//! things live and said what it did, and none of it carried over, so #2 read
//! them all again. At each turn's end, a helper's end and the session's end,
//! Cork keeps the session's trail, built from what it already holds rather
//! than from a new store: the step log (the repo files it read and changed)
//! and the session's title (what it set out to do). Never its closing
//! message: an agent's reply to its person is not a fact about the code, and
//! quoted into another session's briefing it was noise. The next session's
//! start briefing leads with the newest trail of a session that is not at
//! work right now (one that is has its own "right now" line), three lines.
//!
//! A later prompt that means the same task (its vector within SIMILAR_FLOOR
//! of the trail's, kept from the session's own prompt) hears, in two lines,
//! what that agent read and changed, with the line ranges of what it changed:
//! the files a name match never finds for a prompt that names no code.
//!
//! The same record is the slope: a session's discovery tokens, what its
//! messages cost before its first write (the reading and searching it did to
//! find where to work), so the dashboard can show agent #2 spending less than
//! agent #1, beside what Cork saved and cost each session and the
//! coordination it did (messages and changes delivered, writes held back,
//! handoffs).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::{json, Value};

use crate::store::{now, Store};

pub const BUCKET: &str = "trail";
/// How long a trail is offered to the next session at its start.
const OFFER_TTL_S: f64 = 3.0 * 86_400.0;
/// The step log is read this many chunks deep (presence::STEP_CHUNK each).
const MAX_CHUNKS: usize = 20;
const MAX_FILES_KEPT: usize = 12;
const MAX_FILES_SHOWN: usize = 5;
const MAX_LINE: usize = 300;
/// A task this close in meaning is the same kind of task (handoff.rs and
/// examples.rs hold the same floor: bge-small, process sentences removed).
const SIMILAR_FLOOR: f32 = 0.80;
/// How far back an earlier session's similar work is offered.
const SIMILAR_TTL_S: f64 = 7.0 * 86_400.0;
const SIMILAR_FILES: usize = 3;
const SIMILAR_SYMBOLS: usize = 3;

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn strings(v: &Value, k: &str) -> Vec<String> {
    v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

fn key(scope: &str, session: &str) -> String {
    format!("{scope}:{session}")
}

/// A session's steps in this repo, oldest first: its step log, then the step
/// it is on (not logged until the next one replaces it).
fn session_steps(store: &Store, scope: &str, user: &str, session: &str) -> Vec<Value> {
    let repo = crate::presence::split_scope(scope).1;
    let mut chunks: Vec<Vec<Value>> = Vec::new();
    let mut before: Option<i64> = None;
    for _ in 0..MAX_CHUNKS {
        let (items, _chunk, next, _total) = crate::presence::steps(store, scope, user, session, before, 0.0);
        chunks.push(items);
        match next {
            Some(chunk) => before = Some(chunk),
            None => break,
        }
    }
    let mut steps: Vec<Value> = chunks.into_iter().rev().flatten().collect();
    if let Some(last) = store.eph_get(&crate::presence::last_action_key(scope, user, session)) {
        steps.push(last);
    }
    // a session that works across checkouts: only what it did in this one
    steps.retain(|step| {
        let at = text(step, "repo_id");
        at.is_empty() || at == repo
    });
    steps.sort_by(|a, b| {
        let at = |s: &Value| s.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        at(a).partial_cmp(&at(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    steps
}

/// What a session's steps add up to.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Summary {
    pub started: f64,
    pub first_write: Option<f64>,
    /// What the session's own messages cost (input, output and cache tokens,
    /// each message once) before its first write; all of them while it has
    /// not written.
    pub discovery_tokens: i64,
    pub discovery_messages: i64,
    /// Files read, most-read first, and how often.
    pub reads: Vec<(String, i64)>,
    pub edits: Vec<String>,
    pub searches: i64,
    /// Teammates' messages and changes delivered into the session.
    pub messages_in: i64,
    pub deltas_in: i64,
}

/// A path relative to the repo (the hooks report a file in it that way).
fn in_repo(path: &str) -> bool {
    !(path.starts_with('/') || path.starts_with('~') || path.starts_with("..") || path.contains(":\\") || path.contains(":/"))
}

/// The steps, oldest first, as a [`Summary`]. A subagent's steps count as
/// the session's reads and writes, but not as its discovery tokens: their
/// cost is the subagent's own context, billed apart.
pub fn summarize(steps: &[Value]) -> Summary {
    let mut out = Summary { started: f64::MAX, ..Summary::default() };
    let mut counted: HashSet<String> = HashSet::new();
    let mut reads: Vec<(String, i64)> = Vec::new();
    for step in steps {
        let ts = step.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        if ts > 0.0 {
            out.started = out.started.min(ts);
        }
        let kind = text(step, "kind");
        let path = text(step, "path");
        let x = step.get("x").cloned().unwrap_or(Value::Null);
        let n = |k: &str| x.get(k).and_then(Value::as_i64).unwrap_or(0).max(0);
        out.messages_in += n("msgs");
        out.deltas_in += n("deltas");
        let writes = matches!(kind.as_str(), "edit" | "draft");
        if writes && out.first_write.is_none() {
            out.first_write = Some(ts);
        }
        // discovery: every message before the first write's, once each
        // (several tool calls of one message carry the same id)
        let own = text(step, "worker").is_empty();
        if out.first_write.is_none() && own && n("stok") > 0 {
            let message = text(&x, "smsg");
            if message.is_empty() || counted.insert(message) {
                out.discovery_tokens += n("stok");
                out.discovery_messages += 1;
            }
        }
        // a repo file, never one outside the checkout (a screenshot the
        // session took, a scratch file): those are not where things live here
        if path.is_empty() || crate::codegraph::is_collide_artifact(&path) || !in_repo(&path) {
            if kind == "searching" {
                out.searches += 1;
            }
            continue;
        }
        match kind.as_str() {
            "reading" => match reads.iter_mut().find(|(p, _)| *p == path) {
                Some(slot) => slot.1 += 1,
                None => reads.push((path, 1)),
            },
            "searching" => out.searches += 1,
            _ if writes && !out.edits.contains(&path) => out.edits.push(path),
            _ => {}
        }
    }
    if out.started == f64::MAX {
        out.started = 0.0;
    }
    // most-read first; ties keep the order they were first read in
    reads.sort_by(|a, b| b.1.cmp(&a.1));
    out.reads = reads;
    out
}

fn cost_key(scope: &str, session: &str) -> String {
    format!("trailcost:{scope}:{session}")
}

/// Message ids a session's live discovery remembers, to count each once.
const SEEN_IDS: usize = 16;

/// One step's cost (`stok`, `smsg`, as presence::step_usage gives it) into
/// the session's discovery, each message once, until its first write
/// (`write`), which only marks where discovery ended. Counted as the steps
/// arrive because the step log keeps one of several alike in a row (a file
/// read range by range), and each of those messages cost too. A subagent's
/// steps are its own context, billed apart; its write is the session's.
pub fn note_cost(store: &Store, scope: &str, session: &str, worker: &str, cost: &Value, write: bool) {
    if session.is_empty() || (!worker.is_empty() && !write) {
        return;
    }
    let key = cost_key(scope, session);
    let mut live = store.eph_get(&key).unwrap_or_else(|| json!({"tokens": 0, "messages": 0, "seen": []}));
    if live.get("first_write").is_some() {
        return;
    }
    if write {
        live["first_write"] = json!(now());
    } else {
        let tokens = cost.get("stok").and_then(Value::as_i64).unwrap_or(0).max(0);
        let message = text(cost, "smsg");
        let mut seen = strings(&live, "seen");
        if tokens == 0 || (!message.is_empty() && seen.contains(&message)) {
            return;
        }
        live["tokens"] = json!(live.get("tokens").and_then(Value::as_i64).unwrap_or(0) + tokens);
        live["messages"] = json!(live.get("messages").and_then(Value::as_i64).unwrap_or(0) + 1);
        if !message.is_empty() {
            seen.push(message);
            let keep = seen.len().saturating_sub(SEEN_IDS);
            live["seen"] = json!(seen.split_off(keep));
        }
    }
    let _ = store.eph_set(&key, &live, Some(7.0 * 86_400.0));
}

/// Keep the session's trail, from its step log. Called at each turn's end,
/// a helper's end and the session's end (`ended`). None when it took no
/// step here.
pub fn record(store: &Store, scope: &str, user: &str, session: &str, ended: bool) -> Option<Value> {
    if session.is_empty() {
        return None;
    }
    let steps = session_steps(store, scope, user, session);
    if steps.is_empty() {
        return None;
    }
    let mut s = summarize(&steps);
    // the live count, which also has the steps the log folded together; the
    // log's, for a session that began before the live count did
    if let Some(live) = store.eph_get(&cost_key(scope, session)) {
        let n = |k: &str| live.get(k).and_then(Value::as_i64).unwrap_or(0);
        if n("tokens") > s.discovery_tokens {
            (s.discovery_tokens, s.discovery_messages) = (n("tokens"), n("messages"));
        }
        if s.first_write.is_none() {
            s.first_write = live.get("first_write").and_then(Value::as_f64);
        }
    }
    let stamp = now();
    let old = store.kv_get(BUCKET, &key(scope, session)).unwrap_or(Value::Null);
    let mut record = json!({
        "user": user, "session": session,
        "started": if s.started > 0.0 { s.started } else { stamp },
        "updated": stamp,
        "ended": ended || old.get("ended").and_then(Value::as_bool).unwrap_or(false),
        "title": crate::freshness::title_of(store, scope, user, session),
        "reads": s.reads.iter().take(MAX_FILES_KEPT).map(|(p, n)| json!([p, n])).collect::<Vec<_>>(),
        "files_read": s.reads.len(),
        "edits": s.edits.iter().take(MAX_FILES_KEPT).collect::<Vec<_>>(),
        "searches": s.searches,
        "messages_in": s.messages_in, "deltas_in": s.deltas_in,
        "discovery_tokens": s.discovery_tokens, "discovery_messages": s.discovery_messages,
    });
    if let Some(at) = s.first_write {
        record["first_write"] = json!(at);
    }
    if text(&record, "title").is_empty() {
        record["title"] = json!(text(&old, "title"));
    }
    // the session's task as a vector (its newest prompt, process sentences
    // removed: examples::start_task), for a later prompt that means the same;
    // empty without embeddings, and the prompt's words are never kept
    let q = store.eph_get(&format!("extask:{scope}:{session}")).map(|t| text(&t, "q")).filter(|q| !q.is_empty());
    record["q"] = json!(q.unwrap_or_else(|| text(&old, "q")));
    let _ = store.kv_put(BUCKET, &key(scope, session), &record, stamp);
    Some(record)
}

/// "Map from the last agent here": the newest trail another session left in
/// this repo, as three briefing lines at most, or none: where things live
/// (the files it read and changed), never what it said. A session at work
/// right now (its focus marker is up, mid-turn) is skipped: its trail is
/// its last turn's, and the "right now" line already says where it is.
pub fn brief_lines(store: &Store, scope: &str, viewer: &str, session: &str) -> Vec<String> {
    let stamp = now();
    let newest = store
        .kv_list_since(BUCKET, &format!("{scope}:"), stamp - OFFER_TTL_S)
        .into_iter()
        .map(|(_, record, _)| record)
        .filter(|r| (session.is_empty() || text(r, "session") != session) && (!strings(r, "edits").is_empty() || r.get("reads").and_then(Value::as_array).is_some_and(|a| !a.is_empty())))
        .filter(|r| store.eph_get(&crate::presence::focus_key(scope, &text(r, "user"), &text(r, "session"))).is_none())
        .max_by(|a, b| {
            let at = |r: &Value| r.get("updated").and_then(Value::as_f64).unwrap_or(0.0);
            at(a).partial_cmp(&at(b)).unwrap_or(std::cmp::Ordering::Equal)
        });
    let Some(trail) = newest else { return Vec::new() };
    render(store, viewer, &trail, stamp)
}

/// One trail as briefing lines (see `brief_lines`).
pub fn render(store: &Store, viewer: &str, trail: &Value, stamp: f64) -> Vec<String> {
    let user = text(trail, "user");
    let who = if user == viewer { "your earlier session".to_string() } else { format!("{}'s session", crate::inflight::author_label(store, viewer, &user)) };
    let title = text(trail, "title");
    let on = if title.is_empty() { String::new() } else { format!(" \"{}\"", title.chars().take(80).collect::<String>()) };
    let age = |k: &str| crate::deltas::age_text(stamp - trail.get(k).and_then(Value::as_f64).unwrap_or(stamp));
    let last = if trail.get("ended").and_then(Value::as_bool).unwrap_or(false) { "ended" } else { "last step" };
    let mut lines = vec![format!("Map from the last agent here — {who}{on} (started {}, {last} {}):", age("started"), age("updated"))];
    let edits = strings(trail, "edits");
    let reads: Vec<String> = trail
        .get("reads")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get(0).and_then(Value::as_str).map(str::to_string))
        .filter(|p| !edits.contains(p))
        .collect();
    let total = trail.get("files_read").and_then(Value::as_u64).unwrap_or(reads.len() as u64) as usize;
    if !reads.is_empty() {
        let shown: Vec<&str> = reads.iter().take(MAX_FILES_SHOWN).map(String::as_str).collect();
        let more = total.saturating_sub(shown.len() + edits.iter().filter(|e| trail_read(trail, e)).count());
        let tail = if more > 0 { format!(" (+{more} more)") } else { String::new() };
        lines.push(clip_line(&format!("  read: {}{tail}", shown.join(", "))));
    }
    if !edits.is_empty() {
        let shown: Vec<&str> = edits.iter().take(MAX_FILES_SHOWN).map(String::as_str).collect();
        let tail = if edits.len() > shown.len() { format!(" (+{} more)", edits.len() - shown.len()) } else { String::new() };
        lines.push(clip_line(&format!("  changed: {}{tail}", shown.join(", "))));
    }
    if lines.len() == 1 {
        return Vec::new();
    }
    lines
}

fn trail_read(trail: &Value, path: &str) -> bool {
    trail.get("reads").and_then(Value::as_array).is_some_and(|a| a.iter().any(|r| r.get(0).and_then(Value::as_str) == Some(path)))
}

fn clip_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE {
        return line.to_string();
    }
    format!("{}…", line.chars().take(MAX_LINE - 1).collect::<String>().trim_end())
}

/// An earlier session's similar work, as the prompt briefing says it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Similar {
    /// Two lines at most.
    pub lines: Vec<String>,
    /// The files named, for the hook's tally: a write to one is a search saved.
    pub paths: Vec<String>,
    pub from: String,
    /// Its `name [a-b]` ranges, for the hook to check against the reader's
    /// copy (brief.rs `checks`): a range from another checkout is not served.
    pub checks: Vec<Value>,
}

/// "An agent did similar work here (3h ago): it read X / and changed Y
/// (symbol [a-b])": the closest earlier session in this repo whose task
/// means the same as this prompt's (`q`, cosine at least SIMILAR_FLOOR),
/// once per session and earlier session, with what the briefing already
/// lists (`listed`) left out. None without a vector (names-only builds, a
/// plan without meaning match), a match, or anything left to name. Like a
/// handoff, a teammate's is offered only when the plan shares knowledge.
pub fn similar_work(
    store: &Store, scope: &str, viewer: &str, session: &str, q: &[f32], listed: &BTreeSet<String>, share_knowledge: bool,
) -> Option<Similar> {
    if q.is_empty() || session.is_empty() {
        return None;
    }
    let stamp = now();
    let told = |from: &str| format!("trailtold:{scope}:{session}:{from}");
    let (_, trail) = store
        .kv_list_since(BUCKET, &format!("{scope}:"), stamp - SIMILAR_TTL_S)
        .into_iter()
        .map(|(_, record, _)| record)
        .filter(|r| {
            let from = text(r, "session");
            from != session
                && (share_knowledge || text(r, "user") == viewer)
                && !strings(r, "edits").is_empty()
                && store.eph_get(&told(&from)).is_none()
        })
        .filter_map(|r| {
            let theirs = crate::embed::decode_vec(&text(&r, "q"));
            let close = if theirs.is_empty() { 0.0 } else { crate::embed::cosine(q, &theirs) };
            (close >= SIMILAR_FLOOR).then_some((close, r))
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))?;
    let from = text(&trail, "session");
    let edits = strings(&trail, "edits");
    // what it changed, each file with where its changed symbols are now
    let changed_names = changed_symbols(store, scope, &trail);
    let mut refs: Vec<Value> = Vec::new();
    let changed: Vec<(String, String)> = edits
        .iter()
        .filter(|p| !listed.contains(*p))
        .take(SIMILAR_FILES)
        .map(|path| {
            let map = crate::codegraph::symbols_of(store, scope, path);
            let at: Vec<String> = changed_names
                .get(path)
                .into_iter()
                .flatten()
                .filter_map(|name| map.get(name).map(|entry| (name, crate::brief::span_text(entry))))
                .take(SIMILAR_SYMBOLS)
                .map(|(name, span)| {
                    refs.push(json!([path, name]));
                    format!("{name}{span}")
                })
                .collect();
            let shown = if at.is_empty() { path.clone() } else { format!("{path} ({})", at.join(", ")) };
            (path.clone(), shown)
        })
        .collect();
    let reads: Vec<String> = trail
        .get("reads")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get(0).and_then(Value::as_str).map(str::to_string))
        .filter(|p| !edits.contains(p) && !listed.contains(p))
        .take(SIMILAR_FILES)
        .collect();
    if changed.is_empty() && reads.is_empty() {
        return None;
    }
    let age = crate::deltas::age_text(stamp - trail.get("updated").and_then(Value::as_f64).unwrap_or(stamp));
    let head = format!("An agent did similar work here ({age}):");
    let changed_text = changed.iter().map(|(_, shown)| shown.as_str()).collect::<Vec<_>>().join(", ");
    let lines = match (reads.is_empty(), changed.is_empty()) {
        (false, false) => vec![format!("{head} it read {}", reads.join(", ")), format!("  and changed {changed_text}.")],
        (false, true) => vec![format!("{head} it read {}.", reads.join(", "))],
        _ => vec![format!("{head} it changed {changed_text}.")],
    };
    let mut paths = reads;
    paths.extend(changed.into_iter().map(|(path, _)| path));
    let _ = store.eph_set(&told(&from), &json!({"ts": stamp}), Some(SIMILAR_TTL_S));
    // the slope counts it: discovery this session did not have to do
    store.ledger_append_later(scope, "trail_shown", &json!({"user": viewer, "session": session, "from": from, "files": paths.len()}), stamp);
    let lines: Vec<String> = lines.iter().map(|l| clip_line(l)).collect();
    let checks = lines
        .iter()
        .filter(|line| line.contains(" ["))
        .map(|line| json!({"kind": "spans", "line": line, "refs": refs}))
        .collect();
    Some(Similar { lines, paths, from, checks })
}

/// The symbols a session's writes changed, per file, from its reports.
fn changed_symbols(store: &Store, scope: &str, trail: &Value) -> BTreeMap<String, Vec<String>> {
    let session = text(trail, "session");
    let since = trail.get("started").and_then(Value::as_f64).unwrap_or(0.0) - 60.0;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in store.ledger_since_kinds(scope, since, &["edit_reported"]) {
        let p = &row.payload;
        if text(p, "session") != session {
            continue;
        }
        let mut names: Vec<String> = p.get("symbols_changed").and_then(Value::as_object).map(|m| m.keys().cloned().collect()).unwrap_or_default();
        names.extend(p.get("events").and_then(Value::as_array).into_iter().flatten().map(|e| text(e, "symbol")));
        let slot = out.entry(text(p, "path")).or_default();
        for name in names.into_iter().filter(|n| !n.is_empty()) {
            if !slot.contains(&name) {
                slot.push(name);
            }
        }
    }
    out
}

/// What one session did for the slope chart and its row: discovery, net
/// tokens and coordination.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tally {
    pub saved_tokens: i64,
    pub cost_tokens: i64,
    pub caught: i64,
    pub handoffs: i64,
    /// Earlier sessions' similar work handed to it (`similar_work`).
    pub maps: i64,
}

impl Tally {
    /// What Cork saved the session less what its own text cost to carry:
    /// negative when the briefings cost more than they saved.
    pub fn net(&self) -> i64 {
        self.saved_tokens - self.cost_tokens
    }
}

/// Each session's tally from the repo's ledger rows: savings and Cork's own
/// cost (briefstat's rows), writes held back (`gate_block`, and the traffic
/// and claim rows), handoffs given or taken.
pub fn tallies(rows: &[crate::store::LedgerRow]) -> BTreeMap<String, Tally> {
    let mut out: BTreeMap<String, Tally> = BTreeMap::new();
    for row in rows {
        let session = text(&row.payload, "session");
        if session.is_empty() {
            continue;
        }
        let slot = out.entry(session).or_default();
        if let Some(worth) = crate::briefstat::row_saving(&row.kind, &row.payload) {
            slot.saved_tokens += worth.tokens_saved as i64;
            slot.cost_tokens += worth.cost_tokens as i64;
        }
        match row.kind.as_str() {
            "gate_block" | "collision_prevented" | "claim_prevented" => slot.caught += 1,
            "handoff_shown" | "handoff_saved" => slot.handoffs += 1,
            "trail_shown" => slot.maps += 1,
            _ => {}
        }
    }
    out
}

/// The slope on one repo: every session that started in the last `days`, in
/// start order, each with its discovery tokens, its net and its
/// coordination. The dashboard's chart puts agent #1, #2, … on x.
pub fn slope(store: &Store, scope: &str, days: f64) -> Value {
    let stamp = now();
    let since = stamp - days.max(0.02) * 86_400.0;
    let mut trails: Vec<Value> = store
        .kv_list_since(BUCKET, &format!("{scope}:"), since)
        .into_iter()
        .map(|(_, record, _)| record)
        .filter(|r| r.get("started").and_then(Value::as_f64).unwrap_or(0.0) >= since)
        .collect();
    trails.sort_by(|a, b| {
        let at = |r: &Value| r.get("started").and_then(Value::as_f64).unwrap_or(0.0);
        at(a).partial_cmp(&at(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    let rows = store.ledger_since(scope, since);
    let tallies = tallies(&rows);
    let sessions: Vec<Value> = trails
        .iter()
        .enumerate()
        .map(|(index, t)| {
            let session = text(t, "session");
            let tally = tallies.get(&session).cloned().unwrap_or_default();
            json!({
                "n": index + 1, "session": session, "user": text(t, "user"), "title": text(t, "title"),
                "started": t.get("started").cloned().unwrap_or(Value::Null),
                "first_write": t.get("first_write").cloned().unwrap_or(Value::Null),
                "wrote": t.get("first_write").is_some(),
                "ended": t.get("ended").and_then(Value::as_bool).unwrap_or(false),
                "discovery_tokens": t.get("discovery_tokens").and_then(Value::as_i64).unwrap_or(0),
                "discovery_messages": t.get("discovery_messages").and_then(Value::as_i64).unwrap_or(0),
                "files_read": t.get("files_read").and_then(Value::as_i64).unwrap_or(0),
                "searches": t.get("searches").and_then(Value::as_i64).unwrap_or(0),
                "saved_tokens": tally.saved_tokens, "cost_tokens": tally.cost_tokens, "net_tokens": tally.net(),
                "messages_in": t.get("messages_in").and_then(Value::as_i64).unwrap_or(0),
                "deltas_in": t.get("deltas_in").and_then(Value::as_i64).unwrap_or(0),
                "caught": tally.caught, "handoffs": tally.handoffs, "maps": tally.maps,
            })
        })
        .collect();
    json!({"ok": true, "days": days, "sessions": sessions, "how": HOW})
}

/// What the slope's numbers mean, printed under the chart.
pub const HOW: &str = "Discovery is what a session's own messages cost (input, output and cache tokens, each message once) before its first write: the reading and searching it did to find where to work. A subagent's is billed apart and not counted here. Net is what Cork saved the session (messages and reads it did not need) less what Cork's own text cost to carry, and it can be negative. Coordination is teammates' messages and changes delivered into the session, writes held back from a collision, handoffs given or taken, and an earlier agent's similar work handed to it.";

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPE: &str = "w:github.com/acme/app";

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn step(kind: &str, path: &str, ts: f64, stok: i64, smsg: &str) -> Value {
        let mut s = json!({"kind": kind, "path": path, "ts": ts, "repo_id": "github.com/acme/app"});
        if stok > 0 {
            s["x"] = json!({"stok": stok, "smsg": smsg});
        }
        s
    }

    #[test]
    fn discovery_is_every_message_before_the_first_write_each_once() {
        let mut steps = vec![
            step("reading", "src/db.rs", 10.0, 30_000, "m1"),
            // a second tool call of the same message: counted once
            step("reading", "src/api.rs", 11.0, 30_000, "m1"),
            step("searching", "create_post", 12.0, 34_000, "m2"),
            step("reading", "src/db.rs", 13.0, 41_000, "m3"),
        ];
        // a subagent's read: its own context, not the session's discovery
        let mut helper = step("reading", "docs/API.md", 14.0, 90_000, "w1");
        helper["worker"] = json!("a1");
        steps.push(helper);
        steps.push(step("edit", "src/api.rs", 20.0, 45_000, "m4"));
        steps.push(step("reading", "src/lib.rs", 21.0, 47_000, "m5"));
        let s = summarize(&steps);
        assert_eq!((s.discovery_tokens, s.discovery_messages), (105_000, 3));
        assert_eq!(s.first_write, Some(20.0));
        assert_eq!(s.started, 10.0);
        assert_eq!(s.reads[0], ("src/db.rs".to_string(), 2), "most-read first");
        assert_eq!(s.reads.len(), 4);
        assert_eq!(s.edits, vec!["src/api.rs".to_string()]);
        assert_eq!(s.searches, 1);
        // a session that never wrote: all of it was discovery
        let s = summarize(&steps[..4]);
        assert_eq!((s.discovery_tokens, s.first_write), (105_000, None));
    }

    #[test]
    fn a_file_read_range_by_range_costs_every_message_the_log_folded() {
        let store = store();
        let cost = |stok: i64, smsg: &str| json!({"stok": stok, "smsg": smsg});
        // three ranged reads of one file in a row: the log keeps one step
        walk(&store, "ann@x.io", "s1", &[("reading", "src/api.rs", 30_000, "m1"), ("reading", "src/api.rs", 31_000, "m2"), ("reading", "src/api.rs", 32_000, "m3")]);
        for (stok, smsg) in [(30_000, "m1"), (31_000, "m2"), (31_000, "m2"), (32_000, "m3")] {
            note_cost(&store, SCOPE, "s1", "", &cost(stok, smsg), false);
        }
        // a helper's step is its own context
        note_cost(&store, SCOPE, "s1", "a1", &cost(90_000, "w1"), false);
        note_cost(&store, SCOPE, "s1", "", &json!({}), true);
        // past the first write nothing more is discovery
        note_cost(&store, SCOPE, "s1", "", &cost(40_000, "m5"), false);
        let kept = record(&store, SCOPE, "ann@x.io", "s1", false).unwrap();
        assert_eq!((kept["discovery_tokens"].clone(), kept["discovery_messages"].clone()), (json!(93_000), json!(3)));
        assert!(kept.get("first_write").is_some());
    }

    /// Stand-in vectors: the real model is not needed to say two tasks are
    /// close. The free build's stand-in decodes nothing, so it never matches.
    #[cfg(feature = "cloud")]
    #[test]
    fn a_prompt_that_means_an_earlier_task_hears_what_that_agent_read_and_changed() {
        let store = store();
        let api = "\"\"\"Posts API.\"\"\"\n\n\ndef create_post(body):\n    \"\"\"Stores a post.\"\"\"\n    return body\n\n\ndef list_posts():\n    return []\n";
        crate::report::observe_batch(&store, SCOPE, &[("src/api.py", api, None)], 0.0);
        walk(&store, "ann@x.io", "s1", &[
            ("reading", "src/db.py", 30_000, "m1"), ("reading", "docs/API.md", 31_000, "m2"), ("reading", "src/api.py", 32_000, "m3"),
            ("edit", "src/api.py", 40_000, "m4"),
        ]);
        store.ledger_append(SCOPE, "edit_reported", &json!({"session": "s1", "user": "ann@x.io", "path": "src/api.py",
            "symbols_changed": {"create_post": {}}}), now()).unwrap();
        let _ = store.eph_set(&format!("extask:{SCOPE}:s1"), &json!({"q": crate::embed::encode_vec(&[1.0, 0.0, 0.0])}), None);
        let kept = record(&store, SCOPE, "ann@x.io", "s1", false).unwrap();
        assert!(!text(&kept, "q").is_empty(), "the task vector is kept on the trail");
        // names only (no vector): nothing, as in the free build
        assert_eq!(similar_work(&store, SCOPE, "bob@x.io", "s2", &[], &BTreeSet::new(), true), None);
        // another task altogether
        assert_eq!(similar_work(&store, SCOPE, "bob@x.io", "s2", &[0.0, 1.0, 0.0], &BTreeSet::new(), true), None);
        // the same task, said another way (cosine 0.95)
        let close = [0.95, 0.31, 0.0];
        let similar = similar_work(&store, SCOPE, "bob@x.io", "s2", &close, &BTreeSet::new(), true).unwrap();
        assert_eq!(similar.lines, vec![
            "An agent did similar work here (just now): it read src/db.py, docs/API.md".to_string(),
            "  and changed src/api.py (create_post [4-6]).".to_string(),
        ]);
        assert_eq!(similar.paths, vec!["src/db.py", "docs/API.md", "src/api.py"]);
        // once a session
        assert_eq!(similar_work(&store, SCOPE, "bob@x.io", "s2", &close, &BTreeSet::new(), true), None);
        // what the briefing already lists is not said again
        let listed: BTreeSet<String> = ["src/api.py".to_string(), "src/db.py".to_string()].into();
        let rest = similar_work(&store, SCOPE, "bob@x.io", "s3", &close, &listed, true).unwrap();
        assert_eq!(rest.lines, vec!["An agent did similar work here (just now): it read docs/API.md.".to_string()]);
        // a teammate's task, without shared knowledge: not offered
        assert_eq!(similar_work(&store, SCOPE, "bob@x.io", "s4", &close, &BTreeSet::new(), false), None);
        // and the slope counts it for the session it reached
        store.flush();
        let counted = tallies(&store.ledger_since(SCOPE, 0.0));
        assert_eq!((counted["s2"].maps, counted["s3"].maps), (1, 1));
    }

    #[test]
    fn net_is_what_cork_saved_less_what_it_cost_and_can_be_negative() {
        let row = |kind: &str, payload: Value| crate::store::LedgerRow { seq: 0, ts: 1.0, kind: kind.to_string(), payload };
        let rows = vec![
            row("brief_outcome", json!({"session": "s1", "tokens_saved": 4_000, "brief_cost_tokens": 9_000})),
            row("map_answered", json!({"session": "s1", "tokens_saved": 2_000})),
            row("gate_block", json!({"session": "s1", "path": "a.py"})),
            row("handoff_shown", json!({"session": "s1", "from": "s0"})),
            row("brief_outcome", json!({"session": "s2", "tokens_saved": 50_000, "brief_cost_tokens": 3_000})),
        ];
        let t = tallies(&rows);
        assert_eq!(t["s1"].net(), -3_000);
        assert_eq!((t["s1"].caught, t["s1"].handoffs), (1, 1));
        assert_eq!(t["s2"].net(), 47_000);
    }

    fn walk(store: &Store, user: &str, session: &str, steps: &[(&str, &str, i64, &str)]) {
        for (kind, path, stok, smsg) in steps {
            crate::presence::note_last_action(store, SCOPE, user, session, kind, path, "claude-code", "", &crate::presence::Effort::default(), "", "", "", "");
            if *stok > 0 {
                crate::presence::note_exchange(store, SCOPE, user, session, crate::presence::Step::Current, &json!({"stok": stok, "smsg": smsg}));
            }
        }
    }

    #[test]
    fn the_next_session_starts_with_the_last_agents_map() {
        let store = store();
        walk(&store, "ann@x.io", "s1", &[
            ("reading", "src/db.rs", 30_000, "m1"), ("reading", "src/api.rs", 32_000, "m2"), ("reading", "src/db.rs", 35_000, "m3"),
            ("reading", "API.md", 36_000, "m4"),
            // a screenshot it took is not where things live in the repo
            ("reading", "/private/tmp/scratch/space-01.png", 37_000, "m5"),
            ("edit", "src/api.rs", 40_000, "m6"),
        ]);
        let kept = record(&store, SCOPE, "ann@x.io", "s1", false).unwrap();
        assert_eq!(kept["discovery_tokens"], json!(170_000));
        assert!(kept.get("first_write").is_some());
        // mid-turn (its focus marker up), it is at work: the "right now" line says where
        let _ = store.eph_set(&crate::presence::focus_key(SCOPE, "ann@x.io", "s1"), &json!({"path": "src/api.rs"}), Some(60.0));
        assert!(brief_lines(&store, SCOPE, "bob@x.io", "s2").is_empty());
        store.eph_delete(&crate::presence::focus_key(SCOPE, "ann@x.io", "s1"));
        // its turn over: a teammate's next session, any person in the workspace
        let lines = brief_lines(&store, SCOPE, "bob@x.io", "s2");
        assert!(lines[0].starts_with("Map from the last agent here — ann@x.io's session (started "), "{lines:?}");
        assert_eq!(lines[1..], ["  read: src/db.rs, API.md", "  changed: src/api.rs"], "where things live, never what it said");
        // never a session's own trail
        assert!(brief_lines(&store, SCOPE, "ann@x.io", "s1").is_empty());
        let _ = record(&store, SCOPE, "ann@x.io", "s1", true).unwrap();
        assert!(brief_lines(&store, SCOPE, "bob@x.io", "s2")[0].contains(", ended "));
    }

    #[test]
    fn the_slope_lists_sessions_in_start_order_with_discovery_and_net() {
        let store = store();
        walk(&store, "ann@x.io", "s1", &[("reading", "a.py", 30_000, "m1"), ("reading", "b.py", 40_000, "m2"), ("edit", "a.py", 50_000, "m3")]);
        record(&store, SCOPE, "ann@x.io", "s1", false);
        walk(&store, "bob@x.io", "s2", &[("reading", "a.py", 30_000, "n1"), ("edit", "a.py", 35_000, "n2")]);
        record(&store, SCOPE, "bob@x.io", "s2", false);
        let _ = store.ledger_append(SCOPE, "brief_outcome", &json!({"session": "s2", "tokens_saved": 20_000, "brief_cost_tokens": 2_000}), now());
        let out = slope(&store, SCOPE, 7.0);
        let sessions = out["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!((sessions[0]["n"].clone(), sessions[0]["discovery_tokens"].clone()), (json!(1), json!(70_000)));
        assert_eq!((sessions[1]["n"].clone(), sessions[1]["discovery_tokens"].clone()), (json!(2), json!(30_000)));
        assert_eq!(sessions[1]["net_tokens"], json!(18_000));
        assert_eq!(sessions[0]["net_tokens"], json!(0));
    }
}
