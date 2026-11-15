//! Standing decisions: what the owner decided about a part of the code,
//! pinned to it, met by every agent in every session that goes to edit it.
//!
//! A decision is a `remember` note with `kind: "decision"`, kept in the same
//! `memory` bucket so recall, get_symbol and the why page find it. Two things
//! set it apart from a note:
//!
//! * It does not age. A note is marked stale when the code under it is
//!   rewritten and is scored down by what happens next; a decision ("remove
//!   the Python server, don't extend it") is about the code's future, so
//!   rewriting that code is exactly what it is meant to meet. No anchor hash
//!   is captured, and confidence is fixed. It lives until withdrawn
//!   (`supersedes`, or the dashboard's Withdraw).
//! * It holds once. The first write a session (or one of its subagents)
//!   makes under a decision's anchor is blocked by the gate with the
//!   decision as the reason; the retry goes through. One hold per agent per
//!   decision: enough for an agent that never heard it to stop and check,
//!   never enough to wall anyone in. A gate that wrongly blocks is worse than
//!   no gate, so everything here fails open.
//!
//! Why: an owner told one session "remove server/src/collide, don't extend
//! it", and a background agent in another session never heard it and kept
//! adding code there. Cork shared the code changes between sessions but
//! not the owner's decisions.

use serde_json::{json, Map, Value};

use crate::memory::{parse_anchor, Anchor};
use crate::store::{now, Store};

pub const KIND: &str = "decision";
/// The outcome kind (outcomes.rs) and waste-report line of a decision hold.
pub const OUTCOME_KIND: &str = "decision_hold";
/// How long a session's hold on one decision is remembered: a working day
/// and then some. A session that comes back after that is held again, which
/// is the right default for something it may have forgotten.
pub const HELD_TTL_S: f64 = 86_400.0;
/// How long "this session was briefed on this decision" is remembered.
const TOLD_TTL_S: f64 = 86_400.0;
const MAX_FACT: usize = 2000;
/// A decision in a briefing or a gate reason is a line, not an essay.
const MAX_LINE_FACT: usize = 240;
/// At most this many decision lines ride one briefing.
const MAX_BRIEF_LINES: usize = 3;
/// Decision lines take no more than this share (1/N) of a briefing's budget.
const BRIEF_SHARE: usize = 4;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn clip(fact: &str, max: usize) -> String {
    let fact = fact.trim();
    if fact.chars().count() <= max {
        return fact.to_string();
    }
    let cut: String = fact.chars().take(max).collect();
    let cut = match cut.rfind(' ') {
        Some(at) if at > max / 2 => cut[..at].to_string(),
        _ => cut,
    };
    format!("{}…", cut.trim_end())
}

pub fn is_decision(memory: &Value) -> bool {
    memory.get("kind").and_then(Value::as_str) == Some(KIND)
}

/// A decision nobody has superseded or withdrawn.
pub fn is_live(memory: &Value) -> bool {
    is_decision(memory)
        && memory.get("id").and_then(Value::as_str).is_some_and(|id| !id.is_empty())
        && memory.get("fact").and_then(Value::as_str).is_some_and(|fact| !fact.trim().is_empty())
        && !crate::compat::truthy(memory.get("superseded_by"))
        && !crate::compat::truthy(memory.get("withdrawn"))
}

/// Every anchor a note or decision covers: a decision may have several, a
/// note has its one.
pub fn anchors_of(memory: &Value) -> Vec<Value> {
    if let Some(list) = memory.get("anchors").and_then(Value::as_array) {
        let found: Vec<Value> = list.iter().filter(|a| a.is_object()).cloned().collect();
        if !found.is_empty() {
            return found;
        }
    }
    memory.get("anchor").filter(|a| a.is_object()).cloned().into_iter().collect()
}

/// An anchor as people write it: `dir/`, `file`, or `file::symbol`.
pub fn anchor_text(anchor: &Value) -> String {
    let path = text(anchor, "path");
    match text(anchor, "kind").as_str() {
        "dir" => if path.is_empty() { "/".to_string() } else { format!("{path}/") },
        "symbol" => format!("{path}::{}", text(anchor, "symbol")),
        _ => path,
    }
}

/// Whether a write to `path` falls under `anchor`: beneath a directory, the
/// file itself, or the file that holds the symbol (a write is to a file, and
/// the gate cannot tell which symbol it touches).
pub fn covers(anchor: &Value, path: &str) -> bool {
    let path = path.trim().trim_start_matches("./").trim_matches('/');
    if path.is_empty() {
        return false;
    }
    // a path that is not a string is no anchor at all (the empty string is
    // the repo root, a deliberate choice)
    let Some(anchored) = anchor.get("path").and_then(Value::as_str) else { return false };
    let anchored = anchored.trim_matches('/');
    match text(anchor, "kind").as_str() {
        "dir" => anchored.is_empty() || path == anchored || path.starts_with(&format!("{anchored}/")),
        "file" | "symbol" => !anchored.is_empty() && path == anchored,
        _ => false,
    }
}

pub fn covers_path(memory: &Value, path: &str) -> bool {
    anchors_of(memory).iter().any(|anchor| covers(anchor, path))
}

/// The scope's live decisions, oldest first.
pub fn live(store: &Store, scope: &str) -> Vec<Value> {
    let mut found: Vec<Value> = store
        .kv_list("memory", &format!("{scope}:"))
        .into_iter()
        .map(|(_key, memory)| memory)
        .filter(is_live)
        .collect();
    found.sort_by(|a, b| {
        let created = |v: &Value| v.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        created(a).partial_cmp(&created(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    found
}

/// The live decisions whose anchors cover `path`, oldest first.
pub fn covering(store: &Store, scope: &str, path: &str) -> Vec<Value> {
    live(store, scope).into_iter().filter(|d| covers_path(d, path)).collect()
}

/// What to call the person who decided, as other gate messages do: the
/// nickname the reader gave them, else their username, else the email's
/// local part.
pub fn who(store: &Store, reader: &str, user: &str) -> String {
    if user.is_empty() {
        return "the owner".to_string();
    }
    let nick = crate::access::nicknames_of(store, reader)
        .get(user)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !nick.is_empty() {
        return nick;
    }
    let username = store
        .kv_get("username_by_email", user)
        .map(|claim| text(&claim, "username").trim().to_string())
        .unwrap_or_default();
    if !username.is_empty() {
        return username;
    }
    user.split('@').next().unwrap_or(user).to_string()
}

/// `Oct 15 (2026-10-15)`: the day people say and the date nobody misreads.
pub fn day_text(ts: f64) -> String {
    match chrono::DateTime::from_timestamp(ts as i64, 0) {
        Some(at) => format!("{} ({})", at.format("%b %-d"), at.format("%Y-%m-%d")),
        None => "an earlier day".to_string(),
    }
}

// ------------------------------------------------------------------ record

pub struct Record<'a> {
    pub scope: &'a str,
    pub user_id: &'a str,
    pub session: &'a str,
    pub worker: &'a str,
    pub agent: &'a str,
    pub fact: &'a str,
    pub anchors: &'a [String],
    pub tags: &'a [String],
    /// a note or decision this one retires
    pub supersedes: &'a str,
}

/// Record a standing decision. It needs at least one anchor: a decision
/// about nothing in particular cannot be met at the code.
pub fn record(store: &Store, input: &Record) -> Value {
    let fact: String = input.fact.trim().chars().take(MAX_FACT).collect();
    if fact.is_empty() {
        return json!({"ok": false, "error": "empty fact"});
    }
    let mut anchors: Vec<Anchor> = Vec::new();
    for raw in input.anchors.iter().map(|a| a.trim()).filter(|a| !a.is_empty()) {
        let Some(anchor) = parse_anchor(raw) else {
            return json!({"ok": false, "error": format!("bad anchor {raw:?}; use \"dir/\", a file, or \"path::symbol\"")});
        };
        if !anchors.contains(&anchor) {
            anchors.push(anchor);
        }
    }
    if anchors.is_empty() {
        return json!({"ok": false, "error": "a decision needs an anchor: the code it covers, as \"dir/\", a file, or \"path::symbol\""});
    }
    let stamp = now();
    let id = crate::compat::new_id();
    let tags: Vec<String> = input
        .tags
        .iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let anchor_values: Vec<Value> = anchors.iter().map(Anchor::to_value).collect();

    let mut memory = Map::new();
    memory.insert("id".into(), json!(id));
    memory.insert("kind".into(), json!(KIND));
    memory.insert("fact".into(), json!(fact));
    memory.insert("tags".into(), json!(tags));
    memory.insert("owner".into(), json!(input.user_id));
    memory.insert("agent".into(), json!(input.agent));
    if !input.session.is_empty() {
        memory.insert("session".into(), json!(input.session));
    }
    if !input.worker.is_empty() {
        memory.insert("worker".into(), json!(input.worker));
    }
    memory.insert("created".into(), json!(stamp));
    // the first anchor is the note's `anchor`, so everything that reads one
    // (recall, get_symbol, locality) finds a decision as it finds a note; no
    // anchor hash, on purpose: a decision is not stale when its code moves
    memory.insert("anchor".into(), anchor_values[0].clone());
    memory.insert("anchors".into(), json!(anchor_values));

    let mut superseded = String::new();
    if !input.supersedes.trim().is_empty() {
        let key = format!("{}:{}", input.scope, input.supersedes.trim());
        let Some(mut target) = store.kv_get("memory", &key) else {
            return json!({"ok": false, "error": format!("supersedes target {} not found", input.supersedes)});
        };
        if let Some(map) = target.as_object_mut() {
            map.insert("superseded_by".into(), json!(id));
        }
        let _ = store.kv_put("memory", &key, &target, stamp);
        memory.insert("supersedes".into(), json!(input.supersedes.trim()));
        superseded = input.supersedes.trim().to_string();
    }
    let _ = store.kv_put("memory", &format!("{}:{id}", input.scope), &Value::Object(memory), stamp);
    let _ = store.ledger_append(
        input.scope,
        "memory_saved",
        &json!({"memory_id": id, "owner": input.user_id, "agent": input.agent, "tags": tags,
                "anchor": anchor_values[0], "anchors": anchor_values, "kind": KIND,
                "session": input.session, "worker": input.worker}),
        stamp,
    );
    if !superseded.is_empty() {
        let _ = store.ledger_append(
            input.scope,
            "memory_superseded",
            &json!({"memory_id": superseded, "by": id, "owner": input.user_id}),
            stamp,
        );
    }
    json!({"ok": true, "memory_id": id, "kind": KIND,
           "anchors": anchors.iter().map(|a| anchor_text(&a.to_value())).collect::<Vec<_>>(),
           "note": "Recorded as a standing decision: the first write any other agent makes under it is held once with it as the reason."})
}

fn strings_of(value: &Value, key: &str) -> Vec<String> {
    match value.get(key) {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(one)) if !one.trim().is_empty() => vec![one.clone()],
        _ => Vec::new(),
    }
}

/// Whether a `remember` call (or one of its `facts`) asks for a decision.
pub fn wants_decision(args: &Value) -> bool {
    args.get("kind").and_then(Value::as_str).is_some_and(|k| k.trim().eq_ignore_ascii_case(KIND))
}

/// `remember(kind="decision", ...)`: one decision from `fact` + `anchor`
/// (or `anchors`), and/or from `facts`. Facts items that share a text (or
/// carry none, taking the call's `fact`) are ONE decision over all their
/// anchors; an item whose own kind is "note" is left to the caller (the
/// second value). Returns (response, the note items still to save).
pub fn remember(
    store: &Store, scope: &str, user_id: &str, session: &str, worker: &str, agent: &str, args: &Value,
) -> (Value, Vec<Value>) {
    let top = wants_decision(args);
    let top_fact = text(args, "fact");
    // no fact, only `supersedes`: the decision is done, so withdraw it
    let target = text(args, "supersedes");
    if top && top_fact.trim().is_empty() && !target.trim().is_empty() && args.get("facts").is_none() {
        return (withdraw(store, scope, &target, user_id), Vec::new());
    }
    // fact text -> (anchors, tags, supersedes), in the order first seen
    let mut groups: Vec<(String, Vec<String>, Vec<String>, String)> = Vec::new();
    let mut add = |fact: String, anchors: Vec<String>, tags: Vec<String>, supersedes: String| {
        if let Some(group) = groups.iter_mut().find(|g| g.0 == fact) {
            group.1.extend(anchors);
            group.2.extend(tags);
            if group.3.is_empty() {
                group.3 = supersedes;
            }
        } else {
            groups.push((fact, anchors, tags, supersedes));
        }
    };
    if top && !top_fact.trim().is_empty() {
        let mut anchors = strings_of(args, "anchor");
        anchors.extend(strings_of(args, "anchors"));
        add(top_fact.trim().to_string(), anchors, strings_of(args, "tags"), text(args, "supersedes"));
    }
    let mut notes: Vec<Value> = Vec::new();
    for item in args.get("facts").and_then(Value::as_array).into_iter().flatten() {
        let own = item.get("kind").and_then(Value::as_str).map(str::trim).unwrap_or("");
        let decision = if own.is_empty() { top } else { own.eq_ignore_ascii_case(KIND) };
        if !decision {
            notes.push(item.clone());
            continue;
        }
        let fact = match text(item, "fact").trim() {
            "" => top_fact.trim().to_string(),
            given => given.to_string(),
        };
        if fact.is_empty() {
            continue;
        }
        let mut anchors = strings_of(item, "anchor");
        anchors.extend(strings_of(item, "anchors"));
        add(fact, anchors, strings_of(item, "tags"), text(item, "supersedes"));
    }
    let saved: Vec<Value> = groups
        .iter()
        .map(|(fact, anchors, tags, supersedes)| {
            record(store, &Record {
                scope, user_id, session, worker, agent, fact, anchors, tags, supersedes,
            })
        })
        .collect();
    let response = match saved.len() {
        0 => json!({"ok": false, "error": "a decision needs a fact and an anchor"}),
        1 => saved.into_iter().next().unwrap_or(Value::Null),
        n => json!({"ok": saved.iter().all(|s| s.get("ok") == Some(&json!(true))), "count": n, "saved": saved}),
    };
    (response, notes)
}

/// Withdraw a decision: it stops holding and stops being briefed, and stays
/// readable, marked.
pub fn withdraw(store: &Store, scope: &str, id: &str, by: &str) -> Value {
    let key = format!("{scope}:{}", id.trim());
    let Some(mut memory) = store.kv_get("memory", &key) else {
        return json!({"ok": false, "error": "no such decision"});
    };
    if !is_decision(&memory) {
        return json!({"ok": false, "error": "not a decision"});
    }
    if crate::compat::truthy(memory.get("withdrawn")) {
        return json!({"ok": true, "already": true});
    }
    let stamp = now();
    if let Some(map) = memory.as_object_mut() {
        map.insert("withdrawn".into(), json!(stamp));
        map.insert("withdrawn_by".into(), json!(by));
    }
    let _ = store.kv_put("memory", &key, &memory, stamp);
    let _ = store.ledger_append(scope, "decision_withdrawn", &json!({"memory_id": id.trim(), "user": by}), stamp);
    json!({"ok": true})
}

// -------------------------------------------------------------- the gate

fn held_prefix(scope: &str, user: &str, session: &str, worker: &str) -> String {
    let agent = crate::presence::agent_id(user, session);
    if worker.is_empty() {
        format!("decisionheld:{scope}:{agent}:")
    } else {
        format!("decisionheld:{scope}:{agent}/{worker}:")
    }
}

/// The gate's say on a write to `path`: the reason to hold it, the first
/// time this agent (session, or subagent of it when the hook names one)
/// writes under a live decision; None otherwise. Also settles earlier
/// holds: a write under the decision after its hold is a retry (the agent
/// went ahead), and a write elsewhere first means it left that code alone.
///
/// Fails open: any panic in here allows the write.
pub fn hold(
    store: &Store, scope: &str, user_id: &str, session: &str, worker: &str, path: &str, via: &str,
) -> Option<String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hold_inner(store, scope, user_id, session, worker, path, via)
    }))
    .ok()
    .flatten()
}

fn hold_inner(
    store: &Store, scope: &str, user_id: &str, session: &str, worker: &str, path: &str, via: &str,
) -> Option<String> {
    let prefix = held_prefix(scope, user_id, session, worker);
    let stamp = now();
    // a write under a decision this agent was already held on is the retry:
    // it went ahead, counted once per hold for the dashboard (what it did in
    // the end, wrote or left it, is settled by outcomes.rs)
    for (key, mut marker) in store.eph_scan(&prefix) {
        if marker.get("retried").is_some() {
            continue;
        }
        let under = marker
            .get("anchors")
            .and_then(Value::as_array)
            .is_some_and(|anchors| anchors.iter().any(|a| covers(a, path)));
        if !under {
            continue;
        }
        let _ = store.ledger_append(
            scope,
            "decision_retried",
            &json!({"user": user_id, "session": session, "worker": worker, "decision_id": text(&marker, "decision_id"),
                    "path": path, "held_path": text(&marker, "path"), "held_at": marker.get("ts").cloned().unwrap_or(Value::Null)}),
            stamp,
        );
        if let Some(map) = marker.as_object_mut() {
            map.insert("retried".into(), json!(stamp));
        }
        let left = (HELD_TTL_S - (stamp - marker.get("ts").and_then(Value::as_f64).unwrap_or(stamp))).max(60.0);
        let _ = store.eph_set(&key, &marker, Some(left));
    }

    let fresh: Vec<Value> = covering(store, scope, path)
        .into_iter()
        .filter(|d| !text(d, "id").is_empty() && store.eph_get(&format!("{prefix}{}", text(d, "id"))).is_none())
        .collect();
    if fresh.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    let mut events: Vec<Value> = Vec::new();
    for decision in &fresh {
        let id = text(decision, "id");
        let anchors = anchors_of(decision);
        let on: Vec<String> = anchors.iter().filter(|a| covers(a, path)).map(anchor_text).collect();
        let by = who(store, user_id, &text(decision, "owner"));
        let day = day_text(decision.get("created").and_then(Value::as_f64).unwrap_or(0.0));
        let fact = clip(&text(decision, "fact"), MAX_LINE_FACT);
        lines.push(format!("{by} decided on {day}, for {}: \"{fact}\"", on.join(", ")));
        let _ = store.eph_set(
            &format!("{prefix}{id}"),
            &json!({"decision_id": id, "path": path, "ts": stamp, "anchors": anchors}),
            Some(HELD_TTL_S),
        );
        events.push(json!({"kind": "decision", "decision_id": id, "fact": fact, "by": text(decision, "owner"),
                           "anchors": on}));
    }
    let said = lines.join(". Also, ");
    // a fact that ends its own sentence needs no second full stop
    let stop = if said.ends_with(".\"") || said.ends_with('.') { "" } else { "." };
    let reason = format!(
        "Cork held this write: {said}{stop} Check that this edit should happen at all; if it still should, retry and it will go through."
    );
    let _ = store.ledger_append(
        scope,
        "gate_block",
        &json!({"user": user_id, "session": session, "worker": worker, "path": path,
                "decision_ids": fresh.iter().map(|d| text(d, "id")).collect::<Vec<_>>(),
                "events": events}),
        stamp,
    );
    // what the agent then did, wrote anyway or left it, settles like every
    // other block's outcome; the waste report reads it
    crate::outcomes::open(store, scope, OUTCOME_KIND, user_id, session, &[path.to_string()],
                          json!({"decision_ids": fresh.iter().map(|d| text(d, "id")).collect::<Vec<_>>()}), stamp);
    crate::events::publish(
        store,
        scope,
        json!({"kind": "gate", "action": "block", "user": user_id, "path": path, "reason": reason, "decision": true}),
        via,
    );
    Some(reason)
}

/// How many writes each decision held, and how many of those the agent
/// wrote anyway: id -> (held, went ahead).
pub fn hold_counts(store: &Store, scope: &str, since: f64) -> std::collections::BTreeMap<String, (i64, i64)> {
    let mut out: std::collections::BTreeMap<String, (i64, i64)> = std::collections::BTreeMap::new();
    for row in store.ledger_since_kinds(scope, since, &["gate_block", "decision_retried"]) {
        if row.kind == "decision_retried" {
            out.entry(text(&row.payload, "decision_id")).or_default().1 += 1;
            continue;
        }
        for id in row.payload.get("decision_ids").and_then(Value::as_array).into_iter().flatten() {
            if let Some(id) = id.as_str() {
                out.entry(id.to_string()).or_default().0 += 1;
            }
        }
    }
    out
}

/// The live decisions as the dashboard lists them: text, anchors, who,
/// when, and what each held.
pub fn listing(store: &Store, scope: &str) -> Value {
    let found = live(store, scope);
    let since = found.iter().filter_map(|d| d.get("created").and_then(Value::as_f64)).fold(f64::INFINITY, f64::min);
    let counts = if found.is_empty() { Default::default() } else { hold_counts(store, scope, since) };
    let rows: Vec<Value> = found
        .iter()
        .rev()
        .map(|d| {
            let id = text(d, "id");
            let (held, retried) = counts.get(&id).copied().unwrap_or((0, 0));
            json!({"id": id, "fact": text(d, "fact"), "owner": text(d, "owner"), "agent": text(d, "agent"),
                   "session": text(d, "session"), "created": d.get("created").cloned().unwrap_or(json!(0)),
                   "anchors": anchors_of(d).iter().map(anchor_text).collect::<Vec<_>>(),
                   "held": held, "went_ahead": retried})
        })
        .collect();
    json!({"decisions": rows})
}

// ------------------------------------------------------------- briefings

/// One line per live decision that covers any of `paths` and that this
/// session has not been told yet, within `budget` bytes. Marks each told.
/// A sessionless reader is told every time (nothing to remember it by).
pub fn brief_lines(
    store: &Store, scope: &str, reader: &str, session: &str, paths: &[String], budget: usize,
) -> Vec<String> {
    if paths.is_empty() {
        return Vec::new();
    }
    let cap = (budget / BRIEF_SHARE).max(400);
    let agent = crate::presence::agent_id(reader, session);
    let mut used = 0usize;
    let mut out: Vec<String> = Vec::new();
    for decision in live(store, scope) {
        if out.len() >= MAX_BRIEF_LINES {
            break;
        }
        let on: Vec<String> = anchors_of(&decision)
            .iter()
            .filter(|a| paths.iter().any(|p| covers(a, p) || anchor_under(a, p)))
            .map(anchor_text)
            .collect();
        if on.is_empty() {
            continue;
        }
        let id = text(&decision, "id");
        let told = format!("decisiontold:{scope}:{agent}:{id}");
        if !session.is_empty() && store.eph_get(&told).is_some() {
            continue;
        }
        let line = line_for(store, reader, &decision, &on);
        if used + line.len() > cap {
            break;
        }
        used += line.len() + 1;
        if !session.is_empty() {
            let _ = store.eph_set(&told, &json!({"ts": now()}), Some(TOLD_TTL_S));
        }
        out.push(line);
    }
    out
}

/// A decision anchored inside a directory the briefing is about.
fn anchor_under(anchor: &Value, path: &str) -> bool {
    let dir = path.trim().trim_matches('/');
    !dir.is_empty() && text(anchor, "path").starts_with(&format!("{dir}/"))
}

/// The briefing's line for one decision.
pub fn line_for(store: &Store, reader: &str, decision: &Value, on: &[String]) -> String {
    let by = who(store, reader, &text(decision, "owner"));
    let day = day_text(decision.get("created").and_then(Value::as_f64).unwrap_or(0.0));
    format!(
        "Owner's decision ({by}, {day}) on {}: \"{}\" Follow it; the first write there is held once with it.",
        on.join(", "),
        clip(&text(decision, "fact"), MAX_LINE_FACT)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn decide(db: &Store, anchors: &[&str]) -> String {
        let anchors: Vec<String> = anchors.iter().map(|a| a.to_string()).collect();
        let saved = record(db, &Record {
            scope: "w:r", user_id: "kevin@x.com", session: "s0", worker: "", agent: "claude",
            fact: "The Python server is being removed; don't extend it.", anchors: &anchors, tags: &[],
            supersedes: "",
        });
        assert_eq!(saved["ok"], json!(true), "{saved}");
        text(&saved, "memory_id")
    }

    #[test]
    fn a_decision_on_a_dir_holds_the_first_write_under_it_and_the_retry_passes() {
        let db = store();
        let id = decide(&db, &["server/src/collide/"]);
        let reason = hold(&db, "w:r", "kevin@x.com", "s1", "", "server/src/collide/app.py", "t").expect("held");
        assert!(reason.starts_with("Cork held this write: kevin decided on "), "{reason}");
        assert!(reason.contains("\"The Python server is being removed; don't extend it.\""), "{reason}");
        assert!(reason.ends_with("if it still should, retry and it will go through."), "{reason}");
        // the retry, and every later write, passes
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "", "server/src/collide/app.py", "t").is_none());
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "", "server/src/collide/other.py", "t").is_none());
        let counts = hold_counts(&db, "w:r", 0.0);
        assert_eq!(counts.get(&id), Some(&(1, 1)));
        // the hold is a gate_block row, as every block is
        assert_eq!(db.ledger_since_kinds("w:r", 0.0, &["gate_block"]).len(), 1);
        assert_eq!(db.ledger_since_kinds("w:r", 0.0, &["decision_retried"]).len(), 1);
    }

    #[test]
    fn a_reason_ends_each_sentence_once() {
        let db = store();
        decide(&db, &["server/src/collide/"]);
        let reason = hold(&db, "w:r", "kevin@x.com", "s1", "", "server/src/collide/app.py", "t").expect("held");
        assert!(!reason.contains(".\"."), "{reason}");
        assert!(reason.contains("it.\" Check that"), "{reason}");
    }

    #[test]
    fn an_agent_withdraws_a_decision_with_remember_and_no_fact() {
        let db = store();
        let id = decide(&db, &["server/src/collide/"]);
        let args = json!({"kind": "decision", "supersedes": id});
        let (done, notes) = remember(&db, "w:r", "kevin@x.com", "s9", "", "claude", &args);
        assert_eq!(done["ok"], json!(true), "{done}");
        assert!(notes.is_empty());
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "", "server/src/collide/app.py", "t").is_none());
    }

    #[test]
    fn a_second_session_and_a_subagent_are_each_held_once() {
        let db = store();
        decide(&db, &["server/src/collide/"]);
        let path = "server/src/collide/x.py";
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "", path, "t").is_some());
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "", path, "t").is_none());
        assert!(hold(&db, "w:r", "kevin@x.com", "s2", "", path, "t").is_some());
        assert!(hold(&db, "w:r", "kevin@x.com", "s2", "", path, "t").is_none());
        // a subagent of s1, when the hook names it, is its own agent
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "agent-7", path, "t").is_some());
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "agent-7", path, "t").is_none());
        // another person's agent too
        assert!(hold(&db, "w:r", "becca@x.com", "s9", "", path, "t").is_some());
    }

    #[test]
    fn a_path_outside_the_decision_is_never_held() {
        let db = store();
        decide(&db, &["server/src/collide/", "docs/old.md", "server/lib.rs::serve"]);
        for path in ["server/src/cork-rs/main.rs", "server/src/collidex.py", "server/src/other.py", "docs/old.md.bak", "server/lib2.rs"] {
            assert!(hold(&db, "w:r", "kevin@x.com", "s1", "", path, "t").is_none(), "{path}");
        }
        // file and symbol anchors cover their file
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "", "docs/old.md", "t").is_some());
        assert!(hold(&db, "w:r", "kevin@x.com", "s2", "", "server/lib.rs", "t").is_some());
        // a write elsewhere after a hold is not a retry
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "", "README.md", "t").is_none());
        assert!(db.ledger_since_kinds("w:r", 0.0, &["decision_retried"]).is_empty());
    }

    #[test]
    fn a_withdrawn_or_superseded_decision_holds_nothing() {
        let db = store();
        let id = decide(&db, &["server/src/collide/"]);
        assert_eq!(withdraw(&db, "w:r", &id, "kevin@x.com")["ok"], json!(true));
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "", "server/src/collide/a.py", "t").is_none());
        assert!(listing(&db, "w:r")["decisions"].as_array().unwrap().is_empty());

        let other = decide(&db, &["legacy/"]);
        let anchors = vec!["elsewhere/".to_string()];
        record(&db, &Record { scope: "w:r", user_id: "kevin@x.com", session: "", worker: "", agent: "",
                              fact: "moved", anchors: &anchors, tags: &[], supersedes: &other });
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "", "legacy/a.py", "t").is_none());
    }

    #[test]
    fn remember_makes_one_decision_over_every_anchor_of_one_text() {
        let db = store();
        let args = json!({"kind": "decision", "fact": "The Python server is going; don't extend it.",
                          "anchor": "server/src/collide/",
                          "facts": [{"anchor": "server/tests/"}, {"fact": "a plain note", "kind": "note", "anchor": "x.py"}]});
        let (saved, notes) = remember(&db, "w:r", "u@x", "s", "", "claude", &args);
        assert_eq!(saved["ok"], json!(true), "{saved}");
        assert_eq!(saved["anchors"], json!(["server/src/collide/", "server/tests/"]));
        assert_eq!(notes.len(), 1);
        assert!(hold(&db, "w:r", "u@x", "s2", "", "server/tests/t.py", "t").is_some());
        // who and when are kept
        let d = &live(&db, "w:r")[0];
        assert_eq!(text(d, "owner"), "u@x");
        assert_eq!(text(d, "session"), "s");
        assert!(d.get("created").and_then(Value::as_f64).unwrap() > 0.0);
    }

    #[test]
    fn a_decision_needs_an_anchor() {
        let db = store();
        let saved = record(&db, &Record { scope: "w:r", user_id: "u", session: "", worker: "", agent: "",
                                          fact: "never Y", anchors: &[], tags: &[], supersedes: "" });
        assert_eq!(saved["ok"], json!(false));
    }

    #[test]
    fn decisions_do_not_go_stale_when_their_code_is_rewritten() {
        let db = store();
        let id = decide(&db, &["server/app.py"]);
        let decision = db.kv_get("memory", &format!("w:r:{id}")).unwrap();
        // a note written against a version of the file that has since moved
        let note = json!({"id": "n1", "fact": "f", "owner": "u", "created": 1.0,
                          "anchor": {"kind": "file", "path": "server/app.py"},
                          "anchor_hash": "old", "anchor_rev": 0});
        let seen = crate::memory::annotate(&db, "w:r", &[note, decision]);
        assert_eq!(seen[0]["stale"], json!(true));
        assert_eq!(seen[1]["stale"], json!(false));
        assert!(seen[1].get("possibly_stale").is_none());
        assert!(seen[1]["confidence"].as_f64().unwrap() >= 0.9);
        // and it still reaches whoever touches the file
        let near = crate::memory::locality_memories(&db, "w:r", &["server/app.py".to_string()], &[], 5);
        assert!(near.iter().any(|m| text(m, "id") == id));
    }

    #[test]
    fn the_briefing_carries_a_covering_decision_once() {
        let db = store();
        decide(&db, &["server/src/collide/"]);
        let paths = vec!["server/src/collide/app.py".to_string()];
        let first = brief_lines(&db, "w:r", "kevin@x.com", "s1", &paths, 6000);
        assert_eq!(first.len(), 1);
        assert!(first[0].starts_with("Owner's decision (kevin, "), "{}", first[0]);
        assert!(brief_lines(&db, "w:r", "kevin@x.com", "s1", &paths, 6000).is_empty());
        // another session is told too
        assert_eq!(brief_lines(&db, "w:r", "kevin@x.com", "s2", &paths, 6000).len(), 1);
        // a briefing about other code is not
        assert!(brief_lines(&db, "w:r", "kevin@x.com", "s3", &["web/a.ts".to_string()], 6000).is_empty());
    }

    #[test]
    fn the_brief_endpoint_text_carries_the_decision_once() {
        let db = store();
        decide(&db, &["server/src/collide/"]);
        let ids = vec!["server/src/collide/app.py".to_string()];
        let visible = std::collections::BTreeSet::new();
        let input = crate::brief::BriefInput {
            repo_id: "r", since: 0.0, budget: 6000, identifiers: &ids, exclude: &[], session: "s1", resume: false,
            idle_after_s: 300.0, visible: &visible, viewer: "kevin@x.com", share_knowledge: true, verified: true,
        };
        let first = crate::brief::brief(&db, "w:r", &input);
        assert!(text(&first, "text").contains("Owner's decision (kevin"), "{first}");
        let again = crate::brief::brief(&db, "w:r", &input);
        assert!(!text(&again, "text").contains("Owner's decision"), "{again}");
    }

    #[test]
    fn the_gate_holds_once_then_allows() {
        let db = store();
        decide(&db, &["server/src/collide/"]);
        let path = "server/src/collide/a.py";
        let first = crate::gate::gate(&db, "w:r", "kevin@x.com", "s1", "", path, "", "t").body;
        assert_eq!(first["allow"], json!(false));
        assert_eq!(first["decision"], json!(true));
        assert!(text(&first, "reason").starts_with("Cork held this write: kevin decided on "), "{first}");
        let again = crate::gate::gate(&db, "w:r", "kevin@x.com", "s1", "", path, "", "t").body;
        assert_eq!(again["allow"], json!(true), "{again}");
        // outside the decision the gate never holds
        let elsewhere = crate::gate::gate(&db, "w:r", "kevin@x.com", "s5", "", "server/cork-rs/src/main.rs", "", "t").body;
        assert_eq!(elsewhere["allow"], json!(true));
    }

    #[cfg(feature = "cloud")]
    #[test]
    fn the_waste_report_counts_a_hold_as_a_decision_not_a_stale_write() {
        let db = store();
        decide(&db, &["server/src/collide/"]);
        let path = "server/src/collide/a.py";
        assert!(hold(&db, "w:r", "kevin@x.com", "s1", "", path, "t").is_some());
        // the agent wrote there anyway
        crate::outcomes::on_write(&db, "w:r", "kevin@x.com", "s1", path, 0.0, None);
        let rows: Vec<(String, crate::store::LedgerRow)> = db
            .ledger_since_kinds("w:r", 0.0, &crate::waste::kinds())
            .into_iter()
            .map(|row| ("r".to_string(), row))
            .collect();
        let ctx = crate::waste::Context { instrumented_at: Some(0.0), ..Default::default() };
        let report = crate::waste::aggregate(&rows, 2, now(), &ctx);
        assert_eq!(report["totals"]["stale_writes_blocked"]["total"], 0);
        assert_eq!(report["totals"]["decisions_held"]["total"], 1);
        assert_eq!(report["totals"]["decisions_held"]["outcomes"]["overrode"], 1);
    }

    #[test]
    fn the_gate_fails_open_on_a_malformed_record() {
        let db = store();
        // garbage in the memory bucket: wrong types everywhere
        for (i, junk) in [
            json!({"kind": "decision", "id": 5, "anchors": "server/", "anchor": [1, 2], "created": "yesterday"}),
            json!({"kind": "decision", "id": "d2", "anchors": [{"kind": "dir", "path": 7}], "fact": null}),
            json!({"kind": "decision"}),
            json!("just a string"),
            json!(null),
        ]
        .into_iter()
        .enumerate()
        {
            db.kv_put("memory", &format!("w:r:junk{i}"), &junk, 1.0).unwrap();
        }
        assert!(hold(&db, "w:r", "u", "s", "", "server/a.py", "t").is_none());
        // a malformed hold marker is also no reason to block
        db.eph_set(&format!("{}x", held_prefix("w:r", "u", "s", "")), &json!([1, 2, 3]), Some(60.0)).unwrap();
        assert!(hold(&db, "w:r", "u", "s", "", "server/a.py", "t").is_none());
        // and a real decision beside the junk still holds
        decide(&db, &["server/"]);
        assert!(hold(&db, "w:r", "u", "s", "", "server/a.py", "t").is_some());
        assert!(brief_lines(&db, "w:r", "u", "s2", &["server/a.py".to_string()], 6000).len() == 1);
        let _ = listing(&db, "w:r");
    }
}
