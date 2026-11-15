//! The heads-up log: for each heads-up an agent is handed (a teammate's
//! change as a Δ line, a message, a gate notice), when it was sent, when it
//! reached the agent's context, and when the agent was seen past it.
//!
//! - sent: the server queued it (the change reported, the message filed,
//!   the gate's answer);
//! - received: a hook answer carried it into the agent's context (the hooks
//!   print every one they are handed);
//! - seen: the agent acted after it: acked it, answered it, or made a tool
//!   call from a later model message than the one it arrived during (the
//!   hooks name each call's message, `step.msg`).
//!
//! Messages keep their stamps where the dashboard's log reads them
//! (`agenttools::seen_key`); Δ lines and gate notices are rows of their own
//! (`BUCKET`, a few days). What an agent has not been seen past waits under
//! one key per agent, which its next hook call settles: one read a call,
//! and nothing to write when nothing waits. The founder's "heads-ups are not
//! firing" could not be checked: a message's log said "read" the moment a
//! hook took it off the queue, and a Δ line left no trace at all.

use serde_json::{json, Value};

use crate::store::{now, Store};

/// `headsup:{scope}:{received_at}:{id}`: one row per hook answer that
/// carried Δ lines or a gate notice to an agent.
pub const BUCKET: &str = "headsup";
const ROW_TTL_S: f64 = 3.0 * 86_400.0;
/// How long a heads-up waits to be seen past: a day (an agent that made no
/// call since is not working).
const UNSEEN_TTL_S: f64 = 86_400.0;
const TEXT_MAX: usize = 600;
/// What a heads-up handed over with a prompt's briefing arrived during:
/// no model message of the turn yet, so the turn's first call is past it.
pub const PROMPT: &str = "prompt";

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The agent a hook call was made for: `user#session`, or a helper's
/// `user#session/worker`.
pub fn agent(user: &str, session: &str, worker: &str) -> String {
    match (session.is_empty(), worker.is_empty()) {
        (true, _) => user.to_string(),
        (false, true) => format!("{user}#{session}"),
        (false, false) => format!("{user}#{session}/{worker}"),
    }
}

fn unseen_key(scope: &str, agent: &str) -> String {
    format!("headsup_unseen:{scope}:{agent}")
}

/// Waiting to be seen: `{rows: [{key, msg}], ids: [{id, msg}]}`, each with
/// the model message it arrived during ("" until a call names it).
fn pending(store: &Store, key: &str) -> Value {
    store.eph_get(key).unwrap_or_else(|| json!({"rows": [], "ids": []}))
}

fn push(list: &mut Value, field: &str, entry: Value) {
    if let Some(items) = list.get_mut(field).and_then(Value::as_array_mut) {
        items.push(entry);
    } else {
        list[field] = json!([entry]);
    }
}

/// A hook answer carried these Δ lines or gate notices (`kind` "delta" or
/// "gate") to this agent, each with when it was sent. One row for them all.
#[allow(clippy::too_many_arguments)]
pub fn received(store: &Store, scope: &str, user: &str, session: &str, worker: &str, kind: &str, lines: &[(String, f64)], step: &str) {
    let lines: Vec<&(String, f64)> = lines.iter().filter(|(line, _)| !line.trim().is_empty()).collect();
    if lines.is_empty() || session.is_empty() {
        return;
    }
    let stamp = now();
    let id = crate::compat::new_id();
    let to = agent(user, session, worker);
    let key = format!("{scope}:{stamp:017.6}:{id}");
    let row = json!({
        "id": id, "kind": kind, "to": to, "user": user, "session": session, "worker": worker,
        "lines": lines.iter().map(|(line, sent)| json!({"text": line.chars().take(TEXT_MAX).collect::<String>(), "sent_at": sent})).collect::<Vec<_>>(),
        "sent_at": lines.iter().map(|(_, sent)| *sent).fold(f64::MAX, f64::min),
        "received_at": stamp, "seen_at": Value::Null,
    });
    let _ = store.eph_set(&format!("{BUCKET}:{key}"), &row, Some(ROW_TTL_S));
    let waiting = unseen_key(scope, &to);
    let mut list = pending(store, &waiting);
    push(&mut list, "rows", json!({"key": key, "msg": step}));
    let _ = store.eph_set(&waiting, &list, Some(UNSEEN_TTL_S));
}

/// A hook answer carried these messages (their inbox ids) to this agent.
/// Their delivery is stamped where they are handed over; here they wait to
/// be seen past.
pub fn received_messages(store: &Store, scope: &str, user: &str, session: &str, worker: &str, ids: &[String], step: &str) {
    if ids.is_empty() || session.is_empty() {
        return;
    }
    let waiting = unseen_key(scope, &agent(user, session, worker));
    let mut list = pending(store, &waiting);
    for id in ids {
        push(&mut list, "ids", json!({"id": id, "msg": step}));
    }
    let _ = store.eph_set(&waiting, &list, Some(UNSEEN_TTL_S));
}

/// This agent's hook call, made from model message `step` ("" when the
/// call does not say). What arrived during an earlier message is seen now;
/// what arrived during a call that did not say is taken to be this
/// message's. Called before the call's own heads-ups are handed over.
pub fn settle(store: &Store, scope: &str, user: &str, session: &str, worker: &str, step: &str) {
    if step.is_empty() || session.is_empty() {
        return;
    }
    let waiting = unseen_key(scope, &agent(user, session, worker));
    let Some(mut list) = store.eph_get(&waiting) else { return };
    let stamp = now();
    let mut changed = false;
    let mut seen_rows: Vec<String> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();
    for (field, out) in [("rows", &mut seen_rows), ("ids", &mut seen_ids)] {
        let Some(items) = list.get_mut(field).and_then(Value::as_array_mut) else { continue };
        items.retain_mut(|item| {
            let msg = text(item, "msg");
            if msg.is_empty() {
                item["msg"] = json!(step);
                changed = true;
                return true;
            }
            if msg == step {
                return true;
            }
            out.push(text(item, if field == "rows" { "key" } else { "id" }));
            changed = true;
            false
        });
    }
    for key in &seen_rows {
        let full = format!("{BUCKET}:{key}");
        if let Some(mut row) = store.eph_get(&full) {
            if row.get("seen_at").and_then(Value::as_f64).is_none() {
                row["seen_at"] = json!(stamp);
                let _ = store.eph_set(&full, &row, Some(ROW_TTL_S));
            }
        }
    }
    crate::agenttools::mark_seen(store, &seen_ids, stamp);
    let empty = ["rows", "ids"].iter().all(|f| list.get(*f).and_then(Value::as_array).is_none_or(Vec::is_empty));
    if empty {
        store.eph_delete(&waiting);
    } else if changed {
        let _ = store.eph_set(&waiting, &list, Some(UNSEEN_TTL_S));
    }
}

/// The Δ lines and gate notices handed to agents in these repos since
/// `since`, newest first, at most `limit`: each with its repo (the display
/// id `scopes` pairs it with) and its three times.
pub fn log(store: &Store, scopes: &[(String, String)], since: f64, limit: usize) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for (scope, display) in scopes {
        for (_, mut row) in store.eph_scan(&format!("{BUCKET}:{scope}:")) {
            if row.get("received_at").and_then(Value::as_f64).unwrap_or(0.0) < since {
                continue;
            }
            row["repo"] = json!(display);
            rows.push(row);
        }
    }
    let at = |r: &Value| r.get("received_at").and_then(Value::as_f64).unwrap_or(0.0);
    rows.sort_by(|a, b| at(b).partial_cmp(&at(a)).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(limit);
    rows
}

/// `cork message --status`: the messages this agent (`session`, or a
/// helper's `session/worker`) sent in this repo lately, newest first, each
/// recipient with how far it got: "to lattice-views, 3m ago ("Codeword
/// FIG-98"): delivered 3m ago, seen 2m ago".
pub fn sent_status(store: &Store, scope: &str, user: &str, session: &str, worker: &str, limit: usize) -> Vec<String> {
    let mine = if worker.is_empty() { session.to_string() } else { format!("{session}/{worker}") };
    let stamp = now();
    let mut rows: Vec<Value> = store
        .kv_list(crate::agenttools::THREAD_BUCKET, &format!("{scope}:"))
        .into_iter()
        .map(|(_, row)| row)
        .filter(|row| {
            let from = text(row, "from");
            let (person, signed) = from.split_once('#').unwrap_or((from.as_str(), ""));
            // signed by a helper (`user#session/worker`), else by its session
            let by = if signed.contains('/') { signed.to_string() } else { text(row, "from_session") };
            person == user && !mine.is_empty() && by == mine
        })
        .collect();
    rows.reverse();
    rows.truncate(limit);
    let mut lines = Vec::new();
    for row in rows {
        let sent = row.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        let said: String = text(&row, "message").split_whitespace().collect::<Vec<_>>().join(" ");
        let said: String = if said.chars().count() > 60 { format!("{}…", said.chars().take(60).collect::<String>()) } else { said };
        let list: Vec<String> = match row.get("to_list").and_then(Value::as_array) {
            Some(to) => to.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            None => vec![text(&row, "to")],
        };
        for (i, to) in list.iter().filter(|t| !t.is_empty()).enumerate() {
            let id = row.get("ids").and_then(|ids| ids.get(to)).and_then(Value::as_str).map(str::to_string)
                .or_else(|| (i == 0 && row.get("to_list").is_none()).then(|| text(&row, "id")))
                .unwrap_or_default();
            let seen = store.eph_get(&crate::agenttools::seen_key(&id));
            let at = |field: &str| seen.as_ref().and_then(|s| s.get(field)).and_then(Value::as_f64);
            let waiting = store.kv_get("inbox", &format!("{scope}:{to}:{id}")).is_some();
            let state = match (at("delivered_at"), at("seen_at").or(at("read_at"))) {
                (Some(got), Some(past)) => format!("delivered {}, seen {}", ago(got, stamp), ago(past, stamp)),
                (Some(got), None) => format!("delivered {}, not seen yet", ago(got, stamp)),
                (None, Some(past)) => format!("seen {}", ago(past, stamp)),
                (None, None) if waiting && crate::agenttools::is_expired(sent, stamp) => "never delivered (expired)".to_string(),
                (None, None) if waiting => "waiting: it reaches them at their next step".to_string(),
                (None, None) => "taken".to_string(),
            };
            lines.push(format!("to {}, {} (\"{said}\"): {state}", recipient(store, scope, to), ago(sent, stamp)));
        }
    }
    lines
}

/// An inbox as a person reads it: a helper by its name, a session by its
/// title and its Claude Code name (`Cork version number
/// (cork-linear-test-e3)`), else the inbox itself. On this machine's own
/// server an agent's identity is its inbox, and its session is found from
/// it.
pub fn recipient(store: &Store, scope: &str, inbox: &str) -> String {
    let Some((user, rest)) = inbox.split_once('#') else {
        let (agent, worker) = inbox.split_once('/').unwrap_or((inbox, ""));
        if !crate::local::active() || !agent.starts_with("agent-") {
            return inbox.to_string();
        }
        let sessions = crate::freshness::sessions_of(store, scope, agent);
        let named = match (sessions.as_slice(), worker) {
            ([session], "") => crate::freshness::session_label(store, scope, agent, session),
            (_, "lead") => "your lead".to_string(),
            ([session], worker) => crate::local::helper_name(store, scope, agent, session, worker),
            _ => String::new(),
        };
        return if named.is_empty() { inbox.to_string() } else { named };
    };
    match rest.split_once('/') {
        Some((_, "lead")) => "your lead".to_string(),
        Some((session, worker)) => {
            let name = crate::local::helper_name(store, scope, user, session, worker);
            if name.is_empty() { inbox.to_string() } else { name }
        }
        None => {
            let named = crate::freshness::session_label(store, scope, user, rest);
            if named.is_empty() { inbox.to_string() } else { named }
        }
    }
}

/// "3m ago", "2h ago": how long since `ts`, as a line to an agent says it.
pub fn ago(ts: f64, at: f64) -> String {
    let s = (at - ts).max(0.0);
    if s < 60.0 {
        "just now".to_string()
    } else if s < 3600.0 {
        format!("{}m ago", (s / 60.0) as i64)
    } else if s < 86_400.0 {
        format!("{}h ago", (s / 3600.0) as i64)
    } else {
        format!("{}d ago", (s / 86_400.0) as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    /// A Δ line handed to a helper during its tool call from message m1 is
    /// not seen by that call's own PostToolUse (still m1), only by its next
    /// call from a later message.
    #[test]
    fn a_heads_up_is_seen_only_by_a_call_from_a_later_message() {
        let db = store();
        let scope = "ws:lattice";
        let sent = now() - 120.0;
        // the gate's notice, at a PreToolUse that names no message
        received(&db, scope, "kevin@x.com", "s1", "w1", "gate", &[("Cork: ana is changing api.js".into(), sent)], "");
        // that tool call's PostToolUse names its message: the notice arrived during it
        settle(&db, scope, "kevin@x.com", "s1", "w1", "m1");
        received(&db, scope, "kevin@x.com", "s1", "w1", "delta", &[("Δ api.js::req renamed → request".into(), sent)], "m1");
        settle(&db, scope, "kevin@x.com", "s1", "w1", "m1");
        let shown = log(&db, &[(scope.to_string(), "lattice".to_string())], 0.0, 10);
        assert_eq!(shown.len(), 2);
        assert!(shown.iter().all(|r| r["seen_at"].is_null() && r["received_at"].as_f64().is_some()), "{shown:?}");
        assert_eq!(shown[0]["to"], json!("kevin@x.com#s1/w1"));
        assert_eq!(shown[0]["sent_at"], json!(sent));
        // the lead's calls settle nothing of its helper's
        settle(&db, scope, "kevin@x.com", "s1", "", "m9");
        assert!(log(&db, &[(scope.to_string(), String::new())], 0.0, 10).iter().all(|r| r["seen_at"].is_null()));
        // the helper's next message: both seen
        settle(&db, scope, "kevin@x.com", "s1", "w1", "m2");
        assert!(log(&db, &[(scope.to_string(), String::new())], 0.0, 10).iter().all(|r| r["seen_at"].as_f64().is_some()));
        assert!(db.eph_get(&unseen_key(scope, "kevin@x.com#s1/w1")).is_none(), "nothing left waiting");
    }

    #[test]
    fn a_message_handed_over_is_seen_at_the_next_message() {
        let db = store();
        let scope = "ws:lattice";
        let message = json!({"id": "i1", "to": "kevin@x.com#s1/w1"});
        crate::agenttools::mark_delivered(&db, std::slice::from_ref(&message), "s1/w1");
        received_messages(&db, scope, "kevin@x.com", "s1", "w1", &["i1".to_string()], "m1");
        settle(&db, scope, "kevin@x.com", "s1", "w1", "m1");
        let stamp = db.eph_get(&crate::agenttools::seen_key("i1")).unwrap();
        assert!(stamp["delivered_at"].as_f64().is_some() && stamp["seen_at"].is_null(), "{stamp}");
        settle(&db, scope, "kevin@x.com", "s1", "w1", "m2");
        let stamp = db.eph_get(&crate::agenttools::seen_key("i1")).unwrap();
        assert!(stamp["seen_at"].as_f64().is_some(), "{stamp}");
    }
}
