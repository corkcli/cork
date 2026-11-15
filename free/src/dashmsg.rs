//! Messages from people to agents, sent from the dashboard. Agents already
//! message each other (`agenttools::send_agent_message`, the hooks'
//! `POST /message`); this is the same mail with a person at the keyboard:
//! filed in the recipients' inboxes, pushed to an open session through
//! `/inbox/wait` (`push::arrived`), carried by an idle agent's next hook.
//!
//! Three ways to address one, as the composer offers them:
//!
//! - **everyone** (no `to`, no `anchor`): every player who may work in this
//!   repo, the sender included — "all agents in the org". A broadcast, so it
//!   never starts an idle session (`push.rs`), and one line in the thread;
//! - **one person's agents** (`to`, their account id; `session` rides along
//!   so the thread can say which agent was meant — delivery is per person,
//!   the way every inbox is keyed);
//! - **whoever is on the code** (`anchor`, `path::symbol`, a path or `dir/`):
//!   `send_anchored_message`, which routes to the owners of open intents and
//!   recent editors of it and its callers.
//!
//! The thread is `agenttools::THREAD_BUCKET`: every message sent in the
//! repo, agents' replies included, oldest first.

use serde_json::{json, Value};

use crate::agenttools::{self, THREAD_BUCKET};
use crate::store::{now, Store};

/// The longest message the composer may send (the inbox keeps 4000).
pub const MAX_CHARS: usize = 4000;
/// How many thread messages one read returns, newest last.
pub const THREAD_LIMIT: usize = 120;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Send one message from `sender` (the signed-in person's account id).
/// `body`: `{text, to?, session?, anchor?}`.
#[allow(dead_code)]
pub fn send(
    store: &Store, scope: &str, workspace: &str, repo_id: &str, sender: &str, body: &Value, dashboard_url: &str,
) -> Value {
    send_with(store, scope, workspace, repo_id, sender, body, dashboard_url, &|_, _, _| false)
}

/// `send`, with a live line to one agent session: `live(to, session, text)`
/// delivers on the session's protocol socket (`protocol_ws::Hub::
/// deliver_to_session`) and says whether it did. A message it delivered is
/// filed in the thread and the ledger but not the inbox; one it could not
/// goes to the inbox as before.
#[allow(clippy::too_many_arguments)]
pub fn send_with(
    store: &Store, scope: &str, workspace: &str, repo_id: &str, sender: &str, body: &Value, dashboard_url: &str,
    live: &dyn Fn(&str, &str, &str) -> bool,
) -> Value {
    let message: String = text(body, "text").chars().take(MAX_CHARS).collect();
    if message.is_empty() {
        return json!({"ok": false, "error": "nothing to send: write a message first"});
    }
    let (to, anchor, session) = (text(body, "to"), text(body, "anchor"), text(body, "session"));
    if !anchor.is_empty() {
        return agenttools::send_anchored_message(store, scope, workspace, sender, &to, &message, &anchor, "dashboard", dashboard_url);
    }
    if !to.is_empty() {
        if let Some(refusal) = crate::access::messaging_refusal(store, workspace, sender, &to, dashboard_url) {
            return json!({"ok": false, "error": refusal});
        }
        if !session.is_empty() {
            // the inbox is the person's; the session says which agent the
            // sender had in mind, and travels with the message
            let id = crate::compat::new_id();
            let ts = now();
            let mut record = json!({
                "id": id, "to": to, "from": sender, "message": message, "agent": "dashboard", "ts": ts,
                "session": session,
            });
            // that one agent, now, when its socket is open; its inbox otherwise
            let delivered = live(&to, &session, &message);
            if delivered {
                record["live"] = json!(true);
            } else {
                let _ = store.kv_put("inbox", &format!("{scope}:{to}:{id}"), &record, ts);
            }
            let _ = store.ledger_append(scope, "agent_message", &json!({
                "to": to, "from": sender, "id": id, "chars": message.chars().count(), "session": session, "live": delivered,
            }), ts);
            agenttools::file_thread(store, scope, &record);
            if !delivered {
                crate::push::arrived();
            }
            return json!({"ok": true, "message_id": id, "delivered_to": [to], "live": delivered});
        }
        return agenttools::send_agent_message(store, scope, sender, &to, &message, "dashboard");
    }
    // everyone: the owner and every player with this repo, as the seat
    // roster counts them; a plan that keeps messages to one's own agents
    // reaches the sender's alone
    let roster = crate::access::seat_roster(store, workspace);
    let mut people: Vec<String> = Vec::new();
    for member in roster.owner.iter().chain(roster.players.iter()) {
        if !crate::access::has_repo_access(Some(member), repo_id) {
            continue;
        }
        let label = crate::access::member_label(member);
        if label.is_empty() || people.contains(&label) {
            continue;
        }
        if crate::access::messaging_refusal(store, workspace, sender, &label, dashboard_url).is_none() {
            people.push(label);
        }
    }
    if people.is_empty() {
        return json!({"ok": false, "error": "nobody in this workspace may work in this repo yet"});
    }
    // each copy has its own inbox id; the thread row keeps them, so the
    // log can say which of the people have read it
    let mut ids = serde_json::Map::new();
    for person in &people {
        let sent = agenttools::send_agent_message_as(store, scope, sender, person, &message, "dashboard", true);
        if let Some(id) = sent.get("message_id").and_then(Value::as_str) {
            ids.insert(person.clone(), json!(id));
        }
    }
    let id = crate::compat::new_id();
    agenttools::file_thread(store, scope, &json!({
        "id": id, "to": "*", "to_list": people, "from": sender, "message": message, "agent": "dashboard",
        "ts": now(), "broadcast": true, "ids": ids,
    }));
    json!({"ok": true, "message_id": id, "delivered_to": people})
}

/// The repo's messages, oldest first: what people sent from here and what
/// agents sent each other and their people. On a plan that keeps messages
/// to one's own agents, only the reader's own conversation.
pub fn thread(store: &Store, scope: &str, workspace: &str, reader: &str, since: f64) -> Value {
    let own_only = crate::access::plan_for(store, workspace).word("messaging", "team") == "own";
    let me = reader.trim().to_lowercase();
    let mut rows: Vec<Value> = store
        .kv_list(THREAD_BUCKET, &format!("{scope}:"))
        .into_iter()
        .map(|(_, record)| record)
        // new since the last read, or taken back since: its new copy replaces the old
        .filter(|record| {
            let at = |key: &str| record.get(key).and_then(Value::as_f64).unwrap_or(0.0);
            at("ts") > since || at("retracted") > since
        })
        .filter(|record| {
            if !own_only {
                return true;
            }
            let from = text(record, "from").to_lowercase();
            let to = text(record, "to").to_lowercase();
            from == me || to == me
        })
        .collect();
    if rows.len() > THREAD_LIMIT {
        rows.drain(..rows.len() - THREAD_LIMIT);
    }
    json!({"ok": true, "messages": rows})
}

// ------------------------------------------------------------- the log

/// The most messages one log read returns, newest first.
pub const LOG_LIMIT: usize = 500;
/// How far back the log reaches at most, in days (and no further than the
/// plan's history window).
pub const LOG_MAX_DAYS: f64 = 30.0;

/// `"user#session"` (a person's own session, as `local::send` signs and
/// addresses) split in two; a bare user has no session.
fn person_and_session(who: &str) -> (String, String) {
    match who.split_once('#') {
        Some((user, session)) => (user.to_string(), session.to_string()),
        None => (who.to_string(), String::new()),
    }
}

/// How a message was sent, from the agent that sent it.
fn via_of(agent: &str) -> &'static str {
    match agent {
        "dashboard" => "dashboard",
        "collide-hook" => "hook",
        _ => "mcp",
    }
}

/// Every message sent in these repos since `since`, newest first, at most
/// `LOG_LIMIT`: who sent it from where, whom it was for, and how far it got
/// with each recipient. `scopes` pairs each scope the reader may see with
/// the repo id to show for it (the display id of a renamed repo). On a plan
/// that keeps messages to one's own agents, only the reader's own.
pub fn log(store: &Store, scopes: &[(String, String)], workspace: &str, reader: &str, since: f64) -> Value {
    let own_only = crate::access::plan_for(store, workspace).word("messaging", "team") == "own";
    let me = reader.trim().to_lowercase();
    let mut rows: Vec<(&str, &str, Value)> = Vec::new();
    for (scope, display) in scopes {
        for (_, record) in store.kv_list(THREAD_BUCKET, &format!("{scope}:")) {
            if record.get("ts").and_then(Value::as_f64).unwrap_or(0.0) < since {
                continue;
            }
            if own_only {
                let from = person_and_session(&text(&record, "from")).0.to_lowercase();
                let to = person_and_session(&text(&record, "to")).0.to_lowercase();
                let listed = record.get("to_list").and_then(Value::as_array).into_iter().flatten()
                    .filter_map(Value::as_str).any(|p| person_and_session(p).0.to_lowercase() == me);
                if from != me && to != me && !listed {
                    continue;
                }
            }
            rows.push((scope.as_str(), display.as_str(), record));
        }
    }
    let ts = |record: &Value| record.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    rows.sort_by(|a, b| ts(&b.2).partial_cmp(&ts(&a.2)).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(LOG_LIMIT);
    let mut reach = std::collections::HashMap::new();
    let messages: Vec<Value> =
        rows.iter().map(|(scope, display, record)| log_entry(store, scope, display, record, &mut reach)).collect();
    // the Δ lines and gate notices agents were handed, each with when it was
    // sent, received and seen (headsup.rs); on a plan that keeps to one's
    // own agents, only the reader's
    let mut headsups = crate::headsup::log(store, scopes, since, LOG_LIMIT);
    if own_only {
        headsups.retain(|row| text(row, "user").to_lowercase() == me);
    }
    json!({"messages": messages, "headsups": headsups})
}

/// One thread row as the log shows it. A recipient it is still waiting for
/// carries `reach` (`presence::reach_of`): whether an open session hears it
/// at its next step, an MCP client on its next Cork call, or no agent of
/// theirs is open, so "waiting" says what it is waiting for. `reach` caches
/// that per person for one read of the log.
fn log_entry(
    store: &Store, scope: &str, display: &str, record: &Value, reach: &mut std::collections::HashMap<String, Value>,
) -> Value {
    let sent = record.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let agent = text(record, "agent");
    let (from_user, signed_session) = person_and_session(&text(record, "from"));
    let from_session = match text(record, "from_session") {
        empty if empty.is_empty() => signed_session,
        named => named,
    };
    // recorded when it was sent; a row from before that, what is known now.
    // An older row's `from_worktree` is not shown: Cork tracks branches
    let (branch, title) = if record.get("from_branch").is_some() {
        (text(record, "from_branch"), text(record, "from_title"))
    } else {
        agenttools::sender_context(store, scope, &from_user, &from_session)
    };
    let to_raw = text(record, "to");
    let broadcast = record.get("broadcast").and_then(Value::as_bool) == Some(true) || to_raw == "*";
    let list: Vec<String> = match record.get("to_list").and_then(Value::as_array) {
        Some(people) => people.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        None if !to_raw.is_empty() && to_raw != "*" => vec![to_raw.clone()],
        None => Vec::new(),
    };
    let (to_user, to_signed) = if broadcast { (String::new(), String::new()) } else { person_and_session(&to_raw) };
    let to_session = match text(record, "session") {
        empty if empty.is_empty() => to_signed,
        named => named,
    };
    let live = record.get("live").and_then(Value::as_bool) == Some(true);
    let id = text(record, "id");
    let recipients: Vec<Value> = list
        .iter()
        .enumerate()
        .map(|(i, person)| {
            // each copy's inbox id: kept on the row since the log came; before
            // that, the row's own id is the first (or only) person's, and a
            // broadcast's copies are found by sender and text
            let inbox_id = record
                .get("ids")
                .and_then(|ids| ids.get(person))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| (i == 0 && !broadcast).then(|| id.clone()))
                .or_else(|| copy_in_inbox(store, scope, person, record));
            let (in_inbox, seen) = match &inbox_id {
                Some(inbox_id) => (
                    store.kv_get("inbox", &format!("{scope}:{person}:{inbox_id}")).is_some(),
                    store.eph_get(&agenttools::seen_key(inbox_id)),
                ),
                None => (false, None),
            };
            let (status, delivered_at, read_at) =
                agenttools::delivery_status(seen.as_ref(), live, sent, in_inbox);
            let (user, addressed) = person_and_session(person);
            let reached = seen.as_ref().map(|s| text(s, "session")).unwrap_or_default();
            let session = [reached, addressed, if list.len() == 1 { to_session.clone() } else { String::new() }]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or_default();
            let mut row = json!({
                "user": user, "session": session, "status": status, "delivered_at": delivered_at, "read_at": read_at,
                // its agent's next call from a later message (headsup.rs)
                "seen_at": seen.as_ref().and_then(|s| s.get("seen_at")).cloned().unwrap_or(Value::Null),
                "expires_at": sent + agenttools::MESSAGE_TTL_S,
            });
            // a helper inside a session: the name `--to` takes
            if let Some((in_session, worker)) = session.split_once('/') {
                let name = crate::local::helper_name(store, scope, &user, in_session, worker);
                if !name.is_empty() {
                    row["helper"] = json!(name);
                }
            }
            if status == "pending" {
                let workspace = crate::presence::split_scope(scope).0;
                row["reach"] = reach
                    .entry(user.clone())
                    .or_insert_with(|| crate::presence::reach_of(store, workspace, &user))
                    .clone();
            }
            row
        })
        .collect();
    json!({
        "id": id, "ts": sent, "repo": display, "text": text(record, "message"),
        // taken back by its sender: its words are gone (agenttools::retract_message)
        "retracted": record.get("retracted").cloned().unwrap_or(Value::Null),
        "from": {
            "user": from_user, "agent": agent, "session": from_session, "title": title,
            "branch": branch, "via": via_of(&agent),
        },
        "to": {
            "user": to_user, "session": to_session, "anchor": text(record, "anchor"),
            "broadcast": broadcast, "list": list,
        },
        "recipients": recipients,
    })
}

/// A broadcast's copy in one person's inbox, for a row from before the
/// thread kept each copy's id: same sender, same text, filed within a few
/// seconds of it. None once it has been acked.
fn copy_in_inbox(store: &Store, scope: &str, person: &str, record: &Value) -> Option<String> {
    let (from, said, ts) = (text(record, "from"), text(record, "message"), record.get("ts").and_then(Value::as_f64)?);
    store
        .kv_list("inbox", &format!("{scope}:{person}:"))
        .into_iter()
        .find(|(_, copy)| {
            text(copy, "from") == from && text(copy, "message") == said
                && (copy.get("ts").and_then(Value::as_f64).unwrap_or(0.0) - ts).abs() < 5.0
        })
        .and_then(|(key, _)| key.rsplit_once(':').map(|(_, id)| id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    /// A paid workspace: Free keeps messages to one's own agents.
    fn team(store: &Store, ws: &str) {
        store.kv_put(crate::billing::BILLING_BUCKET, ws, &json!({"plan": "team", "seats": 5, "status": "active"}), 1.0).unwrap();
    }

    fn member(store: &Store, ws: &str, uid: &str, email: &str, role: &str) {
        let _ = store.kv_put("member", &format!("{ws}/{uid}"), &json!({
            "uid": uid, "email": email, "role": role, "access": "write", "repos": "*", "added": 0.0,
        }), 0.0);
    }

    #[test]
    fn a_message_to_one_person_lands_in_their_inbox_and_the_thread() {
        let db = store();
        let scope = "ws1:github.com/acme/api";
        team(&db, "ws1");
        let sent = send(&db, scope, "ws1", "github.com/acme/api", "kim@x.dev",
            &json!({"text": "  rebase onto main first  ", "to": "lee@x.dev", "session": "s9"}), "");
        assert_eq!(sent["ok"], json!(true), "{sent}");
        let inbox = db.kv_list("inbox", &format!("{scope}:lee@x.dev:"));
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].1["message"], json!("rebase onto main first"));
        assert_eq!(inbox[0].1["from"], json!("kim@x.dev"));
        assert_eq!(inbox[0].1["session"], json!("s9"));
        let read = thread(&db, scope, "ws1", "kim@x.dev", 0.0);
        let rows = read["messages"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["to"], json!("lee@x.dev"));
    }

    #[test]
    fn a_message_to_a_connected_session_goes_live_and_skips_the_inbox() {
        let db = store();
        let scope = "ws1:github.com/acme/api";
        team(&db, "ws1");
        let seen = std::cell::RefCell::new(Vec::new());
        let live = |to: &str, session: &str, text: &str| {
            seen.borrow_mut().push((to.to_string(), session.to_string(), text.to_string()));
            session == "s9"
        };
        let sent = send_with(&db, scope, "ws1", "github.com/acme/api", "kim@x.dev",
            &json!({"text": "stop, rebase first", "to": "lee@x.dev", "session": "s9"}), "", &live);
        assert_eq!(sent["live"], json!(true), "{sent}");
        assert_eq!(seen.borrow()[0], ("lee@x.dev".into(), "s9".into(), "stop, rebase first".into()));
        assert!(db.kv_list("inbox", &format!("{scope}:lee@x.dev:")).is_empty());
        assert_eq!(thread(&db, scope, "ws1", "kim@x.dev", 0.0)["messages"].as_array().unwrap().len(), 1);
        // a session that is not connected: the inbox, as before
        let sent = send_with(&db, scope, "ws1", "github.com/acme/api", "kim@x.dev",
            &json!({"text": "later", "to": "lee@x.dev", "session": "s10"}), "", &live);
        assert_eq!(sent["live"], json!(false));
        assert_eq!(db.kv_list("inbox", &format!("{scope}:lee@x.dev:")).len(), 1);
    }

    #[test]
    fn everyone_means_each_player_once_and_one_line_in_the_thread() {
        let db = store();
        let (ws, repo) = ("ws2", "github.com/acme/api");
        let scope = format!("{ws}:{repo}");
        team(&db, ws);
        member(&db, ws, "u1", "kim@x.dev", "owner");
        member(&db, ws, "u2", "lee@x.dev", "member");
        let sent = send(&db, &scope, ws, repo, "kim@x.dev", &json!({"text": "freeze at five"}), "");
        assert_eq!(sent["ok"], json!(true), "{sent}");
        let mut to: Vec<String> = sent["delivered_to"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        to.sort();
        assert_eq!(to, vec!["kim@x.dev", "lee@x.dev"], "the sender's own agents hear it too");
        assert_eq!(db.kv_list("inbox", &format!("{scope}:lee@x.dev:")).len(), 1);
        assert_eq!(db.kv_list("inbox", &format!("{scope}:lee@x.dev:"))[0].1["broadcast"], json!(true));
        let rows = thread(&db, &scope, ws, "lee@x.dev", 0.0)["messages"].as_array().unwrap().clone();
        assert_eq!(rows.len(), 1, "one line, not one per recipient");
        assert_eq!(rows[0]["to"], json!("*"));
    }

    #[test]
    fn an_agents_reply_shows_in_the_thread_and_an_empty_message_does_not_send() {
        let db = store();
        let scope = "ws3:github.com/acme/api";
        agenttools::send_agent_message(&db, scope, "lee@x.dev", "kim@x.dev", "done, rebased", "claude-code");
        let rows = thread(&db, scope, "ws3", "kim@x.dev", 0.0)["messages"].as_array().unwrap().clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["agent"], json!("claude-code"));
        assert_eq!(rows[0]["message"], json!("done, rebased"));
        let refused = send(&db, scope, "ws3", "github.com/acme/api", "kim@x.dev", &json!({"text": "   "}), "");
        assert_eq!(refused["ok"], json!(false));
        assert_eq!(thread(&db, scope, "ws3", "kim@x.dev", 0.0)["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn on_free_a_message_reaches_only_the_senders_own_agents() {
        let db = store();
        let (ws, repo) = ("ws6", "github.com/acme/api");
        let scope = format!("{ws}:{repo}");
        member(&db, ws, "u1", "kim@x.dev", "owner");
        member(&db, ws, "u2", "lee@x.dev", "member");
        let refused = send(&db, &scope, ws, repo, "kim@x.dev", &json!({"text": "hi", "to": "lee@x.dev"}), "");
        assert_eq!(refused["ok"], json!(false));
        let everyone = send(&db, &scope, ws, repo, "kim@x.dev", &json!({"text": "hi all"}), "");
        assert_eq!(everyone["delivered_to"], json!(["kim@x.dev"]));
    }

    #[test]
    fn the_thread_keeps_the_newest_and_reads_since() {
        let db = store();
        let scope = "ws4:r";
        for i in 0..(agenttools::THREAD_KEEP + 5) {
            agenttools::file_thread(&db, scope, &json!({"id": format!("m{i}"), "ts": 1000.0 + i as f64, "from": "a", "to": "b", "message": "x"}));
        }
        let all = db.kv_list(THREAD_BUCKET, &format!("{scope}:"));
        assert_eq!(all.len(), agenttools::THREAD_KEEP);
        assert_eq!(all[0].1["id"], json!("m5"), "the oldest went first");
        let since = thread(&db, scope, "ws4", "a", 1000.0 + (agenttools::THREAD_KEEP as f64) + 2.0)["messages"]
            .as_array().unwrap().len();
        assert_eq!(since, 2);
    }

    #[test]
    fn a_recipients_status_comes_from_its_stamps_then_the_inbox() {
        use agenttools::{delivery_status, delivery_status_at, MESSAGE_TTL_S};
        let (status, delivered, read) = delivery_status_at(None, false, 10.0, true, 20.0);
        assert_eq!((status, delivered, read), ("pending", Value::Null, Value::Null), "still in the inbox, never shown");
        assert_eq!(delivery_status_at(None, false, 10.0, true, 11.0 + MESSAGE_TTL_S), ("expired", Value::Null, Value::Null),
            "past its week, never delivered: expired, not waiting for ever");
        let seen_late = json!({"delivered_at": 12.0, "read_at": null});
        assert_eq!(delivery_status_at(Some(&seen_late), false, 10.0, true, 11.0 + MESSAGE_TTL_S).0, "delivered",
            "a delivered one never expires");
        let seen = json!({"delivered_at": 12.0, "read_at": null});
        assert_eq!(delivery_status(Some(&seen), false, 10.0, true), ("delivered", json!(12.0), Value::Null));
        let acked = json!({"delivered_at": 12.0, "read_at": 15.0});
        assert_eq!(delivery_status(Some(&acked), false, 10.0, false), ("read", json!(12.0), json!(15.0)));
        assert_eq!(delivery_status(None, true, 10.0, false), ("delivered", json!(10.0), Value::Null),
            "pushed live on the session's socket: delivered as it was sent");
        assert_eq!(delivery_status(None, false, 10.0, false), ("read", Value::Null, Value::Null),
            "gone from the inbox with no stamp: acked, its time unknown");
    }

    #[test]
    fn stamps_keep_the_first_delivery_and_an_ack_implies_one() {
        let db = store();
        let message = json!({"id": "m1", "to": "lee@x.dev"});
        agenttools::mark_delivered(&db, std::slice::from_ref(&message), "s1");
        let first = db.eph_get(&agenttools::seen_key("m1")).unwrap();
        agenttools::mark_delivered(&db, std::slice::from_ref(&message), "s2");
        assert_eq!(db.eph_get(&agenttools::seen_key("m1")).unwrap(), first, "the first delivery stands");
        agenttools::mark_read(&db, "m1", "lee@x.dev");
        let read = db.eph_get(&agenttools::seen_key("m1")).unwrap();
        assert_eq!(read["delivered_at"], first["delivered_at"]);
        assert_eq!(read["session"], json!("s1"));
        assert!(read["read_at"].as_f64().is_some());
        agenttools::mark_read(&db, "m2", "lee@x.dev");
        let straight = db.eph_get(&agenttools::seen_key("m2")).unwrap();
        assert_eq!(straight["delivered_at"], straight["read_at"], "acked unseen: delivered when read");
    }

    #[test]
    fn the_log_follows_each_message_from_sent_to_read_newest_first() {
        let db = store();
        let (ws, repo) = ("ws7", "github.com/acme/api");
        let scope = format!("{ws}:{repo}");
        team(&db, ws);
        member(&db, ws, "u1", "kim@x.dev", "owner");
        member(&db, ws, "u2", "lee@x.dev", "member");
        // an agent of kim's, on a branch, writes to lee
        db.eph_set(&crate::presence::focus_key(&scope, "kim@x.dev", "s-kim"),
            &json!({"branch": "feat/gate", "ts": now()}), Some(600.0)).unwrap();
        crate::freshness::note_title(&db, &scope, "kim@x.dev", "s-kim", "Gate rework");
        let sent = agenttools::send_agent_message_in(&db, &scope, "kim@x.dev", "s-kim", "lee@x.dev", "gate lands tonight", "claude-code", false);
        let agent_id = sent["message_id"].as_str().unwrap().to_string();
        // kim, on the dashboard, to everyone
        let everyone = send(&db, &scope, ws, repo, "kim@x.dev", &json!({"text": "freeze at five"}), "");
        assert_eq!(everyone["ok"], json!(true), "{everyone}");
        let scopes = vec![(scope.clone(), "github.com/acme/api-v2".to_string())];
        let read = |reader: &str| log(&db, &scopes, ws, reader, 0.0)["messages"].as_array().unwrap().clone();
        let rows = read("kim@x.dev");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["text"], json!("freeze at five"), "newest first");
        assert_eq!(rows[0]["repo"], json!("github.com/acme/api-v2"), "the display id");
        assert_eq!(rows[0]["from"]["via"], json!("dashboard"));
        assert_eq!(rows[0]["to"]["broadcast"], json!(true));
        assert_eq!(rows[0]["recipients"].as_array().unwrap().len(), 2);
        assert!(rows[0]["recipients"].as_array().unwrap().iter().all(|r| r["status"] == json!("pending")));
        let agent = &rows[1];
        assert_eq!(agent["from"], json!({
            "user": "kim@x.dev", "agent": "claude-code", "session": "s-kim", "title": "Gate rework",
            "branch": "feat/gate", "via": "mcp",
        }));
        assert_eq!(agent["to"], json!({"user": "lee@x.dev", "session": "", "anchor": "", "broadcast": false, "list": ["lee@x.dev"]}));
        assert_eq!(agent["recipients"][0]["status"], json!("pending"));
        // lee's session is shown it, then acks it
        let visible = BTreeSet::from([scope.clone()]);
        let inbox = agenttools::inbox_ack(&db, &scope, "lee@x.dev", &visible, &[]);
        agenttools::mark_delivered(&db, inbox["messages"].as_array().unwrap(), "s-lee");
        let shown = read("kim@x.dev")[1]["recipients"][0].clone();
        assert_eq!((shown["status"].clone(), shown["session"].clone()), (json!("delivered"), json!("s-lee")));
        assert!(shown["delivered_at"].as_f64().is_some() && shown["read_at"].is_null());
        agenttools::inbox_ack(&db, &scope, "lee@x.dev", &visible, &[agent_id]);
        let acked = read("kim@x.dev")[1]["recipients"][0].clone();
        assert_eq!(acked["status"], json!("read"));
        assert!(acked["read_at"].as_f64().is_some());
        // lee's copy of the broadcast was delivered with the rest; kim's is still waiting
        let broadcast = read("kim@x.dev")[0]["recipients"].as_array().unwrap().clone();
        let state = |who: &str| broadcast.iter().find(|r| r["user"] == json!(who)).unwrap()["status"].clone();
        assert_eq!((state("lee@x.dev"), state("kim@x.dev")), (json!("delivered"), json!("pending")));
        // a window that starts after both: nothing
        assert!(log(&db, &scopes, ws, "kim@x.dev", now() + 1.0)["messages"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_rows_sender_and_recipient_sessions_come_from_signed_names() {
        let db = store();
        let scope = "ws8:r";
        agenttools::file_thread(&db, scope, &json!({
            "id": "m1", "ts": 100.0, "from": "kim@x.dev#s1", "to": "kim@x.dev#s2", "message": "pull", "agent": "collide-hook",
        }));
        let rows = log(&db, &[(scope.to_string(), "r".to_string())], "ws8", "kim@x.dev", 0.0)["messages"].as_array().unwrap().clone();
        assert_eq!(rows[0]["from"]["user"], json!("kim@x.dev"));
        assert_eq!(rows[0]["from"]["session"], json!("s1"));
        assert_eq!(rows[0]["from"]["via"], json!("hook"));
        assert_eq!((rows[0]["to"]["user"].clone(), rows[0]["to"]["session"].clone()), (json!("kim@x.dev"), json!("s2")));
        assert_eq!(rows[0]["recipients"][0]["session"], json!("s2"));
    }

    #[test]
    fn a_stranger_is_refused_at_the_door() {
        let db = store();
        member(&db, "ws5", "u1", "kim@x.dev", "owner");
        let gate = crate::auth::DashboardAuth { test_mode: true, firebase_project_id: String::new(), firebase_emulator: false };
        let stranger = gate.principal_for(&db, "test:u9", now());
        assert!(stranger.is_some());
        let refused = crate::auth::repo_auth(&db, stranger, "ws5", "github.com/acme/api");
        assert!(matches!(refused, Err((403, _)) | Err((401, _))), "a non-member may not send or read");
        assert!(crate::auth::repo_auth(&db, None, "ws5", "github.com/acme/api").is_err(), "no credential, no thread");
        let owner = gate.principal_for(&db, "test:u1", now());
        assert!(crate::auth::repo_auth(&db, owner, "ws5", "github.com/acme/api").is_ok(), "a member is let in");
    }
}
