//! The live view: who is here, what they are touching, what they have claimed.
//!
//! Everything else in this server answers a question about the code. This
//! answers a question about the people and agents working on it right now,
//! which is what the dashboard's Now panel shows and what an arriving agent
//! reads to know whether it is alone.
//!
//! The room is the WORKSPACE and the row is the AGENT. Presence markers are
//! keyed `focus:{workspace}:{user}#{session}` with the repo in the record, so
//! one read of the workspace prefix lists every agent of every repo — then
//! every record is put through the caller's per-repo allowlist (the
//! `visible` set, a non-optional argument) before it can become a row. A
//! person with two live sessions is two rows; the dashboard groups them.
//!
//! Only ephemeral and shared reads. Nothing here writes, and nothing here is
//! allowed to be expensive: it is polled.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::compat::python_round;
use crate::presence::{agent_id, split_scope};
use crate::store::{now, Store};

/// A workspace is considered online if it has been seen within three minutes.
/// Long enough to survive a slow turn, short enough that a dead agent stops
/// claiming to be present.
const ONLINE_WINDOW_S: f64 = 180.0;
// The protocol block's version, which the setup state is compared against so
// a stale block reports itself rather than silently going out of date — the
// one constant envelope.rs owns, never a second copy that can lag it.
use crate::envelope::BLOCK_VERSION;
const RECENT_EVENTS: usize = 50;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn ts_of(value: &Value) -> f64 {
    value.get("ts").and_then(Value::as_f64).unwrap_or(0.0)
}

/// The first twelve characters of a merkle root — enough to compare two
/// workspaces by eye, short enough to sit in a table cell.
fn short(root: &str) -> String {
    root.chars().take(12).collect()
}

/// `{kind, path, note, age_s}` for the Now panel's status line, with the
/// subagent that made it (`worker`, `worker_type`, `worker_name`) when a
/// subagent did.
fn last_action_view(record: Option<&Value>, stamp: f64) -> Value {
    let Some(record) = record else { return Value::Null };
    let mut view = json!({
        "kind": text(record, "kind"),
        "path": text(record, "path"),
        "note": text(record, "note"),
        "ts": ts_of(record),
        "age_s": python_round(stamp - ts_of(record), 1),
        "via": record.get("via").cloned().unwrap_or(Value::Null),
    });
    // and the branch it was taken on: the timeline's lanes
    for field in ["worker", "worker_type", "worker_name", "branch"] {
        let value = text(record, field);
        if !value.is_empty() {
            view[field] = json!(value);
        }
    }
    // what Cork and the agent exchanged on this step (presence::note_exchange)
    if let Some(x) = record.get("x").filter(|x| x.as_object().is_some_and(|m| !m.is_empty())) {
        view["x"] = x.clone();
    }
    view
}

/// How long a session's step in a repo keeps it on that repo's page after
/// its markers moved to another: a working day.
const ELSEWHERE_WINDOW_S: f64 = 8.0 * 3600.0;

/// Is a marker live at `stamp`? Fresh (under `ONLINE_WINDOW_S`), or a
/// command that started and has not ended, up to the started marker's cap.
/// A long tool call sends nothing between its start and its end, so a
/// three-minute window alone called a session running a test suite idle.
fn marker_live(marker: &Value, stamp: f64) -> bool {
    let age = stamp - ts_of(marker);
    let started = marker.get("started").and_then(Value::as_bool) == Some(true);
    age < ONLINE_WINDOW_S || (started && age < crate::presence::STARTED_TTL_S)
}

/// One live subagent as the roster shows it: who, what it is doing now and
/// when it last moved.
fn worker_view(record: &Value, stamp: f64) -> Value {
    let mut view = json!({
        "worker": text(record, "worker"),
        "type": text(record, "type"),
        "name": text(record, "name"),
        "action": text(record, "action"),
        "path": text(record, "path"),
        "ts": ts_of(record),
        "age_s": python_round(stamp - ts_of(record), 1),
        "live": marker_live(record, stamp),
    });
    if record.get("started").and_then(Value::as_bool) == Some(true) {
        view["started"] = json!(true);
    }
    view
}

/// The newest record under a prefix, by timestamp. Never used for `focus:` —
/// that prefix holds one record PER AGENT, and picking one would fold the
/// agents back into a single row.
fn newest(store: &Store, prefix: &str) -> Option<Value> {
    store
        .eph_scan(prefix)
        .into_iter()
        .map(|(_key, value)| value)
        .max_by(|a, b| ts_of(a).partial_cmp(&ts_of(b)).unwrap_or(std::cmp::Ordering::Equal))
}

/// The newest edit an agent row may claim: only records its own session
/// wrote (`lastedit:` is keyed by person; the record says which session). An
/// agent with no session (an MCP briefing seats the person sessionless)
/// claims only sessionless edits: taking the person's newest put a week-old
/// seat back on the roster as "another session of yours, working on" the
/// file their live session had just written, "just now".
fn own_edit(store: &Store, scope: &str, user: &str, session: &str) -> Option<Value> {
    newest_edit(store, scope, user, Some(session))
}

/// A person-level row (a checkout with no live agent): the person's newest
/// edit, whoever's session wrote it.
fn person_edit(store: &Store, scope: &str, user: &str) -> Option<Value> {
    newest_edit(store, scope, user, None)
}

fn newest_edit(store: &Store, scope: &str, user: &str, session: Option<&str>) -> Option<Value> {
    store
        .eph_scan(&format!("lastedit:{scope}:{user}:"))
        .into_iter()
        .map(|(_key, value)| value)
        .filter(|record| session.is_none_or(|s| text(record, "session") == s))
        .max_by(|a, b| ts_of(a).partial_cmp(&ts_of(b)).unwrap_or(std::cmp::Ordering::Equal))
}

/// What a person is part-way through writing in one repo. A draft is
/// presence, not state: it says symbols are in motion without claiming they
/// landed. Drafts are keyed by person, so every agent row of that person in
/// that repo carries the same one.
fn editing_view(store: &Store, scope: &str, user: &str, stamp: f64) -> Value {
    let Some(latest) = newest(store, &format!("draft:{scope}:{user}:")) else {
        return Value::Null;
    };
    json!({
        "path": text(&latest, "path"),
        "symbols": latest.get("symbols_changed").cloned().unwrap_or(json!([])),
        "parse": text(&latest, "parse"),
        "ts": ts_of(&latest),
        "age_s": python_round(stamp - ts_of(&latest), 1),
        "hunks": latest.get("hunks").cloned().unwrap_or(json!([])),
        "lines_added": latest.get("lines_added").cloned().unwrap_or(json!(0)),
        "lines_removed": latest.get("lines_removed").cloned().unwrap_or(json!(0)),
    })
}

fn focus_view(marker: Option<&Value>, stamp: f64) -> Value {
    let Some(marker) = marker else { return Value::Null };
    // an agent with a focus marker and no action is reading: that is the
    // default a marker means, not a missing value
    let action = match text(marker, "action") {
        empty if empty.is_empty() => "reading".to_string(),
        named => named,
    };
    let mut view = json!({
        "path": text(marker, "path"),
        "action": action,
        "branch": text(marker, "branch"),
        "ts": ts_of(marker),
        "age_s": python_round(stamp - ts_of(marker), 1),
    });
    // a command that has started and not reported its end: running now
    if marker.get("started").and_then(Value::as_bool) == Some(true) {
        view["started"] = json!(true);
    }
    view
}

/// The per-agent markers, gathered under one identity.
#[derive(Default)]
struct Agent {
    user: String,
    session: String,
    focus: Option<Value>,
    last_action: Option<Value>,
    tokens: Option<Value>,
    /// live subagents of this session: worker id -> its marker
    workers: BTreeMap<String, Value>,
}

impl Agent {
    /// The repo this agent is in: the newest of its records', since a
    /// session that `cd`s to another repo moves its markers rather than
    /// leaving two behind. Newest by `ts`, not focus first: a command's
    /// "started" focus stands for half an hour, and taking it first kept a
    /// session that had since acted in another repo placed in the old one.
    /// A tie goes to focus, then the last action.
    fn repo_id(&self) -> String {
        let mut best: Option<(f64, String)> = None;
        for record in [&self.focus, &self.last_action, &self.tokens].into_iter().flatten() {
            let repo = text(record, "repo_id");
            if repo.is_empty() {
                continue;
            }
            let ts = ts_of(record);
            if best.as_ref().is_none_or(|(newest, _)| ts > *newest) {
                best = Some((ts, repo));
            }
        }
        best.map(|(_, repo)| repo).unwrap_or_default()
    }
}

/// Everything one row is built from.
struct RowInput<'a> {
    user: &'a str,
    session: &'a str,
    repo_id: &'a str,
    tree: Option<&'a Value>,
    /// This row's own newest reported edit in this repo (`own_edit`).
    current: Option<&'a Value>,
    focus: Option<&'a Value>,
    last_action: Option<&'a Value>,
    tokens: Option<&'a Value>,
    editing: Value,
    task: String,
    /// live subagents of this session, worker id -> its marker (`type`,
    /// `name`, what it is doing, `ts`, `started`)
    workers: &'a BTreeMap<String, Value>,
}

fn row(input: &RowInput, stamp: f64) -> Value {
    // one rule for both kinds of row: the newest record that knows a field
    // answers for it. Candidates are the row's own markers and its own newest
    // edit; only a person-level (sessionless) row may also claim the
    // checkout's timestamp — a tree is the person's, not one agent's.
    let sessionless = input.session.is_empty();
    let current = input.current;
    let mut candidates: Vec<&Value> = [input.focus, input.last_action, current].into_iter().flatten().collect();
    candidates.sort_by(|a, b| ts_of(b).partial_cmp(&ts_of(a)).unwrap_or(std::cmp::Ordering::Equal));
    let newest_with = |key: &str| -> String {
        candidates.iter().map(|record| text(record, key)).find(|value| !value.is_empty()).unwrap_or_default()
    };

    let tree_updated = if sessionless {
        input.tree.and_then(|tree| tree.get("updated").and_then(Value::as_f64)).unwrap_or(0.0)
    } else {
        0.0
    };
    // the freshest of: checkout, reported edit, presence. Reporting "16
    // minutes ago" while an agent is actively reading was a lie the tree
    // timestamp alone could not avoid.
    let last_seen = candidates.iter().map(|record| ts_of(record)).fold(tree_updated, f64::max);
    let idle_s = python_round(stamp - last_seen, 1);
    let paused = input.focus.map(|m| text(m, "action")) == Some("limit".into())
        || input.last_action.map(|a| text(a, "kind")) == Some("limit".into());
    // the hook's SessionEnd: the row keeps its spend but is not online and
    // not an agent that is here
    let ended = input.last_action.map(|a| text(a, "kind")) == Some("ended".into());
    let agent = newest_with("agent");
    // the agent's token counter names its model from its first report: an
    // agent whose newest records are commands still running (which carried
    // none) was drawn as nobody's. The effort is the one that record sent
    // with its model: a session that moved from medium to max reads max
    // only once a record of the max turn is its newest
    let modelled = candidates
        .iter()
        .copied()
        .find(|record| !text(record, "model").is_empty())
        .or_else(|| input.tokens.filter(|t| !text(t, "model").is_empty()));
    let model = modelled.map(|record| text(record, "model")).unwrap_or_default();
    let effort = modelled.map(crate::presence::Effort::of).unwrap_or_default();

    // a watcher-reported edit was seen on disk; anything else is the agent's
    // own claim, and the two are different kinds of fact
    let source = match current {
        Some(edit) if text(edit, "agent").starts_with("collide-watch") => "disk",
        Some(_) => "claim",
        None if !candidates.is_empty() || !input.editing.is_null() || input.tokens.is_some() => "claim",
        None => "",
    };
    let current_path = current
        .map(|edit| edit.get("path").cloned().unwrap_or(Value::Null))
        .or_else(|| input.focus.map(|m| m.get("path").cloned().unwrap_or(Value::Null)))
        .or_else(|| input.last_action.map(|a| a.get("path").cloned().unwrap_or(Value::Null)))
        .or_else(|| input.editing.get("path").cloned())
        .unwrap_or(Value::Null);
    let root = input.tree.map(|tree| text(tree, "root")).unwrap_or_default();
    let mut worker_types: Vec<String> = input.workers.values().map(|w| text(w, "type")).filter(|t| !t.is_empty()).collect();
    worker_types.sort();
    worker_types.dedup();
    let mut worker_names: Vec<String> = input.workers.values().map(|w| text(w, "name")).filter(|n| !n.is_empty()).collect();
    worker_names.sort();
    worker_names.dedup();
    // liveness: the row's own focus fresh, its own command in flight, or
    // any of its subagents live — a session whose turn ended while its
    // background subagents work is live, and says how many
    let worker_list: Vec<Value> = input.workers.values().map(|w| worker_view(w, stamp)).collect();
    let workers_live = worker_list.iter().filter(|w| w.get("live").and_then(Value::as_bool) == Some(true)).count();
    let running = input.focus.is_some_and(|f| text(f, "action") == "running" && f.get("started").and_then(Value::as_bool) == Some(true) && marker_live(f, stamp));
    let own_live = input.focus.is_some_and(|f| marker_live(f, stamp));
    let live = (own_live || workers_live > 0) && !ended;
    let updated = input
        .tree
        .and_then(|tree| tree.get("updated").cloned())
        .unwrap_or_else(|| json!(last_seen));

    json!({
        "user": input.user,
        "session": input.session,
        "agent_id": agent_id(input.user, input.session),
        "repo_id": input.repo_id,
        "root": root,
        "root_short": short(&root),
        "updated": updated,
        "last_activity_s": idle_s,
        "current_path": current_path,
        "agent": agent,
        "model": model,
        "effort": effort.effort,
        "ultracode": effort.ultracode,
        "branch": newest_with("branch"),
        "task": input.task,
        "tokens_total": input.tokens.and_then(|t| t.get("total").and_then(Value::as_i64)).unwrap_or(0),
        "last_turn": input.tokens.map(|t| text(t, "last_turn")).unwrap_or_default(),
        "online": (idle_s < ONLINE_WINDOW_S || live) && !paused && !ended,
        "live": live,
        "running": running,
        "paused": paused,
        "ended": ended,
        "source": source,
        "editing": input.editing,
        "focus": focus_view(input.focus, stamp),
        "last_action": last_action_view(input.last_action, stamp),
        // the hands this one conversation has on the repo right now: its
        // subagents, each a worker, counted while their markers live
        "workers": input.workers.len(),
        "worker_types": worker_types,
        // and what each was started to do, sorted and deduped like the types
        "worker_names": worker_names,
        // each one: what it is doing, when it last moved, whether it is live
        "worker_list": worker_list,
        "workers_live": workers_live,
    })
}

/// The dashboard's live feed, and the resource an MCP client subscribes to.
///
/// `visible` is the caller's allowlist from `access::visible_scopes` — the
/// scopes of this workspace whose agents the caller may see. The queried
/// scope itself is always included: whoever calls this was authorised for it
/// before the call, and a repo with no tree yet (an agent that has only read
/// so far) is in no scope table to be listed from.
pub fn list_activity(
    store: &Store, scope: &str, repo_id: &str, idle_after_s: f64, visible: &BTreeSet<String>,
) -> Value {
    let stamp = now();
    let (workspace, this_repo) = split_scope(scope);
    let mut scopes: BTreeSet<String> = visible.clone();
    scopes.insert(scope.to_string());

    // intents stay per repo (the unit of code), so a task only ever
    // decorates rows in the queried repo. An intent that names its session
    // decorates that agent alone; one that does not decorates every agent
    // of its owner, as before.
    let intents = crate::collisions::active_intents(store, scope, idle_after_s);
    let task_for = |user: &str, session: &str| -> String {
        intents
            .iter()
            .find(|intent| {
                let owner = text(intent, "owner");
                let theirs = text(intent, "session");
                !owner.is_empty() && owner == user && (theirs.is_empty() || session.is_empty() || theirs == session)
            })
            .map(|intent| text(intent, "summary"))
            .unwrap_or_default()
    };

    // 1. the agents: one workspace-wide read per bucket, the repo taken from
    // the RECORD and checked against the allowlist. A record without
    // `repo_id` predates per-agent presence and cannot be placed, so it is
    // not shown; it ages out on its own TTL.
    let mut agents: BTreeMap<String, Agent> = BTreeMap::new();
    for bucket in ["focus", "lastact", "agenttokens", "worker"] {
        for (_key, record) in store.eph_scan(&format!("{bucket}:{workspace}:")) {
            let Some(repo) = record.get("repo_id").and_then(Value::as_str) else { continue };
            if !scopes.contains(&format!("{workspace}:{repo}")) {
                continue;
            }
            let user = text(&record, "user");
            if user.is_empty() {
                continue;
            }
            let session = text(&record, "session");
            let agent = agents.entry(agent_id(&user, &session)).or_insert_with(|| Agent {
                user: user.clone(), session: session.clone(), ..Agent::default()
            });
            match bucket {
                "focus" => agent.focus = Some(record),
                "lastact" => agent.last_action = Some(record),
                "worker" => {
                    let worker = text(&record, "worker");
                    if !worker.is_empty() {
                        agent.workers.insert(worker, record);
                    }
                }
                _ => agent.tokens = Some(record),
            }
        }
    }

    // 2. the checkouts: who has a tree in which visible repo
    let mut trees: BTreeMap<(String, String), Value> = BTreeMap::new();
    for visible_scope in &scopes {
        for tree in store.list_workspaces(visible_scope) {
            trees.insert((visible_scope.clone(), text(&tree, "user")), tree);
        }
    }

    let mut rows: Vec<Value> = Vec::new();
    // (scope, user) pairs already represented by an agent row
    let mut seated: BTreeSet<(String, String)> = BTreeSet::new();
    // each scope's reconciliations, read once: the collisions' outcomes
    let mut reconciles: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();

    // 3. one row per agent. A worker marker alone (its session's own markers
    // not written yet, or aged out) places nobody: the repo comes from the
    // session's markers, as before
    // a person with a named session in a repo: there, their sessionless seat
    // (an MCP briefing's) is not the one at work, and claims neither the
    // checkout's time nor the drafts their sessions are writing
    let named: BTreeSet<(String, String)> =
        agents.values().filter(|a| !a.session.is_empty()).map(|a| (a.user.clone(), a.repo_id())).collect();
    for agent in agents.values() {
        let repo = agent.repo_id();
        if repo.is_empty() {
            continue;
        }
        let agent_scope = format!("{workspace}:{repo}");
        let seat = (agent_scope.clone(), agent.user.clone());
        let stands_in = agent.session.is_empty() && named.contains(&(agent.user.clone(), repo.clone()));
        let current = own_edit(store, &agent_scope, &agent.user, &agent.session);
        let task = if repo == this_repo { task_for(&agent.user, &agent.session) } else { String::new() };
        rows.push(row(&RowInput {
            user: &agent.user,
            session: &agent.session,
            repo_id: &repo,
            tree: if stands_in { None } else { trees.get(&seat) },
            current: current.as_ref(),
            focus: agent.focus.as_ref(),
            last_action: agent.last_action.as_ref(),
            tokens: agent.tokens.as_ref(),
            editing: if stands_in { Value::Null } else { editing_view(store, &agent_scope, &agent.user, stamp) },
            task,
            workers: &agent.workers,
        }, stamp));
        // what the harness calls the conversation: its name on the roster
        let title = crate::freshness::title_of(store, &agent_scope, &agent.user, &agent.session);
        if let (false, Some(map)) = (title.is_empty(), rows.last_mut().and_then(Value::as_object_mut)) {
            map.insert("title".into(), json!(title));
        }
        // and the name Claude Code gives it, which its own SendMessage uses,
        // and what it is doing now as its hooks name it, when it declared no task
        let facts = crate::freshness::session_facts(store, &agent_scope, &agent.user, &agent.session);
        if let Some(map) = rows.last_mut().and_then(Value::as_object_mut) {
            if !facts.name.is_empty() {
                map.insert("cc_name".into(), json!(facts.name));
            }
            if !facts.task.is_empty() && map.get("task").and_then(Value::as_str).is_none_or(str::is_empty) {
                map.insert("task".into(), json!(facts.task));
            }
        }
        // and the editor it works in, with that editor's workspace
        let (ide, ide_workspace) = crate::freshness::ide_of(store, &agent_scope, &agent.user, &agent.session);
        if let Some(map) = rows.last_mut().and_then(Value::as_object_mut) {
            for (field, value) in [("ide", ide), ("ide_workspace", ide_workspace)] {
                if !value.is_empty() {
                    map.insert(field.into(), json!(value));
                }
            }
        }
        // what it said before its current line, newest first
        let mut earlier = crate::presence::history(store, &agent_scope, &agent.user, &agent.session);
        // a broken reference's outcome is its reconciliation's, as of now
        let states = reconciles.entry(agent_scope.clone()).or_insert_with(|| crate::lint::reconciliation_states(store, &agent_scope));
        for step in earlier.iter_mut() {
            crate::clash::settle_view(step, states);
        }
        if let Some(last) = rows.last_mut().and_then(|row| row.get_mut("last_action")).filter(|l| l.is_object()) {
            crate::clash::settle_view(last, states);
        }
        if let (false, Some(map)) = (earlier.is_empty(), rows.last_mut().and_then(Value::as_object_mut)) {
            map.insert("earlier_actions".into(), Value::Array(earlier.clone()));
        }
        seated.insert(seat);
        // a session whose markers moved to another repo (it writes into a
        // sibling checkout, then reads in its own again) still worked HERE:
        // its steps in this repo give it a row on this page too, marked
        // with where it is now, so a repo an agent dips into is never
        // empty while the agent works in it
        if repo != this_repo && !this_repo.is_empty() {
            let here: Vec<Value> = earlier
                .iter()
                .filter(|step| text(step, "repo_id") == this_repo && stamp - ts_of(step) < ELSEWHERE_WINDOW_S)
                .cloned()
                .collect();
            // the step it took last here, kept when it left: the history
            // holds only its newest steps, and a session busy in its own
            // repo pushes this one out of it within minutes
            let kept = store
                .eph_get(&crate::presence::last_in_key(scope, &agent.user, &agent.session))
                .filter(|step| text(step, "repo_id") == this_repo && stamp - ts_of(step) < ELSEWHERE_WINDOW_S)
                .map(|mut step| {
                    let states = reconciles.entry(scope.to_string()).or_insert_with(|| crate::lint::reconciliation_states(store, scope));
                    crate::clash::settle_view(&mut step, states);
                    step
                });
            let here: Vec<Value> = match &kept {
                Some(step) => std::iter::once(step.clone()).chain(here.into_iter().filter(|h| ts_of(h) != ts_of(step))).collect(),
                None => here,
            };
            if let Some(latest) = here.first() {
                let this_scope = format!("{workspace}:{this_repo}");
                let here_seat = (this_scope.clone(), agent.user.clone());
                let current = own_edit(store, &this_scope, &agent.user, &agent.session);
                let no_workers = BTreeMap::new();
                let mut also = row(&RowInput {
                    user: &agent.user,
                    session: &agent.session,
                    repo_id: &this_repo,
                    tree: trees.get(&here_seat),
                    current: current.as_ref(),
                    focus: None,
                    last_action: Some(latest),
                    tokens: agent.tokens.as_ref(),
                    editing: editing_view(store, &this_scope, &agent.user, stamp),
                    task: task_for(&agent.user, &agent.session),
                    workers: &no_workers,
                }, stamp);
                also["elsewhere"] = json!(repo);
                if here.len() > 1 {
                    also["earlier_actions"] = Value::Array(here[1..].to_vec());
                }
                let title = crate::freshness::title_of(store, &agent_scope, &agent.user, &agent.session);
                if !title.is_empty() {
                    also["title"] = json!(title);
                }
                let cc_name = crate::freshness::session_facts(store, &agent_scope, &agent.user, &agent.session).name;
                if !cc_name.is_empty() {
                    also["cc_name"] = json!(cc_name);
                }
                rows.push(also);
                seated.insert(here_seat);
            }
        }
    }
    let no_workers: BTreeMap<String, Value> = BTreeMap::new();

    // 4. a checkout with no live agent in that repo is still a person in the
    // room: the person-level row this endpoint always showed
    for ((tree_scope, user), tree) in &trees {
        if seated.contains(&(tree_scope.clone(), user.clone())) {
            continue;
        }
        let repo = split_scope(tree_scope).1;
        let current = person_edit(store, tree_scope, user);
        let task = if repo == this_repo { task_for(user, "") } else { String::new() };
        rows.push(row(&RowInput {
            user, session: "", repo_id: repo, tree: Some(tree), current: current.as_ref(),
            focus: None, last_action: None, tokens: None,
            editing: editing_view(store, tree_scope, user, stamp), task, workers: &no_workers,
        }, stamp));
        seated.insert((tree_scope.clone(), user.clone()));
    }

    // 5. someone drafting before their first clean final edit has neither a
    // tree nor, without hooks, a presence marker — the draft alone seats them
    for draft_scope in &scopes {
        for (_key, marker) in store.eph_scan(&format!("draft:{draft_scope}:")) {
            let user = text(&marker, "user");
            if user.is_empty() || seated.contains(&(draft_scope.clone(), user.clone())) {
                continue;
            }
            let repo = split_scope(draft_scope).1;
            let current = person_edit(store, draft_scope, &user);
            let task = if repo == this_repo { task_for(&user, "") } else { String::new() };
            rows.push(row(&RowInput {
                user: &user, session: "", repo_id: repo, tree: None, current: current.as_ref(),
                focus: None, last_action: None, tokens: None,
                editing: editing_view(store, draft_scope, &user, stamp), task, workers: &no_workers,
            }, stamp));
            seated.insert((draft_scope.clone(), user.clone()));
        }
    }

    // a stable order, and one the dashboard can group
    // by person without re-sorting
    rows.sort_by(|a, b| {
        (text(a, "user"), text(a, "session"), text(a, "repo_id"))
            .cmp(&(text(b, "user"), text(b, "session"), text(b, "repo_id")))
    });

    let setup = match store.kv_get("setup_state", scope) {
        Some(mut state) => {
            if let Some(map) = state.as_object_mut() {
                map.insert("latest_version".into(), json!(BLOCK_VERSION));
            }
            state
        }
        None => Value::Null,
    };

    let recent = store
        .eph_get(&format!("recent:{scope}"))
        .and_then(|record| record.get("events").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let tail = recent.len().saturating_sub(RECENT_EVENTS);

    let login_users: BTreeSet<String> =
        rows.iter().filter_map(|r| r.get("user").and_then(Value::as_str).map(str::to_string)).collect();
    let logins = crate::sharing::logins(store, crate::presence::split_scope(scope).0, &login_users, stamp);
    const INTENT_FIELDS: [&str; 13] = [
        "intent_id", "owner", "paths", "symbols", "change_type", "before", "after",
        "summary", "created", "idle", "operations", "ref", "ttl_remaining_s",
    ];
    json!({
        "repo_id": repo_id,
        // the PHYSICAL repo of the queried scope — the id rows in this repo
        // carry. After a rename (or under an aliased id) it differs from
        // `repo_id`, and the dashboard needs it to know which rows are "here"
        "scope_repo": this_repo,
        "setup": setup,
        "workspaces": rows,
        "intents": intents.iter().map(|intent| {
            let mut row = serde_json::Map::new();
            for field in INTENT_FIELDS.iter().chain(["agent"].iter()) {
                row.insert(
                    (*field).to_string(),
                    intent.get(*field).cloned().unwrap_or(json!("")),
                );
            }
            Value::Object(row)
        }).collect::<Vec<_>>(),
        "recent_events": recent[tail..].to_vec(),
        // each login's machines in the last two hours
        "logins": logins,
    })
}

// ------------------------------------------------------------- identity

/// Full display identity for agent-facing output.
///
/// Who-did-it should be legible without the reader mapping email addresses in
/// their head, so every actor carries a label rather than a raw address. The
/// label always ends with the username and email even when a nickname exists,
/// because a nickname alone is ambiguous across workspaces — "Robbie" is only
/// meaningful to whoever chose it.
///
/// Nicknames are private to the caller: they are that person's names for
/// other people, not a shared directory, so they are keyed by caller and
/// never leak between them.
pub fn identity_labels(
    store: &Store, caller: &str, users: &BTreeSet<String>,
) -> BTreeMap<String, Value> {
    let nicknames = crate::access::nicknames_of(store, caller);
    let mut out = BTreeMap::new();
    for user in users.iter().filter(|user| !user.is_empty()) {
        let username = store
            .kv_get("username_by_email", user)
            .map(|claim| text(&claim, "username").trim().to_string())
            .unwrap_or_default();
        let nick = nicknames
            .get(user)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let who = if !nick.is_empty() {
            nick.clone()
        } else if !username.is_empty() {
            username.clone()
        } else {
            user.split('@').next().unwrap_or(user).to_string()
        };
        let handle = if username.is_empty() { String::new() } else { format!("@{username}") };
        let label = if !nick.is_empty() {
            let inside: Vec<&str> =
                [handle.as_str(), user.as_str()].into_iter().filter(|p| !p.is_empty()).collect();
            if inside.is_empty() { nick.clone() } else { format!("{nick} ({})", inside.join(", ")) }
        } else if !handle.is_empty() {
            format!("{handle} ({user})")
        } else {
            user.clone()
        };
        out.insert(user.clone(), json!({"who": who, "username": username, "label": label}));
    }
    out
}

/// Attach a resolved identity to every actor in an activity view.
///
/// Done here rather than inside `list_activity` because the labels depend on
/// WHO IS ASKING — nicknames are the caller's own — while the activity itself
/// does not.
pub fn label_actors(store: &Store, caller: &str, view: &mut Value) {
    let mut users: BTreeSet<String> = BTreeSet::new();
    let collect = |rows: Option<&Vec<Value>>, keys: &[&str], into: &mut BTreeSet<String>| {
        for row in rows.into_iter().flatten() {
            for key in keys {
                let value = text(row, key);
                if !value.is_empty() {
                    into.insert(value);
                    break;
                }
            }
        }
    };
    collect(view.get("workspaces").and_then(Value::as_array), &["user"], &mut users);
    collect(view.get("intents").and_then(Value::as_array), &["owner"], &mut users);
    collect(
        view.get("recent_events").and_then(Value::as_array), &["user", "owner"], &mut users);

    let labels = identity_labels(store, caller, &users);
    let apply = |rows: Option<&mut Vec<Value>>, keys: &[&str]| {
        for row in rows.into_iter().flatten() {
            let who = keys
                .iter()
                .map(|key| text(row, key))
                .find(|value| !value.is_empty())
                .unwrap_or_default();
            if let (Some(info), Some(map)) = (labels.get(&who), row.as_object_mut()) {
                map.insert("by".into(), info.get("label").cloned().unwrap_or(Value::Null));
            }
        }
    };
    let Some(view) = view.as_object_mut() else { return };
    apply(view.get_mut("workspaces").and_then(Value::as_array_mut), &["user"]);
    apply(view.get_mut("intents").and_then(Value::as_array_mut), &["owner"]);
    apply(view.get_mut("recent_events").and_then(Value::as_array_mut), &["user", "owner"]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(repo: &str, ts: f64) -> Option<Value> {
        Some(json!({"user": "b@x.io", "session": "s1", "repo_id": repo, "ts": ts}))
    }

    #[test]
    fn an_agent_is_placed_by_its_newest_record_not_its_focus_first() {
        // a command started in A half an hour's marker ago; it has since acted in B
        let agent = Agent { focus: marker("github.com/acme/a", 100.0), last_action: marker("github.com/acme/b", 200.0), ..Agent::default() };
        assert_eq!(agent.repo_id(), "github.com/acme/b");
        // a tie goes to focus
        let tied = Agent { focus: marker("github.com/acme/a", 200.0), last_action: marker("github.com/acme/b", 200.0), ..Agent::default() };
        assert_eq!(tied.repo_id(), "github.com/acme/a");
        // the token counter, which carries no time of its own, places only an agent with nothing else
        let counted = Agent { tokens: Some(json!({"repo_id": "github.com/acme/c", "total": 5})), ..Agent::default() };
        assert_eq!(counted.repo_id(), "github.com/acme/c");
        let both = Agent { last_action: marker("github.com/acme/b", 50.0), tokens: Some(json!({"repo_id": "github.com/acme/c"})), ..Agent::default() };
        assert_eq!(both.repo_id(), "github.com/acme/b");
    }

    #[test]
    fn the_roster_shows_a_session_in_the_repo_it_moved_to() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let stamp = now();
        let (a, b) = ("w:github.com/acme/a", "w:github.com/acme/b");
        let mut focus = marker("github.com/acme/a", stamp - 600.0).unwrap();
        focus["started"] = json!(true);
        focus["action"] = json!("running");
        store.eph_set("focus:w:b@x.io#s1", &focus, Some(1800.0)).unwrap();
        store.eph_set("lastact:w:b@x.io#s1", &marker("github.com/acme/b", stamp - 5.0).unwrap(), Some(3600.0)).unwrap();
        let visible: BTreeSet<String> = [a.to_string(), b.to_string()].into_iter().collect();
        let view = list_activity(&store, b, "github.com/acme/b", 600.0, &visible);
        let rows = view["workspaces"].as_array().expect("rows");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["repo_id"], json!("github.com/acme/b"));
    }

    #[test]
    fn a_row_carries_the_sessions_title_and_its_live_workers_names() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let (scope, user, session) = ("w:github.com/acme/a", "b@x.io", "s1");
        crate::presence::note_last_action(&store, scope, user, session, "reading", "pkg/a.py", "claude-code", "", &crate::presence::Effort::default(), "", "", "", "");
        crate::freshness::note_title(&store, scope, user, session, "  Fix spend\naccounting ");
        crate::presence::note_worker(&store, scope, user, session, "a1", "Explore", "Audit savings");
        crate::presence::note_worker(&store, scope, user, session, "a2", "Explore", "");
        crate::presence::note_worker(&store, scope, user, session, "a3", "general-purpose", "Fix the hook");
        let visible: BTreeSet<String> = [scope.to_string()].into_iter().collect();
        let row = |store: &Store| list_activity(store, scope, "github.com/acme/a", 600.0, &visible)["workspaces"][0].clone();
        let shown = row(&store);
        assert_eq!(shown["title"], json!("Fix spend accounting"));
        assert_eq!(shown["workers"], json!(3));
        assert_eq!(shown["worker_types"], json!(["Explore", "general-purpose"]));
        assert_eq!(shown["worker_names"], json!(["Audit savings", "Fix the hook"]));
        // a subagent that finished is no longer one of its hands
        crate::presence::forget_worker(&store, scope, user, session, "a3");
        let shown = row(&store);
        assert_eq!((shown["workers"].clone(), shown["worker_names"].clone()), (json!(2), json!(["Audit savings"])));
    }

    #[test]
    fn a_session_that_dipped_into_a_sibling_repo_stays_on_that_repos_page_marked_with_where_it_is_now() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let (a, b, user, session) = ("w:github.com/acme/a", "w:github.com/acme/b", "k@x.io", "s9");
        let presence = |scope: &str, path: &str| {
            crate::presence::report_presence(&store, &crate::presence::PresenceInput {
                scope, user_id: user, session, path, action: "reading", agent: "claude-code", model: "opus", effort: Default::default(),
                branch: "dev", tokens: 0, turn_id: "", context: 0, via: "", worker: "",
            });
        };
        // the session reads in a, writes in b (a sibling checkout), reads in a again
        presence(a, "src/one.rs");
        presence(b, "edge/agent.py");
        presence(a, "src/two.rs");
        let visible: BTreeSet<String> = [a.to_string(), b.to_string()].into_iter().collect();
        let on_b = list_activity(&store, b, "github.com/acme/b", 600.0, &visible);
        let rows = on_b["workspaces"].as_array().cloned().unwrap_or_default();
        let here: Vec<&Value> = rows.iter().filter(|r| r["repo_id"] == "github.com/acme/b").collect();
        assert_eq!(here.len(), 1, "b's page shows the session from its step here: {on_b}");
        assert_eq!(here[0]["session"], json!(session));
        assert_eq!(here[0]["elsewhere"], json!("github.com/acme/a"), "and says where it is now");
        assert_eq!(here[0]["last_action"]["path"], json!("edge/agent.py"));
        // a's page has it as its own row, no "elsewhere"
        let on_a = list_activity(&store, a, "github.com/acme/a", 600.0, &visible);
        let own: Vec<&Value> = on_a["workspaces"].as_array().unwrap().iter().filter(|r| r["repo_id"] == "github.com/acme/a").collect();
        assert_eq!(own.len(), 1);
        assert!(own[0].get("elsewhere").is_none());
    }

    #[test]
    fn a_sibling_repo_keeps_the_session_however_many_steps_it_takes_at_home_after() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let (a, b, user, session) = ("w:github.com/acme/a", "w:github.com/acme/b", "k@x.io", "s9");
        let presence = |scope: &str, path: &str| {
            crate::presence::report_presence(&store, &crate::presence::PresenceInput {
                scope, user_id: user, session, path, action: "reading", agent: "claude-code", model: "opus", effort: Default::default(),
                branch: "dev", tokens: 0, turn_id: "", context: 0, via: "", worker: "",
            });
        };
        // one step in the sibling checkout, then a long stretch at home:
        // more steps than the history keeps
        presence(a, "src/one.rs");
        presence(b, "edge/agent.py");
        for i in 0..crate::presence::HISTORY_LEN + 50 {
            presence(a, &format!("src/f{i}.rs"));
        }
        let visible: BTreeSet<String> = [a.to_string(), b.to_string()].into_iter().collect();
        let on_b = |store: &Store| -> Vec<Value> {
            list_activity(store, b, "github.com/acme/b", 600.0, &visible)["workspaces"]
                .as_array().cloned().unwrap_or_default().into_iter()
                .filter(|r| r["repo_id"] == "github.com/acme/b" && r["session"] == session).collect()
        };
        let here = on_b(&store);
        assert_eq!(here.len(), 1, "b's page still has the session");
        assert_eq!(here[0]["last_action"]["path"], json!("edge/agent.py"));
        assert_eq!(here[0]["elsewhere"], json!("github.com/acme/a"));
        // past the window it is gone, as before
        let key = crate::presence::last_in_key(b, user, session);
        let mut kept = store.eph_get(&key).expect("the step in b is kept");
        kept["ts"] = json!(now() - ELSEWHERE_WINDOW_S - 60.0);
        store.eph_set(&key, &kept, Some(600.0)).unwrap();
        assert!(on_b(&store).is_empty(), "an old visit leaves no row");
    }

    const SCOPE: &str = "w:github.com/acme/a";
    const USER: &str = "b@x.io";
    const SESSION: &str = "s1";

    fn roster_row(store: &Store) -> Value {
        let visible: BTreeSet<String> = [SCOPE.to_string()].into_iter().collect();
        list_activity(store, SCOPE, "github.com/acme/a", 600.0, &visible)["workspaces"][0].clone()
    }

    /// A hook post from the session (`worker` "") or one of its subagents.
    fn post(store: &Store, action: &str, path: &str, worker: &str) {
        crate::presence::note_worker(store, SCOPE, USER, SESSION, worker, "", "");
        crate::presence::report_presence(store, &crate::presence::PresenceInput {
            scope: SCOPE, user_id: USER, session: SESSION, path, action, agent: "claude-code", model: "opus", effort: Default::default(),
            branch: "dev", tokens: 0, turn_id: "", context: 0, via: "", worker,
        });
    }

    /// The marker under `key`, made `age_s` old (the store keeps it longer
    /// than that, as a marker that old with that lifetime would still be).
    fn age(store: &Store, key: &str, age_s: f64) {
        let mut record = store.eph_get(key).expect("marker");
        record["ts"] = json!(now() - age_s);
        store.eph_set(key, &record, Some(3600.0)).unwrap();
    }

    #[test]
    fn a_roster_row_names_the_effort_its_model_ran_at() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let effort = |named: &str, ultracode: bool| crate::presence::Effort { effort: named.into(), ultracode };
        crate::presence::report_presence(&store, &crate::presence::PresenceInput {
            scope: SCOPE, user_id: USER, session: SESSION, path: "pkg/a.py", action: "reading", agent: "claude-code",
            model: "claude-opus-5-5", effort: effort("xhigh", true),
            branch: "dev", tokens: 0, turn_id: "", context: 0, via: "", worker: "",
        });
        let row = roster_row(&store);
        assert_eq!(row["model"], json!("claude-opus-5-5"));
        assert_eq!((row["effort"].clone(), row["ultracode"].clone()), (json!("xhigh"), json!(true)), "{row}");
        // the newest record answers, model and effort together
        crate::presence::note_started(&store, SCOPE, USER, SESSION, "cargo test", "dev", "claude-opus-5-5", &effort("medium", false));
        let row = roster_row(&store);
        assert_eq!((row["effort"].clone(), row["ultracode"].clone()), (json!("medium"), json!(false)), "{row}");
        // a turn's end knows no model: the line before's effort stays with it
        crate::presence::report_done(&store, SCOPE, USER, SESSION, "Done.");
        let row = roster_row(&store);
        assert_eq!((row["model"].clone(), row["effort"].clone(), row["ultracode"].clone()),
                   (json!("claude-opus-5-5"), json!("xhigh"), json!(true)), "{row}");
        // a harness that names none: the row says so with an empty effort
        post(&store, "reading", "pkg/b.py", "");
        let row = roster_row(&store);
        assert_eq!((row["model"].clone(), row["effort"].clone(), row["ultracode"].clone()), (json!("opus"), json!(""), json!(false)));
    }

    #[test]
    fn a_running_command_stays_live_past_three_minutes_until_it_ends_capped_at_half_an_hour() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let focus = crate::presence::focus_key(SCOPE, USER, SESSION);
        post(&store, "reading", "pkg/a.py", "");
        crate::presence::note_started(&store, SCOPE, USER, SESSION, "cargo test --release", "dev", "opus", &crate::presence::Effort::default());
        // ten minutes into a test suite: nothing has been posted since it started
        age(&store, &focus, 600.0);
        let row = roster_row(&store);
        assert_eq!((row["live"].clone(), row["running"].clone(), row["online"].clone()), (json!(true), json!(true), json!(true)), "{row}");
        assert_eq!(row["focus"]["started"], json!(true));
        // past the cap, a command that never reported its end is not live
        age(&store, &focus, 1900.0);
        let row = roster_row(&store);
        assert_eq!((row["live"].clone(), row["running"].clone()), (json!(false), json!(false)), "{row}");
        // its end (PostToolUse) completes it: from then on three minutes of quiet is idle
        crate::presence::note_started(&store, SCOPE, USER, SESSION, "cargo test --release", "dev", "opus", &crate::presence::Effort::default());
        post(&store, "running", "cargo test --release", "");
        assert!(store.eph_get(&focus).expect("focus").get("started").is_none(), "the end clears started");
        age(&store, &focus, 200.0);
        let row = roster_row(&store);
        assert_eq!((row["live"].clone(), row["running"].clone()), (json!(false), json!(false)), "{row}");
    }

    #[test]
    fn a_subagents_command_keeps_it_live_until_it_ends_capped_at_half_an_hour() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let key = crate::presence::worker_key(SCOPE, USER, SESSION, "a1");
        let wait = "until grep -q DONE out.log; do sleep 15; done";
        post(&store, "reading", "pkg/a.py", "");
        crate::presence::note_worker(&store, SCOPE, USER, SESSION, "a1", "general-purpose", "Wait for the run");
        crate::presence::note_worker_started(&store, SCOPE, USER, SESSION, "a1", "", "", wait);
        age(&store, &key, 600.0);
        let row = roster_row(&store);
        let worker = &row["worker_list"][0];
        assert_eq!(worker["live"], json!(true), "{row}");
        assert_eq!(worker["started"], json!(true));
        assert_eq!((worker["action"].clone(), worker["path"].clone()), (json!("running"), json!(wait)));
        // the name it was started with outlives a post that names none
        assert_eq!((worker["name"].clone(), worker["type"].clone()), (json!("Wait for the run"), json!("general-purpose")));
        assert_eq!(row["workers_live"], json!(1));
        // the session's own focus was never the subagent's to move
        assert_eq!(row["focus"]["action"], json!("reading"));
        age(&store, &key, 1900.0);
        assert_eq!(roster_row(&store)["worker_list"][0]["live"], json!(false));
        // its end report completes it: then the short window again
        crate::presence::note_worker_started(&store, SCOPE, USER, SESSION, "a1", "", "", wait);
        post(&store, "running", wait, "a1");
        assert!(store.eph_get(&key).expect("worker").get("started").is_none());
        age(&store, &key, 200.0);
        assert_eq!(roster_row(&store)["worker_list"][0]["live"], json!(false));
        // and SubagentStop removes it at once
        crate::presence::forget_worker(&store, SCOPE, USER, SESSION, "a1");
        assert_eq!(roster_row(&store)["workers"], json!(0));
    }

    #[test]
    fn a_row_is_live_while_its_subagents_work_after_its_turn_ended() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        post(&store, "reading", "pkg/a.py", "");
        crate::presence::note_worker(&store, SCOPE, USER, SESSION, "a1", "Explore", "Audit savings");
        crate::presence::note_worker(&store, SCOPE, USER, SESSION, "a2", "Explore", "Read the hooks");
        crate::presence::report_done(&store, SCOPE, USER, SESSION, "Started two agents.");
        // the session's last action is ten minutes old: by itself it is idle
        age(&store, &crate::presence::last_action_key(SCOPE, USER, SESSION), 600.0);
        let row = roster_row(&store);
        assert_eq!(row["focus"], Value::Null);
        assert_eq!(row["last_action"]["kind"], json!("done"));
        assert_eq!((row["live"].clone(), row["online"].clone(), row["workers_live"].clone()), (json!(true), json!(true), json!(2)), "{row}");
        // once both have gone quiet past the window, so has the row
        for worker in ["a1", "a2"] {
            age(&store, &crate::presence::worker_key(SCOPE, USER, SESSION, worker), 200.0);
        }
        let row = roster_row(&store);
        assert_eq!((row["live"].clone(), row["workers_live"].clone()), (json!(false), json!(0)), "{row}");
    }

    #[test]
    fn a_subagents_step_leaves_the_sessions_running_command_alone() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let focus = crate::presence::focus_key(SCOPE, USER, SESSION);
        crate::presence::note_started(&store, SCOPE, USER, SESSION, "npm test", "dev", "opus", &crate::presence::Effort::default());
        post(&store, "reading", "pkg/b.py", "a1");
        assert_eq!(store.eph_get(&focus).expect("focus")["path"], json!("npm test"));
        // a hook that does not name the worker on its started post put the
        // subagent's command on the session; the subagent's end report clears it
        crate::presence::note_started(&store, SCOPE, USER, SESSION, "sleep 100", "dev", "opus", &crate::presence::Effort::default());
        post(&store, "running", "sleep 100", "a1");
        assert!(store.eph_get(&focus).is_none());
    }

    #[test]
    fn a_subagents_steps_carry_its_label_in_the_stream() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        crate::presence::note_worker(&store, SCOPE, USER, SESSION, "a1", "Explore", "Audit savings");
        crate::presence::note_worker(&store, SCOPE, USER, SESSION, "a2", "general-purpose", "");
        post(&store, "reading", "pkg/a.py", "a1");
        post(&store, "running", "cargo check", "a2");
        post(&store, "reading", "pkg/b.py", "");
        let row = roster_row(&store);
        // the session's own step carries no label
        assert_eq!(row["last_action"]["path"], json!("pkg/b.py"));
        assert!(row["last_action"].get("worker").is_none(), "{row}");
        let earlier = row["earlier_actions"].as_array().expect("history");
        assert_eq!(earlier[0]["path"], json!("cargo check"));
        assert_eq!((earlier[0]["worker"].clone(), earlier[0]["worker_type"].clone()), (json!("a2"), json!("general-purpose")));
        assert!(earlier[0].get("worker_name").is_none(), "no name: the type stands for it");
        assert_eq!(earlier[1]["path"], json!("pkg/a.py"));
        assert_eq!((earlier[1]["worker"].clone(), earlier[1]["worker_name"].clone()), (json!("a1"), json!("Audit savings")));
        // a subagent's step while it is the newest is labelled too
        post(&store, "searching", "spend", "a1");
        let row = roster_row(&store);
        assert_eq!((row["last_action"]["worker"].clone(), row["last_action"]["worker_name"].clone()), (json!("a1"), json!("Audit savings")));
    }
}

#[cfg(test)]
mod identity_ported_tests {
    use super::*;

    #[test]
    fn labels_resolve_nickname_then_username_then_email() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("in-memory store");
        store.kv_put("nicknames", "alice@example.com", &json!({"bob@example.com": "Robbie"}), 0.0).unwrap();
        store.kv_put("username_by_email", "alice@example.com", &json!({"username": "alice"}), 0.0).unwrap();
        store.kv_put("username_by_email", "bob@example.com", &json!({"username": "bob"}), 0.0).unwrap();
        let users: BTreeSet<String> = ["alice@example.com", "bob@example.com", "stranger@x.com"]
            .into_iter().map(String::from).collect();
        let labels = identity_labels(&store, "alice@example.com", &users);
        assert_eq!(labels["bob@example.com"]["label"], "Robbie (@bob, bob@example.com)");
        assert_eq!(labels["bob@example.com"]["who"], "Robbie");
        assert_eq!(labels["alice@example.com"]["label"], "@alice (alice@example.com)");
        assert_eq!(labels["stranger@x.com"]["label"], "stranger@x.com");
        // nicknames are the caller's own: another caller sees none
        let theirs = identity_labels(&store, "bob@example.com", &users);
        assert_eq!(theirs["bob@example.com"]["label"], "@bob (bob@example.com)");
    }

    /// A person's own name is their username: a nickname they saved for
    /// themself before that rule (any case) names them nowhere, while
    /// their nicknames for others still win.
    #[test]
    fn a_nickname_for_yourself_is_never_used() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("in-memory store");
        store.kv_put("nicknames", "alice@example.com", &json!({"Alice@Example.com": "ALLY", "bob@example.com": "Robbie"}), 0.0).unwrap();
        store.kv_put("username_by_email", "alice@example.com", &json!({"username": "alice"}), 0.0).unwrap();
        let users: BTreeSet<String> = ["alice@example.com", "bob@example.com"].into_iter().map(String::from).collect();
        let labels = identity_labels(&store, "alice@example.com", &users);
        assert_eq!(labels["alice@example.com"]["who"], "alice");
        assert_eq!(labels["bob@example.com"]["who"], "Robbie");
        let names = crate::access::display_names(&store, "alice@example.com", &["alice@example.com".to_string()]);
        assert_eq!(names, vec!["alice".to_string()]);
        assert!(crate::access::nicknames_of(&store, "alice@example.com").get("Alice@Example.com").is_none());
    }
}

#[cfg(test)]
mod no_viewer_tests {
    use super::*;

    /// An empty allowlist means the queried repo alone, never the whole
    /// workspace: an agent reading in a sibling repo is not listed, and
    /// nothing of what it reads leaks into the view.
    #[test]
    fn a_read_with_no_allowlist_fails_closed_to_the_queried_repo() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let stamp = now();
        let other = json!({"user": "a@x.io", "session": "s", "repo_id": "github.com/acme/web",
                           "path": "web/secret.tsx", "action": "reading", "ts": stamp});
        store.eph_set("focus:w:a@x.io#s", &other, Some(600.0)).unwrap();
        store.eph_set("lastact:w:a@x.io#s", &other, Some(600.0)).unwrap();
        let view = list_activity(&store, "w:github.com/acme/api", "github.com/acme/api", 600.0, &BTreeSet::new());
        assert_eq!(view["workspaces"], json!([]), "{view}");
        assert!(!view.to_string().contains("secret"), "{view}");
        // the queried repo itself is always readable
        let here = json!({"user": "a@x.io", "session": "t", "repo_id": "github.com/acme/api",
                          "path": "api/ok.py", "action": "reading", "ts": stamp});
        store.eph_set("focus:w:a@x.io#t", &here, Some(600.0)).unwrap();
        let view = list_activity(&store, "w:github.com/acme/api", "github.com/acme/api", 600.0, &BTreeSet::new());
        let rows = view["workspaces"].as_array().expect("rows");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["repo_id"], json!("github.com/acme/api"));
    }
}
