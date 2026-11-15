//! What the dashboard says about a collision and a change, in sentences.
//!
//! The founder, 2026-11-13, on the Collisions page and the event page:
//! "why? why wont it tell me why properly" and "what?". The list read
//! "kevin was blocked from changing useChannelList · insert × insert", the
//! "Why" went to the symbol's edit history, and the change page showed
//! content hashes and "+0 −0". This module answers from the ledger row and
//! what Cork recorded around it:
//!
//! - a **collision** (an `op_conflict` row from the protocol, or a
//!   `gate_block` from the gate): what this side tried, whose op it met and
//!   when, pushed or only in their copy, what Cork did and why, whether the
//!   two sides were the same code, and how it ended (`block_outcome`);
//! - the **Collisions list**: those rows grouped into bursts, one write's
//!   46 refused symbols one row ("46 symbols in 4 files");
//! - a **change** (`edit_reported`): each symbol added, changed or removed,
//!   where it sits now and its lines in and out where Cork knows them, and
//!   plainly what Cork does not know (a file's first report has no lines).
//!
//! A commit Cork read rather than an agent's write (op ids `git.<sha>.<n>`,
//! from a hook's `<session>.ingest` side session) says so, with the commit
//! linked where the repo is on GitHub.
//!
//! Names and line numbers only, as everywhere on the dashboard: no hunk,
//! hash or signature goes out, and a viewer's answer is redacted like every
//! other read (`access::redact_for_viewer`).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::store::{LedgerRow, Store};

/// One writer's refused ops this close together are one burst: a write of
/// a file (or a commit's files) is refused key by key, within seconds.
pub const BURST_GAP_S: f64 = 30.0;
/// The Collisions list's window: the last day, as the inbox has always been.
pub const LIST_WINDOW_S: f64 = 86_400.0;
/// How far before a collision Cork looks for the other side's edit.
const THEIR_EDIT_S: f64 = 86_400.0;
const MAX_GROUPS: usize = 100;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn num(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// What one of the protocol's op kinds (or the gate's change kinds) is, as
/// a verb: (to do it, done).
fn verbs(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "insert" | "added" => ("add", "added"),
        "modify_body" => ("change", "changed"),
        "signature_change" | "signature" => ("change the signature of", "changed the signature of"),
        "rename" | "renamed" => ("rename", "renamed"),
        "delete" | "removed" => ("remove", "removed"),
        "move" | "moved" => ("move", "moved"),
        "file_replace" => ("rewrite", "rewrote"),
        _ => ("change", "changed"),
    }
}

/// "3 minutes", "40 seconds", "2 hours".
fn span_words(seconds: f64) -> String {
    let s = seconds.abs();
    let (n, unit) = if s < 90.0 {
        (s.round().max(1.0), "second")
    } else if s < 5400.0 {
        ((s / 60.0).round(), "minute")
    } else if s < 172_800.0 {
        ((s / 3600.0).round(), "hour")
    } else {
        ((s / 86_400.0).round(), "day")
    };
    format!("{} {unit}{}", n as i64, if n as i64 == 1 { "" } else { "s" })
}

/// The harness an agent runs in, from what its session reported.
fn tool_name(agent: &str, model: &str) -> &'static str {
    let (a, m) = (agent.to_lowercase(), model.to_lowercase());
    if a.contains("cursor") {
        "Cursor"
    } else if a.contains("codex") || a.contains("openai") {
        "Codex"
    } else if a.contains("hermes") {
        "Hermes"
    } else if a.contains("gemini") {
        "Gemini CLI"
    } else if a.contains("claude") || ["claude", "opus", "sonnet", "haiku", "fable"].iter().any(|w| m.contains(w)) {
        "Claude Code"
    } else if ["gpt", "codex", "o3", "o4"].iter().any(|w| m.contains(w)) {
        "Codex"
    } else {
        ""
    }
}

/// The commit an op came from: `git.<sha>.<n>`.
fn commit_of(op_id: &str) -> Option<String> {
    let rest = op_id.strip_prefix("git.")?;
    let sha = rest.split('.').next()?;
    (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())).then(|| sha.to_string())
}

/// A user and session as rows name them (`user#session`, a side session
/// taken back to its agent's).
fn split_agent(agent: &str, person: &str, session: &str) -> (String, String) {
    match agent.split_once('#') {
        Some((user, session)) => (user.to_string(), crate::protocol_ws::agent_session(session).to_string()),
        None => (person.to_string(), crate::protocol_ws::agent_session(session).to_string()),
    }
}

/// An agent, as a sentence names it.
#[derive(Clone, Debug, Default)]
struct Who {
    user: String,
    session: String,
    /// the reader's name for the person
    label: String,
    tool: String,
    title: String,
}

impl Who {
    /// "kevin's Claude Code (session "Channels")".
    fn named(&self) -> String {
        let tool = if self.tool.is_empty() { "agent" } else { self.tool.as_str() };
        let title = if self.title.is_empty() { String::new() } else { format!(" (session \"{}\")", self.title) };
        format!("{}'s {tool}{title}", self.label)
    }

    /// "kevin's agent", or "another of kevin's agents" beside one of theirs.
    fn short(&self, beside: &Who) -> String {
        if self.user == beside.user && !self.session.is_empty() && self.session != beside.session {
            format!("another of {}'s agents", self.label)
        } else if self.user == beside.user && self.session == beside.session && !self.session.is_empty() {
            "the same agent".to_string()
        } else {
            format!("{}'s agent", self.label)
        }
    }

    fn value(&self) -> Value {
        json!({"user": self.user, "session": self.session, "label": self.label, "tool": self.tool, "title": self.title})
    }
}

/// What explaining needs, read once per answer.
pub struct Ctx<'a> {
    store: &'a Store,
    scope: &'a str,
    /// the repo as the dashboard names it: a GitHub repo links its commits
    repo_id: &'a str,
    reader: &'a str,
    labels: BTreeMap<String, String>,
    agents: BTreeMap<(String, String), (String, String)>,
    /// edits, promotions and settled outcomes since `since`, oldest first
    rows: Vec<LedgerRow>,
}

impl<'a> Ctx<'a> {
    pub fn new(store: &'a Store, scope: &'a str, repo_id: &'a str, reader: &'a str, since: f64) -> Ctx<'a> {
        let rows = store.ledger_since_kinds(scope, since, &["edit_reported", "op_promoted", "block_outcome"]);
        Ctx { store, scope, repo_id, reader, labels: BTreeMap::new(), agents: BTreeMap::new(), rows }
    }

    fn label(&mut self, user: &str) -> String {
        if let Some(label) = self.labels.get(user) {
            return label.clone();
        }
        let one: BTreeSet<String> = [user.to_string()].into_iter().collect();
        let label = crate::activity::identity_labels(self.store, self.reader, &one)
            .get(user)
            .map(|l| text(l, "who"))
            .filter(|w| !w.is_empty())
            .unwrap_or_else(|| user.split('@').next().unwrap_or(user).to_string());
        self.labels.insert(user.to_string(), label.clone());
        label
    }

    /// The agent a user and session were: its harness and its title.
    fn who(&mut self, user: &str, session: &str) -> Who {
        let key = (user.to_string(), session.to_string());
        if !self.agents.contains_key(&key) {
            let (mut tool, mut title) = (String::new(), String::new());
            if !session.is_empty() {
                let last = self.store.eph_get(&crate::presence::last_action_key(self.scope, user, session)).unwrap_or(Value::Null);
                tool = tool_name(&text(&last, "agent"), &text(&last, "model")).to_string();
                if tool.is_empty() {
                    // the session's own edits name the model it ran
                    tool = self
                        .rows
                        .iter()
                        .rev()
                        .filter(|r| r.kind == "edit_reported" && text(&r.payload, "user") == user && text(&r.payload, "session") == session)
                        .map(|r| tool_name("", &text(&r.payload, "model")))
                        .find(|t| !t.is_empty())
                        .unwrap_or("")
                        .to_string();
                }
                title = crate::freshness::title_of(self.store, self.scope, user, session);
            }
            self.agents.insert(key.clone(), (tool, title));
        }
        let (tool, title) = self.agents[&key].clone();
        Who { user: user.to_string(), session: session.to_string(), label: self.label(user), tool, title }
    }

    /// A GitHub link for a commit, when the repo is on GitHub.
    fn commit_value(&self, sha: &str) -> Value {
        let short: String = sha.chars().take(7).collect();
        let url = self
            .repo_id
            .strip_prefix("github.com/")
            .map(|repo| format!("https://github.com/{}/commit/{sha}", repo.trim_end_matches(".git")))
            .unwrap_or_default();
        json!({"sha": sha, "short": short, "url": url})
    }

    /// The other side's newest edit of `path` at or before `at`.
    fn their_edit(&self, user: &str, session: &str, path: &str, at: f64) -> Option<&LedgerRow> {
        self.rows.iter().rev().find(|r| {
            r.kind == "edit_reported"
                && r.ts <= at + 2.0
                && at - r.ts < THEIR_EDIT_S
                && text(&r.payload, "path") == path
                && text(&r.payload, "user") == user
                && (session.is_empty() || text(&r.payload, "session") == session)
        })
    }

    /// Whether `user` had pushed `key` by `at`.
    fn pushed_by(&self, user: &str, key: &str, at: f64) -> bool {
        self.rows.iter().any(|r| {
            r.kind == "op_promoted" && r.ts <= at && text(&r.payload, "by") == user && strings(r.payload.get("keys")).iter().any(|k| k == key)
        })
    }

    /// How a block of this agent on this path ended: a settled outcome, one
    /// still open, or nothing Cork tracked.
    fn outcome(&self, user: &str, session: &str, path: &str, at: f64) -> (String, f64, String) {
        let names = |v: &Value| strings(v.get("paths")).iter().any(|p| p == path);
        let settled = self.rows.iter().find(|r| {
            r.kind == "block_outcome"
                && text(&r.payload, "user") == user
                && text(&r.payload, "session") == session
                && names(&r.payload)
                && num(&r.payload, "opened") <= at + 10.0
                && num(&r.payload, "settled_at") >= at - 10.0
        });
        if let Some(row) = settled {
            return (text(&row.payload, "outcome"), num(&row.payload, "settled_at"), text(&row.payload, "settled_by"));
        }
        let prefix = format!("{}|{}|", self.scope, crate::presence::agent_id(user, session));
        let open = self.store.kv_list(crate::outcomes::BUCKET, &prefix).into_iter().any(|(_, record)| {
            names(&record) && num(&record, "opened") <= at + 10.0
        });
        if open { ("pending".into(), 0.0, String::new()) } else { (String::new(), 0.0, String::new()) }
    }
}

/// How it ended, in a sentence.
fn outcome_words(state: &str, by: &str, path: &str, their: &str) -> String {
    let file = basename(path);
    match state {
        "adapted" => format!("It read {file} again and wrote it with {their} change in it."),
        "overrode" => format!("It wrote {file} anyway, without taking {their} change in."),
        "waited" => format!("It waited until {their} work was done, then wrote {file}."),
        "abandoned" if by == "session_end" => format!("Its session ended before it wrote {file} again."),
        "abandoned" => format!("It left {file} alone: no write to it in the half hour after."),
        "pending" => "Not settled yet: Cork is waiting to see what the agent writes next.".into(),
        _ => String::new(),
    }
}

// ------------------------------------------------------------ collisions

/// One collision explained, from its ledger row: an `op_conflict` or a
/// `gate_block`. None for any other row.
pub fn collision(ctx: &mut Ctx, row: &LedgerRow) -> Option<Value> {
    match row.kind.as_str() {
        "op_conflict" => Some(protocol_collision(ctx, row)),
        "gate_block" => Some(gate_block(ctx, row)),
        _ => None,
    }
}

fn protocol_collision(ctx: &mut Ctx, row: &LedgerRow) -> Value {
    let p = &row.payload;
    let at = row.ts;
    let path = text(p, "path");
    let symbol = Some(text(p, "symbol")).filter(|s| !s.is_empty()).unwrap_or_else(|| basename(&path).to_string());
    let key = text(p, "key");
    let kind = text(p, "kind");
    let reason = text(p, "reason");
    let stage = Some(text(p, "stage")).filter(|s| !s.is_empty()).unwrap_or_else(|| "write".into());
    let raw_session = text(p, "writer_session");
    let commit = commit_of(&text(p, "op_id"));
    let from_commit = commit.is_some() || raw_session.ends_with(".ingest");
    let (wuser, wsession) = split_agent(&text(p, "writer_agent"), &text(p, "writer"), &raw_session);
    let mut writer = ctx.who(&wuser, &wsession);
    if from_commit {
        // the writer is Cork's reader of the commit, not the agent whose
        // session it ran beside
        writer.tool.clear();
        writer.title.clear();
    }
    let (now_verb, done_verb) = verbs(&kind);

    // the other side: the lease's holder, or the newest op it met
    let leased = reason == "leased";
    let theirs: Vec<Value> = p.get("theirs").and_then(Value::as_array).cloned().unwrap_or_default();
    let newest = theirs.last().cloned().unwrap_or(Value::Null);
    let (tuser, tsession) = if leased || newest.is_null() {
        split_agent(&text(p, "holder_agent"), &text(p, "holder"), &text(p, "holder_session"))
    } else {
        split_agent(&text(&newest, "agent"), &text(&newest, "author"), &text(&newest, "session"))
    };
    let their_kind = text(&newest, "kind");
    let their_commit = commit_of(&text(&newest, "op_id"));
    let their = if tuser.is_empty() { Who { label: "another agent".into(), ..Who::default() } } else { ctx.who(&tuser, &tsession) };
    let their_edit = (!tuser.is_empty()).then(|| ctx.their_edit(&tuser, &tsession, &path, at).map(|r| (r.ts, r.seq))).flatten();
    let their_at = newest.get("at").and_then(Value::as_f64).filter(|t| *t > 0.0).or(their_edit.map(|(ts, _)| ts));
    let their_pushed = newest.get("pushed").and_then(Value::as_bool).or_else(|| ctx.pushed_by(&tuser, &key, at).then_some(true));
    let identical = p.get("identical").and_then(Value::as_bool);

    let this_named = match &commit {
        Some(sha) => format!("Cork's reader of {}'s commit {}", writer.label, &sha[..7]),
        None if from_commit => format!("Cork's reader of {}'s commits", writer.label),
        None => writer.named(),
    };
    let their_named = match &their_commit {
        Some(sha) => format!("{}'s commit {}", their.label, &sha[..7]),
        None => if their.session.is_empty() { format!("{}'s agent", their.label) } else { their.named() },
    };
    let their_short = match &their_commit {
        Some(sha) => format!("commit {}", &sha[..7]),
        None if from_commit => format!("{}'s agent", their.label),
        None => their.short(&writer),
    };
    // whose change it was, as "before {owner}'s change" says it
    let their_owner = match &their_commit {
        Some(sha) => format!("commit {}", &sha[..7]),
        None => their.label.clone(),
    };

    let (_, their_done) = verbs(&their_kind);
    let headline = if leased {
        format!("{this_named} tried to {now_verb} {symbol} while {their_short} was editing it")
    } else if !their_kind.is_empty() && verbs(&their_kind) == verbs(&kind) && !from_commit {
        format!("{this_named} tried to {now_verb} {symbol}, which {their_short} had already {their_done}")
    } else if !their_kind.is_empty() {
        format!("{this_named} tried to {now_verb} {symbol}, which {their_short} had {their_done} first")
    } else {
        format!("{this_named} tried to {now_verb} {symbol}, and met {their_short}'s change to it")
    };

    let mut sentences: Vec<String> = Vec::new();
    // 1. what this side tried
    sentences.push(match (&commit, from_commit) {
        (Some(sha), _) => format!(
            "Cork reads each commit that arrives with a pull, so its record follows work no agent reported. It read {}'s commit {} and tried to record that it {done_verb} {symbol} in {path}.",
            writer.label, &sha[..7]
        ),
        (None, true) => format!("Cork read a commit on {}'s machine and tried to record that it {done_verb} {symbol} in {path}.", writer.label),
        (None, false) => format!("{} tried to {now_verb} {symbol} in {path}.", writer.named()),
    });
    // 2. whose op it met
    let when = their_at.map(|t| format!(" {} earlier", span_words(at - t))).unwrap_or_default();
    let pushed_words = match their_pushed {
        Some(true) => ", and had pushed it",
        Some(false) => ". It was only in their copy, not pushed",
        None => "",
    };
    sentences.push(if leased {
        format!("{their_named} was editing {symbol} at that moment: it held the lease on it.")
    } else if their_kind.is_empty() {
        format!("{their_named} had changed {symbol}{when}{pushed_words}.")
    } else {
        format!("{their_named} had {their_done} {symbol}{when}{pushed_words}.")
    });
    // 3. what Cork did, and why
    sentences.push(if from_commit {
        format!("Cork turned the commit's version of {symbol} away, because the record already held a newer one. No agent was writing, so nothing was stopped.")
    } else if stage == "gate" {
        let because = if leased { format!("{} was in the middle of it", their.label) } else { format!("its copy did not have {} change yet", possessive(&their_owner)) };
        format!("Cork stopped the write before it reached the file, because {because}, and told the agent why. A retry goes through.")
    } else {
        format!("The write had already landed in the agent's copy. Cork did not record it as the next version of {symbol}, because it was based on a version before {} change, and told the agent to read that first.", possessive(&their_owner))
    });
    // 4. the same code on both sides?
    match identical {
        Some(true) => sentences.push("Both sides were the same code: the same change arriving twice, not a real collision.".into()),
        Some(false) => sentences.push("The two sides were different code.".into()),
        None if from_commit && their.user == writer.user => sentences.push(format!(
            "Both sides are {}'s: the commit carried work {}'s agent had already reported.",
            writer.label, writer.label
        )),
        None => {}
    }
    // 5. how it ended
    let (state, settled_at, by) = if from_commit {
        (String::new(), 0.0, String::new())
    } else {
        ctx.outcome(&writer.user, &writer.session, &path, at)
    };
    let ending = if from_commit {
        "There was nothing to settle: no agent was waiting on it.".to_string()
    } else {
        outcome_words(&state, &by, &path, &possessive(&their_owner))
    };
    if !ending.is_empty() {
        sentences.push(ending);
    }

    let mut value = json!({
        "type": "collision", "source": "protocol", "seq": row.seq, "ts": at,
        "path": path, "symbol": symbol, "stage": stage, "reason": reason,
        "did": kind, "their_did": their_kind,
        "headline": headline, "sentences": sentences,
        "this": writer.value(), "theirs": their.value(),
        "their_at": their_at, "their_pushed": their_pushed, "their_seq": their_edit.map(|(_, seq)| seq),
        "identical": identical, "from_commit": from_commit,
        "outcome": {"state": state, "at": settled_at},
    });
    if let Some(sha) = &commit {
        value["commit"] = ctx.commit_value(sha);
    }
    value
}

/// "kevin's agent" -> "kevin's agent's"; "commit b266fed" -> "commit b266fed's".
fn possessive(name: &str) -> String {
    format!("{name}'s").replace("'s's", "'s")
}

fn gate_block(ctx: &mut Ctx, row: &LedgerRow) -> Value {
    let p = &row.payload;
    let at = row.ts;
    let path = text(p, "path");
    let file = basename(&path).to_string();
    let writer = ctx.who(&text(p, "user"), &text(p, "session"));
    let mut sentences: Vec<String> = Vec::new();
    let (headline, their) = if p.get("stale_copy").and_then(Value::as_bool) == Some(true) {
        let by = text(p, "by");
        let their = if by.is_empty() { Who { label: "a teammate".into(), ..Who::default() } } else { ctx.who(&by, "") };
        let written = num(p, "written_at");
        let when = if written > 0.0 { format!(" {} before", span_words(at - written)) } else { String::new() };
        let pushed = if p.get("pushed").and_then(Value::as_bool) == Some(true) { " and pushed it" } else { "" };
        sentences.push(format!("{} was about to write {path}.", writer.named()));
        sentences.push(format!("{}'s agent had changed {file}{when}{pushed}, and the copy this write came from did not have that change.", their.label));
        sentences.push(format!("Cork stopped the write once, so it would not undo {}'s change, and told the agent to read {file} again.", their.label));
        (format!("{} was about to write {file} from an old copy: {}'s agent had changed it since", writer.named(), their.label), their)
    } else {
        let events: Vec<Value> = p.get("events").and_then(Value::as_array).cloned().unwrap_or_default();
        let first = events.first().cloned().unwrap_or(Value::Null);
        let author = text(&first, "author");
        let their = if author.is_empty() { Who { label: "a teammate".into(), ..Who::default() } } else { ctx.who(&author, "") };
        let changes: Vec<String> = events
            .iter()
            .take(4)
            .map(|e| {
                let (_, done) = verbs(&text(e, "kind"));
                let to = text(e, "new_name");
                if to.is_empty() { format!("{done} {}", text(e, "symbol")) } else { format!("{done} {} to {to}", text(e, "symbol")) }
            })
            .collect();
        let what = if changes.is_empty() { format!("changed {file}") } else { changes.join(", ") };
        sentences.push(format!("{} was about to write {path}.", writer.named()));
        sentences.push(format!("{}'s agent had {what}, and the write would have undone it.", their.label));
        sentences.push(format!("Cork stopped the write once and told the agent what {} had changed, so it could write against the new version.", their.label));
        (format!("{} was about to write {file} without {}'s change: {what}", writer.named(), their.label), their)
    };
    let (state, settled_at, by) = ctx.outcome(&writer.user, &writer.session, &path, at);
    let ending = outcome_words(&state, &by, &path, &format!("{}'s", their.label));
    if !ending.is_empty() {
        sentences.push(ending);
    }
    json!({
        "type": "collision", "source": "gate", "seq": row.seq, "ts": at,
        "path": path, "symbol": "", "stage": "gate", "reason": if p.get("stale_copy").is_some() { "stale_copy" } else { "teammate_change" },
        "headline": headline, "sentences": sentences,
        "this": writer.value(), "theirs": their.value(),
        "identical": Value::Null, "from_commit": false,
        "outcome": {"state": state, "at": settled_at},
    })
}

/// The Collisions list: the last day's collisions, newest first, one
/// writer's burst (one write, or one commit, refused key by key) as one
/// group with its count and every item explained.
pub fn collisions(store: &Store, scope: &str, repo_id: &str, reader: &str, since: f64) -> Value {
    let rows: Vec<LedgerRow> = store
        .ledger_since_kinds(scope, since, &["op_conflict", "gate_block"])
        .into_iter()
        // a stale write (an unknown base, a lapsed lease) is the waste
        // report's, not a clash between two agents
        .filter(|r| r.kind == "gate_block" || matches!(text(&r.payload, "reason").as_str(), "conflict" | "leased"))
        // nor was a commit's ops refused, or the same code refused twice
        // (`explain` still answers for one of them by its seq)
        .filter(|r| r.kind != "op_conflict" || !crate::protocol_ws::no_clash(&r.payload))
        .collect();
    let mut ctx = Ctx::new(store, scope, repo_id, reader, since - THEIR_EDIT_S);
    let mut explained: Vec<Value> = rows.iter().filter_map(|row| collision(&mut ctx, row)).collect();
    explained.sort_by(|a, b| num(a, "ts").partial_cmp(&num(b, "ts")).unwrap_or(std::cmp::Ordering::Equal));

    // bursts: the same writer and source, each item within BURST_GAP_S of the last
    let burst_key = |item: &Value| -> String {
        let commit = item.pointer("/commit/sha").and_then(Value::as_str).unwrap_or("");
        format!("{}#{}|{}|{commit}", text(&item["this"], "user"), text(&item["this"], "session"), text(item, "source"))
    };
    let mut groups: Vec<Vec<Value>> = Vec::new();
    for item in explained {
        let key = burst_key(&item);
        match groups.iter_mut().rev().find(|g| burst_key(&g[0]) == key) {
            Some(group) if num(&item, "ts") - num(group.last().unwrap(), "ts") <= BURST_GAP_S => group.push(item),
            _ => groups.push(vec![item]),
        }
    }
    let total = groups.iter().map(Vec::len).sum::<usize>();
    let mut out: Vec<Value> = groups.into_iter().map(group_value).collect();
    out.sort_by(|a, b| num(b, "ts").partial_cmp(&num(a, "ts")).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(MAX_GROUPS);
    json!({"groups": out, "total": total, "since": since})
}

/// A burst as one row: its headline names what all its items share.
fn group_value(items: Vec<Value>) -> Value {
    let first = items[0].clone();
    let last_ts = items.iter().map(|i| num(i, "ts")).fold(0.0, f64::max);
    if items.len() == 1 {
        return json!({
            "id": format!("{}", first["seq"]), "ts": last_ts, "count": 1, "files": 1, "symbols": 1,
            "headline": first["headline"], "this": first["this"], "theirs": first["theirs"],
            "source": first["source"], "commit": first.get("commit").cloned().unwrap_or(Value::Null),
            "identical": first["identical"], "outcome": first["outcome"], "from_commit": first["from_commit"],
            "items": items,
        });
    }
    let files: BTreeSet<String> = items.iter().map(|i| text(i, "path")).collect();
    let symbols = items.len();
    let theirs: BTreeSet<String> = items.iter().map(|i| format!("{}#{}", text(&i["theirs"], "user"), text(&i["theirs"], "session"))).collect();
    let kinds: BTreeSet<(String, String)> = items.iter().map(|i| (text(i, "did"), text(i, "their_did"))).collect();
    let this: Value = first["this"].clone();
    let label = text(&this, "label");
    let what = format!("{symbols} symbols in {} file{}", files.len(), if files.len() == 1 { "" } else { "s" });
    let who = match first.pointer("/commit/short").and_then(Value::as_str) {
        Some(short) => format!("Cork's reader of {label}'s commit {short}"),
        None if first["from_commit"] == json!(true) => format!("Cork's reader of {label}'s commits"),
        None => {
            let tool = text(&this, "tool");
            let title = text(&this, "title");
            format!(
                "{label}'s {}{}",
                if tool.is_empty() { "agent" } else { &tool },
                if title.is_empty() { String::new() } else { format!(" (session \"{title}\")") }
            )
        }
    };
    let their_label = text(&first["theirs"], "label");
    let headline = if text(&first, "source") == "gate" {
        format!("{who} was stopped {} times, on {}", symbols, what.replacen(&format!("{symbols} symbols in "), "", 1))
    } else if kinds.len() == 1 && theirs.len() == 1 {
        let (did, their_did) = kinds.iter().next().unwrap();
        let (now_verb, _) = verbs(did);
        let (_, their_done) = verbs(their_did);
        let same = if their_label == label { format!("{label}'s own agent") } else { format!("{their_label}'s agent") };
        format!("{who} tried to {now_verb} {what}, which {same} had already {their_done}")
    } else {
        format!("{who} tried to change {what} that other agents had changed first")
    };
    let identical = if items.iter().all(|i| i["identical"] == json!(true)) {
        json!(true)
    } else if items.iter().any(|i| i["identical"] == json!(false)) {
        json!(false)
    } else {
        Value::Null
    };
    json!({
        "id": format!("{}-{}", first["seq"], items.last().unwrap()["seq"]), "ts": last_ts,
        "count": items.len(), "files": files.len(), "symbols": symbols,
        "headline": headline, "this": this, "theirs": first["theirs"], "source": first["source"],
        "commit": first.get("commit").cloned().unwrap_or(Value::Null),
        "identical": identical, "outcome": first["outcome"], "from_commit": first["from_commit"],
        "items": items,
    })
}

// --------------------------------------------------------------- changes

/// One reported edit explained: who, each symbol added, changed or
/// removed with where it is now and its lines in and out where Cork knows
/// them, and plainly what Cork does not know.
pub fn change(ctx: &mut Ctx, row: &LedgerRow) -> Value {
    let p = &row.payload;
    let path = text(p, "path");
    let who = ctx.who(&text(p, "user"), &text(p, "session"));
    let changed: Map<String, Value> = p.get("symbols_changed").and_then(Value::as_object).cloned().unwrap_or_default();
    let events: Vec<Value> = p.get("events").and_then(Value::as_array).cloned().unwrap_or_default();
    let hunks: Vec<Value> = p.get("hunks").and_then(Value::as_array).cloned().unwrap_or_default();
    let added_lines = p.get("lines_added").and_then(Value::as_i64).unwrap_or(0);
    let removed_lines = p.get("lines_removed").and_then(Value::as_i64).unwrap_or(0);
    let lines_known = !hunks.is_empty() || added_lines + removed_lines > 0;
    // a file's first report: every symbol is new to Cork and there was no
    // earlier copy to diff against
    let first_sight = !changed.is_empty() && events.is_empty() && !lines_known && changed.values().all(|d| d.get("before").is_none_or(Value::is_null));
    let earlier = ctx.rows.iter().any(|r| r.kind == "edit_reported" && r.seq < row.seq && text(&r.payload, "path") == path);
    let new_file = first_sight && !earlier;

    let graph = crate::graphview::snapshot(ctx.store, ctx.scope);
    let spans = crate::tree::spans_of(&graph, &path);
    let ranges: Vec<(String, i64, i64, i64, i64)> = hunks
        .iter()
        .filter(|h| text(h, "op") != "truncated")
        .map(|h| {
            let at = |k: &str| h.get(k).and_then(Value::as_i64).unwrap_or(0);
            (text(h, "op"), at("old_start"), at("old_end"), at("new_start"), at("new_end"))
        })
        .collect();
    let mut symbols: Vec<Value> = changed
        .iter()
        .map(|(name, delta)| {
            let before = delta.get("before").is_some_and(|v| !v.is_null());
            let after = delta.get("after").is_some_and(|v| !v.is_null());
            let event = events.iter().find(|e| text(e, "symbol") == *name).map(|e| text(e, "kind")).unwrap_or_default();
            let change = match (before, after, event.as_str()) {
                (_, _, "renamed") => "renamed",
                (_, _, "signature" | "signature_change") => "signature",
                (false, true, _) => "added",
                (true, false, _) => "removed",
                _ => "changed",
            };
            let span = spans.iter().find(|s| s.name == *name);
            // lines in and out: the hunks that sit wholly inside this symbol
            let (mut plus, mut minus, mut counted) = (0, 0, false);
            if let Some(span) = span {
                for (op, old_start, old_end, new_start, new_end) in &ranges {
                    let (lo, hi) = if new_end < new_start { (*new_end, *new_end) } else { (*new_start, *new_end) };
                    if lo >= span.first && hi <= span.last {
                        counted = true;
                        if op != "delete" {
                            plus += (new_end - new_start + 1).max(0);
                        }
                        if op != "insert" {
                            minus += (old_end - old_start + 1).max(0);
                        }
                    }
                }
            }
            json!({
                "name": name, "kind": span.map(|s| s.kind.clone()).unwrap_or_default(), "change": change,
                "lines": span.map(|s| json!([s.first, s.last])).unwrap_or(Value::Null),
                "plus": if counted { json!(plus) } else { Value::Null },
                "minus": if counted { json!(minus) } else { Value::Null },
            })
        })
        .collect();
    symbols.sort_by_key(|s| s["lines"].get(0).and_then(Value::as_i64).unwrap_or(i64::MAX));

    let file = basename(&path).to_string();
    let n = symbols.len();
    let plural = |n: usize, one: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") };
    let headline = if new_file {
        format!("{} added {file}: a new file, {}", who.named(), plural(n, "symbol"))
    } else if first_sight {
        format!("{} reported {file} to Cork for the first time: {}", who.named(), plural(n, "symbol"))
    } else if n > 0 {
        let added = symbols.iter().filter(|s| s["change"] == "added").count();
        let removed = symbols.iter().filter(|s| s["change"] == "removed").count();
        let rest = n - added - removed;
        let mut parts: Vec<String> = Vec::new();
        if added > 0 {
            parts.push(format!("added {}", plural(added, "symbol")));
        }
        if rest > 0 {
            parts.push(format!("changed {}", plural(rest, "symbol")));
        }
        if removed > 0 {
            parts.push(format!("removed {}", plural(removed, "symbol")));
        }
        format!("{} {} in {file}", who.named(), parts.join(", "))
    } else {
        format!("{} edited {file}", who.named())
    };
    let lines_note = if lines_known {
        String::new()
    } else if first_sight {
        "Cork had no earlier copy of this file to compare with, so it recorded which symbols the file holds, not the lines.".to_string()
    } else if n > 0 {
        "Cork recorded which symbols changed, not the lines.".to_string()
    } else {
        "Cork saw the write, but neither the symbols nor the lines it changed.".to_string()
    };
    json!({
        "type": "change", "seq": row.seq, "ts": row.ts, "path": path,
        "who": who.value(), "headline": headline,
        "symbols": symbols, "first_sight": first_sight, "new_file": new_file,
        "lines_known": lines_known, "lines_added": added_lines, "lines_removed": removed_lines,
        "lines_note": lines_note,
    })
}

/// What a viewer does not get of a change: where each symbol's lines moved
/// is read from the hunks, which a viewer never sees
/// (`access::redact_for_viewer`). The names and what happened to them stay.
pub fn for_viewer(value: &mut Value) {
    for symbol in value.get_mut("symbols").and_then(Value::as_array_mut).into_iter().flatten() {
        for key in ["lines", "plus", "minus"] {
            symbol[key] = Value::Null;
        }
    }
}

/// Any ledger row the dashboard opens: a collision or a change explained;
/// None for a row this module has nothing to say about.
pub fn explain(store: &Store, scope: &str, repo_id: &str, reader: &str, seq: i64) -> Option<Value> {
    let row = store
        .ledger_page(scope, 1, Some(seq + 1))
        .into_iter()
        .find(|r| r.get("seq").and_then(Value::as_i64) == Some(seq))?;
    let row = LedgerRow {
        seq,
        kind: text(&row, "kind"),
        payload: row.get("payload").cloned().unwrap_or(Value::Null),
        ts: num(&row, "ts"),
    };
    let mut ctx = Ctx::new(store, scope, repo_id, reader, row.ts - THEIR_EDIT_S);
    match row.kind.as_str() {
        "edit_reported" => Some(change(&mut ctx, &row)),
        _ => collision(&mut ctx, &row),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::now;

    const SCOPE: &str = "w1:github.com/acme/app";
    const REPO: &str = "github.com/acme/app";

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("store")
    }

    fn agent(db: &Store, user: &str, session: &str, title: &str) {
        db.eph_set(&crate::presence::last_action_key(SCOPE, user, session), &json!({"kind": "edit", "agent": "claude-code", "model": "claude-opus-5-5", "ts": now()}), Some(3600.0)).unwrap();
        db.eph_set(&format!("title:{SCOPE}:{user}:{session}"), &json!(title), Some(3600.0)).unwrap();
    }

    /// An op_conflict row as the protocol writes it.
    fn conflict(db: &Store, at: f64, writer: &str, session: &str, op_id: &str, symbol: &str, path: &str, their_session: &str, extra: Value) -> i64 {
        let mut row = json!({
            "key": format!("code:{path}::{symbol}"), "path": path, "symbol": symbol, "reason": "conflict", "op_id": op_id,
            "kind": "insert", "writer": writer, "writer_agent": format!("{writer}#{session}"), "writer_session": session,
            "holder": "", "holder_agent": "", "holder_session": "",
            "theirs": [{"op_id": "a1", "kind": "insert", "version": 1, "author": "kevin@x.io", "agent": format!("kevin@x.io#{their_session}"), "session": their_session}],
            "stage": "write", "ts": at,
        });
        if let (Some(map), Some(more)) = (row.as_object_mut(), extra.as_object()) {
            map.extend(more.clone());
        }
        db.ledger_append(SCOPE, "op_conflict", &row, at).unwrap();
        db.ledger_page(SCOPE, 1, None)[0]["seq"].as_i64().unwrap()
    }

    #[test]
    fn a_commit_replaying_an_agents_own_work_says_so_in_plain_words() {
        let db = store();
        agent(&db, "kevin@x.io", "s1", "Channels");
        let t = now() - 120.0;
        db.ledger_append(SCOPE, "edit_reported", &json!({"user": "kevin@x.io", "session": "s1", "path": "channels/use-channels.ts",
            "symbols_changed": {"useChannelList": {"before": null, "after": "h"}}, "hunks": []}), t - 600.0).unwrap();
        let seq = conflict(&db, t, "kevin@x.io", "s1.ingest", "git.b266fed0c1d2e3f4.3", "useChannelList", "channels/use-channels.ts", "s1", json!({}));
        let out = explain(&db, SCOPE, REPO, "kevin@x.io", seq).unwrap();
        assert_eq!(out["from_commit"], json!(true));
        assert_eq!(out["commit"]["short"], "b266fed");
        assert_eq!(out["commit"]["url"], "https://github.com/acme/app/commit/b266fed0c1d2e3f4");
        assert_eq!(out["headline"], "Cork's reader of kevin's commit b266fed tried to add useChannelList, which kevin's agent had added first");
        let said = out["sentences"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect::<Vec<_>>().join(" ");
        assert!(said.contains("kevin's Claude Code (session \"Channels\") had added useChannelList 10 minutes earlier"), "{said}");
        assert!(said.contains("No agent was writing, so nothing was stopped"), "{said}");
        assert!(said.contains("Both sides are kevin's: the commit carried work kevin's agent had already reported"), "{said}");
        assert!(!said.contains('×') && !said.contains("insert"), "no op-kind jargon: {said}");
    }

    #[test]
    fn an_agents_refused_write_names_both_sides_what_cork_did_and_how_it_ended() {
        let db = store();
        agent(&db, "maya@x.io", "m1", "Paging");
        agent(&db, "kevin@x.io", "s1", "Channels");
        let t = now() - 300.0;
        let seq = conflict(&db, t, "maya@x.io", "m1", "op-7", "listCollisions", "api/collisions.ts", "s1",
            json!({"kind": "modify_body", "identical": false, "theirs": [{"op_id": "a1", "kind": "modify_body", "author": "kevin@x.io",
                   "agent": "kevin@x.io#s1", "session": "s1", "at": t - 45.0, "pushed": false}]}));
        db.ledger_append(SCOPE, "block_outcome", &json!({"kind": "protocol_stale", "outcome": "adapted", "settled_by": "write",
            "user": "maya@x.io", "session": "m1", "paths": ["api/collisions.ts"], "opened": t, "settled_at": t + 40.0}), t + 40.0).unwrap();
        let out = explain(&db, SCOPE, REPO, "maya@x.io", seq).unwrap();
        assert_eq!(out["headline"], "maya's Claude Code (session \"Paging\") tried to change listCollisions, which kevin's agent had already changed");
        let said: Vec<String> = out["sentences"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect();
        assert_eq!(said[1], "kevin's Claude Code (session \"Channels\") had changed listCollisions 45 seconds earlier. It was only in their copy, not pushed.");
        assert!(said[2].starts_with("The write had already landed in the agent's copy."), "{}", said[2]);
        assert_eq!(said[3], "The two sides were different code.");
        assert_eq!(said[4], "It read collisions.ts again and wrote it with kevin's change in it.");
        assert_eq!(out["outcome"]["state"], "adapted");
    }

    #[test]
    fn a_burst_from_one_write_is_one_row_with_a_count() {
        let db = store();
        agent(&db, "kevin@x.io", "s1", "Channels");
        agent(&db, "maya@x.io", "m1", "Paging");
        let t = now() - 200.0;
        for (i, (sym, path)) in [("useChannelList", "a/one.ts"), ("ChannelRow", "a/one.ts"), ("send", "b/two.ts")].iter().enumerate() {
            conflict(&db, t + i as f64, "maya@x.io", "m1", &format!("m1.4.{i}"), sym, path, "s1", json!({"identical": false}));
        }
        // a lone refusal by the same agent, later, stays its own row
        conflict(&db, t + 100.0, "maya@x.io", "m1", "m1.5.0", "send", "b/two.ts", "s1", json!({}));
        // a stale write is not a collision
        db.ledger_append(SCOPE, "op_conflict", &json!({"reason": "unknown_base", "path": "c.ts", "writer": "maya@x.io"}), t + 5.0).unwrap();
        // nor was a commit's replay of an agent's own work, or the same code
        // refused twice: off the list, still explained one by one
        let replay = conflict(&db, t + 50.0, "kevin@x.io", "s1.ingest", "git.b266fed0c1d2e3f4.0", "Inbox", "c/three.ts", "s1", json!({"identical": true}));
        conflict(&db, t + 60.0, "kevin@x.io", "s2", "s2.1.0", "Inbox", "c/three.ts", "s1", json!({"identical": true}));
        let list = collisions(&db, SCOPE, REPO, "kevin@x.io", now() - LIST_WINDOW_S);
        let groups = list["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "{list:#}");
        assert_eq!(list["total"], 4);
        let burst = &groups[1];
        assert_eq!(burst["count"], 3);
        assert_eq!((burst["files"].as_i64(), burst["symbols"].as_i64()), (Some(2), Some(3)));
        assert_eq!(burst["headline"], "maya's Claude Code (session \"Paging\") tried to add 3 symbols in 2 files, which kevin's agent had already added");
        assert_eq!(burst["identical"], json!(false));
        assert_eq!(burst["items"].as_array().unwrap().len(), 3);
        assert_eq!(groups[0]["count"], 1);
        assert_eq!(explain(&db, SCOPE, REPO, "kevin@x.io", replay).unwrap()["from_commit"], json!(true));
    }

    #[test]
    fn a_lease_clash_at_the_gate_says_who_was_in_the_middle_of_it() {
        let db = store();
        agent(&db, "maya@x.io", "m1", "Paging");
        let t = now() - 30.0;
        let seq = conflict(&db, t, "maya@x.io", "m1", "", "claim", "hooks/claim.ts", "s1",
            json!({"reason": "leased", "stage": "gate", "kind": "modify_body", "theirs": [],
                   "holder": "kevin@x.io", "holder_agent": "kevin@x.io#s1.d812", "holder_session": "s1.d812"}));
        let out = explain(&db, SCOPE, REPO, "maya@x.io", seq).unwrap();
        assert_eq!(out["headline"], "maya's Claude Code (session \"Paging\") tried to change claim while kevin's agent was editing it");
        let said = out["sentences"].as_array().unwrap();
        assert!(said[1].as_str().unwrap().contains("was editing claim at that moment"), "{said:?}");
        assert!(said[2].as_str().unwrap().starts_with("Cork stopped the write before it reached the file, because kevin was in the middle of it"), "{said:?}");
        assert_eq!(out["theirs"]["session"], "s1", "the hook's side session is its agent's");
    }

    #[test]
    fn a_gate_block_on_a_stale_copy_explains_itself() {
        let db = store();
        agent(&db, "maya@x.io", "m1", "Paging");
        let t = now() - 100.0;
        db.ledger_append(SCOPE, "gate_block", &json!({"user": "maya@x.io", "session": "m1", "path": "api/routes.ts", "events": [],
            "stale_copy": true, "by": "kevin@x.io", "pushed": true, "written_at": t - 180.0}), t).unwrap();
        let list = collisions(&db, SCOPE, REPO, "maya@x.io", now() - LIST_WINDOW_S);
        let item = &list["groups"][0]["items"][0];
        assert_eq!(item["headline"], "maya's Claude Code (session \"Paging\") was about to write routes.ts from an old copy: kevin's agent had changed it since");
        assert_eq!(item["sentences"][1], "kevin's agent had changed routes.ts 3 minutes before and pushed it, and the copy this write came from did not have that change.");
        assert_eq!(item["outcome"]["state"], "");
    }

    #[test]
    fn a_files_first_report_is_a_new_file_with_its_symbols_not_plus_zero_minus_zero() {
        let db = store();
        agent(&db, "kevin@x.io", "s1", "Channels");
        let mut symbols = Map::new();
        for (name, a, b) in [("Conversation", 20, 140), ("POLL_MS", 8, 8), ("merge", 150, 170)] {
            symbols.insert(name.into(), json!({"kind": if name == "POLL_MS" { "constant" } else { "function" }, "edges": [], "span": [a, b], "params": [], "sites": []}));
        }
        crate::codegraph::update_file(&db, SCOPE, "channels/Conversation.tsx", json!({"path": "channels/Conversation.tsx", "language": "typescript", "symbols": symbols, "imports": [], "ts": 1.0}), 1.0);
        db.ledger_append(SCOPE, "edit_reported", &json!({"user": "kevin@x.io", "session": "s1", "path": "channels/Conversation.tsx",
            "symbols_changed": {"Conversation": {"before": null, "after": "7290c4d2d9"}, "POLL_MS": {"before": null, "after": "7af614e192"}, "merge": {"before": null, "after": "acbf7ad0e6"}},
            "hunks": [], "events": [], "lines_added": 0, "lines_removed": 0}), now() - 60.0).unwrap();
        let seq = db.ledger_page(SCOPE, 1, None)[0]["seq"].as_i64().unwrap();
        let out = explain(&db, SCOPE, REPO, "kevin@x.io", seq).unwrap();
        assert_eq!(out["headline"], "kevin's Claude Code (session \"Channels\") added Conversation.tsx: a new file, 3 symbols");
        assert_eq!(out["new_file"], json!(true));
        assert_eq!(out["lines_known"], json!(false));
        assert!(out["lines_note"].as_str().unwrap().contains("not the lines"));
        assert_eq!(out["symbols"][0], json!({"name": "POLL_MS", "kind": "constant", "change": "added", "lines": [8, 8], "plus": null, "minus": null}));
        assert!(!out.to_string().contains("7290c4d2d9"), "no hashes");
    }

    #[test]
    fn a_change_counts_each_symbols_lines_where_its_hunks_sit_inside_it() {
        let db = store();
        let mut symbols = Map::new();
        symbols.insert("send".into(), json!({"kind": "function", "edges": [], "span": [10, 30], "params": [], "sites": []}));
        symbols.insert("receive".into(), json!({"kind": "function", "edges": [], "span": [40, 60], "params": [], "sites": []}));
        crate::codegraph::update_file(&db, SCOPE, "b/two.ts", json!({"path": "b/two.ts", "language": "typescript", "symbols": symbols, "imports": [], "ts": 1.0}), 1.0);
        db.ledger_append(SCOPE, "edit_reported", &json!({"user": "maya@x.io", "session": "m1", "path": "b/two.ts",
            "symbols_changed": {"send": {"before": "a", "after": "b"}, "receive": {"before": "c", "after": null}},
            "hunks": [{"op": "replace", "old_start": 12, "old_end": 13, "new_start": 12, "new_end": 15}, {"op": "insert", "old_start": 20, "old_end": 19, "new_start": 22, "new_end": 22}],
            "events": [], "lines_added": 5, "lines_removed": 2}), now() - 60.0).unwrap();
        let seq = db.ledger_page(SCOPE, 1, None)[0]["seq"].as_i64().unwrap();
        let out = explain(&db, SCOPE, REPO, "maya@x.io", seq).unwrap();
        assert_eq!(out["headline"], "maya's agent changed 1 symbol, removed 1 symbol in two.ts");
        let send = out["symbols"].as_array().unwrap().iter().find(|s| s["name"] == "send").unwrap();
        assert_eq!((send["change"].as_str(), send["plus"].as_i64(), send["minus"].as_i64()), (Some("changed"), Some(5), Some(2)));
        assert_eq!(out["lines_note"], "");
        let mut shown = out.clone();
        crate::access::redact_for_viewer(&mut shown);
        for_viewer(&mut shown);
        let shown_send = shown["symbols"].as_array().unwrap().iter().find(|s| s["name"] == "send").unwrap();
        assert_eq!(shown_send["change"], "changed", "a viewer sees what happened to it");
        assert_eq!((shown_send["lines"].clone(), shown_send["plus"].clone()), (Value::Null, Value::Null), "not where its lines moved");
    }

    #[test]
    fn plain_words_for_every_kind() {
        assert_eq!(verbs("insert"), ("add", "added"));
        assert_eq!(verbs("signature_change").1, "changed the signature of");
        assert_eq!(verbs("file_replace").0, "rewrite");
        assert_eq!(span_words(45.0), "45 seconds");
        assert_eq!(span_words(61.0), "61 seconds");
        assert_eq!(span_words(600.0), "10 minutes");
        assert_eq!(span_words(7200.0), "2 hours");
        assert_eq!(commit_of("git.b266fed0c1d2e3f4.12").as_deref(), Some("b266fed0c1d2e3f4"));
        assert_eq!(commit_of("op-1"), None);
        assert_eq!(possessive("kevin's agent"), "kevin's agent's");
        assert_eq!(tool_name("cork-report-hook/104", "gpt-5.5-codex"), "Codex");
    }
}
