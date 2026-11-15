//! The Overview's Tree: where everyone is, file by file and function by
//! function.
//!
//! The founder's ask (2026-11-13): every file and function an agent is in,
//! named and never quoted, with the faces of the people whose agents are
//! there, what each one touches and on which lines. One read folds the live
//! stores into folders, files and rows. A row is a symbol, a run of lines
//! (in a file no parser reads, a stylesheet, or between a file's symbols),
//! or the whole file.
//!
//! Each agent on a row is in one state, strongest first:
//!
//! - **editing**: a protocol lease on the symbol's key (the gate leases what
//!   a write will change, and the daemon lets go ten minutes after its last
//!   touch), a draft report naming it, the gate's "editing" marker on the
//!   file, or a landed edit in the last three minutes by a session still
//!   live;
//! - **blocked**: Cork held this agent's write back here (a collision on the
//!   feed);
//! - **changed**: an `edit_reported` row in the ledger's last hour. `pushed`
//!   is false while the protocol's head for the key is not promoted and no
//!   promotion by that agent followed the edit: the change has not reached
//!   origin. Unknown (null) where the protocol has no key for it;
//! - **claimed**: an open intent naming the path (and its symbols);
//! - **reading**: a live focus marker on the file. Readers sit on the file,
//!   never on a row.
//!
//! A row is a **collision** when two agents contend for it and one of them
//! is at it now (editing, or held back), or when Cork recorded a collision
//! on it in the last fifteen minutes. A change counts as contending until it
//! is pushed.
//!
//! Names only, never code: paths, symbol names, line numbers and one line of
//! the agents' own words (a lease's intent, an intent's summary, an edit's
//! rationale, the session's title). Hunks are ranges (diff.rs) and nothing
//! here reads a file. A viewer's answer loses the notes, as every dashboard
//! read does (`access::redact_for_viewer`).
//!
//! Polled with the roster, so every read is ephemeral, in memory (the
//! protocol's engine) or the ledger's in-memory tail, and every list is
//! capped.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::graphview::Snapshot;
use crate::presence::{agent_id, split_scope};
use crate::store::{now, Store};

/// How far back a landed edit still shows as changed: the ledger's
/// in-memory tail, less a minute, so the read never goes to disk.
pub const CHANGED_WINDOW_S: f64 = crate::store::TAIL_WINDOW_S - 60.0;
/// A landed edit this recent, by a session still live, is editing now:
/// where the protocol is off, the only word for it.
pub const RECENT_EDIT_S: f64 = 180.0;
/// How long a collision Cork recorded keeps its row yellow, and how long an
/// unpushed change of unknown fate still contends with a live edit.
pub const COLLISION_WINDOW_S: f64 = 900.0;
/// The most files one answer lists, rows one file lists, and paths one
/// intent claims rows on.
const MAX_FILES: usize = 60;
const MAX_ROWS: usize = 30;
const MAX_INTENT_PATHS: usize = 12;
/// A note is one line.
const MAX_NOTE: usize = 160;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn ts_of(value: &Value) -> f64 {
    value.get("ts").and_then(Value::as_f64).unwrap_or(0.0)
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// One line, at most [`MAX_NOTE`] characters, its first line only.
fn one_line(note: &str) -> String {
    let line = note.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut out: String = line.chars().take(MAX_NOTE).collect();
    if line.chars().count() > MAX_NOTE {
        out.push('…');
    }
    out
}

/// What an agent is doing on a row, weakest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Reading,
    Claimed,
    Changed,
    Blocked,
    Editing,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Reading => "reading",
            State::Claimed => "claimed",
            State::Changed => "changed",
            State::Blocked => "blocked",
            State::Editing => "editing",
        }
    }

    /// At it right now.
    fn live(self) -> bool {
        matches!(self, State::Blocked | State::Editing)
    }
}

// ------------------------------------------------------------- protocol

/// What the real-time protocol knows of one repo right now: who holds a
/// lease on which key, and whether each key's newest version is promoted
/// (pushed by its author).
#[derive(Debug, Default)]
pub struct Protocol {
    pub leases: Vec<Lease>,
    /// key -> every version promoted
    pub heads: BTreeMap<String, bool>,
}

#[derive(Debug, Clone)]
pub struct Lease {
    pub key: String,
    pub user: String,
    pub session: String,
    /// the holder's line of why, when it gave one
    pub intent: String,
}

impl Protocol {
    /// From the hub's dashboard read (`protocol_ws::Hub::view`). A holder is
    /// named as reports name it (`user#session`), the hook's side session
    /// (`<session>.d<pid>`) taken back to the agent's own.
    pub fn from_view(view: &Value) -> Protocol {
        let leases = view
            .get("leases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|lease| {
                let holder = lease.get("holder")?;
                let named = text(holder, "agent_id");
                let (user, session) = match named.split_once('#') {
                    Some((user, session)) => (user.to_string(), session.to_string()),
                    None => (text(holder, "person"), text(holder, "session")),
                };
                let key = text(lease, "key");
                if user.is_empty() || key.is_empty() {
                    return None;
                }
                Some(Lease {
                    key,
                    user,
                    session: crate::protocol_ws::agent_session(&session).to_string(),
                    intent: lease.get("intent").map(|intent| text(intent, "text")).unwrap_or_default(),
                })
            })
            .collect();
        let heads = view
            .pointer("/keys/list")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|head| {
                let key = text(head, "key");
                let version = head.get("version").and_then(Value::as_u64)?;
                let promoted = head.get("promoted_version").and_then(Value::as_u64).unwrap_or(0);
                (!key.is_empty()).then_some((key, promoted >= version))
            })
            .collect();
        Protocol { leases, heads }
    }
}

/// A symbol's name as a protocol key spells it (the hooks' `sym_key`): a
/// name with spaces (`impl Foo`) joined by underscores.
fn key_name(symbol: &str) -> String {
    symbol.split_whitespace().collect::<Vec<_>>().join("_")
}

fn symbol_key(path: &str, symbol: &str) -> String {
    cork_protocol_code::symbol_key(path, &key_name(symbol))
}

/// The path a key names, and its symbol (none for a whole file's key).
fn split_key(key: &str) -> Option<(String, Option<String>)> {
    if let Some(path) = key.strip_prefix("file:") {
        return Some((path.to_string(), None));
    }
    cork_protocol_code::split_symbol_key(key).map(|(path, symbol)| (path.to_string(), Some(symbol.to_string())))
}

// ---------------------------------------------------------------- hands

/// One agent's touch of one row.
#[derive(Clone, Debug)]
struct Hand {
    user: String,
    session: String,
    agent: String,
    state: State,
    /// the lines it touches, 1-based and inclusive, when known
    lines: Option<(i64, i64)>,
    ts: f64,
    /// for a change: on origin yet? None where the protocol cannot say
    pushed: Option<bool>,
    /// its own words for what it is doing here
    note: String,
}

impl Hand {
    fn new(user: &str, session: &str, state: State, ts: f64) -> Hand {
        Hand {
            user: user.to_string(),
            session: session.to_string(),
            agent: String::new(),
            state,
            lines: None,
            ts,
            pushed: None,
            note: String::new(),
        }
    }

    /// The same agent: one person's one session, or the person when either
    /// side does not know the session (a draft, a collision on the feed).
    fn same_agent(&self, other: &Hand) -> bool {
        self.user == other.user && (self.session == other.session || self.session.is_empty() || other.session.is_empty())
    }

    /// Another touch by the same agent: the stronger state (and its words),
    /// every line, the newest time. A change not on origin stays not on
    /// origin.
    fn absorb(&mut self, other: Hand) {
        if other.state > self.state {
            self.state = other.state;
            if !other.note.is_empty() {
                self.note = other.note.clone();
            }
        } else if self.note.is_empty() {
            self.note = other.note.clone();
        }
        self.lines = union(self.lines, other.lines);
        self.ts = self.ts.max(other.ts);
        self.pushed = match (self.pushed, other.pushed) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), _) | (_, Some(true)) => Some(true),
            _ => None,
        };
        if self.session.is_empty() {
            self.session = other.session;
        }
        if self.agent.is_empty() {
            self.agent = other.agent;
        }
    }
}

fn union(a: Option<(i64, i64)>, b: Option<(i64, i64)>) -> Option<(i64, i64)> {
    match (a, b) {
        (Some(x), Some(y)) => Some((x.0.min(y.0), x.1.max(y.1))),
        (x, None) => x,
        (None, y) => y,
    }
}

fn overlaps(a: (i64, i64), b: (i64, i64)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

fn add(hands: &mut Vec<Hand>, hand: Hand) {
    match hands.iter_mut().find(|h| h.same_agent(&hand)) {
        Some(kept) => kept.absorb(hand),
        None => hands.push(hand),
    }
}

/// A hunk's lines in the file as the edit left it. A deletion leaves none:
/// it is the two lines either side of where they were.
fn hunk_ranges(hunks: Option<&Value>) -> Vec<(i64, i64)> {
    hunks
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|hunk| hunk.get("op").and_then(Value::as_str) != Some("truncated"))
        .filter_map(|hunk| {
            let start = hunk.get("new_start").and_then(Value::as_i64)?;
            let end = hunk.get("new_end").and_then(Value::as_i64)?;
            Some(if end < start { ((start - 1).max(1), start.max(1)) } else { (start.max(1), end) })
        })
        .collect()
}

// ---------------------------------------------------------------- files

/// A symbol of a file as its last clean parse has it.
#[derive(Clone, Debug)]
pub(crate) struct Span {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) first: i64,
    pub(crate) last: i64,
}

/// A file's symbols, from the graph: one range read of the snapshot.
pub(crate) fn spans_of(graph: &Snapshot, path: &str) -> Vec<Span> {
    let prefix = crate::graphview::node_id(path, "");
    graph
        .nodes
        .range(prefix.clone()..)
        .take_while(|(id, _)| id.starts_with(&prefix))
        .filter(|(_, node)| !node.symbol.is_empty() && node.path == path)
        .map(|(_, node)| {
            let span = node.span();
            let at = |i: usize| span.get(i).and_then(Value::as_i64).unwrap_or(0);
            Span { name: node.symbol.clone(), kind: node.kind.clone(), first: at(0), last: at(1) }
        })
        .collect()
}

/// A file's facts, begun with its symbols the first time it is touched.
fn facts_of<'a>(files: &'a mut BTreeMap<String, FileFacts>, graph: &Snapshot, path: &str) -> &'a mut FileFacts {
    files.entry(path.to_string()).or_insert_with(|| FileFacts { spans: spans_of(graph, path), ..FileFacts::default() })
}

/// The symbol a name (or a key's spelling of one) means in this file.
fn find<'a>(spans: &'a [Span], name: &str) -> Option<&'a Span> {
    spans.iter().find(|s| s.name == name).or_else(|| spans.iter().find(|s| key_name(&s.name) == name))
}

#[derive(Default)]
struct FileFacts {
    spans: Vec<Span>,
    symbols: BTreeMap<String, Vec<Hand>>,
    /// one hand per range, on lines inside no symbol
    lines: Vec<Hand>,
    /// a touch of the file as a whole: a lease or a claim on the file, an
    /// edit with neither symbols nor lines
    whole: Vec<Hand>,
    readers: Vec<Hand>,
    /// symbol ("" the file) -> Cork recorded a collision there; true when it
    /// held a write back
    collided: BTreeMap<String, bool>,
}

impl FileFacts {
    /// One agent's touch, put where it belongs: on each named symbol the
    /// parse knows, with the lines of the hunks inside it; a hunk inside
    /// none of them on lines; a touch with neither on the whole file.
    /// With no symbol named, a hunk lands on whichever symbols it falls in.
    /// `pushed` answers for a symbol's key (None: the file's).
    fn place(&mut self, named: &[String], ranges: &[(i64, i64)], hand: Hand, pushed: &dyn Fn(Option<&str>) -> Option<bool>) {
        let known: Vec<Span> = named.iter().filter_map(|name| find(&self.spans, name).cloned()).collect();
        let mut on: BTreeMap<String, Option<(i64, i64)>> = BTreeMap::new();
        for name in named {
            let name = find(&self.spans, name).map(|s| s.name.clone()).unwrap_or_else(|| name.clone());
            on.entry(name).or_insert(None);
        }
        let pool: Vec<Span> = if known.is_empty() { self.spans.clone() } else { known };
        for &(lo, hi) in ranges {
            let inside: Vec<&Span> = pool.iter().filter(|s| overlaps((s.first, s.last), (lo, hi))).collect();
            if inside.is_empty() {
                let mut alone = hand.clone();
                alone.lines = Some((lo, hi));
                if alone.state == State::Changed {
                    alone.pushed = pushed(None);
                }
                self.lines.push(alone);
                continue;
            }
            for span in inside {
                let slot = on.entry(span.name.clone()).or_insert(None);
                *slot = union(*slot, Some((lo.max(span.first), hi.min(span.last))));
            }
        }
        if on.is_empty() {
            if ranges.is_empty() {
                let mut whole = hand;
                if whole.state == State::Changed {
                    whole.pushed = pushed(None);
                }
                add(&mut self.whole, whole);
            }
            return;
        }
        for (name, lines) in on {
            let mut one = hand.clone();
            one.lines = lines;
            if one.state == State::Changed {
                one.pushed = pushed(Some(&name));
            }
            add(self.symbols.entry(name).or_default(), one);
        }
    }

    /// Every agent in the file, each once, in its strongest state.
    fn everyone(&self) -> Vec<Hand> {
        let mut out: Vec<Hand> = Vec::new();
        let all = self.symbols.values().flatten().chain(&self.lines).chain(&self.whole).chain(&self.readers);
        for hand in all {
            let mut one = hand.clone();
            one.lines = None;
            add(&mut out, one);
        }
        out
    }
}

// ----------------------------------------------------------------- rows

/// A row's status, by severity: what the tree sorts and colours by.
fn rank(status: &str) -> u8 {
    match status {
        "collision" => 5,
        "editing" => 4,
        "changed" => 3,
        "claimed" => 2,
        "reading" => 1,
        _ => 0,
    }
}

fn number_word(n: usize) -> String {
    match n {
        2 => "Two".into(),
        3 => "Three".into(),
        4 => "Four".into(),
        n => n.to_string(),
    }
}

/// What the kind of a symbol is called in a note.
fn kind_word(kind: &str) -> &'static str {
    match kind {
        "function" | "method" | "async_function" | "arrow_function" => "function",
        "class" => "class",
        "struct" => "struct",
        "interface" => "interface",
        "enum" => "enum",
        "trait" => "trait",
        "type" | "type_alias" => "type",
        _ => "definition",
    }
}

struct Row {
    id: String,
    kind: &'static str,
    symbol: String,
    symbol_kind: String,
    span: Option<(i64, i64)>,
    lines: Option<(i64, i64)>,
    hands: Vec<Hand>,
    /// Cork recorded a collision here (true: and held a write back)
    collided: Option<bool>,
}

impl Row {
    /// The agents contending for this row: at it now, or holding a change
    /// that is not on origin (one of unknown fate while it is recent).
    fn contenders(&self, stamp: f64) -> Vec<&Hand> {
        self.hands
            .iter()
            .filter(|h| {
                h.state.live()
                    || (h.state == State::Changed
                        && (h.pushed == Some(false) || (h.pushed.is_none() && stamp - h.ts < COLLISION_WINDOW_S)))
            })
            .collect()
    }

    fn status(&self, stamp: f64) -> &'static str {
        let contenders = self.contenders(stamp);
        let agents: BTreeSet<String> = contenders.iter().map(|h| agent_id(&h.user, &h.session)).collect();
        if self.collided.is_some() || (agents.len() >= 2 && contenders.iter().any(|h| h.state.live())) {
            return "collision";
        }
        match self.hands.iter().map(|h| h.state).max() {
            Some(State::Editing | State::Blocked) => "editing",
            Some(State::Changed) => "changed",
            Some(State::Claimed) => "claimed",
            _ => "reading",
        }
    }

    /// One line on the row: what the collision is, else the agents' own
    /// words, strongest and newest first.
    fn note(&self, status: &str, stamp: f64) -> String {
        let contenders = self.contenders(stamp);
        let agents: BTreeSet<String> = contenders.iter().map(|h| agent_id(&h.user, &h.session)).collect();
        let users: BTreeSet<&str> = contenders.iter().map(|h| h.user.as_str()).collect();
        let who = |n: usize, users: usize| format!("{} {}", number_word(n.max(2)), if users >= 2 { "people" } else { "agents" });
        if status == "collision" {
            if agents.len() >= 2 {
                let same_lines = contenders.iter().enumerate().any(|(i, a)| {
                    contenders[i + 1..].iter().any(|b| {
                        !a.same_agent(b) && matches!((a.lines, b.lines), (Some(x), Some(y)) if overlaps(x, y))
                    })
                });
                let place = if same_lines || self.kind == "lines" {
                    "on the same lines".to_string()
                } else if self.kind == "file" {
                    "in this file".to_string()
                } else {
                    format!("in this {}", kind_word(&self.symbol_kind))
                };
                return format!("{} {place}", who(agents.len(), users.len()));
            }
            return if self.collided == Some(true) { "Cork held a write back here".into() } else { "Two agents met here".into() };
        }
        let unpushed: Vec<&Hand> = self.hands.iter().filter(|h| h.state == State::Changed && h.pushed == Some(false)).collect();
        let changed_by: BTreeSet<String> = unpushed.iter().map(|h| agent_id(&h.user, &h.session)).collect();
        if changed_by.len() >= 2 {
            let people: BTreeSet<&str> = unpushed.iter().map(|h| h.user.as_str()).collect();
            return format!("{} changed this and nothing is pushed yet", who(changed_by.len(), people.len()));
        }
        let mut by_strength: Vec<&Hand> = self.hands.iter().collect();
        by_strength.sort_by(|a, b| b.state.cmp(&a.state).then(b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal)));
        by_strength.iter().map(|h| h.note.clone()).find(|n| !n.is_empty()).unwrap_or_default()
    }

    fn ts(&self) -> f64 {
        self.hands.iter().map(|h| h.ts).fold(0.0, f64::max)
    }

    /// Where the row sits in its file: in line order, the whole file last.
    fn order(&self) -> (i64, String) {
        let first = self.span.map(|s| s.0).or(self.lines.map(|l| l.0)).unwrap_or(i64::MAX);
        (first, self.symbol.clone())
    }
}

/// Runs of lines, each the union of the ranges that overlap or touch it,
/// with every agent on it once.
fn cluster(mut hands: Vec<Hand>) -> Vec<((i64, i64), Vec<Hand>)> {
    hands.sort_by_key(|h| h.lines.map(|l| l.0).unwrap_or(0));
    let mut runs: Vec<((i64, i64), Vec<Hand>)> = Vec::new();
    for hand in hands {
        let Some(lines) = hand.lines else { continue };
        match runs.last_mut() {
            Some((run, on)) if lines.0 <= run.1 + 1 => {
                *run = (run.0.min(lines.0), run.1.max(lines.1));
                add(on, hand);
            }
            _ => runs.push((lines, vec![hand])),
        }
    }
    runs
}

fn rows_of(path: &str, facts: &FileFacts) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for (name, hands) in &facts.symbols {
        let span = find(&facts.spans, name);
        let lines = hands.iter().fold(None, |acc, h| union(acc, h.lines));
        rows.push(Row {
            id: format!("{path}::{name}"),
            kind: "symbol",
            symbol: name.clone(),
            symbol_kind: span.map(|s| s.kind.clone()).unwrap_or_default(),
            span: span.map(|s| (s.first, s.last)),
            lines,
            hands: hands.clone(),
            collided: facts.collided.get(name).copied(),
        });
    }
    for (lines, hands) in cluster(facts.lines.clone()) {
        rows.push(Row {
            id: format!("{path}#L{}-{}", lines.0, lines.1),
            kind: "lines",
            symbol: String::new(),
            symbol_kind: String::new(),
            span: None,
            lines: Some(lines),
            hands,
            collided: None,
        });
    }
    // the whole file is a row of its own for an agent with nothing more
    // precise in it (or a collision Cork recorded on the file)
    let placed: Vec<Hand> = rows.iter().flat_map(|r| r.hands.clone()).collect();
    let whole: Vec<Hand> = facts
        .whole
        .iter()
        .filter(|h| facts.collided.contains_key("") || !placed.iter().any(|p| p.same_agent(h)))
        .cloned()
        .collect();
    if !whole.is_empty() {
        rows.push(Row {
            id: format!("{path}#file"),
            kind: "file",
            symbol: String::new(),
            symbol_kind: String::new(),
            span: None,
            lines: None,
            hands: whole,
            collided: facts.collided.get("").copied(),
        });
    }
    rows
}

// ----------------------------------------------------------------- read

/// The model and tool a session reported last: its focus marker, else its
/// last action.
struct Sessions<'a> {
    store: &'a Store,
    scope: &'a str,
    focus: BTreeMap<String, Value>,
    seen: BTreeMap<String, Option<Value>>,
}

impl Sessions<'_> {
    fn last_action(&mut self, user: &str, session: &str) -> Option<Value> {
        if session.is_empty() {
            return None;
        }
        let id = agent_id(user, session);
        let (store, scope) = (self.store, self.scope);
        self.seen
            .entry(id)
            .or_insert_with(|| store.eph_get(&crate::presence::last_action_key(scope, user, session)))
            .clone()
    }

    /// `(agent, model)` for a hand.
    fn tool(&mut self, hand: &Hand) -> (String, String) {
        let focus = self.focus.get(&agent_id(&hand.user, &hand.session)).cloned();
        let last = self.last_action(&hand.user, &hand.session);
        let pick = |field: &str| {
            [focus.as_ref(), last.as_ref()].into_iter().flatten().map(|r| text(r, field)).find(|v| !v.is_empty()).unwrap_or_default()
        };
        let named = crate::presence::agent_name(&hand.agent);
        let agent = if named.is_empty() { crate::presence::agent_name(&pick("agent")) } else { named };
        (agent, pick("model"))
    }

    /// The session's turn ended on a question for its person: it waits on
    /// them.
    fn asks(&mut self, hand: &Hand) -> bool {
        self.last_action(&hand.user, &hand.session)
            .is_some_and(|last| text(&last, "kind") == "done" && text(&last, "note").contains('?'))
    }
}

/// Where everyone is in one repo: folders, files and rows, every person on
/// them with their state, lines and age. `me` is the reader, whose agents'
/// rows the dashboard's "Mine" keeps.
pub fn tree(store: &Store, scope: &str, protocol: &Protocol, me: &str) -> Value {
    let stamp = now();
    let (workspace, repo) = split_scope(scope);
    let graph = crate::graphview::snapshot(store, scope);
    let mut files: BTreeMap<String, FileFacts> = BTreeMap::new();

    // the ledger's last hour: the edits, and the promotions that followed
    // them ((user, session, key) -> newest promotion)
    let rows = store.ledger_since_kinds(scope, stamp - CHANGED_WINDOW_S, &["edit_reported", "op_promoted"]);
    let mut promoted: BTreeMap<(String, String, String), f64> = BTreeMap::new();
    for row in rows.iter().filter(|r| r.kind == "op_promoted") {
        let by = text(&row.payload, "by");
        let session = crate::protocol_ws::agent_session(&text(&row.payload, "session")).to_string();
        for key in strings(row.payload.get("keys")) {
            let at = promoted.entry((by.clone(), session.clone(), key)).or_insert(row.ts);
            *at = at.max(row.ts);
        }
    }
    let pushed_of = |user: &str, session: &str, key: &str, ts: f64| -> Option<bool> {
        match protocol.heads.get(key) {
            Some(true) => Some(true),
            Some(false) => Some(
                promoted
                    .range((user.to_string(), String::new(), String::new())..)
                    .take_while(|((u, _, _), _)| u == user)
                    .any(|((_, s, k), at)| k == key && (s == session || session.is_empty()) && *at >= ts),
            ),
            None => None,
        }
    };

    // 1. changed: every landed edit of the last hour, newest first, so an
    // agent's newest rationale is the one its hand keeps
    for row in rows.iter().rev().filter(|r| r.kind == "edit_reported") {
        let p = &row.payload;
        let (path, user, session) = (text(p, "path"), text(p, "user"), text(p, "session"));
        if path.is_empty() || user.is_empty() {
            continue;
        }
        let named: Vec<String> = p.get("symbols_changed").and_then(Value::as_object).map(|m| m.keys().cloned().collect()).unwrap_or_default();
        let ranges = hunk_ranges(p.get("hunks"));
        let mut hand = Hand::new(&user, &session, State::Changed, row.ts);
        hand.agent = text(p, "agent");
        hand.note = one_line(&text(p, "why"));
        let pushed = |symbol: Option<&str>| {
            let key = symbol.map(|s| symbol_key(&path, s)).unwrap_or_else(|| cork_protocol_code::file_key(&path));
            pushed_of(&user, &session, &key, row.ts)
        };
        facts_of(&mut files, &graph, &path).place(&named, &ranges, hand, &pushed);
    }

    // 2. editing: drafts (an MCP report_edit with draft), each its person's
    // newest on the file; the session is the one that last edited it
    for (_key, draft) in store.eph_scan(&format!("draft:{scope}:")) {
        let (path, user) = (text(&draft, "path"), text(&draft, "user"));
        if path.is_empty() || user.is_empty() {
            continue;
        }
        let session = store
            .eph_get(&format!("lastedit:{scope}:{user}:{}", crate::repo::path_key(&path)))
            .map(|last| text(&last, "session"))
            .unwrap_or_default();
        let mut hand = Hand::new(&user, &session, State::Editing, ts_of(&draft));
        hand.agent = text(&draft, "agent");
        facts_of(&mut files, &graph, &path).place(&strings(draft.get("symbols_changed")), &hunk_ranges(draft.get("hunks")), hand, &|_| None);
    }

    // 3. editing: the protocol's leases, with the holder's intent
    for lease in &protocol.leases {
        let Some((path, symbol)) = split_key(&lease.key) else { continue };
        let mut hand = Hand::new(&lease.user, &lease.session, State::Editing, stamp);
        hand.note = one_line(&lease.intent);
        let facts = facts_of(&mut files, &graph, &path);
        match symbol {
            Some(symbol) => facts.place(&[symbol], &[], hand, &|_| None),
            None => add(&mut facts.whole, hand),
        }
    }

    // 4. claimed: open intents, on the symbols they name in each path (the
    // whole file where they name none there)
    for (_key, intent) in store.eph_scan(&format!("intent:{scope}:")) {
        let owner = text(&intent, "owner");
        if owner.is_empty() {
            continue;
        }
        let symbols = strings(intent.get("symbols"));
        for path in strings(intent.get("paths")).into_iter().take(MAX_INTENT_PATHS) {
            let mut hand = Hand::new(&owner, &text(&intent, "session"), State::Claimed, intent.get("created").and_then(Value::as_f64).unwrap_or(stamp));
            hand.agent = text(&intent, "agent");
            hand.note = one_line(&text(&intent, "summary"));
            let facts = facts_of(&mut files, &graph, &path);
            let named: Vec<String> = symbols
                .iter()
                .filter_map(|s| {
                    let bare = s.rsplit("::").next().unwrap_or(s).rsplit('.').next().unwrap_or(s);
                    find(&facts.spans, s).or_else(|| find(&facts.spans, bare)).map(|span| span.name.clone())
                })
                .collect();
            if named.is_empty() {
                add(&mut facts.whole, hand);
            } else {
                facts.place(&named, &[], hand, &|_| None);
            }
        }
    }

    // 5. the agents in this repo right now: the gate's "editing" marker puts
    // its agent on the file; a reader sits on a file the graph knows (a
    // focus path can be a command or a search)
    let mut focus: BTreeMap<String, Value> = BTreeMap::new();
    for (_key, marker) in store.eph_scan(&format!("focus:{workspace}:")) {
        if text(&marker, "repo_id") != repo {
            continue;
        }
        let (user, session, path) = (text(&marker, "user"), text(&marker, "session"), text(&marker, "path"));
        if user.is_empty() {
            continue;
        }
        focus.insert(agent_id(&user, &session), marker.clone());
        let action = text(&marker, "action");
        if path.is_empty() {
            continue;
        }
        let hand = Hand::new(&user, &session, State::Reading, ts_of(&marker));
        if action == "editing" {
            add(&mut facts_of(&mut files, &graph, &path).whole, Hand { state: State::Editing, ..hand });
        } else if matches!(action.as_str(), "" | "reading")
            && (files.contains_key(&path) || graph.has_node(&crate::graphview::node_id(&path, "")))
        {
            add(&mut facts_of(&mut files, &graph, &path).readers, hand);
        }
    }

    // a landed edit minutes old, by a session still in the repo, is being
    // edited now
    for facts in files.values_mut() {
        for hand in facts.symbols.values_mut().flatten().chain(facts.lines.iter_mut()).chain(facts.whole.iter_mut()) {
            if hand.state == State::Changed
                && stamp - hand.ts < RECENT_EDIT_S
                && !hand.session.is_empty()
                && focus.contains_key(&agent_id(&hand.user, &hand.session))
            {
                hand.state = State::Editing;
            }
        }
    }

    // 6. the collisions Cork recorded: the row goes yellow, and both agents
    // stand on it (the one held back, and the one whose work was there)
    let events = store
        .eph_get(&format!("recent:{scope}"))
        .and_then(|ring| ring.get("events").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    for event in events.iter().filter(|e| text(e, "kind") == "collision" && stamp - ts_of(e) < COLLISION_WINDOW_S) {
        let held = text(event, "mode") == "blocked";
        let checker = text(event, "checker");
        let checker_session = text(event, "checker_agent")
            .split_once('#')
            .map(|(_, s)| crate::protocol_ws::agent_session(s).to_string())
            .unwrap_or_default();
        for item in event.get("items").and_then(Value::as_array).into_iter().flatten() {
            let path = text(item, "path");
            if path.is_empty() {
                continue;
            }
            let facts = facts_of(&mut files, &graph, &path);
            let symbol = text(item, "symbol");
            let name = if symbol.is_empty() || symbol == path {
                String::new()
            } else {
                find(&facts.spans, &symbol).map(|s| s.name.clone()).unwrap_or(symbol)
            };
            let was = facts.collided.get(&name).copied().unwrap_or(false);
            facts.collided.insert(name.clone(), was || held);
            let mut parties = Vec::new();
            if !checker.is_empty() {
                parties.push(Hand::new(&checker, &checker_session, if held { State::Blocked } else { State::Changed }, ts_of(event)));
            }
            let author = text(item, "author");
            if !author.is_empty() && author != checker {
                parties.push(Hand::new(&author, "", State::Changed, ts_of(event)));
            }
            for hand in parties {
                if name.is_empty() {
                    add(&mut facts.whole, hand);
                } else {
                    facts.place(&[name.clone()], &[], hand, &|_| None);
                }
            }
        }
    }

    let mut sessions = Sessions { store, scope, focus, seen: BTreeMap::new() };
    let mut users: BTreeSet<String> = BTreeSet::new();
    // each agent's newest touch anywhere: a question it ended its turn on is
    // about that work, not about everything it did this hour
    let mut newest: BTreeMap<String, f64> = BTreeMap::new();
    for facts in files.values() {
        for hand in facts.symbols.values().flatten().chain(&facts.lines).chain(&facts.whole) {
            let at = newest.entry(agent_id(&hand.user, &hand.session)).or_insert(hand.ts);
            *at = at.max(hand.ts);
        }
    }

    // the files, at most MAX_FILES of them, the most pressing kept
    struct File {
        path: String,
        status: &'static str,
        ts: f64,
        value: Value,
    }
    let mut built: Vec<File> = Vec::new();
    for (path, facts) in &files {
        let mut rows = rows_of(path, facts);
        if rows.is_empty() && facts.readers.is_empty() {
            continue;
        }
        let mut statuses: Vec<(&'static str, usize)> = rows.iter().enumerate().map(|(i, r)| (r.status(stamp), i)).collect();
        // the most pressing rows survive the cap, then the file reads in order
        statuses.sort_by_key(|(status, i)| (std::cmp::Reverse(rank(status)), *i));
        let more = statuses.len().saturating_sub(MAX_ROWS);
        let kept: BTreeSet<usize> = statuses.iter().take(MAX_ROWS).map(|(_, i)| *i).collect();
        let mut kept_rows: Vec<Row> = rows.drain(..).enumerate().filter(|(i, _)| kept.contains(i)).map(|(_, r)| r).collect();
        kept_rows.sort_by(|a, b| a.order().cmp(&b.order()));

        let mut out_rows: Vec<Value> = Vec::new();
        let mut file_status = if facts.readers.is_empty() { "" } else { "reading" };
        for row in &kept_rows {
            let status = row.status(stamp);
            if rank(status) > rank(file_status) {
                file_status = status;
            }
            let mut note = row.note(status, stamp);
            let mut hands = row.hands.clone();
            hands.sort_by(|a, b| b.state.cmp(&a.state).then(b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal)));
            if note.is_empty() {
                // the session's own title, last: what the agent is about
                note = hands
                    .iter()
                    .map(|h| crate::freshness::title_of(store, scope, &h.user, &h.session))
                    .find(|t| !t.is_empty())
                    .map(|t| one_line(&t))
                    .unwrap_or_default();
            }
            let waiting = hands
                .iter()
                .find(|h| newest.get(&agent_id(&h.user, &h.session)) == Some(&h.ts) && sessions.asks(h))
                .map(|h| h.user.clone());
            let changed: Vec<&Hand> = hands.iter().filter(|h| h.state >= State::Changed).collect();
            let pushed = if changed.iter().any(|h| h.pushed == Some(false)) {
                json!(false)
            } else if !changed.is_empty() && changed.iter().all(|h| h.pushed == Some(true)) {
                json!(true)
            } else {
                Value::Null
            };
            let people: Vec<Value> = hands
                .iter()
                .map(|h| {
                    users.insert(h.user.clone());
                    let (agent, model) = sessions.tool(h);
                    json!({
                        "user": h.user, "session": h.session, "agent": agent, "model": model,
                        "state": h.state.name(),
                        "lines": h.lines.map(|l| json!([l.0, l.1])).unwrap_or(Value::Null),
                        "ts": h.ts, "age_s": crate::compat::python_round((stamp - h.ts).max(0.0), 1),
                        "pushed": h.pushed.map(Value::Bool).unwrap_or(Value::Null),
                    })
                })
                .collect();
            let ts = row.ts();
            out_rows.push(json!({
                "id": row.id, "kind": row.kind, "symbol": row.symbol, "symbol_kind": row.symbol_kind,
                "span": row.span.map(|s| json!([s.0, s.1])).unwrap_or(Value::Null),
                "lines": row.lines.map(|l| json!([l.0, l.1])).unwrap_or(Value::Null),
                // a claim covers what it names whole: no lines of it are touched yet
                "whole": status == "claimed",
                "status": status, "pushed": pushed, "note": note,
                "waiting": waiting.map(Value::String).unwrap_or(Value::Null),
                "ts": ts, "age_s": crate::compat::python_round((stamp - ts).max(0.0), 1),
                "people": people,
            }));
        }
        let everyone: Vec<Value> = facts
            .everyone()
            .iter()
            .map(|h| {
                users.insert(h.user.clone());
                let (agent, model) = sessions.tool(h);
                json!({"user": h.user, "session": h.session, "agent": agent, "model": model, "state": h.state.name()})
            })
            .collect();
        let ts = facts.everyone().iter().map(|h| h.ts).fold(0.0, f64::max);
        let mut value = json!({
            "path": path,
            "name": path.rsplit('/').next().unwrap_or(path),
            "status": file_status,
            "people": everyone,
            "rows": out_rows,
            "ts": ts,
        });
        if more > 0 {
            value["more_rows"] = json!(more);
        }
        built.push(File { path: path.clone(), status: file_status, ts, value });
    }
    built.sort_by(|a, b| rank(b.status).cmp(&rank(a.status)).then(b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal)));
    let total_files = built.len();
    built.truncate(MAX_FILES);

    // folders and their files by name, rows in line order: a status changes
    // where it stands, and nothing on the page moves when it does
    let mut folders: BTreeMap<String, Vec<File>> = BTreeMap::new();
    for file in built {
        let dir = file.path.rsplit_once('/').map(|(dir, _)| dir.to_string()).unwrap_or_default();
        folders.entry(dir).or_default().push(file);
    }
    for files in folders.values_mut() {
        files.sort_by(|a, b| a.path.cmp(&b.path));
    }
    let rows_total: usize = folders.iter().flat_map(|(_, f)| f).map(|f| f.value["rows"].as_array().map_or(0, Vec::len)).sum();
    let folders: Vec<Value> = folders
        .into_iter()
        .map(|(path, files)| {
            let status = files.iter().map(|f| f.status).max_by_key(|s| rank(s)).unwrap_or("");
            json!({"path": path, "status": status, "files": files.into_iter().map(|f| f.value).collect::<Vec<_>>()})
        })
        .collect();

    json!({
        "scope_repo": repo,
        "me": me,
        "ts": stamp,
        "files": total_files,
        "rows": rows_total,
        "window_s": CHANGED_WINDOW_S,
        // whether "not pushed" can be said at all: the protocol keys this repo
        "protocol": !protocol.heads.is_empty() || !protocol.leases.is_empty(),
        "people": people(store, workspace, me, &users),
        "folders": folders,
    })
}

/// Each person on the tree as the dashboard draws them: their account, the
/// name this reader knows them by (their own nicknames first), and their
/// picture.
pub fn people(store: &Store, workspace: &str, reader: &str, users: &BTreeSet<String>) -> Value {
    let members = crate::workspaces::members(store, workspace);
    let labels = crate::activity::identity_labels(store, reader, users);
    let mut out = Map::new();
    for user in users {
        let member = members
            .iter()
            .find(|m| text(m, "email").eq_ignore_ascii_case(user) || text(m, "uid") == *user);
        let uid = member.map(|m| text(m, "uid")).unwrap_or_default();
        let profile = if uid.is_empty() { Value::Null } else { store.kv_get("profile", &uid).unwrap_or(Value::Null) };
        let label = labels.get(user).map(|l| text(l, "who")).filter(|w| !w.is_empty()).unwrap_or_else(|| {
            user.split('@').next().unwrap_or(user).to_string()
        });
        out.insert(user.clone(), json!({
            "uid": uid, "label": label,
            "avatar": text(&profile, "avatar"), "github_login": text(&profile, "github_login"),
        }));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPE: &str = "w1:github.com/acme/app";
    const NAV: &str = "components/nav/NavIsland.tsx";

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("store")
    }

    /// A parsed file with these symbols at these lines.
    fn parsed(db: &Store, path: &str, symbols: &[(&str, &str, i64, i64)]) {
        let mut map = Map::new();
        for (name, kind, first, last) in symbols {
            map.insert(name.to_string(), json!({"kind": kind, "edges": [], "span": [first, last], "params": [], "sites": []}));
        }
        crate::codegraph::update_file(db, SCOPE, path, json!({"path": path, "language": "typescript", "symbols": map, "imports": [], "ts": 1.0}), 1.0);
    }

    fn edit(db: &Store, user: &str, session: &str, path: &str, symbols: &[&str], hunks: &[(i64, i64)], ago: f64) {
        let changed: Map<String, Value> = symbols.iter().map(|s| (s.to_string(), json!({"before": "h-old", "after": "h-new"}))).collect();
        let hunks: Vec<Value> = hunks.iter().map(|(a, b)| json!({"op": "replace", "old_start": a, "old_end": b, "new_start": a, "new_end": b})).collect();
        db.ledger_append(SCOPE, "edit_reported", &json!({
            "user": user, "session": session, "agent": "claude-code", "path": path,
            "symbols_changed": changed, "hunks": hunks,
            "events": [{"symbol": symbols.first().copied().unwrap_or(""), "kind": "signature_change", "signature_after": "def leaked(x)"}],
        }), now() - ago).unwrap();
    }

    fn lease(user: &str, session: &str, key: &str) -> Lease {
        Lease { key: key.into(), user: user.into(), session: session.into(), intent: String::new() }
    }

    fn files(tree: &Value) -> Vec<Value> {
        tree["folders"].as_array().unwrap().iter().flat_map(|f| f["files"].as_array().unwrap().clone()).collect()
    }

    fn row<'a>(tree: &'a Value, id: &str) -> &'a Value {
        tree["folders"].as_array().unwrap().iter()
            .flat_map(|f| f["files"].as_array().unwrap())
            .flat_map(|f| f["rows"].as_array().unwrap())
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("no row {id} in {tree:#}"))
    }

    #[test]
    fn a_lease_on_a_function_just_edited_is_editing_now_on_the_edits_lines() {
        let db = store();
        parsed(&db, NAV, &[("NavIsland", "function", 40, 80), ("useIslandLayout", "function", 10, 30)]);
        edit(&db, "vera@x.io", "s1", NAV, &["NavIsland"], &[(46, 49)], 600.0);
        let protocol = Protocol { leases: vec![lease("vera@x.io", "s1", "code:components/nav/NavIsland.tsx::NavIsland")], heads: BTreeMap::new() };
        let tree = tree(&db, SCOPE, &protocol, "vera@x.io");
        let nav = row(&tree, "components/nav/NavIsland.tsx::NavIsland");
        assert_eq!(nav["status"], "editing");
        assert_eq!(nav["lines"], json!([46, 49]));
        assert_eq!(nav["span"], json!([40, 80]));
        assert_eq!(nav["people"][0]["state"], "editing");
        assert_eq!(nav["people"][0]["agent"], "claude-code");
        assert_eq!(tree["folders"][0]["path"], "components/nav");
        assert_eq!(tree["files"], 1);
        assert_eq!(tree["me"], "vera@x.io");
        // the function nobody touched is not a row
        assert_eq!(files(&tree)[0]["rows"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn two_people_on_the_same_lines_collide_and_the_file_says_so() {
        let db = store();
        parsed(&db, NAV, &[("NavIsland", "function", 40, 80)]);
        edit(&db, "vera@x.io", "s1", NAV, &["NavIsland"], &[(46, 49)], 120.0);
        edit(&db, "maya@x.io", "m1", NAV, &["NavIsland"], &[(47, 48)], 300.0);
        let key = "code:components/nav/NavIsland.tsx::NavIsland".to_string();
        let protocol = Protocol {
            leases: vec![lease("vera@x.io", "s1", &key)],
            heads: [(key, false)].into_iter().collect(),
        };
        let tree = tree(&db, SCOPE, &protocol, "maya@x.io");
        let nav = row(&tree, "components/nav/NavIsland.tsx::NavIsland");
        assert_eq!(nav["status"], "collision");
        assert_eq!(nav["note"], "Two people on the same lines");
        assert_eq!(nav["lines"], json!([46, 49]));
        assert_eq!(nav["pushed"], json!(false));
        let file = &files(&tree)[0];
        assert_eq!(file["status"], "collision");
        assert_eq!(file["people"].as_array().unwrap().len(), 2);
        assert_eq!(tree["folders"][0]["status"], "collision");
    }

    #[test]
    fn a_pushed_change_does_not_contend_and_an_unpushed_one_says_so() {
        let db = store();
        parsed(&db, NAV, &[("NavIsland", "function", 40, 80), ("useIslandLayout", "function", 10, 30)]);
        let nav_key = "code:components/nav/NavIsland.tsx::NavIsland".to_string();
        let layout_key = "code:components/nav/NavIsland.tsx::useIslandLayout".to_string();
        // maya changed NavIsland and pushed it; vera edits it now: teamwork, not a collision
        edit(&db, "maya@x.io", "m1", NAV, &["NavIsland"], &[(50, 52)], 1200.0);
        db.ledger_append(SCOPE, "op_promoted", &json!({"by": "maya@x.io", "session": "m1.d4242", "keys": [nav_key]}), now() - 1000.0).unwrap();
        // vera changed useIslandLayout ten minutes ago and has not pushed
        edit(&db, "vera@x.io", "s1", NAV, &["useIslandLayout"], &[(12, 30)], 600.0);
        let protocol = Protocol {
            leases: vec![lease("vera@x.io", "s1", &nav_key)],
            heads: [(nav_key, false), (layout_key, false)].into_iter().collect(),
        };
        let tree = tree(&db, SCOPE, &protocol, "vera@x.io");
        let nav = row(&tree, "components/nav/NavIsland.tsx::NavIsland");
        assert_eq!(nav["status"], "editing", "{nav:#}");
        let maya = nav["people"].as_array().unwrap().iter().find(|p| p["user"] == "maya@x.io").unwrap();
        assert_eq!(maya["pushed"], json!(true), "her promotion followed her edit");
        let layout = row(&tree, "components/nav/NavIsland.tsx::useIslandLayout");
        assert_eq!(layout["status"], "changed");
        assert_eq!(layout["pushed"], json!(false));
        assert_eq!(layout["lines"], json!([12, 30]));
        assert!(layout["age_s"].as_f64().unwrap() >= 599.0);
        assert_eq!(tree["protocol"], json!(true));
        // where the protocol has no key, nobody can say
        let quiet = super::tree(&db, SCOPE, &Protocol::default(), "vera@x.io");
        assert_eq!(row(&quiet, "components/nav/NavIsland.tsx::useIslandLayout")["pushed"], Value::Null);
        assert_eq!(quiet["protocol"], json!(false));
    }

    #[test]
    fn a_stylesheet_no_parser_reads_is_its_lines() {
        let db = store();
        edit(&db, "maya@x.io", "m1", "styles/lanes.css", &[], &[(12, 12)], 60.0);
        edit(&db, "kai@x.io", "k1", "styles/lanes.css", &[], &[(80, 84)], 900.0);
        let tree = tree(&db, SCOPE, &Protocol::default(), "maya@x.io");
        let twelve = row(&tree, "styles/lanes.css#L12-12");
        assert_eq!(twelve["kind"], "lines");
        assert_eq!(twelve["lines"], json!([12, 12]));
        assert_eq!(twelve["people"][0]["user"], "maya@x.io");
        assert_eq!(row(&tree, "styles/lanes.css#L80-84")["people"][0]["user"], "kai@x.io");
        assert_eq!(files(&tree)[0]["name"], "lanes.css");
    }

    #[test]
    fn an_intent_claims_whole_functions_next_with_its_summary() {
        let db = store();
        parsed(&db, "components/nav/MobileNav.tsx", &[("MobileNav", "function", 1, 60)]);
        db.eph_set(&format!("intent:{SCOPE}:i1"), &json!({
            "intent_id": "i1", "owner": "vera@x.io", "session": "s1", "agent": "claude-code",
            "paths": ["components/nav/MobileNav.tsx"], "symbols": ["MobileNav"],
            "summary": "Will apply the same padding here next", "created": now() - 30.0,
        }), Some(600.0)).unwrap();
        let tree = tree(&db, SCOPE, &Protocol::default(), "maya@x.io");
        let next = row(&tree, "components/nav/MobileNav.tsx::MobileNav");
        assert_eq!(next["status"], "claimed");
        assert_eq!(next["whole"], json!(true));
        assert_eq!(next["note"], "Will apply the same padding here next");
        assert_eq!(next["people"][0]["state"], "claimed");
    }

    #[test]
    fn a_reader_sits_on_the_file_and_makes_no_row() {
        let db = store();
        parsed(&db, "api/collisions.ts", &[("listCollisions", "function", 20, 58)]);
        db.eph_set("focus:w1:dee@x.io#d1", &json!({
            "user": "dee@x.io", "session": "d1", "repo_id": "github.com/acme/app", "path": "api/collisions.ts",
            "action": "reading", "agent": "codex", "model": "gpt-5.5-codex", "ts": now() - 5.0,
        }), Some(180.0)).unwrap();
        // a running command's "path" is a command, not a file: no row, no file
        db.eph_set("focus:w1:dee@x.io#d2", &json!({
            "user": "dee@x.io", "session": "d2", "repo_id": "github.com/acme/app", "path": "cargo test", "action": "running", "ts": now(),
        }), Some(180.0)).unwrap();
        let tree = tree(&db, SCOPE, &Protocol::default(), "maya@x.io");
        let all = files(&tree);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["status"], "reading");
        assert!(all[0]["rows"].as_array().unwrap().is_empty());
        assert_eq!(all[0]["people"][0]["state"], "reading");
        assert_eq!(all[0]["people"][0]["model"], "gpt-5.5-codex");
    }

    #[test]
    fn a_collision_cork_recorded_turns_its_row_yellow_with_both_agents_on_it() {
        let db = store();
        parsed(&db, "hooks/claim.ts", &[("claim", "function", 12, 30)]);
        db.eph_set(&format!("recent:{SCOPE}"), &json!({"events": [{
            "kind": "collision", "checker": "kai@x.io", "checker_agent": "kai@x.io#k1", "mode": "blocked", "ts": now() - 60.0,
            "items": [{"symbol": "claim", "path": "hooks/claim.ts", "author": "vera@x.io", "detail": "vera holds a lease"}],
        }]}), Some(3600.0)).unwrap();
        let tree = tree(&db, SCOPE, &Protocol::default(), "kai@x.io");
        let claim = row(&tree, "hooks/claim.ts::claim");
        assert_eq!(claim["status"], "collision");
        assert_eq!(claim["note"], "Two people in this function");
        let states: BTreeSet<String> = claim["people"].as_array().unwrap().iter().map(|p| p["state"].as_str().unwrap().to_string()).collect();
        assert_eq!(states, ["blocked", "changed"].into_iter().map(String::from).collect());
    }

    #[test]
    fn an_agent_whose_turn_ended_on_a_question_is_waiting_on_its_person() {
        let db = store();
        parsed(&db, "hooks/claim.ts", &[("claim", "function", 12, 30)]);
        parsed(&db, "hooks/handoff.ts", &[("sendHandoff", "function", 8, 44)]);
        edit(&db, "kai@x.io", "k1", "hooks/handoff.ts", &["sendHandoff"], &[(8, 44)], 2400.0);
        edit(&db, "kai@x.io", "k1", "hooks/claim.ts", &["claim"], &[(12, 30)], 300.0);
        db.eph_set(&crate::presence::last_action_key(SCOPE, "kai@x.io", "k1"), &json!({
            "kind": "done", "note": "Added the retries input. Should it default to three?", "ts": now() - 100.0,
            "agent": "claude-code", "model": "claude-opus-5-5",
        }), Some(3600.0)).unwrap();
        // the lease outlives the turn: the function is still its, now
        let protocol = Protocol { leases: vec![lease("kai@x.io", "k1", "code:hooks/claim.ts::claim")], heads: BTreeMap::new() };
        let tree = tree(&db, SCOPE, &protocol, "kai@x.io");
        let claim = row(&tree, "hooks/claim.ts::claim");
        assert_eq!(claim["waiting"], "kai@x.io");
        assert_eq!(claim["status"], "editing");
        assert_eq!(claim["people"][0]["model"], "claude-opus-5-5");
        // the question is about its newest work, not the hour before
        assert_eq!(row(&tree, "hooks/handoff.ts::sendHandoff")["waiting"], Value::Null);
    }

    #[test]
    fn names_and_lines_only_never_code_and_a_viewer_loses_the_notes() {
        let db = store();
        parsed(&db, NAV, &[("NavIsland", "function", 40, 80)]);
        edit(&db, "vera@x.io", "s1", NAV, &["NavIsland"], &[(46, 49)], 60.0);
        db.eph_set(&format!("intent:{SCOPE}:i1"), &json!({
            "owner": "vera@x.io", "session": "s1", "paths": [NAV], "symbols": ["NavIsland"],
            "summary": "Now also returns a compact flag", "created": now() - 120.0,
        }), Some(600.0)).unwrap();
        let tree = tree(&db, SCOPE, &Protocol::default(), "vera@x.io");
        let text = tree.to_string();
        for code in ["h-old", "h-new", "def leaked", "signature"] {
            assert!(!text.contains(code), "{code} leaked: {text}");
        }
        assert_eq!(row(&tree, "components/nav/NavIsland.tsx::NavIsland")["note"], "Now also returns a compact flag");
        let mut shown = tree.clone();
        crate::access::redact_for_viewer(&mut shown);
        assert!(!shown.to_string().contains("compact flag"));
        assert_eq!(row(&shown, "components/nav/NavIsland.tsx::NavIsland")["lines"], json!([46, 49]), "lines stay");
    }

    #[test]
    fn the_protocols_view_names_holders_as_reports_do() {
        let view = json!({
            "leases": [
                {"key": "code:a.ts::f", "holder": {"session": "s1.d812", "agent_id": "vera@x.io#s1.d812", "person": "vera@x.io"}, "kinds": [], "intent": {"text": "pad the island"}},
                {"key": "file:b.css", "holder": {"session": "", "agent_id": "", "person": ""}, "kinds": []},
            ],
            "keys": {"list": [
                {"key": "code:a.ts::f", "version": 3, "promoted_version": 2},
                {"key": "code:a.ts::g", "version": 2, "promoted_version": 2},
            ]},
        });
        let protocol = Protocol::from_view(&view);
        assert_eq!(protocol.leases.len(), 1, "a holder with no name is no one");
        assert_eq!((protocol.leases[0].user.as_str(), protocol.leases[0].session.as_str()), ("vera@x.io", "s1"));
        assert_eq!(protocol.leases[0].intent, "pad the island");
        assert_eq!(protocol.heads.get("code:a.ts::f"), Some(&false));
        assert_eq!(protocol.heads.get("code:a.ts::g"), Some(&true));
    }

    #[test]
    fn people_carry_their_account_name_and_picture() {
        let db = store();
        db.kv_put("member", "w1/u-maya", &json!({"uid": "u-maya", "email": "Maya@x.io", "role": "member", "added": 1.0}), 1.0).unwrap();
        db.kv_put("profile", "u-maya", &json!({"avatar": "https://img/maya.png", "github_login": "maya"}), 1.0).unwrap();
        db.kv_put("username_by_email", "maya@x.io", &json!({"username": "maya"}), 1.0).unwrap();
        let users: BTreeSet<String> = ["maya@x.io".to_string(), "kai@x.io".to_string()].into_iter().collect();
        let people = people(&db, "w1", "vera@x.io", &users);
        assert_eq!(people["maya@x.io"], json!({"uid": "u-maya", "label": "maya", "avatar": "https://img/maya.png", "github_login": "maya"}));
        assert_eq!(people["kai@x.io"]["label"], "kai");
        assert_eq!(people["kai@x.io"]["uid"], "");
    }
}
