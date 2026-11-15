//! The smaller agent-facing tools: directed messaging, the reconciliation
//! claim queue, `explain_code`'s ledger-with-why, graph interchange, and the
//! dashboard deep-link builder.
//!
//! Each function takes `&Store` plus an explicit scope/user/etc. and
//! returns the `Value` an MCP tool call answers with. `move_repo`,
//! `remove_repo` and `switch_workspace` live in `watch.rs`.
//!
//! Wired from `mcp.rs`'s dispatch and the `/inbox` route; `inbox_ack` also
//! feeds the `messages_pending` delivery every carrying tool call makes
//! (`advisory::apply_setup_hint`).

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::presence::split_scope;
use crate::store::{now, Store};

// ------------------------------------------------------------- small helpers

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// First 240 Unicode characters — by code point, not byte: a multi-byte
/// character split mid-sequence would not be valid UTF-8.
fn truncate_chars(value: &str, n: usize) -> String {
    value.chars().take(n).collect()
}

// ------------------------------------------------------- agent-to-agent mail

/// Queue a directed message for one teammate's agents. Delivered two ways:
/// the awake path (rides the recipient's next tool-call response) and
/// `GET /api/inbox`, which a wake poller reads to start a headless agent on
/// an otherwise idle machine. Mirrors `service.send_agent_message`.
pub fn send_agent_message(
    store: &Store, scope: &str, from_user: &str, to: &str, message: &str, agent: &str,
) -> Value {
    send_agent_message_as(store, scope, from_user, to, message, agent, false)
}

/// `send_agent_message`, saying whether it is one copy of a note to every
/// agent at work in the repo (`broadcast`): news for whoever is working,
/// carried by their next hook, never a reason to start an idle session.
pub fn send_agent_message_as(
    store: &Store, scope: &str, from_user: &str, to: &str, message: &str, agent: &str, broadcast: bool,
) -> Value {
    send_agent_message_in(store, scope, from_user, "", to, message, agent, broadcast)
}

/// `send_agent_message_as` from one session of the sender (`session`, ""
/// when unknown): the thread row says which agent wrote it and on what
/// branch (`with_sender`), for the dashboard's message log.
#[allow(clippy::too_many_arguments)]
pub fn send_agent_message_in(
    store: &Store, scope: &str, from_user: &str, session: &str, to: &str, message: &str, agent: &str, broadcast: bool,
) -> Value {
    send_message_in(store, scope, from_user, session, to, message, agent, broadcast, false)
}

/// A handoff to one agent (`local::handoff` picks which): work the person
/// wants done, as a spec the recipient starts on by itself. Its record says
/// so (`is_handoff`), so every path that delivers it (the session's
/// listener, a hook's inbox lines, the wake watcher) frames it as the
/// recipient's to do, not as news.
pub fn send_handoff_in(store: &Store, scope: &str, from_user: &str, session: &str, to: &str, message: &str, agent: &str) -> Value {
    send_message_in(store, scope, from_user, session, to, message, agent, false, true)
}

/// Whether a message is a handoff: work for its recipient, not news.
pub fn is_handoff(message: &Value) -> bool {
    message.get("handoff").and_then(Value::as_bool) == Some(true)
}

#[allow(clippy::too_many_arguments)]
fn send_message_in(
    store: &Store, scope: &str, from_user: &str, session: &str, to: &str, message: &str, agent: &str, broadcast: bool,
    handoff: bool,
) -> Value {
    let to = to.trim();
    let message = message.trim();
    if message.is_empty() || to.is_empty() {
        return json!({"ok": false, "error": "empty recipient or message"});
    }
    let id = crate::compat::new_id();
    let ts = now();
    let truncated = truncate_chars(message, 4000);
    let mut record = json!({
        "id": id, "to": to, "from": from_user, "message": truncated, "agent": agent, "ts": ts,
    });
    if handoff {
        record["handoff"] = json!(true);
    }
    // the session it came from, as its hooks stamped the command: who the
    // reader is told sent it (`vouch`)
    if !session.trim().is_empty() {
        record["sender_session"] = json!(session.trim());
    }
    if broadcast {
        record["broadcast"] = json!(true);
    } else {
        // one note to one person is one line in the repo's thread; a
        // broadcast is filed there once, by whoever sent it to everyone
        file_thread(store, scope, &with_sender(store, scope, &record, session));
    }
    if !broadcast && is_to_self(from_user, to) && !session.trim().is_empty() {
        // a note from one of the person's sessions to their own agents is
        // for the OTHER sessions, each once: the sending one is marked as
        // told now, so its own next hook does not hand it back, and the
        // first session to see it does not use it up for the rest
        // (`push::hook_lines`, `push::take`)
        record["from_session"] = json!(session.trim());
        let _ = store.eph_set(&format!("briefmsg:{}:{id}", session.trim()), &json!({"ts": ts}),
            Some(crate::access::TOLD_ONCE_TTL_S));
    }
    let _ = store.kv_put("inbox", &format!("{scope}:{to}:{id}"), &record, ts);
    crate::push::arrived();
    let mut row = json!({"to": to, "from": from_user, "id": id, "chars": truncated.chars().count()});
    // a handoff is counted apart (habits.rs): work routed, not news
    if handoff {
        row["handoff"] = json!(true);
    }
    let _ = store.ledger_append(scope, "agent_message", &row, ts);
    // the receipt: who it went to, as the anchored form already says
    json!({"ok": true, "message_id": id, "delivered_to": [to]})
}

/// Take back a message this person sent: `id` from the send's receipt, or
/// "last" for their newest one in this repo (from this session, when it is
/// named). A copy no one has received yet is withdrawn before anyone reads
/// it; whoever already has it is sent a short note to disregard it (a
/// message read cannot be unread), and so are the sender's own other
/// sessions, which may each have been told. The thread keeps the row,
/// marked retracted, without its words. Only the account that sent it may
/// take it back.
pub fn retract_message(store: &Store, scope: &str, by_user: &str, session: &str, id: &str, agent: &str) -> Value {
    let account = |label: &str| label.split('#').next().unwrap_or("").trim().to_ascii_lowercase();
    let me = account(by_user);
    let id = id.trim();
    let rows = store.kv_list(THREAD_BUCKET, &format!("{scope}:"));
    let session = session.trim();
    let found = if id.is_empty() || id == "last" {
        // the newest this caller sent: from this session when it names one
        // (a message filed without a session is taken back by its id), and
        // the newest even once it is taken back, so asking twice answers
        // "already" and never reaches an older message
        rows.iter().rev().find(|(_, row)| {
            account(&text(row, "from")) == me && (session.is_empty() || text(row, "from_session") == session)
        })
    } else {
        rows.iter().find(|(_, row)| text(row, "id") == id)
    };
    let Some((key, row)) = found.cloned() else {
        return json!({"ok": false, "error": if id.is_empty() || id == "last" {
            "no message from this session in this repo: take one back by its id"
        } else {
            "no message of yours by that id in this repo"
        }});
    };
    let message_id = text(&row, "id");
    if account(&text(&row, "from")) != me {
        return json!({"ok": false, "error": "only the person who sent a message can take it back"});
    }
    if row.get("retracted").is_some() {
        return json!({"ok": true, "message_id": message_id, "already": true, "note": "it was already taken back"});
    }
    // each copy: who it went to, and its id in their inbox
    let copies: Vec<(String, String)> = match row.get("ids").and_then(Value::as_object).filter(|ids| !ids.is_empty()) {
        Some(ids) => ids.iter().filter_map(|(who, inbox)| inbox.as_str().map(|i| (who.clone(), i.to_string()))).collect(),
        None => vec![(text(&row, "to"), message_id.clone())],
    };
    let ts = now();
    let snippet: String = text(&row, "message").chars().take(80).collect();
    let snippet = if text(&row, "message").chars().count() > 80 { format!("{snippet}…") } else { snippet };
    let note = format!(
        "{} took back the message they sent you (\"{snippet}\"): disregard it. If you already acted on it, tell your person.",
        text(&row, "from")
    );
    let (mut withdrawn, mut told) = (Vec::new(), Vec::new());
    for (who, inbox_id) in copies {
        if who.is_empty() || who == "*" {
            continue;
        }
        let inbox_key = format!("{scope}:{who}:{inbox_id}");
        let waiting = store.kv_get("inbox", &inbox_key).is_some();
        if waiting {
            let _ = store.kv_delete("inbox", &inbox_key);
        }
        if waiting && !is_to_self(by_user, &who) {
            withdrawn.push(who);
            continue;
        }
        let mut record = json!({
            "id": crate::compat::new_id(), "to": who, "from": text(&row, "from"), "message": note,
            "agent": agent, "ts": ts, "retraction_of": message_id,
        });
        if is_to_self(by_user, &who) && !session.is_empty() {
            // the sending session is not handed its own retraction
            record["from_session"] = json!(session);
            let _ = store.eph_set(&format!("briefmsg:{session}:{}", text(&record, "id")), &json!({"ts": ts}),
                Some(crate::access::TOLD_ONCE_TTL_S));
        }
        let _ = store.kv_put("inbox", &format!("{scope}:{who}:{}", text(&record, "id")), &record, ts);
        told.push(who);
    }
    if !told.is_empty() {
        crate::push::arrived();
    }
    let mut kept = row.clone();
    kept["message"] = json!("");
    kept["retracted"] = json!(ts);
    kept["retracted_by"] = json!(by_user);
    let _ = store.kv_put(THREAD_BUCKET, &key, &kept, ts);
    let _ = store.ledger_append(scope, "message_retracted", &json!({
        "id": message_id, "by": by_user, "withdrawn": withdrawn.len(), "told": told.len(),
    }), ts);
    let said = match (withdrawn.len(), told.len()) {
        (_, 0) => "taken back before anyone read it".to_string(),
        (0, n) => format!("taken back; {n} recipient(s) already had it and are told to disregard it"),
        (w, n) => format!("taken back; {w} copy(ies) withdrawn unread, {n} recipient(s) told to disregard it"),
    };
    json!({"ok": true, "message_id": message_id, "withdrawn": withdrawn, "told": told, "note": said})
}

/// `to` names the sender themself (their own agents), by the same account
/// label, any case; a `user#session` address is one session, not this.
pub fn is_to_self(from_user: &str, to: &str) -> bool {
    let from = from_user.split('#').next().unwrap_or("").trim();
    let to = to.trim();
    !from.is_empty() && !to.contains('#') && from.eq_ignore_ascii_case(to)
}

/// A message one session of a person sent to that person's own agents
/// (`send_agent_message_in` marks it with `from_session`): every one of
/// their other sessions is told it once, rather than the first one to look
/// taking it for all of them, and the sender is never handed its own.
pub fn is_own_session_note(message: &Value, user_id: &str) -> bool {
    !text(message, "from_session").is_empty() && is_to_self(&text(message, "from"), user_id)
}

/// How long a message waits to be delivered. Past it, an undelivered one is
/// no longer handed to anyone (a week-old instruction is not one to act on)
/// and the dashboard's log says it expired instead of "waiting" for ever.
/// A week: as long as the "a message is waiting" email keeps asking.
pub const MESSAGE_TTL_S: f64 = 7.0 * 86_400.0;

/// Sent longer ago than `MESSAGE_TTL_S`.
pub fn is_expired(sent_ts: f64, at: f64) -> bool {
    sent_ts > 0.0 && at - sent_ts > MESSAGE_TTL_S
}

/// Where every message sent in a repo is kept whole, for the dashboard's
/// thread: an inbox row is deleted when its recipient acks it, and the
/// ledger keeps only who and how long. Keyed by time, so the newest are the
/// last; the oldest past `THREAD_KEEP` are dropped as new ones arrive.
pub const THREAD_BUCKET: &str = "msgthread";
pub const THREAD_KEEP: usize = 400;

/// File one message in its repo's thread (see `THREAD_BUCKET`).
pub fn file_thread(store: &Store, scope: &str, record: &Value) {
    let ts = record.get("ts").and_then(Value::as_f64).unwrap_or_else(now);
    let id = text(record, "id");
    let prefix = format!("{scope}:");
    let _ = store.kv_put(THREAD_BUCKET, &format!("{prefix}{:017.6}:{id}", ts), record, ts);
    let (count, _) = store.kv_stat(THREAD_BUCKET, &prefix);
    if count as usize > THREAD_KEEP {
        let rows = store.kv_list(THREAD_BUCKET, &prefix);
        for (key, _) in rows.iter().take(rows.len().saturating_sub(THREAD_KEEP)) {
            let _ = store.kv_delete(THREAD_BUCKET, key);
        }
    }
}

/// A thread row with where its sender was when it was sent: the session,
/// its branch and title as the hooks last named them. The inbox
/// copy stays as it was; only the thread (the dashboard's log) carries
/// these. No session (a person on the dashboard, an MCP client that names
/// none) adds nothing.
pub fn with_sender(store: &Store, scope: &str, record: &Value, session: &str) -> Value {
    let session = session.trim();
    let mut row = record.clone();
    if session.is_empty() {
        return row;
    }
    let user = text(record, "from");
    let user = user.split_once('#').map(|(u, _)| u.to_string()).unwrap_or(user);
    // a helper's `session/worker`: the branch and title are its session's
    let (branch, title) = sender_context(store, scope, &user, session.split('/').next().unwrap_or(session));
    if let Some(map) = row.as_object_mut() {
        map.insert("from_session".into(), json!(session));
        map.insert("from_branch".into(), json!(branch));
        map.insert("from_title".into(), json!(title));
    }
    row
}

/// One session's branch and title as known now: the branch off its newest
/// presence marker (focus, then the week-long last action), the title as
/// `freshness` keeps it; "" for what no hook said.
pub fn sender_context(store: &Store, scope: &str, user: &str, session: &str) -> (String, String) {
    if session.is_empty() || user.is_empty() {
        return (String::new(), String::new());
    }
    let markers = [
        store.eph_get(&crate::presence::focus_key(scope, user, session)),
        store.eph_get(&crate::presence::last_action_key(scope, user, session)),
    ];
    let branch = markers
        .iter()
        .flatten()
        .filter(|m| !text(m, "branch").is_empty())
        .max_by(|a, b| {
            let ts = |m: &Value| m.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            ts(a).partial_cmp(&ts(b)).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|m| text(m, "branch"))
        .unwrap_or_default();
    (branch, crate::freshness::title_of(store, scope, user, session))
}

// ------------------------------------------------ Claude Code peer delivery
//
// A message to a Claude Code session on its sender's machine is also posted
// into that session through Claude Code's own peer messaging. The socket
// never answers, so a post that went through proves nothing: Cork's inbox
// copy stays, held back from the hooks and the listener, until the
// recipient's own hooks find the message in its transcript (then it is
// delivered, and never printed again) or a window passes without that
// (then it goes the usual way: worst case the agent reads it twice, never
// zero times).

/// How long a message is held while its sender posts it: past it, unsaid,
/// it goes the usual way.
pub const PEER_CLAIM_S: f64 = 30.0;
/// How long a posted message waits to be found in the recipient's
/// transcript, at the least, before the usual path delivers it.
pub const PEER_WINDOW_S: f64 = 60.0;
/// And how many of the recipient's hook calls, at the least.
pub const PEER_CALLS: u64 = 2;
/// How long a posted message's hold is kept at all.
const PEER_SENT_TTL_S: f64 = 86_400.0;

fn peer_claim_key(scope: &str, inbox: &str, id: &str) -> String {
    format!("peerclaim:{scope}:{inbox}:{id}")
}

/// Hold one inbox message (`inbox`'s, by its id) back for its sender's
/// peer delivery to `session`.
pub fn claim_for_peer(store: &Store, scope: &str, id: &str, inbox: &str, session: &str, by: &str) {
    let _ = store.eph_set(&peer_claim_key(scope, inbox, id), &json!({"session": session, "by": by}), Some(PEER_CLAIM_S));
}

/// Whether a message is held back for a peer delivery now. A posted one is
/// let go once `PEER_WINDOW_S` passed and the recipient made `PEER_CALLS`
/// hook calls without finding it; for the listener (`listener`), which
/// serves a session making no calls, once the window passed alone.
pub fn peer_held(store: &Store, scope: &str, inbox: &str, id: &str, listener: bool) -> bool {
    let key = peer_claim_key(scope, inbox, id);
    let Some(claim) = store.eph_get(&key) else { return false };
    let Some(sent) = claim.get("peer_sent").and_then(Value::as_f64) else { return true };
    let calls = claim.get("calls").and_then(Value::as_u64).unwrap_or(0);
    if now() - sent >= PEER_WINDOW_S && (listener || calls >= PEER_CALLS) {
        store.eph_delete(&key);
        return false;
    }
    true
}

/// One hook call of the session whose inbox this is: counted on each
/// message posted to it and not yet found.
pub fn peer_call(store: &Store, scope: &str, inbox: &str) {
    for (key, mut claim) in store.eph_scan(&format!("peerclaim:{scope}:{inbox}:")) {
        if claim.get("peer_sent").is_none() {
            continue;
        }
        claim["calls"] = json!(claim.get("calls").and_then(Value::as_u64).unwrap_or(0) + 1);
        let _ = store.eph_set(&key, &claim, Some(PEER_SENT_TTL_S));
    }
}

/// The sender says whether its post went through. It did: the message
/// stays held, waiting to be found in the recipient's transcript
/// (`peer_found`). It did not: it goes the usual way at once. Only the
/// claim's own sender may say.
pub fn settle_peer(store: &Store, scope: &str, by: &str, inbox: &str, id: &str, landed: bool) -> Value {
    let key = peer_claim_key(scope, inbox, id);
    let Some(mut claim) = store.eph_get(&key) else { return json!({"ok": false, "error": "no message held for that"}) };
    if text(&claim, "by") != by {
        return json!({"ok": false, "error": "only its sender settles a delivery"});
    }
    if !landed {
        store.eph_delete(&key);
        crate::push::arrived();
        return json!({"ok": true, "landed": false});
    }
    claim["peer_sent"] = json!(now());
    claim["calls"] = json!(0);
    let _ = store.eph_set(&key, &claim, Some(PEER_SENT_TTL_S));
    json!({"ok": true, "landed": true})
}

/// The recipient's hooks found these posted messages in its own
/// transcript: delivered and received, off the inbox, never printed again.
/// Returns how many it settled.
pub fn peer_found(store: &Store, scope: &str, inbox: &str, ids: &[String]) -> usize {
    let mut found = 0;
    for id in ids.iter().filter(|id| !id.is_empty()) {
        let key = peer_claim_key(scope, inbox, id);
        let Some(claim) = store.eph_get(&key) else { continue };
        store.eph_delete(&key);
        let inbox_key = format!("{scope}:{inbox}:{id}");
        if let Some(record) = store.kv_get("inbox", &inbox_key) {
            let _ = store.kv_delete("inbox", &inbox_key);
            mark_delivered(store, &[record], &text(&claim, "session"));
            mark_seen(store, std::slice::from_ref(id), now());
            found += 1;
        }
    }
    found
}

// --------------------------------------------------------- delivery stamps

/// When each inbox message reached its recipient, when they acked it and
/// when they were seen past it (`headsup.rs`), keyed by the inbox id (one
/// id, one recipient): `{to, session, delivered_at, read_at, seen_at}`. The
/// inbox row itself is deleted on ack, so without these the dashboard's log
/// could only guess. Kept a month, as long as the log reaches back.
pub const SEEN_TTL_S: f64 = 30.0 * 86_400.0;

pub fn seen_key(id: &str) -> String {
    format!("msgseen:{id}")
}

/// These messages were just handed to their recipient (a hook answer, a
/// tool reply, an inbox read, a push to a listening session): stamped once,
/// the first time, with the session it reached when the caller knows it.
pub fn mark_delivered(store: &Store, messages: &[Value], session: &str) {
    let stamp = now();
    for message in messages {
        let id = text(message, "id");
        if id.is_empty() {
            continue;
        }
        let key = seen_key(&id);
        let existing = store.eph_get(&key);
        if existing.as_ref().is_some_and(|seen| seen.get("delivered_at").and_then(Value::as_f64).is_some()) {
            continue;
        }
        let kept = |field: &str| existing.as_ref().and_then(|seen| seen.get(field).cloned()).unwrap_or(Value::Null);
        let _ = store.eph_set(&key, &json!({
            "to": text(message, "to"), "session": session, "delivered_at": stamp, "read_at": kept("read_at"),
            "seen_at": kept("seen_at"),
        }), Some(SEEN_TTL_S));
        // a handoff taken: its sender hears who took it
        if is_handoff(message) {
            crate::local::handoff_taken(store, &id, &text(message, "to"));
        }
    }
}

/// Their recipient was seen past these messages at `stamp` (`headsup::settle`):
/// a call from a later model message than the one they arrived during.
pub fn mark_seen(store: &Store, ids: &[String], stamp: f64) {
    for id in ids.iter().filter(|id| !id.is_empty()) {
        let key = seen_key(id);
        let Some(mut seen) = store.eph_get(&key) else { continue };
        if seen.get("seen_at").and_then(Value::as_f64).is_some() {
            continue;
        }
        seen["seen_at"] = json!(stamp);
        let _ = store.eph_set(&key, &seen, Some(SEEN_TTL_S));
    }
}

/// The recipient acked this message (`inbox_ack`, the hook poll's `ack`):
/// read now, and delivered no later than that.
pub fn mark_read(store: &Store, id: &str, to: &str) {
    if id.is_empty() {
        return;
    }
    let key = seen_key(id);
    let existing = store.eph_get(&key).unwrap_or_else(|| json!({}));
    if existing.get("read_at").and_then(Value::as_f64).is_some() {
        return;
    }
    let stamp = now();
    let delivered = existing.get("delivered_at").and_then(Value::as_f64).unwrap_or(stamp);
    // an ack is the agent acting on it: seen, if nothing said so sooner
    let seen = existing.get("seen_at").and_then(Value::as_f64).unwrap_or(stamp);
    let _ = store.eph_set(&key, &json!({
        "to": to, "session": text(&existing, "session"), "delivered_at": delivered, "read_at": stamp, "seen_at": seen,
    }), Some(SEEN_TTL_S));
}

/// One recipient's state as the log shows it: `(status, delivered_at,
/// read_at)`. The stamps are exact; without one, a message pushed live on
/// the session's socket (`live`) was delivered when it was sent, one still
/// in the inbox is pending, and one gone from it was acked (an inbox row
/// is only ever deleted by an ack, or by the push of a session-to-session
/// note, which stamps) — read, its time unknown.
pub fn delivery_status(seen: Option<&Value>, live: bool, sent_ts: f64, in_inbox: bool) -> (&'static str, Value, Value) {
    delivery_status_at(seen, live, sent_ts, in_inbox, now())
}

/// `delivery_status` as of `as_of`: one still in the inbox past
/// `MESSAGE_TTL_S` was never delivered and no longer will be, "expired".
pub fn delivery_status_at(
    seen: Option<&Value>, live: bool, sent_ts: f64, in_inbox: bool, as_of: f64,
) -> (&'static str, Value, Value) {
    let at = |key: &str| seen.and_then(|s| s.get(key)).and_then(Value::as_f64).map(|t| json!(t)).unwrap_or(Value::Null);
    let (delivered, read, past) = (at("delivered_at"), at("read_at"), at("seen_at"));
    if !read.is_null() {
        return ("read", delivered, read);
    }
    // its agent made a call after it from a later message: as good as read
    if !past.is_null() {
        return ("read", delivered, past);
    }
    if !delivered.is_null() {
        return ("delivered", delivered, Value::Null);
    }
    if live {
        return ("delivered", json!(sent_ts), Value::Null);
    }
    if in_inbox {
        if is_expired(sent_ts, as_of) {
            return ("expired", Value::Null, Value::Null);
        }
        return ("pending", Value::Null, Value::Null);
    }
    ("read", Value::Null, Value::Null)
}

/// How far back an edit counts as "working on it" when a message is routed
/// by code instead of by name.
pub const ROUTE_WINDOW_S: f64 = 24.0 * 3600.0;
/// A routed message reaches at most this many people.
pub const MAX_ROUTED: usize = 10;

/// The anchor as agents write it: `path::symbol`, `path`, or `dir/`.
pub fn anchor_label(anchor: &crate::memory::Anchor) -> String {
    match anchor {
        crate::memory::Anchor::Symbol { path, symbol } => format!("{path}::{symbol}"),
        crate::memory::Anchor::File { path } => path.clone(),
        crate::memory::Anchor::Dir { path } => format!("{path}/"),
    }
}

/// Who is working on the anchored code right now: the owners of open
/// intents over it, and whoever edited it in the last day — on the anchor
/// itself and on the files that call it, because a change to a symbol is
/// news to its callers first. Everyone but the sender, sorted, capped.
pub fn route_recipients(store: &Store, scope: &str, sender: &str, anchor: &crate::memory::Anchor) -> Vec<String> {
    route_recipients_from(store, scope, sender, "", anchor)
}

/// How long a read or write of the anchored code counts as being on it.
pub const ROUTE_LIVE_S: f64 = 2.0 * 3600.0;

/// `route_recipients` from one session of the sender: the sender's OTHER
/// sessions on that code count too, each addressed on its own inbox
/// (`user#session`). One person running several agents is the common case
/// (Study 8: sixteen agents, one login), and comparing people alone found
/// "nobody else is working on" code while fifteen of its own agents were.
/// Besides intents and the day's edits, a session that read or wrote the
/// anchored files in the last two hours is on it.
pub fn route_recipients_from(
    store: &Store, scope: &str, sender: &str, sender_session: &str, anchor: &crate::memory::Anchor,
) -> Vec<String> {
    route_recipients_for(store, scope, sender, sender_session, "", anchor)
}

/// `route_recipients_from` for one agent of the sender's session (a helper
/// when `sender_worker` names one). Inside that session, the helpers whose
/// work is on the anchored code count as agents on it, each on its own
/// inbox (`local::agent_inbox`); and a helper's message reaches its lead
/// when the session is on that code. Never the agent that sent it.
pub fn route_recipients_for(
    store: &Store, scope: &str, sender: &str, sender_session: &str, sender_worker: &str, anchor: &crate::memory::Anchor,
) -> Vec<String> {
    let (dir, mut paths): (Option<String>, BTreeSet<String>) = match anchor {
        crate::memory::Anchor::Dir { path } => (Some(format!("{path}/")), BTreeSet::new()),
        other => (None, BTreeSet::from([other.path().to_string()])),
    };
    if let crate::memory::Anchor::Symbol { path, symbol } = anchor {
        for caller in crate::recipes::callers(store, scope, path, symbol) {
            paths.insert(text(&caller, "path"));
        }
    }
    let covers = |path: &str| paths.contains(path) || dir.as_deref().is_some_and(|d| path.starts_with(d));
    let stamp = now();
    let sender_l = sender.trim().to_lowercase();
    let mut people: BTreeSet<String> = BTreeSet::new();
    // the sender's own session is on the code: its lead hears a helper's note
    let mut own_session_on_it = false;
    // (who, session, how recent): a teammate by person, the sender's own
    // other live sessions each by its own inbox
    let mut add = |user: &str, session: &str, ts: f64| {
        if user.trim().is_empty() {
            return;
        }
        if user.trim().to_lowercase() != sender_l {
            people.insert(user.to_string());
            return;
        }
        let live = stamp - ts <= ROUTE_LIVE_S && store.eph_get(&format!("sessended:{scope}:{session}")).is_none();
        if !session.is_empty() && session == sender_session && live {
            own_session_on_it = true;
        }
        // a person writing from the dashboard (no session of their own)
        // reaches every one of their agents on that code
        if !session.is_empty() && session != sender_session && live {
            people.insert(crate::local::session_inbox(user, session));
        }
    };
    for (_, intent) in store.eph_scan(&format!("intent:{scope}:")) {
        if text(&intent, "status") != "active" {
            continue;
        }
        let touched = intent.get("paths").and_then(Value::as_array).into_iter().flatten()
            .filter_map(Value::as_str).any(covers);
        if touched {
            add(&text(&intent, "owner"), &text(&intent, "session"), stamp);
        }
    }
    for row in store.ledger_since(scope, stamp - ROUTE_WINDOW_S) {
        if row.kind == "edit_reported" && covers(&text(&row.payload, "path")) {
            add(&text(&row.payload, "user"), &text(&row.payload, "session"), row.ts);
        }
    }
    // reading it counts: an agent that opened the file is on that code
    for (_, copy) in store.eph_scan(&format!("copyhash:{scope}:")) {
        if covers(&text(&copy, "path")) {
            let ts = copy.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            if stamp - ts <= ROUTE_LIVE_S {
                add(&text(&copy, "user"), &text(&copy, "session"), ts);
            }
        }
    }
    // inside the sender's session: its helpers on that code, and the lead
    // when a helper sends and the session is on it
    let mut within: Vec<String> = Vec::new();
    if !sender_session.is_empty() || crate::local::active() {
        if !sender_worker.is_empty() && own_session_on_it {
            within.push(crate::local::agent_inbox(sender, sender_session, ""));
        }
        for (worker, path) in crate::local::session_workers(store, scope, sender, sender_session) {
            if worker != sender_worker && covers(&path) {
                within.push(crate::local::agent_inbox(sender, sender_session, &worker));
            }
        }
    }
    within.into_iter().chain(people).take(MAX_ROUTED).collect()
}

/// `send_agent_message` with an anchor: the message points at exact code
/// (the anchor's hash and revision are captured now, so delivery can say
/// whether the code has moved since) and, with no `to`, goes to whoever is
/// working on that code. Messaging limits apply per recipient: on a plan
/// that keeps messages to your own agents, a routed message reaches no one
/// and says why.
pub fn send_anchored_message(
    store: &Store, scope: &str, workspace: &str, from_user: &str, to: &str, message: &str, anchor: &str,
    agent: &str, dashboard_url: &str,
) -> Value {
    send_anchored_message_in(store, scope, workspace, from_user, "", to, message, anchor, agent, dashboard_url)
}

/// `send_anchored_message` from one session of the sender (see
/// `send_agent_message_in`).
#[allow(clippy::too_many_arguments)]
pub fn send_anchored_message_in(
    store: &Store, scope: &str, workspace: &str, from_user: &str, session: &str, to: &str, message: &str,
    anchor: &str, agent: &str, dashboard_url: &str,
) -> Value {
    send_anchored_message_from(store, scope, workspace, from_user, session, "", to, message, anchor, agent, dashboard_url)
}

/// `send_anchored_message_in` from one agent of the session: a helper
/// (`worker`) reaches its lead and the session's other helpers on that
/// code, and signs as itself, so an answer reaches it.
#[allow(clippy::too_many_arguments)]
pub fn send_anchored_message_from(
    store: &Store, scope: &str, workspace: &str, from_user: &str, session: &str, worker: &str, to: &str, message: &str,
    anchor: &str, agent: &str, dashboard_url: &str,
) -> Value {
    let message = message.trim();
    if message.is_empty() {
        return json!({"ok": false, "error": "empty recipient or message"});
    }
    let Some(parsed) = crate::memory::parse_anchor(anchor) else {
        return json!({"ok": false, "error": "anchor must be path::symbol, a path, or dir/"});
    };
    let label = anchor_label(&parsed);
    let routed = to.trim().is_empty();
    let recipients: Vec<String> = if routed {
        route_recipients_for(store, scope, from_user, session, worker, &parsed)
    } else {
        vec![to.trim().to_string()]
    };
    if recipients.is_empty() {
        // nobody is on that code: nobody hears it, and the sender is told
        // exactly that. Until v105 it went to every agent at work in the
        // repo instead, and a probe about a file no one was near reached
        // both sessions of a test, as noise.
        let on = match &parsed {
            crate::memory::Anchor::Symbol { path, .. } => path.clone(),
            _ => label.clone(),
        };
        return json!({"ok": true, "sent_to": [], "delivered_to": [], "anchor": label,
                      "note": format!("nobody is working on {on}; not delivered")});
    }
    let mut allowed: Vec<String> = Vec::new();
    let mut refusal = String::new();
    // a helper signs as itself (`local::agent_inbox`), so an answer reaches it
    let signed = if worker.is_empty() { from_user.to_string() } else { crate::local::agent_inbox(from_user, session, worker) };
    for person in &recipients {
        match crate::access::messaging_refusal(store, workspace, from_user, person, dashboard_url) {
            Some(why) => refusal = why,
            None => allowed.push(person.clone()),
        }
    }
    if allowed.is_empty() {
        return json!({"ok": false, "error": refusal});
    }
    let (hash, rev, _) = crate::memory::anchor_state(store, scope, &parsed);
    let ts = now();
    let truncated = truncate_chars(message, 4000);
    let mut ids: Vec<String> = Vec::new();
    for person in &allowed {
        let id = crate::compat::new_id();
        let mut record = json!({
            "id": id, "to": person, "from": signed, "message": truncated, "agent": agent, "ts": ts,
            "anchor": label, "anchor_hash": hash, "anchor_rev": rev, "routed": routed,
        });
        if !session.trim().is_empty() {
            record["sender_session"] = json!(session.trim());
        }
        let _ = store.kv_put("inbox", &format!("{scope}:{person}:{id}"), &record, ts);
        let _ = store.ledger_append(scope, "agent_message", &json!({
            "to": person, "from": signed, "id": id, "chars": truncated.chars().count(),
            "anchor": label, "routed": routed,
        }), ts);
        ids.push(id);
    }
    crate::push::arrived();
    // `ids`: each recipient's own inbox id, so the log can say who has read it
    let by_person: serde_json::Map<String, Value> =
        allowed.iter().cloned().zip(ids.iter().map(|id| json!(id))).collect();
    file_thread(store, scope, &with_sender(store, scope, &json!({
        "id": ids[0], "to": if routed { json!("") } else { json!(allowed[0]) }, "to_list": allowed, "from": signed,
        "message": truncated, "agent": agent, "ts": ts, "anchor": label, "routed": routed, "ids": by_person,
    }), session));
    let mut out = json!({"ok": true, "message_id": ids[0], "delivered_to": allowed, "anchor": label});
    if allowed.len() < recipients.len() {
        out["withheld"] = json!(recipients.len() - allowed.len());
        out["notice"] = json!(refusal);
    }
    out
}

/// Whether a message asks something of its reader (a question, a request,
/// a "don't"), as opposed to news ("done: X now takes currency"). Only
/// these carry a call to action: every agent was told to "act on" every
/// other agent's plan, and did (Study 8, second study).
pub fn asks(message: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let lower = message.to_lowercase();
    lower.contains('?')
        || RE.get_or_init(|| {
            regex::Regex::new(concat!(
                r"\b(?:please|pls|can you|could you|would you|will you|can someone|you need to|you must|you should|",
                r"make sure|don't|do not|stop|wait|hold off|hold on|avoid|leave .{1,30} alone|let me know|need you|",
                r"need your|before you|instead of|switch (?:over|to)|update your|use .{1,30} instead)\b"
            ))
            .expect("asks pattern")
        })
        .is_match(&lower)
}

/// The heading over messages handed to an agent: a call to action only
/// when one of them asks something; news is just news.
pub fn messages_heading(messages: &[Value], from_own_sessions: bool) -> String {
    let who = if from_own_sessions { "your other sessions in this repo" } else { "teammates' agents" };
    // a handoff is work sent to this agent alone: it is to be done, not weighed
    if messages.iter().any(is_handoff) {
        return format!("A handoff for you from {who}: work your user wants done, sent to you alone. Start on it now, \
tell the user in one line that you picked it up, and when it is done tell the sender with the cork message command \
(--to <sender>).");
    }
    if messages.iter().any(|m| asks(&text(m, "message"))) {
        format!("Messages for you from {who} (not your user). One asks something of you: act on it if it fits your \
task, and say so to the user.")
    } else {
        format!("For your information, from {who} (not your user; nothing to do unless it changes your work):")
    }
}

/// One message as a briefing line: who, what it is about and what became
/// of that code since, then the text, and under it who Cork says sent it
/// and what Cork saw that session do (`vouch`).
pub fn message_line(message: &Value) -> String {
    // a handoff's spec is the work itself: never cut
    message_line_within(message, if is_handoff(message) { HANDOFF_CHARS } else { MESSAGE_LINE_CHARS })
}

/// How much of a message one line carries: past it the line says how to
/// read the rest. 600 cut instructions an agent then never saw.
pub const MESSAGE_LINE_CHARS: usize = 2000;
/// A handoff's spec is kept whole up to what a message holds.
pub const HANDOFF_CHARS: usize = 4000;

/// `message_line` with the message text cut at `limit` characters, saying
/// how much was left out and where to read it whole.
pub fn message_line_within(message: &Value, limit: usize) -> String {
    let mut about = String::new();
    let anchor = text(message, "anchor");
    if !anchor.is_empty() {
        let moved = match message.get("stale").and_then(Value::as_bool) {
            Some(true) => "changed since it was sent",
            Some(false) => "unchanged since",
            None => "its state unknown",
        };
        let o = message.get("outcomes").cloned().unwrap_or(json!({}));
        let n = |k: &str| o.get(k).and_then(Value::as_i64).unwrap_or(0);
        let tests = if n("passes") + n("failures") + n("refixes") == 0 {
            String::new()
        } else {
            format!("; since: {} test run(s) passed, {} failed, {} refix(es)", n("passes"), n("failures"), n("refixes"))
        };
        // routed by the code: why this agent hears it
        let yours = if message.get("routed").and_then(Value::as_bool) == Some(true) { "code you work on; " } else { "" };
        about = format!(" about {anchor} ({yours}{moved}{tests})");
    }
    let id = text(message, "id");
    let said = text(message, "message");
    let total = said.chars().count();
    let rest = if total > limit {
        format!(" [{} more characters; read it whole: cork message --show {id}]", total - limit)
    } else {
        String::new()
    };
    let mut line = format!("- [{id}] from {}{about}: {}{rest}", text(message, "from"), truncate_chars(&said, limit));
    for key in ["verified", "evidence"] {
        let note = text(message, key);
        if !note.is_empty() {
            line.push_str(&format!("\n  {note}"));
        }
    }
    line
}

// ------------------------------------------------------- who really sent it

/// The repo a record was filed in: the reader's, or the one `inbox_ack`
/// names as `from_repo` for a message from another repo of the workspace.
fn home_scope(scope: &str, record: &Value) -> String {
    let repo = text(record, "from_repo");
    if repo.is_empty() { scope.to_string() } else { format!("{}:{repo}", split_scope(scope).0) }
}

/// The harness as a person names it.
fn harness_label(harness: &str) -> String {
    match harness {
        "claude" => "Claude Code".to_string(),
        "codex" => "Codex".to_string(),
        "cursor" => "Cursor".to_string(),
        "gemini" => "Gemini CLI".to_string(),
        other => other.to_string(),
    }
}

/// The signer of a message taken apart: the identity whose records name
/// it (`kevin@x.com`, or `agent-3f2a91c0` on this machine's own server),
/// the session and the helper. A helper signs as `user#session/worker`,
/// or `agent-…/worker`; `sender_session` names the session when the
/// signature does not.
fn signer_parts(record: &Value) -> (String, String, String) {
    let from = text(record, "from");
    let (signer, after) = from.split_once('#').map(|(u, r)| (u.to_string(), r.to_string())).unwrap_or((from.clone(), String::new()));
    let (user, own_worker) = signer.split_once('/').map(|(u, w)| (u.to_string(), w.to_string())).unwrap_or((signer.clone(), String::new()));
    let (signed_session, worker) =
        after.split_once('/').map(|(s, w)| (s.to_string(), w.to_string())).unwrap_or((after.clone(), own_worker));
    let stamped = text(record, "sender_session");
    let stamped = stamped.split('/').next().unwrap_or("").to_string();
    (user, if stamped.is_empty() { signed_session } else { stamped }, worker)
}

/// File extensions a bare file name is recognised by (`store.js`); with a
/// folder in front (`docs/notes.adoc`) any extension is a path.
const NAMED_EXTS: &[&str] = &[
    "rs", "py", "pyi", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "java", "kt", "cs", "rb", "c", "h", "cc", "cpp",
    "hpp", "php", "swift", "vue", "svelte", "css", "scss", "html", "sql", "sh", "md", "json", "toml", "yaml", "yml",
    "lock", "txt", "proto",
];

/// The paths a message names, as written (`src/api.rs`, `static/`,
/// `store.js`): words with a folder slash or a known file extension. At
/// most four.
pub fn paths_named(message: &str) -> Vec<String> {
    static WORD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static FILE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let word = WORD.get_or_init(|| regex::Regex::new(r"[A-Za-z0-9_./\-]+").expect("path word pattern"));
    let file = FILE.get_or_init(|| regex::Regex::new(r"^[A-Za-z0-9_\-][A-Za-z0-9_.\-]*\.([A-Za-z][A-Za-z0-9]{0,5})$").expect("file pattern"));
    let mut out: Vec<String> = Vec::new();
    for m in word.find_iter(message) {
        let start = m.start();
        // a URL's host and path, or an email's domain, are not the repo's
        if message[..start].ends_with("://") || message[..start].ends_with('@') {
            continue;
        }
        let w = m.as_str().trim_start_matches("./").trim_end_matches(['.', ',', ':', ';', '-']);
        if w.starts_with('/') {
            continue;
        }
        let last = w.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let is_dir = w.ends_with('/') && w.len() > 1;
        let is_file = file.captures(last).and_then(|c| c.get(1)).is_some_and(|ext| {
            w.contains('/') || NAMED_EXTS.contains(&ext.as_str().to_ascii_lowercase().as_str())
        });
        if (is_dir || is_file) && !out.iter().any(|p| p == w) {
            out.push(w.to_string());
        }
        if out.len() >= 4 {
            break;
        }
    }
    out
}

/// What Cork saw the sending session do on the paths a message names, from
/// its reported edits of the last day: "Cork sees: that session last edited
/// src/api.rs 40m ago; nothing from it in static/ in the last day". ""
/// when the message names no path.
fn evidence(store: &Store, scope: &str, user: &str, session: &str, message: &str) -> String {
    let named = paths_named(message);
    if named.is_empty() || session.is_empty() {
        return String::new();
    }
    let stamp = now();
    let rows = store.ledger_since_kinds(scope, stamp - ROUTE_WINDOW_S, &["edit_reported"]);
    let theirs: Vec<(f64, String)> = rows
        .iter()
        .filter(|r| text(&r.payload, "user") == user && text(&r.payload, "session") == session)
        .map(|r| (r.ts, text(&r.payload, "path")))
        .collect();
    let (mut edited, mut untouched) = (Vec::new(), Vec::new());
    for wanted in &named {
        let covers = |path: &str| {
            if wanted.ends_with('/') {
                path.starts_with(wanted.as_str()) || path.contains(&format!("/{wanted}"))
            } else {
                path == wanted || path.ends_with(&format!("/{wanted}"))
            }
        };
        let last = theirs.iter().filter(|(_, path)| covers(path)).map(|(ts, _)| *ts).fold(None, |a: Option<f64>, t| Some(a.map_or(t, |a| a.max(t))));
        match last {
            Some(ts) => edited.push(format!("{wanted} {}", crate::deltas::age_text(stamp - ts))),
            None => untouched.push(wanted.clone()),
        }
    }
    let mut said = Vec::new();
    if !edited.is_empty() {
        said.push(format!("that session last edited {}", edited.join(", ")));
    }
    if !untouched.is_empty() {
        said.push(format!("nothing from it in {} in the last day", untouched.join(", ")));
    }
    format!("Cork sees: {}", said.join("; "))
}

/// Put on a message, before it is handed to an agent, who sent it as Cork
/// authenticated it (`verified`): the account its credential belongs to,
/// the session its hooks reported (title and Claude Code name), the tool
/// it runs in and whether it is on the reader's machine; and what Cork saw
/// that session do on the paths it names (`evidence`). A message's text
/// can claim anything ("the lead owns src/"); these lines are what the
/// reader checks it against. `reader_user`/`reader_session` are the
/// receiving agent's, whose machine is compared with the sender's ("" when
/// unknown, and then the line does not say).
pub fn vouch(store: &Store, scope: &str, record: &mut Value, reader_user: &str, reader_session: &str) {
    let from = text(record, "from");
    if from.is_empty() || from == "collide" || !record.is_object() {
        return;
    }
    let home = home_scope(scope, record);
    let (user, session, worker) = signer_parts(record);
    let account = if crate::local::active() {
        // on this machine's own server every agent is the one person here
        let profile = store.kv_get("profile", crate::local::LOCAL_UID).unwrap_or_default();
        let email = text(&profile, "email");
        if email.is_empty() { "this machine's user".to_string() } else { email }
    } else {
        user.clone()
    };
    let mut parts = vec![format!("verified by Cork: {account}")];
    if session.is_empty() {
        parts.push("not from an agent session".to_string());
    } else {
        let title = crate::freshness::title_of(store, &home, &user, &session);
        let facts = crate::freshness::session_facts(store, &home, &user, &session);
        let short: String = session.chars().take(8).collect();
        // by its Claude Code name and what it is doing now, else its title
        parts.push(match (title.is_empty(), facts.name.is_empty()) {
            _ if !facts.task.is_empty() => format!("session {}", crate::freshness::work_label("", &facts.name, &facts.task)),
            (false, false) => format!("session \"{title}\" ({})", facts.name),
            (false, true) => format!("session \"{title}\""),
            (true, false) => format!("session {}", facts.name),
            (true, true) => format!("session {short}"),
        });
        if !worker.is_empty() {
            let name = crate::local::helper_name(store, &home, &user, &session, &worker);
            parts.push(format!("its helper {}", if name.is_empty() { worker.clone() } else { name }));
        }
        if !facts.harness.is_empty() {
            parts.push(harness_label(&facts.harness));
        }
        let reader = crate::freshness::session_facts(store, scope, reader_user, reader_session.split('/').next().unwrap_or(""));
        if crate::local::active() || (!facts.machine.is_empty() && facts.machine == reader.machine) {
            parts.push("this machine".to_string());
        } else if !facts.machine.is_empty() && !reader.machine.is_empty() {
            parts.push("another machine".to_string());
        }
    }
    record["verified"] = json!(parts.join(" · "));
    let seen = evidence(store, &home, &user, &session, &text(record, "message"));
    if !seen.is_empty() {
        record["evidence"] = json!(seen);
    }
}

/// `cork message --show <id>`: one message whole, with who sent it, for an
/// agent that was handed it cut. Only its sender and its recipients may
/// read it (on this machine's own server every agent is the one person).
/// Looked up in the repo's thread, then the workspace's other repos.
pub fn show_message(store: &Store, scope: &str, caller: &str, session: &str, id: &str) -> Value {
    let id = id.trim();
    if id.is_empty() {
        return json!({"ok": false, "error": "name the message: cork message --show <id>"});
    }
    let account = |label: &str| label.split(['#', '/']).next().unwrap_or("").trim().to_ascii_lowercase();
    let me = account(caller);
    let workspace = split_scope(scope).0;
    let mut scopes = vec![scope.to_string()];
    scopes.extend(store.list_scopes(&format!("{workspace}:")).into_iter().filter(|s| s != scope));
    for in_scope in scopes {
        let found = store.kv_list(THREAD_BUCKET, &format!("{in_scope}:")).into_iter().map(|(_, row)| row).find(|row| {
            text(row, "id") == id || row.get("ids").and_then(Value::as_object).is_some_and(|ids| ids.values().any(|v| v.as_str() == Some(id)))
        });
        let Some(mut row) = found else { continue };
        let mut readers: Vec<String> = vec![text(&row, "from"), text(&row, "to")];
        readers.extend(row.get("to_list").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string));
        readers.extend(row.get("ids").and_then(Value::as_object).into_iter().flatten().map(|(who, _)| who.clone()));
        if !crate::local::active() && !readers.iter().any(|r| !r.is_empty() && r != "*" && account(r) == me) {
            return json!({"ok": false, "error": "that message was not sent to you or by you"});
        }
        if row.get("retracted").is_some() {
            return json!({"ok": true, "text": format!("Cork: {} took message {id} back; disregard it.", text(&row, "from"))});
        }
        row["id"] = json!(id);
        if in_scope != scope {
            row["from_repo"] = json!(split_scope(&in_scope).1);
        }
        vouch(store, scope, &mut row, caller, session);
        return json!({"ok": true, "text": message_line_within(&row, usize::MAX)});
    }
    json!({"ok": false, "error": "no message by that id in this workspace (a message is kept a while, then only its log line)"})
}

/// What happened to the anchored code since the message was sent: whether
/// it moved (its hash no longer the one captured at send time — `null` when
/// either side is unknown), and the verified test runs and refixes on its
/// file since — the same outcomes an anchored note is scored by.
fn annotate_anchor(store: &Store, scope: &str, record: &mut Value) {
    let Some(parsed) = record.get("anchor").and_then(Value::as_str).and_then(crate::memory::parse_anchor) else {
        return;
    };
    let (current, _, _) = crate::memory::anchor_state(store, scope, &parsed);
    let sent = record.get("anchor_hash").and_then(Value::as_str).map(str::to_string);
    let stale = match (sent, current) {
        (Some(sent), Some(current)) => json!(sent != current),
        _ => Value::Null,
    };
    let since = record.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let outcome = if matches!(parsed, crate::memory::Anchor::Dir { .. }) {
        crate::memory::Outcomes::default()
    } else {
        crate::memory::outcomes(store, scope, parsed.path(), since, now())
    };
    record["stale"] = stale;
    record["outcomes"] = json!({"passes": outcome.passes, "failures": outcome.failures, "refixes": outcome.refixes});
}

/// A message one person addressed to another by name (`to`), as opposed to
/// a broadcast to the repo's agents, a message routed by the code it is
/// about, or a notice Cork itself filed: the only kind that may reach
/// the recipient in a repo other than the one it was sent in.
pub fn is_personal(record: &Value) -> bool {
    record.get("broadcast").and_then(Value::as_bool) != Some(true)
        && record.get("routed").and_then(Value::as_bool) != Some(true)
        && text(record, "from") != "collide"
}

/// Pending directed messages for the calling user; `ack` clears the ids the
/// caller has already acted on. Unacked messages redeliver on every call —
/// there is no other retry signal a wake-polled headless agent could give.
///
/// A message is filed under the SENDER's scope (`send_agent_message`), and
/// the recipient may well be working in another repo of the same workspace —
/// so a read of the recipient's own scope alone made a message to the other
/// repo undeliverable. The read fans across `visible`, the caller's allowlist
/// from `access::visible_scopes` (non-optional: a caller that forgot it does
/// not compile), plus the queried scope; a message filed under a repo the
/// recipient cannot see stays undelivered, because it names paths and work
/// in that repo. An ack matches by id wherever the message was filed, so an
/// ack from the recipient's repo clears a message written under the
/// sender's. The queried repo's messages come first, then the rest of the
/// workspace, oldest first within each; foreign rows carry `from_repo`, the
/// recipe convention. Mirrors `service._inbox`. Only a personal message
/// (`is_personal`) crosses repos.
pub fn inbox_ack(
    store: &Store, scope: &str, user_id: &str, visible: &BTreeSet<String>, ack: &[String],
) -> Value {
    let mut scopes: Vec<&str> = vec![scope];
    scopes.extend(visible.iter().map(String::as_str).filter(|other| *other != scope));

    let mut pending: Vec<Value> = Vec::new();
    for in_scope in scopes {
        for (key, mut record) in store.kv_list("inbox", &format!("{in_scope}:{user_id}:")) {
            // the id is the key's last segment by construction; matching on
            // the key rather than the record survives a record without one
            let id = key.rsplit_once(':').map(|(_, id)| id).unwrap_or("");
            if ack.iter().any(|acked| acked == id) {
                let _ = store.kv_delete("inbox", &key);
                mark_read(store, id, user_id);
                continue;
            }
            // posted to it through Claude Code, and not yet known to have arrived
            if peer_held(store, in_scope, user_id, id, false) {
                continue;
            }
            // past its week it waits for no one (the log says it expired)
            if is_expired(record.get("ts").and_then(Value::as_f64).unwrap_or(0.0), now()) {
                continue;
            }
            if in_scope != scope {
                // only a message addressed to this person crosses repos: a
                // note to everyone at work in that repo, one routed by the
                // code it is about, and Cork's own notices are about that
                // repo's work, and stay there for the agents working in it
                if !is_personal(&record) {
                    continue;
                }
                if let Some(map) = record.as_object_mut() {
                    map.insert("from_repo".into(), json!(split_scope(in_scope).1));
                }
            }
            annotate_anchor(store, in_scope, &mut record);
            pending.push(record);
        }
    }
    pending.sort_by(|a, b| {
        let rank = |record: &Value| {
            (record.get("from_repo").is_some(), record.get("ts").and_then(Value::as_f64).unwrap_or(0.0))
        };
        let (fa, ta) = rank(a);
        let (fb, tb) = rank(b);
        fa.cmp(&fb).then(ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal))
    });
    json!({"ok": true, "messages": pending})
}

// ------------------------------------------------------- reconciliation queue

/// The mandate text handed back with every freshly claimed reconciliation.
/// An agent reads this sentence to decide how to resolve a collision, and a
/// reworded one is different guidance.
const RECONCILE_MANDATE: &str = "ADDITIVE RESOLUTION: build out what BOTH agents were building. \
Never resolve by deleting either side's work — synthesize the union (e.g. keep the new feature \
AND adapt it to the new interface). Verify with tests and differential_check, report the merged \
edit normally, then call pending_reconciliations again with resolve=[this id] and complete any \
covering intent with a rationale describing the synthesis.";

/// The inline reconciler entry point behind `pending_reconciliations`: fetch
/// open collisions, claim the unclaimed ones (15-minute TTL so a stalled
/// claimant releases), and optionally mark ids resolved. Records are created
/// by the lint pipeline on report; this only reads and updates the ones
/// already in the `reconcile:{scope}` ephemeral blob.
///
/// Iteration order: by each record's own `opened` timestamp, the order the
/// reconciliations were (last) opened.
pub fn claim_reconciliations(
    store: &Store, scope: &str, user_id: &str, resolve: &[String], idle_after_s: f64,
) -> Value {
    let key = format!("reconcile:{scope}");
    let mut records: serde_json::Map<String, Value> =
        store.eph_get(&key).and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let stamp = now();
    let mut changed = false;

    for rid in resolve {
        let Some(record) = records.get_mut(rid) else { continue };
        if record.get("status").and_then(Value::as_str) != Some("open") {
            continue;
        }
        let symbol = record.get("symbol").cloned().unwrap_or(Value::Null);
        let rule = record.get("rule").cloned().unwrap_or(Value::Null);
        if let Some(map) = record.as_object_mut() {
            map.insert("status".into(), json!("resolved"));
            map.insert("resolved_by".into(), json!(user_id));
            map.insert("resolved_ts".into(), json!(stamp));
        }
        changed = true;
        let _ = store.ledger_append(scope, "reconciliation_resolved", &json!({
            "id": rid, "symbol": symbol, "user": user_id, "rule": rule,
        }), stamp);
    }

    let mut order: Vec<String> = records.keys().cloned().collect();
    order.sort_by(|a, b| {
        let opened = |rid: &str| records[rid].get("opened").and_then(Value::as_f64).unwrap_or(0.0);
        opened(a).partial_cmp(&opened(b)).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out: Vec<Value> = Vec::new();
    for rid in &order {
        let Some(record) = records.get_mut(rid) else { continue };
        if record.get("status").and_then(Value::as_str) != Some("open") {
            continue;
        }
        let claimant = text(record, "claimed_by");
        let claim_ts = record.get("claim_ts").and_then(Value::as_f64).unwrap_or(0.0);
        let claim_fresh = stamp - claim_ts < 900.0;
        if !claimant.is_empty() && claimant != user_id && claim_fresh {
            out.push(json!({
                "id": record.get("id").cloned().unwrap_or_else(|| json!(rid)),
                "symbol": record.get("symbol").cloned().unwrap_or(Value::Null),
                "rule": record.get("rule").cloned().unwrap_or(Value::Null),
                "status": "claimed", "claimed_by": claimant,
            }));
            continue;
        }
        if let Some(map) = record.as_object_mut() {
            map.insert("claimed_by".into(), json!(user_id));
            map.insert("claim_ts".into(), json!(stamp));
        }
        changed = true;

        let record_path = text(record, "path");
        let record_symbol = text(record, "symbol");
        let mut entry = record.clone();
        if let Some(map) = entry.as_object_mut() {
            map.remove("claim_ts");
            map.insert("status".into(), json!("claimed_by_you"));
        }

        let intents = crate::collisions::active_intents(store, scope, idle_after_s);
        let involved: Vec<Value> = intents
            .iter()
            .filter(|intent| {
                let in_symbols = intent.get("symbols").and_then(Value::as_array)
                    .is_some_and(|a| a.iter().any(|s| s.as_str() == Some(record_symbol.as_str())));
                let in_paths = intent.get("paths").and_then(Value::as_array)
                    .is_some_and(|a| a.iter().any(|p| p.as_str() == Some(record_path.as_str())));
                in_symbols || in_paths
            })
            .take(4)
            .map(|intent| json!({
                "intent_id": intent.get("intent_id").cloned().unwrap_or(Value::Null),
                "owner": intent.get("owner").cloned().unwrap_or(Value::Null),
                "summary": intent.get("summary").cloned().unwrap_or(Value::Null),
                "operations": intent.get("operations").cloned().unwrap_or(Value::Null),
            }))
            .collect();
        let notes = crate::memory::locality_memories(
            store, scope, &[record_path.clone()], &[record_symbol.clone()], 10);

        if let Some(map) = entry.as_object_mut() {
            map.insert("involved_intents".into(), json!(involved));
            if !notes.is_empty() {
                map.insert("anchored_memories".into(), json!(notes));
            }
            map.insert("mandate".into(), json!(RECONCILE_MANDATE));
        }
        out.push(entry);
    }

    let open_count =
        records.values().filter(|r| r.get("status").and_then(Value::as_str) == Some("open")).count() as i64;
    if changed {
        let _ = store.eph_set(&key, &Value::Object(records), Some(6.0 * 3600.0));
    }
    json!({"reconciliations": out, "open": open_count})
}

// --------------------------------------------------------------- explain_code

/// Notes store anchors as `{"kind","path","symbol"}` objects, or (older
/// notes) as a bare `"path::symbol"` string — normalize both to (path,
/// symbol), matching `insights._note_anchor`.
fn note_anchor(note: &Value) -> (String, String) {
    match note.get("anchor") {
        Some(Value::Object(map)) => (
            map.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
            map.get("symbol").and_then(Value::as_str).unwrap_or("").to_string(),
        ),
        Some(Value::String(raw)) => match raw.split_once("::") {
            Some((path, symbol)) => (path.to_string(), symbol.to_string()),
            None => (raw.clone(), String::new()),
        },
        _ => (String::new(), String::new()),
    }
}

fn note_text(note: &Value) -> String {
    let t = text(note, "text");
    if !t.is_empty() { t } else { text(note, "fact") }
}

fn note_matches_anchor(note: &Value, path: &str, symbol: &str) -> bool {
    let (a_path, a_symbol) = note_anchor(note);
    if a_path.is_empty() && a_symbol.is_empty() {
        return false;
    }
    if !symbol.is_empty() && !a_symbol.is_empty() && a_symbol != symbol {
        return false;
    }
    if path.is_empty() {
        return !symbol.is_empty() && a_symbol == symbol;
    }
    if a_path.ends_with('/') {
        return path.starts_with(&a_path);
    }
    a_path == path
}

fn edit_row_touches(payload: &Value, path: &str, symbol: &str) -> bool {
    if !path.is_empty() && text(payload, "path") != path {
        return false;
    }
    if !symbol.is_empty() {
        let in_changed = payload.get("symbols_changed").and_then(Value::as_object)
            .is_some_and(|m| m.contains_key(symbol));
        let in_events = payload.get("events").and_then(Value::as_array)
            .is_some_and(|events| events.iter().any(|e| e.get("symbol").and_then(Value::as_str) == Some(symbol)));
        if !in_changed && !in_events {
            return false;
        }
    }
    true
}

/// `[settled <intent-id>]`, anchored at the START of the text — the prefix
/// `intents::complete` mints for the anchored twin of a completed intent's
/// rationale (see `intents.rs`'s `[settled {id}] {rationale}`).
fn settled_intent_id(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("[settled ")?;
    let end = rest.find(']')?;
    let candidate = &rest[..end];
    let is_hex = !candidate.is_empty()
        && candidate.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    is_hex.then_some(candidate)
}

/// Hard bound on ledger rows walked, matching `insights::MAX_ROWS` /
/// `service._walk_ledger`'s `MAX_ROWS`: on a very busy scope this answers
/// from the most RECENT rows in history rather than growing without bound.
const WHY_MAX_ROWS: usize = 4000;

/// The chain behind one path or symbol, oldest first: edits, the rationales
/// behind them, scars (attempts that were reverted), and anchored notes.
/// `git blame` says who and when; this says what was tried, what was
/// settled, and why.
///
/// `"seq"` (the ledger row's sequence number) is NOT in a chain entry:
/// `store::LedgerRow` deliberately does not carry it (every
/// existing caller only needs ordering, which the row order already answers
/// — see its doc comment), and adding it back just for this one caller was
/// not worth reopening that decision. An agent reading `explain_code` loses
/// the ledger deep-link number; everything else in the chain is unaffected.
/// `since_ts` is the plan's history window: rows before it are not read
/// (0 = all of history).
pub fn why(store: &Store, scope: &str, path: &str, symbol: &str, limit: usize, since_ts: f64) -> Value {
    let mut rows = store.ledger_since(scope, since_ts);
    if rows.len() > WHY_MAX_ROWS {
        rows = rows.split_off(rows.len() - WHY_MAX_ROWS);
    }
    let considered_rows = rows.len();

    let mut intent_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // (ts, entry); a final stable sort by ts (a missing ts sorts as 0)
    let mut chain: Vec<(f64, Value)> = Vec::new();

    for row in &rows {
        if row.kind != "edit_reported" || !edit_row_touches(&row.payload, path, symbol) {
            continue;
        }
        let payload = &row.payload;
        let mut changed_symbols: Vec<String> = payload.get("symbols_changed").and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect()).unwrap_or_default();
        changed_symbols.sort();
        changed_symbols.truncate(8);
        let intent_id = text(payload, "intent_id");
        let mut entry = json!({
            "type": "edit", "ts": row.ts, "seq": row.seq,
            "by": text(payload, "user"), "agent": text(payload, "agent"),
            "path": text(payload, "path"), "symbols": changed_symbols,
            "lines_added": payload.get("lines_added").and_then(Value::as_i64).unwrap_or(0),
            "lines_removed": payload.get("lines_removed").and_then(Value::as_i64).unwrap_or(0),
        });
        if !intent_id.is_empty() {
            entry["intent_id"] = json!(intent_id);
            intent_ids.insert(intent_id);
        }
        chain.push((row.ts, entry));
    }

    let mut rationale_intents: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for row in &rows {
        if row.kind != "intent_completed" {
            continue;
        }
        let payload = &row.payload;
        let this_intent = text(payload, "intent_id");
        let mut related = !this_intent.is_empty() && intent_ids.contains(&this_intent);
        if !related && !symbol.is_empty() {
            related = payload.get("operations").and_then(Value::as_array)
                .is_some_and(|ops| ops.iter().any(|op| op.get("symbol").and_then(Value::as_str) == Some(symbol)));
        }
        let rationale = text(payload, "rationale");
        if related && !rationale.is_empty() {
            if !this_intent.is_empty() {
                rationale_intents.insert(this_intent.clone());
            }
            let entry = json!({
                "type": "rationale", "ts": row.ts, "seq": row.seq,
                "by": text(payload, "owner"), "intent_id": this_intent, "text": rationale,
            });
            chain.push((row.ts, entry));
        }
    }

    // anchored memory: notes, auto-rationales, and scars pinned to this code
    for (_key, note) in store.kv_list("memory", &format!("{scope}:")) {
        if !note_matches_anchor(&note, path, symbol) {
            continue;
        }
        let auto = note.get("auto").and_then(Value::as_str).unwrap_or("");
        if auto == "rationale" {
            // complete_intent mints an anchored twin of its rationale; if the
            // ledger row is already in the chain, the twin is a duplicate
            if let Some(settled) = settled_intent_id(&note_text(&note)) {
                if rationale_intents.contains(settled) {
                    continue;
                }
            }
        }
        let (a_path, a_symbol) = note_anchor(&note);
        let entry_type = match auto {
            "scar" => "scar",
            "rationale" => "rationale",
            _ => "note",
        };
        let created = note.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        let anchor = if !a_symbol.is_empty() { format!("{a_path}::{a_symbol}") } else { a_path };
        let entry = json!({
            "type": entry_type, "ts": created, "by": text(&note, "by"),
            "text": truncate_chars(&note_text(&note), 500), "anchor": anchor,
        });
        chain.push((created, entry));
    }

    chain.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut entries: Vec<Value> = chain.into_iter().map(|(_, entry)| entry).collect();
    if entries.len() > limit {
        entries = entries.split_off(entries.len() - limit);
    }
    json!({"path": path, "symbol": symbol, "chain": entries, "considered_rows": considered_rows})
}

/// `explain_code`'s entry point — `insights.why` with the tool's fixed
/// 40-entry limit.
pub fn explain_code(store: &Store, scope: &str, path: &str, symbol: &str, since_ts: f64) -> Value {
    why(store, scope, path, symbol, 40, since_ts)
}

// ------------------------------------------------------------ graph export

/// Hand the graph to other tools: Graphify's `graph.json` shape, GraphML for
/// Gephi/yEd, or Cypher for Neo4j/FalkorDB. Mirrors `service.graph_export`.
pub fn graph_export(store: &Store, scope: &str, repo_id: &str, format: &str, idle_after_s: f64) -> Value {
    // `fmt = (fmt or "graphify").strip().lower()`: the default only kicks in
    // on a genuinely EMPTY input, before trimming — "  " stays "  ", trims to
    // "", and falls through to the format-not-recognized error below.
    let raw = if format.is_empty() { "graphify" } else { format };
    let normalized = raw.trim().to_lowercase();
    if !matches!(normalized.as_str(), "graphify" | "graphml" | "cypher") {
        return json!({"ok": false, "error": "format must be graphify, graphml, or cypher"});
    }

    let graph = crate::graphview::snapshot(store, scope);
    let overlays = crate::graphview::overlays_for(store, scope, &graph, idle_after_s, 30.0, true, true);
    let comms = crate::graphview::communities_cached(&graph, scope, &overlays.co_change);

    if normalized == "graphify" {
        return json!({
            "ok": true, "format": "graphify",
            "graph": crate::graphview::to_graphify(&graph, comms.as_slice(), &overlays, repo_id),
        });
    }
    let (content, filename) = if normalized == "graphml" {
        (crate::graphview::to_graphml(&graph, comms.as_slice()), "cork-graph.graphml")
    } else {
        (crate::graphview::to_cypher(&graph, comms.as_slice()), "cork-graph.cypher")
    };
    json!({"ok": true, "format": normalized, "content": content, "filename": filename})
}

fn text_field(value: &Value, key: &str) -> String {
    text(value, key)
}

/// `(path, symbol)` from a Graphify node: `source_file` + `label`, or an id
/// of the form `path::symbol` / `path:symbol` / `path#symbol`. Mirrors
/// `graph._graphify_node_parts`.
fn graphify_node_parts(node: &Value) -> (String, String) {
    let mut path = {
        let a = text_field(node, "source_file");
        if !a.is_empty() { a } else {
            let b = text_field(node, "file");
            if !b.is_empty() { b } else { text_field(node, "path") }
        }
    };
    let mut label = {
        let a = text_field(node, "label");
        if !a.is_empty() { a } else { text_field(node, "name") }
    };
    let nid = text_field(node, "id");
    if path.is_empty() {
        for sep in ["::", "#", ":"] {
            if let Some(pos) = nid.find(sep) {
                let (before, after) = (nid[..pos].to_string(), nid[pos + sep.len()..].to_string());
                path = before;
                if label.is_empty() {
                    label = after;
                }
                break;
            }
        }
    }
    if label.is_empty() && !nid.is_empty() && nid != path {
        label = nid;
    }
    if label == path {
        label = String::new();
    }
    (path, label)
}

const GRAPHIFY_KIND_MAP: &[(&str, &str)] = &[
    ("calls", "calls"), ("call", "calls"),
    ("imports", "references"), ("import", "references"),
    ("inherits", "inherits"), ("extends", "inherits"), ("implements", "inherits"), ("mixes_in", "inherits"),
    ("uses", "uses_type"), ("uses_type", "uses_type"),
    ("references", "references"), ("depends_on", "references"),
];

fn map_graphify_kind(relation: &str) -> &'static str {
    GRAPHIFY_KIND_MAP.iter().find(|(k, _)| *k == relation).map(|(_, v)| *v).unwrap_or("references")
}

/// Bootstrap the graph from a Graphify `graph.json`: files Cork has not
/// parsed itself (a language it has no grammar for yet, or code nobody has
/// edited) get nodes and edges straight from the import. A file Cork
/// already parses is left alone — the live parse is the truth for it.
/// Mirrors `graph.import_graphify`.
pub fn graph_import(store: &Store, scope: &str, data: &Value, ts: f64) -> Value {
    let Some(nodes) = data.get("nodes").and_then(Value::as_array).filter(|a| !a.is_empty()) else {
        return json!({"ok": false, "error": "expected a graph.json object with a 'nodes' array"});
    };
    let edges_in: Vec<Value> = data.get("edges").and_then(Value::as_array).cloned()
        .or_else(|| data.get("links").and_then(Value::as_array).cloned())
        .unwrap_or_default();

    // existing per-file records, keyed by path (same shape `codegraph`'s
    // internal `records()` builds), so a file Cork already parses itself
    // is left untouched
    let existing: std::collections::BTreeMap<String, Value> = store
        .kv_list(crate::codegraph::GRAPH_BUCKET, &format!("{scope}:"))
        .into_iter()
        .filter_map(|(_key, record)| {
            let path = record.get("path").and_then(Value::as_str)?.to_string();
            Some((path, record))
        })
        .collect();

    let mut by_id: std::collections::HashMap<String, (String, String)> = std::collections::HashMap::new();
    let mut per_file: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    let mut skipped_parsed: i64 = 0;

    for node in nodes {
        let (path, label) = graphify_node_parts(node);
        if path.is_empty() {
            continue;
        }
        let id_field = text_field(node, "id");
        let key = if !id_field.is_empty() { id_field } else { crate::graphview::node_id(&path, &label) };
        by_id.insert(key, (path.clone(), label.clone()));

        if let Some(existing_rec) = existing.get(&path) {
            if !crate::compat::truthy(existing_rec.get("imported")) {
                skipped_parsed += 1;
                continue;
            }
        }
        let rec = per_file.entry(path.clone()).or_insert_with(|| {
            let language = { let l = text_field(node, "language"); if l.is_empty() { "graphify".to_string() } else { l } };
            json!({"path": path, "language": language, "symbols": {}, "imports": [], "edges": [],
                   "imported": true, "ts": ts})
        });
        if !label.is_empty() {
            let kind = { let t = text_field(node, "type"); if t.is_empty() { "definition".to_string() } else { t } };
            if let Some(symbols) = rec.get_mut("symbols").and_then(Value::as_object_mut) {
                symbols.insert(label.clone(), json!({"kind": kind, "edges": []}));
            }
        }
    }

    let mut edge_count: i64 = 0;
    for edge in &edges_in {
        let src_id = { let a = text_field(edge, "source"); if !a.is_empty() { a } else { text_field(edge, "from") } };
        let dst_id = { let a = text_field(edge, "target"); if !a.is_empty() { a } else { text_field(edge, "to") } };
        let (Some((src_path, src_sym)), Some((dst_path, dst_sym))) =
            (by_id.get(&src_id), by_id.get(&dst_id))
        else {
            continue;
        };
        let from_id = crate::graphview::node_id(src_path, src_sym);
        let to_id = crate::graphview::node_id(dst_path, dst_sym);
        let relation = {
            let r = text_field(edge, "relation");
            let r = if !r.is_empty() { r } else { text_field(edge, "type") };
            let r = if !r.is_empty() { r } else { text_field(edge, "kind") };
            if r.is_empty() { "references".to_string() } else { r }
        }.to_lowercase();
        let confidence = { let c = text_field(edge, "confidence"); if c.is_empty() { "inferred".to_string() } else { c } }
            .to_lowercase();
        let kind = map_graphify_kind(&relation);
        let Some(rec) = per_file.get_mut(src_path) else { continue };
        if let Some(edges_arr) = rec.get_mut("edges").and_then(Value::as_array_mut) {
            edges_arr.push(json!({"from": from_id, "to": to_id, "kind": kind, "confidence": confidence}));
        }
        edge_count += 1;
    }

    for (path, rec) in &per_file {
        let _ = store.kv_put(
            crate::codegraph::GRAPH_BUCKET, &format!("{scope}:{}", crate::codegraph::path_key(path)), rec, ts);
        if let Some(edges_arr) = rec.get("edges").and_then(Value::as_array) {
            for edge in edges_arr {
                let to = text(edge, "to");
                let from = text(edge, "from");
                let kind = text(edge, "kind");
                let rev_key = format!("{scope}:{to}");
                let mut doc = store.kv_get(crate::codegraph::REV_BUCKET, &rev_key)
                    .unwrap_or_else(|| json!({"dependents": {}}));
                if let Some(map) = doc.as_object_mut() {
                    if let Some(dependents) = map.get_mut("dependents").and_then(Value::as_object_mut) {
                        dependents.insert(from, json!(kind));
                    }
                    map.insert("ts".into(), json!(ts));
                }
                let _ = store.kv_put(crate::codegraph::REV_BUCKET, &rev_key, &doc, ts);
            }
        }
    }
    crate::graphview::invalidate(store, scope);

    json!({
        "ok": true, "files_imported": per_file.len() as i64, "edges_imported": edge_count,
        "files_skipped_already_parsed": skipped_parsed,
    })
}

// --------------------------------------------------------------- dashboard

const DASH_ROUTES: &[(&str, &str)] = &[
    ("overview", "/dashboard"), ("dashboard", "/dashboard"), ("home", "/dashboard"), ("settings", "/dashboard"),
    ("activity", "/dashboard/activity"), ("feed", "/dashboard/activity"),
    ("change", "/dashboard/change"), ("edit", "/dashboard/change"),
    ("why", "/dashboard/why"), ("history", "/dashboard/why"),
    ("worklog", "/dashboard/worklog"),
    ("journal", "/dashboard/journal"), ("sessions", "/dashboard/journal"),
    ("billing", "/dashboard/billing"), ("invoices", "/dashboard/billing"), ("checkout", "/dashboard/billing"),
    ("pricing", "/pricing"), ("plans", "/pricing"), ("seats", "/pricing"),
    ("upgrade", "/pricing"), ("downgrade", "/pricing"),
];

fn dash_route(section: &str) -> &'static str {
    DASH_ROUTES.iter().find(|(k, _)| *k == section).map(|(_, v)| *v).unwrap_or("/dashboard")
}

/// A human-clickable deep link into one dashboard view. Relative (no origin)
/// when no dashboard URL is configured — the dev-server case. Mirrors
/// `mcp_server._dashboard_link`.
fn dashboard_link(
    dashboard_url: &str, section: &str, path: &str, seq: i64, seats: i64, interval: &str, plan: &str,
) -> String {
    let origin = dashboard_url.trim_end_matches('/');
    let normalized = {
        let s = if section.is_empty() { "overview" } else { section };
        let s = s.trim().to_lowercase();
        if s.is_empty() { "overview".to_string() } else { s }
    };
    let route = dash_route(&normalized);
    let mut params: Vec<(&str, String)> = Vec::new();
    if route.ends_with("/change") && seq != 0 {
        params.push(("seq", seq.to_string()));
    }
    if route.ends_with("/why") && !path.is_empty() {
        params.push(("path", path.to_string()));
    }
    if route == "/pricing" || route == "/dashboard/billing" {
        if !plan.is_empty() {
            params.push(("plan", plan.to_string()));
        }
        if seats != 0 {
            params.push(("seats", seats.to_string()));
        }
        if !interval.is_empty() {
            params.push(("interval", interval.to_string()));
        }
    }
    let query = if params.is_empty() {
        String::new()
    } else {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &params {
            ser.append_pair(k, v);
        }
        format!("?{}", ser.finish())
    };
    format!("{origin}{route}{query}")
}

/// A human-clickable deep link into the web dashboard. Call this whenever
/// the person wants to SEE something rather than just hear it, and put the
/// returned `url` at the end of the reply. Pure link formatting: the
/// caller is responsible for the membership gate before reaching this.
pub fn open_dashboard(
    dashboard_url: &str, section: &str, path: &str, seq: i64, seats: i64, interval: &str, plan: &str,
) -> Value {
    let sec = {
        let s = if section.is_empty() { "overview" } else { section };
        let s = s.trim().to_lowercase();
        if s.is_empty() { "overview".to_string() } else { s }
    };
    let url = dashboard_link(dashboard_url, &sec, path, seq, seats, interval, plan);
    let label = match sec.as_str() {
        "activity" => "Live activity in Cork".to_string(),
        "change" => if seq != 0 { format!("Change #{seq} in Cork") } else { "A change in Cork".to_string() },
        "why" => if !path.is_empty() { format!("Why {path} is like this") } else { "Code history in Cork".to_string() },
        "worklog" => "The Cork worklog".to_string(),
        "journal" => "Agent sessions & cost".to_string(),
        "billing" => "Plan, seats & invoices".to_string(),
        "pricing" => "Plans & pricing".to_string(),
        "seats" => "Add or change seats".to_string(),
        "upgrade" => "Upgrade your plan".to_string(),
        "downgrade" => "Change your plan".to_string(),
        _ => "The Cork dashboard".to_string(),
    };
    json!({
        "section": sec,
        "url": url,
        "label": label,
        "quick_links": {
            "overview": dashboard_link(dashboard_url, "overview", "", 0, 0, "", ""),
            "activity": dashboard_link(dashboard_url, "activity", "", 0, 0, "", ""),
            "billing": dashboard_link(dashboard_url, "billing", "", 0, 0, "", ""),
            "pricing": dashboard_link(dashboard_url, "pricing", "", 0, 0, "", ""),
        },
        "hint": "Put this url at the end of your reply — it opens the exact view in the human's browser.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    /// An agent at work: its presence marker, as a hook's call leaves it.
    fn live(db: &Store, scope: &str, user: &str, session: &str, path: &str) {
        let _ = db.eph_set(
            &crate::presence::focus_key(scope, user, session),
            &json!({"user": user, "session": session, "repo_id": split_scope(scope).1, "path": path, "action": "reading", "ts": now()}),
            Some(600.0),
        );
    }

    fn inbox_of(db: &Store, scope: &str, who: &str) -> Vec<Value> {
        db.kv_list("inbox", &format!("{scope}:{who}:")).into_iter().map(|(_, v)| v).collect()
    }

    #[test]
    fn a_message_taken_back_unread_is_withdrawn_and_one_already_read_is_disregarded() {
        let db = store();
        let scope = "ws:shop";
        let sent = send_agent_message_in(&db, scope, "kevin@x.com", "s1", "bob@x.com", "delete the Acme clone", "claude", false);
        let id = sent["message_id"].as_str().unwrap().to_string();
        // nobody else may take it back
        let refused = retract_message(&db, scope, "mallory@x.com", "", &id, "claude");
        assert_eq!(refused["ok"], json!(false), "{refused}");
        // unread: the copy is withdrawn, no note follows it
        let back = retract_message(&db, scope, "kevin@x.com", "s1", &id, "claude");
        assert_eq!((back["withdrawn"].clone(), back["told"].clone()), (json!(["bob@x.com"]), json!([])), "{back}");
        assert!(inbox_of(&db, scope, "bob@x.com").is_empty());
        let thread = db.kv_list(THREAD_BUCKET, &format!("{scope}:"));
        assert_eq!(thread.len(), 1);
        assert_eq!((thread[0].1["message"].clone(), thread[0].1["retracted"].is_number()), (json!(""), true), "the words go");
        // twice is once
        assert_eq!(retract_message(&db, scope, "kevin@x.com", "s1", &id, "claude")["already"], json!(true));

        // read already: the recipient is told to disregard it
        send_agent_message_in(&db, scope, "kevin@x.com", "s1", "bob@x.com", "and the domains", "claude", false);
        for (key, _) in db.kv_list("inbox", &format!("{scope}:bob@x.com:")) {
            let _ = db.kv_delete("inbox", &key); // what an ack does
        }
        let back = retract_message(&db, scope, "kevin@x.com#s1", "s1", "last", "claude");
        assert_eq!(back["told"], json!(["bob@x.com"]), "{back}");
        let notes = inbox_of(&db, scope, "bob@x.com");
        assert_eq!(notes.len(), 1);
        assert!(notes[0]["message"].as_str().unwrap().contains("disregard it"), "{}", notes[0]);
        assert!(notes[0]["message"].as_str().unwrap().contains("and the domains"));
    }

    #[test]
    fn last_is_this_sessions_newest_and_asking_twice_never_reaches_an_older_one() {
        let db = store();
        let scope = "ws:shop";
        // an older message, sent from a shell that named no session, already read
        let ts = now();
        file_thread(&db, scope, &json!({"id": "old-1", "to": "bob@x.com", "from": "kevin@x.com", "message": "purge restores first", "ts": ts - 60.0}));
        // this session's newest, unread
        send_agent_message_in(&db, scope, "kevin@x.com", "s2", "ana@x.com", "scrap that", "claude", false);
        let back = retract_message(&db, scope, "kevin@x.com", "s2", "last", "claude");
        assert_eq!(back["withdrawn"], json!(["ana@x.com"]), "{back}");
        // again: the same one, already taken back; the older message stands
        let again = retract_message(&db, scope, "kevin@x.com", "s2", "last", "claude");
        assert_eq!((again["already"].clone(), again["message_id"].clone()), (json!(true), back["message_id"].clone()), "{again}");
        assert!(inbox_of(&db, scope, "bob@x.com").is_empty(), "no note about the older message");
        let old = db.kv_list(THREAD_BUCKET, &format!("{scope}:")).into_iter().find(|(_, row)| row["id"] == json!("old-1")).unwrap().1;
        assert!(old.get("retracted").is_none(), "{old}");
        // another session's "last" is not this one's, nor the shell's message
        let other = retract_message(&db, scope, "kevin@x.com", "s3", "last", "claude");
        assert_eq!(other["ok"], json!(false), "{other}");
        // by its id, the shell's message can still be taken back
        assert_eq!(retract_message(&db, scope, "kevin@x.com", "s3", "old-1", "claude")["ok"], json!(true));
    }

    #[test]
    fn a_message_with_many_copies_is_taken_back_from_every_inbox_it_reached() {
        // a broadcast or an anchored message: one thread row, a copy per
        // recipient, each under its own inbox id (`ids`)
        let db = store();
        let scope = "ws:shop";
        let ts = now();
        for (who, inbox_id) in [("ana@x.com", "i-ana"), ("ben@x.com", "i-ben")] {
            let _ = db.kv_put("inbox", &format!("{scope}:{who}:{inbox_id}"),
                &json!({"id": inbox_id, "to": who, "from": "kevin@x.com", "message": "pausing deploys", "ts": ts}), ts);
        }
        file_thread(&db, scope, &json!({
            "id": "t-1", "to": "*", "to_list": ["ana@x.com", "ben@x.com"], "from": "kevin@x.com",
            "message": "pausing deploys", "ts": ts, "broadcast": true, "ids": {"ana@x.com": "i-ana", "ben@x.com": "i-ben"},
        }));
        // ben's agent already read it
        let _ = db.kv_delete("inbox", &format!("{scope}:ben@x.com:i-ben"));
        let back = retract_message(&db, scope, "kevin@x.com", "", "t-1", "claude");
        assert_eq!((back["withdrawn"].clone(), back["told"].clone()), (json!(["ana@x.com"]), json!(["ben@x.com"])), "{back}");
        assert!(inbox_of(&db, scope, "ana@x.com").is_empty(), "the unread copy is gone");
        let told = inbox_of(&db, scope, "ben@x.com");
        assert_eq!(told.len(), 1);
        assert_eq!(told[0]["retraction_of"], json!("t-1"));
    }

    /// Study 8: one login, sixteen agents. "Tell the others" through an
    /// anchored message found "nobody else is working on" the code, because
    /// it compared people, and every agent was the same person. The sender's
    /// own other sessions on that code are recipients, each on its own inbox.
    #[test]
    fn an_anchored_message_reaches_the_senders_own_sessions_on_that_code() {
        let db = store();
        let scope = "ws:shop";
        let p = "shop/total.py";
        crate::freshness::saw(&db, scope, "kevin@x.com", "s2", p, "h1", false); // read it
        crate::freshness::saw(&db, scope, "kevin@x.com", "s3", "shop/other.py", "h2", false);
        crate::freshness::saw(&db, scope, "kevin@x.com", "s4", p, "h3", true); // wrote it, then ended
        crate::freshness::note_ended(&db, scope, "s4");
        let anchor = crate::memory::parse_anchor(&format!("{p}::amount_due")).unwrap();
        let to = route_recipients_from(&db, scope, "kevin@x.com", "s1", &anchor);
        assert_eq!(to, vec!["kevin@x.com#s2".to_string()], "not s1 (the sender), s3 (elsewhere), s4 (ended)");
        let sent = send_anchored_message_in(&db, scope, "ws", "kevin@x.com", "s1", "", "renamed it; switch over", &format!("{p}::amount_due"), "claude", "");
        assert_eq!(sent["ok"], json!(true), "{sent}");
        assert_eq!(inbox_of(&db, scope, "kevin@x.com#s2").len(), 1);
    }

    /// A handoff is work for ONE agent: the sender's other session whose
    /// listener is waiting (it starts on it by itself, however long it has
    /// been idle) before one merely at work, never a teammate's, never the
    /// sender's own; and it reads as the recipient's to do, not as news.
    #[test]
    fn a_handoff_goes_to_one_of_the_senders_sessions_listening_ones_first() {
        let db = store();
        let scope = "ws:shop";
        live(&db, scope, "kevin@x.com", "s1", "a.py");
        live(&db, scope, "kevin@x.com", "s2", "b.py");
        live(&db, scope, "ana@x.com", "a1", "c.py");
        crate::local::note_listening(&db, scope, "kevin@x.com", "s3"); // idle, its listener waiting
        let sent = crate::local::handoff(&db, scope, "ws", "kevin@x.com", "", "s1", "Build the invite panel: ...", "claude", 600.0);
        assert_eq!(sent["sent_to"], json!(["kevin@x.com#s3"]), "{sent}");
        assert_eq!(inbox_of(&db, scope, "kevin@x.com#s3").len(), 1);
        assert!(inbox_of(&db, scope, "kevin@x.com#s2").is_empty() && inbox_of(&db, scope, "ana@x.com").is_empty());
        // nobody listening: the other session at work takes it
        let db = store();
        live(&db, scope, "kevin@x.com", "s1", "a.py");
        live(&db, scope, "kevin@x.com", "s2", "b.py");
        live(&db, scope, "ana@x.com", "a1", "c.py");
        let sent = crate::local::handoff(&db, scope, "ws", "kevin@x.com", "", "s1", "Build the invite panel", "claude", 600.0);
        assert_eq!(sent["sent_to"], json!(["kevin@x.com#s2"]), "{sent}");
        let mut answer = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s2", &mut answer);
        let note = text(&answer, "inbox_note");
        assert!(note.starts_with("A handoff for you") && note.contains("Start on it now") && note.contains("invite panel"), "{note}");
    }

    /// With no other session of the sender open, a handoff waits: the next
    /// session they start in that repo takes it, once; not a teammate's, and
    /// not the session that sent it.
    #[test]
    fn a_handoff_with_no_session_open_waits_for_the_next_one() {
        let db = store();
        let scope = "ws:shop";
        live(&db, scope, "ana@x.com", "a1", "c.py");
        let spec = format!("Ship the yellow banner. {}", "Context. ".repeat(200));
        let sent = crate::local::handoff(&db, scope, "ws", "kevin@x.com", "", "s1", &spec, "claude", 600.0);
        assert_eq!((sent["queued"].clone(), sent["sent_to"].clone()), (json!(true), json!([])), "{sent}");
        assert!(text(&sent, "note").contains("next session you start here"), "{sent}");
        assert!(crate::local::take_handoffs(&db, scope, "ana@x.com", "a1").is_empty(), "a teammate's");
        assert!(crate::local::take_handoffs(&db, scope, "kevin@x.com", "s1").is_empty(), "the sender's own");
        assert!(crate::local::take_handoffs(&db, "ws:blog", "kevin@x.com", "s9").is_empty(), "another repo's");
        let taken = crate::local::take_handoffs(&db, scope, "kevin@x.com", "s5");
        assert_eq!(taken.len(), 1);
        assert_eq!(text(&taken[0], "message"), spec.trim(), "whole, never cut");
        assert!(is_handoff(&taken[0]));
        assert!(crate::local::take_handoffs(&db, scope, "kevin@x.com", "s6").is_empty(), "taken once");
    }

    /// A helper at work inside a session, on one file.
    fn helper(db: &Store, scope: &str, user: &str, session: &str, worker: &str, path: &str) {
        crate::presence::note_worker_activity(db, scope, user, session, worker, "general-purpose", "", "editing", path, false);
    }

    /// The report from the field (v96): a lead and its helper, a subagent in
    /// the same session, could not reach each other, either way. Every try
    /// answered "no other agent is at work in this repo right now", because
    /// the helper shares its lead's session and every route skipped the
    /// sender's own session. Each agent of the session has its own inbox now.
    #[test]
    fn a_lead_reaches_its_helper_on_the_code_it_names() {
        let db = store();
        let scope = "ws:lattice";
        live(&db, scope, "kevin@x.com", "s1", "src/main.rs");
        helper(&db, scope, "kevin@x.com", "s1", "w1", "static/css/app.css");
        helper(&db, scope, "kevin@x.com", "s1", "w2", "src/api.rs");
        let sent = send_anchored_message_from(&db, scope, "ws", "kevin@x.com", "s1", "", "", "keep .chip-project", "static/css/app.css", "claude", "");
        assert_eq!(sent["delivered_to"], json!(["kevin@x.com#s1/w1"]), "only the helper on that file: {sent}");
        // it reaches that helper on its next step, and only that helper
        let mut theirs = json!({});
        crate::local::attach_inbox_for(&db, scope, "kevin@x.com", "s1", "w1", &mut theirs);
        let note = text(&theirs, "inbox_note");
        assert!(note.starts_with("From the other agents of your own session") && note.contains("keep .chip-project"), "{note}");
        let mut other = json!({});
        crate::local::attach_inbox_for(&db, scope, "kevin@x.com", "s1", "w2", &mut other);
        assert!(text(&other, "inbox_note").is_empty(), "{other}");
        let mut lead = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s1", &mut lead);
        assert!(text(&lead, "inbox_note").is_empty(), "never back to the lead: {lead}");
    }

    #[test]
    fn a_helper_reaches_its_lead_and_the_other_helpers_never_itself() {
        let db = store();
        let scope = "ws:lattice";
        live(&db, scope, "kevin@x.com", "s1", "src/main.rs");
        helper(&db, scope, "kevin@x.com", "s1", "w1", "static/css/app.css");
        helper(&db, scope, "kevin@x.com", "s1", "w2", "src/api.rs");
        let sent = crate::local::broadcast_from(&db, scope, "ws", "kevin@x.com", "s1", "w1", "five classes had no styles", "claude", 600.0);
        let to: BTreeSet<&str> = sent["sent_to"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert_eq!(to, BTreeSet::from(["kevin@x.com#s1/lead", "kevin@x.com#s1/w2"]), "{sent}");
        // signed by the helper, so the lead's answer reaches it
        let mut lead = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s1", &mut lead);
        let note = text(&lead, "inbox_note");
        assert!(note.contains("from kevin@x.com#s1/w1") && note.contains("five classes"), "{note}");
        let mut itself = json!({});
        crate::local::attach_inbox_for(&db, scope, "kevin@x.com", "s1", "w1", &mut itself);
        assert!(text(&itself, "inbox_note").is_empty(), "{itself}");
        // the answer, --to the helper, waits for that helper alone
        let answer = crate::local::send(&db, scope, "lattice", "ws", "kevin@x.com",
            &json!({"session": "s1", "to": "kevin@x.com#s1/w1", "text": "thanks, commit it"}), 600.0);
        assert_eq!(answer["ok"], json!(true), "{answer}");
        let mut helper_answer = json!({});
        crate::local::attach_inbox_for(&db, scope, "kevin@x.com", "s1", "w1", &mut helper_answer);
        assert!(text(&helper_answer, "inbox_note").contains("thanks, commit it"), "{helper_answer}");
        // a helper's --about the session's own file: its lead hears it
        crate::freshness::saw(&db, scope, "kevin@x.com", "s1", "src/main.rs", "h1", true);
        let about = send_anchored_message_from(&db, scope, "ws", "kevin@x.com", "s1", "w2", "", "404 for missing files", "src/main.rs", "claude", "");
        assert_eq!(about["delivered_to"], json!(["kevin@x.com#s1/lead"]), "{about}");
    }

    #[test]
    fn a_lead_alone_is_told_who_is_there_and_its_listener_takes_a_helpers_note() {
        let db = store();
        let scope = "ws:lattice";
        live(&db, scope, "kevin@x.com", "s1", "src/main.rs");
        let none = crate::local::broadcast_from(&db, scope, "ws", "kevin@x.com", "s1", "", "anyone on the CSS?", "claude", 600.0);
        assert!(text(&none, "note").contains("this session has no helper running"), "{none}");
        // a helper's note to its lead: the session's listener takes it now
        helper(&db, scope, "kevin@x.com", "s1", "w1", "static/css/app.css");
        crate::local::broadcast_from(&db, scope, "ws", "kevin@x.com", "s1", "w1", "done with the CSS", "claude", 600.0);
        let pushed = crate::push::take_for_session(&db, scope, "kevin@x.com", "s1");
        assert_eq!(pushed.len(), 1, "{pushed:?}");
        assert_eq!(pushed[0]["within_session"], json!(true));
        assert!(crate::push::take_for_session(&db, scope, "kevin@x.com", "s1").is_empty(), "taken once");
    }

    /// The field reports (v89 to v97): `--to lattice-qa` (a helper's name)
    /// and `--to cork-linear-test-37` (a session's title) said "sent" and
    /// filed the message under that string, where nobody ever read it. A
    /// name now resolves to the agent it means here, or nothing is sent.
    fn titled(db: &Store, scope: &str, user: &str, session: &str, title: &str) {
        live(db, scope, user, session, "src/db.rs");
        crate::freshness::note_title(db, scope, user, session, title);
    }

    fn send_to(db: &Store, scope: &str, session: &str, worker: &str, to: &str, text: &str) -> Value {
        crate::local::send(db, scope, "lattice", "ws", "kevin@x.com",
            &json!({"session": session, "worker": worker, "to": to, "text": text}), 600.0)
    }

    #[test]
    fn a_session_is_reached_by_its_title_or_its_id_and_its_listener_takes_it() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "8d0c1a2b-aaaa", "cork-linear-test-8d");
        titled(&db, scope, "kevin@x.com", "fe74b93b-bbbb", "cork-linear-test-37");
        let sent = send_to(&db, scope, "8d0c1a2b-aaaa", "", "Cork-Linear-Test-37", "seed the db first");
        assert_eq!(sent["delivered_to"], json!(["kevin@x.com#fe74b93b-bbbb"]), "{sent}");
        // it arrives as `--about` does: that session's listener takes it now
        let pushed = crate::push::take_for_session(&db, scope, "kevin@x.com", "fe74b93b-bbbb");
        assert_eq!(pushed.len(), 1, "{pushed:?}");
        assert!(text(&pushed[0], "message").contains("seed the db"));
        // the other way, by the 8-character prefix of its id
        let back = send_to(&db, scope, "fe74b93b-bbbb", "", "8d0c1a2b", "done");
        assert_eq!(back["delivered_to"], json!(["kevin@x.com#8d0c1a2b-aaaa"]), "{back}");
        // and by the exact key a receipt shows
        let exact = send_to(&db, scope, "fe74b93b-bbbb", "", "kevin@x.com#8d0c1a2b-aaaa", "again");
        assert_eq!(exact["ok"], json!(true), "{exact}");
    }

    #[test]
    fn a_helper_is_reached_by_its_name_and_a_helper_reaches_its_lead_by_lead() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "s1", "lattice-build");
        crate::presence::note_worker_activity(&db, scope, "kevin@x.com", "s1", "w1", "general-purpose", "lattice-qa", "reading", "static/js/issue.js", false);
        let sent = send_to(&db, scope, "s1", "", "lattice-qa", "check issue.js::mountIssue");
        assert_eq!(sent["delivered_to"], json!(["kevin@x.com#s1/w1"]), "{sent}");
        let mut helper = json!({});
        crate::local::attach_inbox_for(&db, scope, "kevin@x.com", "s1", "w1", &mut helper);
        assert!(text(&helper, "inbox_note").contains("check issue.js::mountIssue"), "{helper}");
        // by its type too, while it is the only one of that type
        assert_eq!(send_to(&db, scope, "s1", "", "general-purpose", "x")["delivered_to"], json!(["kevin@x.com#s1/w1"]));
        // the helper answers its lead by "lead", or by its session's title
        let up = send_to(&db, scope, "s1", "w1", "lead", "mountIssue renders");
        assert_eq!(up["delivered_to"], json!(["kevin@x.com#s1/lead"]), "{up}");
        let titled_up = send_to(&db, scope, "s1", "w1", "lattice-build", "and the CSS");
        assert_eq!(titled_up["delivered_to"], json!(["kevin@x.com#s1/lead"]), "{titled_up}");
        // the lead has no lead
        let none = send_to(&db, scope, "s1", "", "lead", "x");
        assert_eq!(none["ok"], json!(false), "{none}");
    }

    /// The field report (v99): a lead started an in-process teammate,
    /// `lattice-views`, and `cork message --to lattice-views` answered "no
    /// agent called lattice-views is at work in this repo". The helper had
    /// been thinking for four minutes since its last tool call, and its
    /// three-minute marker had run out. A helper is listed from its first
    /// hook call until it ends, by the name its lead gave it.
    #[test]
    fn a_teammate_thinking_between_steps_is_reached_by_its_name_until_it_ends() {
        let db = store();
        let scope = "ws:cork-linear-test";
        let (lead, helper_id) = ("e7c3a2d4", "alattice-views-59e1693d4e58b264");
        titled(&db, scope, "kevin@x.com", lead, "cork-linear-test-5a");
        titled(&db, scope, "kevin@x.com", "f109957f", "Linear backend work");
        // its first hook call (a Bash read), named as its meta names it
        crate::presence::note_worker_activity(&db, scope, "kevin@x.com", lead, helper_id, "lattice-views", "lattice-views", "running", "cat API.md", false);
        // minutes of thinking: its marker runs out
        db.eph_delete(&crate::presence::worker_key(scope, "kevin@x.com", lead, helper_id));
        let sent = send_to(&db, scope, lead, "", "lattice-views", "Codeword FIG-98");
        assert_eq!(sent["delivered_to"], json!([format!("kevin@x.com#{lead}/{helper_id}")]), "{sent}");
        // into its own context at its next hook call, and only its own
        let mut lead_answer = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", lead, &mut lead_answer);
        assert!(text(&lead_answer, "inbox_note").is_empty(), "{lead_answer}");
        let status = |db: &Store| crate::local::send(db, scope, "lattice", "ws", "kevin@x.com",
            &json!({"session": lead, "status": true}), 600.0)["lines"].clone();
        let waiting = status(&db);
        assert!(waiting[0].as_str().unwrap().starts_with("to lattice-views, just now (\"Codeword FIG-98\"): waiting"), "{waiting}");
        let mut helper_answer = json!({});
        let handed = crate::local::attach_inbox_for(&db, scope, "kevin@x.com", lead, helper_id, &mut helper_answer);
        assert!(text(&helper_answer, "inbox_note").contains("Codeword FIG-98"), "{helper_answer}");
        // its hook call from message m1 carried it; its next one, from m2, is past it
        crate::headsup::received_messages(&db, scope, "kevin@x.com", lead, helper_id, &handed, "m1");
        assert!(status(&db)[0].as_str().unwrap().ends_with("delivered just now, not seen yet"), "{}", status(&db));
        crate::headsup::settle(&db, scope, "kevin@x.com", lead, helper_id, "m2");
        assert!(status(&db)[0].as_str().unwrap().ends_with("delivered just now, seen just now"), "{}", status(&db));
        // it answers its lead
        let up = send_to(&db, scope, lead, helper_id, "lead", "FIG-98 received");
        assert_eq!(up["delivered_to"], json!([format!("kevin@x.com#{lead}/lead")]), "{up}");
        let mut lead_answer = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", lead, &mut lead_answer);
        assert!(text(&lead_answer, "inbox_note").contains("FIG-98 received"), "{lead_answer}");
        // a session's broadcast reaches it too, thinking or not
        let all = crate::local::broadcast_from(&db, scope, "ws", "kevin@x.com", lead, "", "the API moved to /api/v2", "claude", 600.0);
        assert!(all["sent_to"].as_array().unwrap().contains(&json!(format!("kevin@x.com#{lead}/{helper_id}"))), "{all}");
        // its SubagentStop: no longer an address
        crate::presence::forget_worker(&db, scope, "kevin@x.com", lead, helper_id);
        let gone = send_to(&db, scope, lead, "", "lattice-views", "still there?");
        assert_eq!(gone["ok"], json!(false), "{gone}");
        // back at work, then its session ends: gone with it
        crate::presence::note_worker_activity(&db, scope, "kevin@x.com", lead, helper_id, "lattice-views", "lattice-views", "reading", "static/js/views/issues.js", false);
        assert_eq!(send_to(&db, scope, lead, "", "lattice-views", "x")["ok"], json!(true));
        crate::presence::report_presence(&db, &crate::presence::PresenceInput {
            scope, user_id: "kevin@x.com", session: lead, path: "", action: "ended", agent: "claude-code", model: "", effort: Default::default(),
            branch: "", tokens: 0, turn_id: "", context: 0, via: "", worker: "",
        });
        assert!(crate::presence::session_helpers(&db, scope, "kevin@x.com", lead).is_empty());
    }

    #[test]
    fn a_name_that_fits_two_agents_or_none_sends_nothing_and_says_who_is_here() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "s1", "me");
        titled(&db, scope, "kevin@x.com", "s2", "fix tests");
        titled(&db, scope, "kevin@x.com", "s3", "Fix Tests");
        let two = send_to(&db, scope, "s1", "", "fix tests", "x");
        assert_eq!(two["ok"], json!(false), "{two}");
        let why = text(&two, "error");
        assert!(why.starts_with("nothing was sent") && why.contains("kevin@x.com#s2") && why.contains("kevin@x.com#s3"), "{why}");
        let none = send_to(&db, scope, "s1", "", "lattice-qa", "x");
        let why = text(&none, "error");
        assert_eq!(none["ok"], json!(false), "{none}");
        assert!(why.starts_with("nothing was sent: no agent called \"lattice-qa\" is at work in this repo; the agents here are: ")
            && why.contains("fix tests") && !why.contains(" me"), "{why}");
        // nothing filed under a name nobody reads
        assert!(inbox_of(&db, scope, "lattice-qa").is_empty() && inbox_of(&db, scope, "fix tests").is_empty());
        // a teammate by account still goes by account (a plan whose agents message teammates)
        let _ = db.kv_put(crate::billing::BILLING_BUCKET, "ws", &json!({"plan": "business", "status": "active"}), now());
        assert_eq!(send_to(&db, scope, "s1", "", "ana@x.com", "hi")["ok"], json!(true));
    }

    #[test]
    fn a_handoff_to_a_title_goes_to_that_session() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "s1", "planner");
        titled(&db, scope, "kevin@x.com", "s2", "builder");
        titled(&db, scope, "kevin@x.com", "s3", "reviewer");
        let sent = crate::local::handoff(&db, scope, "ws", "kevin@x.com", "builder", "s1", "Build the invite panel", "claude", 600.0);
        assert_eq!(sent["sent_to"], json!(["kevin@x.com#s2"]), "{sent}");
        let refused = crate::local::handoff(&db, scope, "ws", "kevin@x.com", "nobody", "s1", "Build it", "claude", 600.0);
        assert_eq!(refused["ok"], json!(false), "{refused}");
    }

    /// A handoff's sender hears who took it, then that the taker finished,
    /// at its next hook output (`attach_inbox`), each once.
    #[test]
    fn a_handoffs_sender_hears_who_took_it_and_when_it_finished() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "s1", "planner");
        titled(&db, scope, "kevin@x.com", "s2", "builder");
        let sent = crate::local::handoff(&db, scope, "ws", "kevin@x.com", "builder", "s1", "Build the invite panel: an email field and a send button", "claude", 600.0);
        assert_eq!(sent["sent_to"], json!(["kevin@x.com#s2"]), "{sent}");
        let mut before = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s1", &mut before);
        assert!(text(&before, "inbox_note").is_empty(), "not taken yet: {before}");
        // the builder's next step takes it
        let mut builder = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s2", &mut builder);
        assert!(text(&builder, "inbox_note").contains("Build the invite panel"), "{builder}");
        let mut planner = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s1", &mut planner);
        let note = text(&planner, "inbox_note");
        assert!(note.starts_with("Cork: builder took your handoff (\"Build the invite panel"), "{note}");
        let mut again = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s1", &mut again);
        assert!(text(&again, "inbox_note").is_empty(), "said once: {again}");
        // its turn ends with a closing message
        crate::local::handoffs_finished(&db, scope, "kevin@x.com", "s2", "", "The invite panel is in. Tests pass.", false);
        let mut done = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s1", &mut done);
        let note = text(&done, "inbox_note");
        assert!(note.contains("builder finished a turn on your handoff") && note.ends_with(": The invite panel is in."), "{note}");
        // nothing more to say once finished
        crate::local::handoffs_finished(&db, scope, "kevin@x.com", "s2", "", "", true);
        let mut after = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s1", &mut after);
        assert!(text(&after, "inbox_note").is_empty(), "{after}");
        // a waiting handoff, taken by the next session: who took it, then its end
        let docs = "ws:docs";
        let queued = crate::local::handoff(&db, docs, "ws", "kevin@x.com", "", "s9", "Write the docs", "claude", 600.0);
        assert_eq!(queued["queued"], json!(true), "{queued}");
        assert_eq!(crate::local::take_handoffs(&db, docs, "kevin@x.com", "s7").len(), 1);
        crate::local::handoffs_finished(&db, docs, "kevin@x.com", "s7", "", "", true);
        let mut s9 = json!({});
        crate::local::attach_inbox(&db, docs, "kevin@x.com", "s9", &mut s9);
        let note = text(&s9, "inbox_note");
        assert!(note.contains("took your handoff (\"Write the docs\")") && note.contains("ended") && note.contains("Check that it was done"), "{note}");
    }

    /// With no anchor or recipient at all, a message goes to every agent at
    /// work in the repo (the sender's own other sessions too), never to an
    /// agent in another repo. A message about code nobody is on reaches
    /// nobody (v104's test: a probe about cork-probe/probe.js, which neither
    /// session was near, reached both).
    #[test]
    fn an_anchorless_message_broadcasts_to_the_repo_and_only_the_repo() {
        let db = store();
        let (scope, other) = ("ws:shop", "ws:blog");
        live(&db, scope, "kevin@x.com", "s1", "a.py");
        live(&db, scope, "kevin@x.com", "s2", "b.py");
        live(&db, scope, "ana@x.com", "a1", "c.py");
        live(&db, other, "kevin@x.com", "s9", "post.md");
        // a plan whose agents message teammates (Free keeps them to your own)
        let _ = db.kv_put(crate::billing::BILLING_BUCKET, "ws", &json!({"plan": "business", "status": "active"}), now());
        let sent = crate::local::broadcast(&db, scope, "ws", "kevin@x.com", "s1", "tests are red on main", "claude", 600.0);
        let to: BTreeSet<&str> = sent["sent_to"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert_eq!(to, BTreeSet::from(["kevin@x.com#s2", "ana@x.com"]), "{sent}");
        assert!(inbox_of(&db, other, "kevin@x.com#s9").is_empty() && inbox_of(&db, other, "kevin@x.com").is_empty());
        // anchored, with nobody on that code: nobody, and the sender told so
        let before: usize = ["kevin@x.com#s2", "ana@x.com"].iter().map(|who| inbox_of(&db, scope, who).len()).sum();
        let routed = send_anchored_message_in(&db, scope, "ws", "kevin@x.com", "s1", "", "heads up", "shop/new.py::f", "claude", "");
        assert_eq!(routed["ok"], json!(true), "{routed}");
        assert_eq!(text(&routed, "note"), "nobody is working on shop/new.py; not delivered", "{routed}");
        assert_eq!(routed["delivered_to"], json!([]));
        let after: usize = ["kevin@x.com#s2", "ana@x.com"].iter().map(|who| inbox_of(&db, scope, who).len()).sum();
        assert_eq!(before, after, "nothing filed for anyone");
        // and through the cork message command too
        let cli = crate::local::send(&db, scope, "shop", "ws", "kevin@x.com",
            &json!({"session": "s1", "about": "shop/new.py::f", "text": "heads up"}), 600.0);
        assert_eq!(text(&cli, "note"), "nobody is working on shop/new.py; not delivered", "{cli}");
    }

    /// Sixteen agents at work, one announces a change to `fmt` with no
    /// recipient: only the sessions on that code receive it (Study 8, second
    /// study: every agent read every agent's plan). News gets no call to
    /// action; a question does.
    #[test]
    fn sixteen_sessions_one_announces_only_the_overlapping_ones_hear_it() {
        let db = store();
        let scope = "ws:shop";
        let _ = db.kv_put(crate::billing::BILLING_BUCKET, "ws", &json!({"plan": "business", "status": "active"}), now());
        crate::codegraph::update_file(&db, scope, "money.py", json!({"path": "money.py", "language": "python",
            "symbols": {"fmt": {"kind": "function", "edges": [], "span": [1, 2], "params": ["x"], "sites": []}}, "imports": [], "ts": 1.0}), 1.0);
        crate::codegraph::update_file(&db, scope, "app.py", json!({"path": "app.py", "language": "python",
            "symbols": {"tile": {"kind": "function", "edges": [["fmt", "", "calls"]], "span": [1, 2], "params": ["x"], "sites": []}},
            "imports": [{"module": "money", "names": [["fmt", "fmt"]]}], "ts": 1.0}), 1.0);
        for i in 0..16 {
            let session = format!("s{i}");
            let file = match i { 1 | 2 => "app.py", 3 => "money.py", _ => "orders.py" };
            live(&db, scope, "kevin@x.com", &session, file);
            crate::deltas::touch(&db, scope, &session, &[file.to_string()], now());
        }
        let sent = crate::local::broadcast(&db, scope, "ws", "kevin@x.com", "s0", "done: `fmt` takes a currency now", "claude", 600.0);
        let to: BTreeSet<&str> = sent["sent_to"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert_eq!(to, BTreeSet::from(["kevin@x.com#s1", "kevin@x.com#s2", "kevin@x.com#s3"]), "{sent}");
        // what s1 hears: news, with no call to action
        let mut answer = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s1", &mut answer);
        let note = text(&answer, "inbox_note");
        assert!(note.starts_with("For your information") && !note.contains("Act on") && note.contains("takes a currency"), "{note}");
        // a message that names no code reaches all fifteen; a question asks for action
        let all = crate::local::broadcast(&db, scope, "ws", "kevin@x.com", "s0", "is anyone else seeing the test db locked?", "claude", 600.0);
        assert_eq!(all["sent_to"].as_array().unwrap().len(), 15, "{all}");
        let mut answer = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s9", &mut answer);
        assert!(text(&answer, "inbox_note").contains("One asks something of you"), "{answer}");
    }

    /// The field report (v103): two Claude Code sessions in one repo, and
    /// `cork message --to cork-linear-test-e3`, the name Claude Code's own
    /// ListAgents and SendMessage use, was refused: Cork knew the session
    /// only by its title, "Cork version number". That name is taken now,
    /// before titles, and the agents are listed by both.
    #[test]
    fn a_session_is_reached_by_its_claude_code_name_and_listed_by_both_names() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "s1", "Linear backend work");
        titled(&db, scope, "kevin@x.com", "s2", "Cork version number");
        crate::freshness::note_session(&db, scope, "kevin@x.com", "s1", "cork-linear-test-0e", "claude", "m1");
        crate::freshness::note_session(&db, scope, "kevin@x.com", "s2", "cork-linear-test-e3", "claude", "m1");
        let sent = send_to(&db, scope, "s1", "", "Cork-Linear-Test-E3", "the API is up");
        assert_eq!(sent["delivered_to"], json!(["kevin@x.com#s2"]), "{sent}");
        // the receipt names it as a person reads it, by both names
        assert_eq!(sent["sent_to_names"], json!(["Cork version number (cork-linear-test-e3)"]), "{sent}");
        // a name nobody has: the agents here, by both names
        let none = send_to(&db, scope, "s1", "", "cork-linear-test-zz", "x");
        assert!(text(&none, "error").contains("Cork version number (cork-linear-test-e3)"), "{none}");
        // the Claude Code name comes before a title that happens to read the same
        titled(&db, scope, "kevin@x.com", "s3", "cork-linear-test-e3");
        assert_eq!(send_to(&db, scope, "s1", "", "cork-linear-test-e3", "x")["delivered_to"], json!(["kevin@x.com#s2"]));
        // and the sender's receipts name it both ways
        let status = crate::local::send(&db, scope, "lattice", "ws", "kevin@x.com", &json!({"session": "s1", "status": true}), 600.0);
        let lines: Vec<&str> = status["lines"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(lines.iter().any(|l| l.starts_with("to Cork version number (cork-linear-test-e3),")), "{lines:?}");
    }

    /// The field report (v104): a session's edits were labelled with the
    /// title Claude Code generated for its first task ("Lattice 3D engine")
    /// while it added an issue `kind` field. Once its hooks name its task,
    /// it is listed by its Claude Code name and that task, everywhere.
    #[test]
    fn a_session_is_labelled_by_its_name_and_current_task_not_a_stale_title() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "s1", "Cork version number");
        titled(&db, scope, "kevin@x.com", "s2", "Lattice 3D engine");
        crate::freshness::note_session(&db, scope, "kevin@x.com", "s2", "cork-linear-test-0a", "claude", "m1");
        crate::freshness::note_task(&db, scope, "kevin@x.com", "s2", "add an issue kind field");
        let none = send_to(&db, scope, "s1", "", "nobody-here", "x");
        let why = text(&none, "error");
        assert!(why.contains("cork-linear-test-0a (on \"add an issue kind field\")") && !why.contains("Lattice 3D engine"), "{why}");
        // the stale title still reaches it, as an address
        assert_eq!(send_to(&db, scope, "s1", "", "Lattice 3D engine", "x")["delivered_to"], json!(["kevin@x.com#s2"]));
        // its changes, as the other session hears of them
        db.ledger_append(scope, "edit_reported", &json!({"path": "src/db.rs", "user": "kevin@x.com", "session": "s2",
            "events": [{"symbol": "Issue", "kind": "renamed", "new_name": "Ticket"}]}), now() + 0.5).unwrap();
        crate::deltas::touch_as(&db, scope, "s1", &["src/db.rs".to_string()], true, now());
        let said = crate::deltas::deltas_told(&db, scope, "kevin@x.com", "s1", now(), 0, &BTreeSet::new());
        let lines = said["lines"].as_array().unwrap().iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
        assert!(lines.contains("another session of yours (cork-linear-test-0a, on \"add an issue kind field\")"), "{lines}");
        // and who Cork says sent what it sends
        let mut sent = json!({"id": "m9", "from": "kevin@x.com", "sender_session": "s2", "message": "done"});
        vouch(&db, scope, &mut sent, "kevin@x.com", "s1");
        assert!(text(&sent, "verified").contains("session cork-linear-test-0a (on \"add an issue kind field\")"), "{sent}");
    }

    /// The field report (v104): session A handed B work and then read B's
    /// log to learn it was done. A session that finishes its task, or ends,
    /// tells the sessions it exchanged messages or handoffs with, once, with
    /// its closing words; a session it never talked with hears nothing.
    #[test]
    fn a_session_that_finishes_tells_the_sessions_it_talked_with_once() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "a", "Linear backend work");
        titled(&db, scope, "kevin@x.com", "b", "Start date");
        titled(&db, scope, "kevin@x.com", "c", "Docs");
        crate::freshness::note_session(&db, scope, "kevin@x.com", "b", "cork-linear-test-35", "claude", "m1");
        crate::freshness::note_task(&db, scope, "kevin@x.com", "b", "add an issue start_date field");
        let heard = |session: &str| {
            let mut answer = json!({});
            crate::local::attach_inbox(&db, scope, "kevin@x.com", session, &mut answer);
            text(&answer, "inbox_note")
        };
        send_to(&db, scope, "a", "", "Start date", "build start_date next to my kind field");
        heard("b");
        // still at it: nothing yet
        assert!(!crate::local::reads_finished("Still wiring the API; the tests come next."));
        let done = "Done: start_date is in, end to end, and the 28 tests pass.";
        assert!(crate::local::reads_finished(done));
        crate::local::tell_partners_finished(&db, scope, "kevin@x.com", "b", done, false);
        let told = heard("a");
        assert!(told.contains("Cork: cork-linear-test-35 (on \"add an issue start_date field\") finished its task: \"Done: start_date is in"), "{told}");
        assert_eq!(heard("c"), "", "it never talked with c");
        // once: another finished turn, or its end, says nothing more
        crate::local::tell_partners_finished(&db, scope, "kevin@x.com", "b", done, false);
        crate::local::tell_partners_finished(&db, scope, "kevin@x.com", "b", "", true);
        assert_eq!(heard("a"), "");
        // a later exchange, then its end: told again, that it ended
        send_to(&db, scope, "a", "", "Start date", "and the docs?");
        heard("b");
        crate::local::tell_partners_finished(&db, scope, "kevin@x.com", "b", "API.md documents it.", true);
        assert!(heard("a").contains("finished: its session ended. Its last words: \"API.md documents it.\""));
    }

    /// A message past what a line carries says how to read the rest, and
    /// `cork message --show <id>` gives it whole, with who sent it, to its
    /// recipient (and its sender) only.
    #[test]
    fn a_cut_message_points_at_show_and_show_gives_it_whole() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "s1", "Linear backend work");
        titled(&db, scope, "kevin@x.com", "s2", "Cork version number");
        let long = format!("Start: {} End.", "the store needs a reducer. ".repeat(120));
        let sent = send_to(&db, scope, "s1", "", "Cork version number", &long);
        let id = text(&sent, "message_id");
        let mut inbox = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s2", &mut inbox);
        let note = text(&inbox, "inbox_note");
        let more = long.chars().count() - MESSAGE_LINE_CHARS;
        assert!(note.contains(&format!("[{more} more characters; read it whole: cork message --show {id}]")), "{note}");
        assert!(!note.contains(" End."), "{note}");
        let shown = crate::local::send(&db, scope, "lattice", "ws", "kevin@x.com", &json!({"session": "s2", "show": id}), 600.0);
        let whole = text(&shown, "text");
        assert!(whole.contains("Start:") && whole.contains(" End.") && !whole.contains("more characters"), "{whole}");
        assert!(whole.contains("verified by Cork: kevin@x.com · session \"Linear backend work\""), "{whole}");
        // a teammate's agent cannot read it
        let other = crate::local::send(&db, scope, "lattice", "ws", "ana@x.com", &json!({"session": "a1", "show": id}), 600.0);
        assert_eq!(other["ok"], json!(false), "{other}");
        // nor is an id nobody sent found
        let missing = crate::local::send(&db, scope, "lattice", "ws", "kevin@x.com", &json!({"show": "nope"}), 600.0);
        assert_eq!(missing["ok"], json!(false), "{missing}");
    }

    /// The field report (v103): one session told the other "the lead owns
    /// src/", and the reader had only the message's word for it. Every
    /// delivered message now says who Cork authenticated as its sender, and
    /// what Cork saw that session do on the paths it names.
    #[test]
    fn a_delivered_message_says_who_cork_verified_sent_it_and_what_it_did() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "s1", "Cork version number");
        titled(&db, scope, "kevin@x.com", "s2", "Linear frontend");
        crate::freshness::note_session(&db, scope, "kevin@x.com", "s1", "cork-linear-test-e3", "claude", "m1");
        crate::freshness::note_session(&db, scope, "kevin@x.com", "s2", "cork-linear-test-0e", "codex", "m1");
        let edit = json!({"user": "kevin@x.com", "session": "s1", "path": "src/api.rs"});
        db.ledger_append(scope, "edit_reported", &edit, now() - 2400.0).unwrap();
        // someone else's edit to static/ is not the sender's
        db.ledger_append(scope, "edit_reported", &json!({"user": "kevin@x.com", "session": "s2", "path": "static/app.js"}), now()).unwrap();
        send_to(&db, scope, "s1", "", "cork-linear-test-0e", "I own src/api.rs and static/, leave both alone");
        let mut inbox = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s2", &mut inbox);
        let note = text(&inbox, "inbox_note");
        let lines: Vec<&str> = note.lines().collect();
        let at = lines.iter().position(|l| l.contains("I own src/api.rs")).expect("the message");
        assert_eq!(lines[at + 1], "  verified by Cork: kevin@x.com · session \"Cork version number\" (cork-linear-test-e3) · Claude Code · this machine", "{note}");
        assert_eq!(lines[at + 2], "  Cork sees: that session last edited src/api.rs 40m ago; nothing from it in static/ in the last day", "{note}");
        // on another machine, it says so; with no session, it says that
        crate::freshness::note_session(&db, scope, "kevin@x.com", "s2", "", "", "m2");
        send_to(&db, scope, "s1", "", "cork-linear-test-0e", "fyi");
        let mut again = json!({});
        crate::local::attach_inbox(&db, scope, "kevin@x.com", "s2", &mut again);
        assert!(text(&again, "inbox_note").contains("· Claude Code · another machine"), "{again}");
        let mut from_dashboard = json!({"id": "d1", "from": "ana@x.com", "message": "hi"});
        vouch(&db, scope, &mut from_dashboard, "kevin@x.com", "s2");
        assert_eq!(text(&from_dashboard, "verified"), "verified by Cork: ana@x.com · not from an agent session");
    }

    /// A message to a Claude Code session on the sender's machine is also
    /// posted through Claude Code's own peer messaging. The socket never
    /// answers, so Cork's copy stays, held, until the recipient's hooks find
    /// it in its transcript: then it is delivered and never printed again.
    #[test]
    fn a_message_found_in_the_recipients_transcript_is_never_printed_again() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "s1", "Linear backend work");
        titled(&db, scope, "kevin@x.com", "s2", "Cork version number");
        let sent = peer_send(&db, scope, "the API is up", json!(["s2", "s9"]));
        let id = text(&sent, "message_id");
        let handed = sent["peer"].as_array().cloned().unwrap_or_default();
        assert_eq!(handed.len(), 1, "{sent}");
        assert_eq!((text(&handed[0], "to"), text(&handed[0], "session")), ("kevin@x.com#s2".to_string(), "s2".to_string()));
        assert_eq!(text(&handed[0]["message"], "message"), "the API is up");
        assert!(text(&handed[0]["message"], "verified").starts_with("verified by Cork: kevin@x.com"), "{sent}");
        // held while the sender posts it: neither the hooks nor the listener take it
        assert_eq!(peer_hook(&db, scope), "");
        assert!(crate::push::take_for_session(&db, scope, "kevin@x.com", "s2").is_empty());
        // only its sender says how the post went
        let stranger = crate::local::send(&db, scope, "lattice", "ws", "ana@x.com",
            &json!({"peer_done": id, "to": "kevin@x.com#s2", "landed": true}), 600.0);
        assert_eq!(stranger["ok"], json!(false), "{stranger}");
        peer_done(&db, scope, &id, true);
        // through: still held, and still Cork's (it can be read whole)
        assert_eq!(peer_hook(&db, scope), "");
        assert_eq!(send_as(&db, scope, json!({"session": "s2", "show": id}))["ok"], json!(true));
        // the recipient's hooks find it in its transcript: delivered and received
        crate::local::peer_seen(&db, scope, "kevin@x.com", &json!({"session": "s2", "peer_seen": [id]}));
        let seen = db.eph_get(&seen_key(&id)).unwrap_or_default();
        assert!(seen["delivered_at"].is_f64() && seen["seen_at"].is_f64(), "{seen}");
        // and no hook or listener prints it again, however long after
        backdate(&db, scope, &id, PEER_WINDOW_S + 1.0);
        assert_eq!(peer_hook(&db, scope), "");
        assert_eq!(peer_hook(&db, scope), "");
        assert!(crate::push::take_for_session(&db, scope, "kevin@x.com", "s2").is_empty());
        // a post that did not go through goes the usual way at once
        let again = peer_send(&db, scope, "and the board view", json!(["s2"]));
        peer_done(&db, scope, &text(&again, "message_id"), false);
        assert!(peer_hook(&db, scope).contains("and the board view"));
        // a recipient that is not a Claude Code session on the sender's machine: nothing held
        let elsewhere = peer_send(&db, scope, "fyi", json!(["s7"]));
        assert!(elsewhere.get("peer").is_none(), "{elsewhere}");
        assert!(peer_hook(&db, scope).contains("fyi"));
    }

    /// Not found in the transcript: once the window passed and the
    /// recipient made two hook calls, a hook prints it after all. Worst
    /// case the agent reads it twice, never zero times.
    #[test]
    fn a_posted_message_never_found_is_printed_after_the_window() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "s1", "Linear backend work");
        titled(&db, scope, "kevin@x.com", "s2", "Cork version number");
        let id = text(&peer_send(&db, scope, "the API is up", json!(["s2"])), "message_id");
        peer_done(&db, scope, &id, true);
        // two calls inside the window: held
        assert_eq!(peer_hook(&db, scope), "");
        assert_eq!(peer_hook(&db, scope), "");
        // the window passed, and the calls were made: printed
        backdate(&db, scope, &id, PEER_WINDOW_S + 1.0);
        assert!(peer_hook(&db, scope).contains("the API is up"));
        // the window alone is not enough for the hooks: one call since is too few
        let other = text(&peer_send(&db, scope, "and the board view", json!(["s2"])), "message_id");
        peer_done(&db, scope, &other, true);
        backdate(&db, scope, &other, PEER_WINDOW_S + 1.0);
        assert_eq!(peer_hook(&db, scope), "", "one call");
        assert!(peer_hook(&db, scope).contains("and the board view"), "two calls");
    }

    /// A recipient that makes no hook calls (the post never reached it, and
    /// it sits idle) still gets it: its listener takes it once the window
    /// passed.
    #[test]
    fn a_posted_message_to_a_silent_session_goes_to_its_listener_after_the_window() {
        let db = store();
        let scope = "ws:lattice";
        titled(&db, scope, "kevin@x.com", "s1", "Linear backend work");
        titled(&db, scope, "kevin@x.com", "s2", "Cork version number");
        let id = text(&peer_send(&db, scope, "the API is up", json!(["s2"])), "message_id");
        peer_done(&db, scope, &id, true);
        assert!(crate::push::take_for_session(&db, scope, "kevin@x.com", "s2").is_empty(), "inside the window");
        backdate(&db, scope, &id, PEER_WINDOW_S + 1.0);
        let taken = crate::push::take_for_session(&db, scope, "kevin@x.com", "s2");
        assert_eq!(taken.len(), 1, "{taken:?}");
        assert_eq!(text(&taken[0], "message"), "the API is up");
    }

    fn send_as(db: &Store, scope: &str, body: Value) -> Value {
        crate::local::send(db, scope, "lattice", "ws", "kevin@x.com", &body, 600.0)
    }

    /// Session s1 tells s2, listing the Claude Code sessions on its machine.
    fn peer_send(db: &Store, scope: &str, said: &str, peers: Value) -> Value {
        send_as(db, scope, json!({"session": "s1", "to": "Cork version number", "text": said, "peers": peers}))
    }

    fn peer_done(db: &Store, scope: &str, id: &str, landed: bool) {
        let done = send_as(db, scope, json!({"session": "s1", "peer_done": id, "to": "kevin@x.com#s2", "landed": landed}));
        assert_eq!(done["ok"], json!(true), "{done}");
    }

    /// One hook call of s2: what it prints of its messages.
    fn peer_hook(db: &Store, scope: &str) -> String {
        let mut answer = json!({});
        crate::local::attach_inbox(db, scope, "kevin@x.com", "s2", &mut answer);
        text(&answer, "inbox_note")
    }

    /// The post of `id` to s2 happened `ago` seconds ago.
    fn backdate(db: &Store, scope: &str, id: &str, ago: f64) {
        let key = peer_claim_key(scope, "kevin@x.com#s2", id);
        if let Some(mut claim) = db.eph_get(&key) {
            claim["peer_sent"] = json!(now() - ago);
            db.eph_set(&key, &claim, Some(600.0)).unwrap();
        }
    }

    #[test]
    fn the_paths_a_message_names_are_found_and_prose_is_not() {
        assert_eq!(
            paths_named("Mail kevin@x.com or see https://x.dev/a/: I own src/ and static/js/store.js; Cargo.toml, e.g. the api.rs file"),
            vec!["src/", "static/js/store.js", "Cargo.toml", "api.rs"]
        );
        assert!(paths_named("and/or v1.2 is fine, i.e. done").is_empty());
    }

    /// The leak: a session working in repo B read the person's inbox across
    /// every repo of the workspace, so a note to everyone at work in repo A
    /// (and messages routed by A's code) reached it. Only a message
    /// addressed to the person crosses repos.
    #[test]
    fn a_session_in_repo_b_hears_only_personal_messages_from_repo_a() {
        let db = store();
        let (a, b) = ("ws:a", "ws:b");
        send_agent_message_as(&db, a, "ana@x.com", "kevin@x.com", "everyone: main is red", "claude", true);
        let _ = db.kv_put("inbox", &format!("{a}:kevin@x.com:r1"),
            &json!({"id": "r1", "to": "kevin@x.com", "from": "ana@x.com", "message": "about a.py", "routed": true, "ts": now()}), now());
        send_agent_message_in(&db, a, "ana@x.com", "", "kevin@x.com", "can you review my PR in repo a?", "claude", false);
        let visible: BTreeSet<String> = [a.to_string(), b.to_string()].into();
        let got = inbox_ack(&db, b, "kevin@x.com", &visible, &[]);
        let said: Vec<&str> = got["messages"].as_array().unwrap().iter().map(|m| m["message"].as_str().unwrap()).collect();
        assert_eq!(said, vec!["can you review my PR in repo a?"], "{got}");
        // in repo a itself all three arrive
        assert_eq!(inbox_ack(&db, a, "kevin@x.com", &visible, &[])["messages"].as_array().unwrap().len(), 3);
    }
}
