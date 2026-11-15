//! Messages pushed to an agent the moment they are sent, instead of waiting
//! for its next hook call. `POST /inbox/wait` holds the caller until a
//! message is there for it (or a short while passes), and hands each
//! message to exactly one taker:
//!
//! - a **session** listener (`cork listen`, one per open Claude Code
//!   session) takes a message at once when it was sent in the session's
//!   own repo, and from another repo of the workspace after a short grace,
//!   so an open session in the right repo gets first pick;
//! - the machine's **wake** watcher (`cork wake`) takes what no open
//!   session took after a longer grace, and only for a repo the machine has
//!   a checkout of and no agent of the person is working in right now: it
//!   starts an agent there to weigh the message.
//!
//! A message the hooks or the MCP tools already showed whole (the
//! `mcpmsg:` mark) is not pushed again, and a pushed one is marked shown for
//! the session it went to, so the hooks do not repeat it either.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::presence::split_scope;
use crate::store::{now, Store};

/// How long an open session in the message's own repo has to take it
/// before a session in another repo of the workspace may.
pub const OTHER_REPO_GRACE_S: f64 = 20.0;
/// How long open sessions have before the wake watcher may start an agent.
pub const WAKE_GRACE_S: f64 = 45.0;
/// Older messages are left to the inbox: a watcher switched on today does
/// not start agents for last month's.
pub const MAX_AGE_S: f64 = 24.0 * 3600.0;
/// One taker per message, for as long as a message can be taken.
const CLAIM_TTL_S: f64 = MAX_AGE_S + 3600.0;
/// The longest one wait holds the caller.
pub const MAX_WAIT_S: f64 = 25.0;

/// Wakes every waiting `/inbox/wait` when a message is filed.
pub fn arrivals() -> &'static tokio::sync::Notify {
    static ARRIVED: OnceLock<tokio::sync::Notify> = OnceLock::new();
    ARRIVED.get_or_init(tokio::sync::Notify::new)
}

/// Called wherever a message is filed into an inbox.
pub fn arrived() {
    arrivals().notify_waiters();
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Taker {
    Session,
    Wake,
}

impl Taker {
    pub fn parse(role: &str) -> Taker {
        if role == "wake" { Taker::Wake } else { Taker::Session }
    }
}

pub struct Ask<'a> {
    pub taker: Taker,
    pub workspace: &'a str,
    pub user_id: &'a str,
    /// the caller's own repo scope; for the wake watcher, none in particular
    pub scope: &'a str,
    pub session: &'a str,
    /// the repo scopes the caller may read, `access::visible_scopes`
    pub visible: &'a BTreeSet<String>,
    /// the wake watcher's checkouts: scope -> the repo id as the machine spells it
    pub checkouts: &'a [(String, String)],
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Some agent of this person is working in the repo right now (a hook or a
/// tool call in the last three minutes): the hooks will hand it the message.
fn someone_live(store: &Store, workspace: &str, user_id: &str, scope: &str) -> bool {
    let repo = split_scope(scope).1;
    store
        .eph_scan(&format!("focus:{workspace}:{user_id}#"))
        .iter()
        .any(|(_, marker)| text(marker, "repo_id") == repo)
}

/// The messages this caller takes now, each claimed so no one else does.
/// `filter` applies the plan's messaging rule to the candidates.
pub fn take(store: &Store, ask: &Ask, filter: impl Fn(Vec<Value>) -> Vec<Value>) -> Vec<Value> {
    let stamp = now();
    let mut scopes: Vec<&str> = Vec::new();
    if !ask.scope.is_empty() {
        scopes.push(ask.scope);
    }
    scopes.extend(ask.visible.iter().map(String::as_str).filter(|s| *s != ask.scope));

    let mut candidates: Vec<Value> = Vec::new();
    for scope in &scopes {
        for (key, mut record) in store.kv_list("inbox", &format!("{scope}:{}:", ask.user_id)) {
            let id = key.rsplit_once(':').map(|(_, id)| id).unwrap_or("").to_string();
            if let Some(map) = record.as_object_mut() {
                map.insert("id".into(), json!(id));
                map.insert("_scope".into(), json!(scope));
            }
            candidates.push(record);
        }
    }
    let mut taken: Vec<Value> = Vec::new();
    for mut message in filter(candidates) {
        let id = text(&message, "id");
        let scope = text(&message, "_scope");
        let age = stamp - message.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        if id.is_empty() || age > MAX_AGE_S {
            continue;
        }
        // a note from one of this person's sessions to their own agents:
        // each OTHER open session takes its own copy, the sender none, and
        // it never starts an agent (it is news for the ones at work)
        let own_note = crate::agenttools::is_own_session_note(&message, ask.user_id);
        if own_note
            && (ask.taker == Taker::Wake
                || ask.session.is_empty()
                || text(&message, "from_session") == ask.session
                || store.eph_get(&format!("briefmsg:{}:{id}", ask.session)).is_some())
        {
            continue;
        }
        let claim = if own_note {
            format!("pushed:{}:{id}", crate::local::session_inbox(ask.user_id, ask.session))
        } else {
            format!("pushed:{}:{id}", ask.user_id)
        };
        if store.eph_get(&claim).is_some() {
            continue;
        }
        // shown whole by a hook or a tool reply: that agent has it
        if !own_note && store.eph_get(&format!("mcpmsg:{}:{id}", ask.user_id)).is_some() {
            let _ = store.eph_set(&claim, &json!({"ts": stamp, "by": "shown"}), Some(CLAIM_TTL_S));
            continue;
        }
        // only a message addressed to this person is worth a turn of its own:
        // a note to everyone at work, one routed by the code it is about and
        // Cork's own notices ride the next hook answer, as before
        let personal = message.get("broadcast").and_then(Value::as_bool) != Some(true)
            && message.get("routed").and_then(Value::as_bool) != Some(true)
            && text(&message, "from") != "collide";
        if !personal {
            continue;
        }
        let mut repo_id = split_scope(&scope).1.to_string();
        match ask.taker {
            Taker::Session => {
                if scope != ask.scope && age < OTHER_REPO_GRACE_S {
                    continue;
                }
            }
            Taker::Wake => {
                if age < WAKE_GRACE_S || someone_live(store, ask.workspace, ask.user_id, &scope) {
                    continue;
                }
                match ask.checkouts.iter().find(|(s, _)| *s == scope) {
                    Some((_, spelled)) => repo_id = spelled.clone(),
                    // no checkout of it here: another machine may have one
                    None => continue,
                }
            }
        }
        let by = if ask.taker == Taker::Wake { "wake" } else { "session" };
        let _ = store.eph_set(&claim, &json!({"ts": stamp, "by": by, "session": ask.session}), Some(CLAIM_TTL_S));
        if ask.taker == Taker::Session && !ask.session.is_empty() {
            // the hooks of that session, and the MCP replies, name it from now on
            let _ = store.eph_set(&format!("briefmsg:{}:{id}", ask.session), &json!({"ts": stamp}),
                Some(crate::access::TOLD_ONCE_TTL_S));
            if !own_note {
                let _ = store.eph_set(&format!("mcpmsg:{}:{id}", ask.user_id), &json!({"ts": stamp}), Some(3600.0));
            }
        }
        // who sent it, as Cork authenticated it, for the listener to print
        crate::agenttools::vouch(store, &scope, &mut message, ask.user_id, ask.session);
        if let Some(map) = message.as_object_mut() {
            map.remove("_scope");
            map.insert("repo_id".into(), json!(repo_id));
            map.insert("age_s".into(), json!(age.max(0.0).round()));
        }
        taken.push(message);
    }
    taken.sort_by(|a, b| {
        let ts = |m: &Value| m.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        ts(a).partial_cmp(&ts(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    taken
}

/// Messages one of this person's sessions sent to this session alone
/// (`cork message --to` naming it): taken off its queue as they are
/// pushed, as the hooks take them when they print them.
pub fn take_for_session(store: &Store, scope: &str, user_id: &str, session: &str) -> Vec<Value> {
    if session.is_empty() || scope.is_empty() {
        return Vec::new();
    }
    let inbox = crate::local::session_inbox(user_id, session);
    let mut taken = Vec::new();
    for (key, mut record) in store.kv_list("inbox", &format!("{scope}:{inbox}:")) {
        // a note to every session rides the hooks; one sent to this session alone comes now
        if record.get("broadcast").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        // posted to it through Claude Code, and not yet known to have
        // arrived: this session may be making no hook calls, so the
        // listener lets it go once the window passed
        if crate::agenttools::peer_held(store, scope, &inbox, key.rsplit(':').next().unwrap_or(""), true) {
            continue;
        }
        let _ = store.kv_delete("inbox", &key);
        if let Some(map) = record.as_object_mut() {
            map.insert("repo_id".into(), json!(split_scope(scope).1));
            map.insert("own_session".into(), json!(true));
        }
        taken.push(record);
    }
    // what the session's own helpers sent its lead (`local::agent_inbox`):
    // the lead's alone, so its listener takes every one of them now
    let lead = crate::local::agent_inbox(user_id, session, "");
    for (key, mut record) in store.kv_list("inbox", &format!("{scope}:{lead}:")) {
        let _ = store.kv_delete("inbox", &key);
        if let Some(map) = record.as_object_mut() {
            map.insert("repo_id".into(), json!(split_scope(scope).1));
            map.insert("own_session".into(), json!(true));
            map.insert("within_session".into(), json!(true));
        }
        taken.push(record);
    }
    // who sent each, as Cork authenticated it, for the listener to print
    for record in taken.iter_mut() {
        crate::agenttools::vouch(store, scope, record, user_id, session);
    }
    taken
}

/// The waiting messages one hook answer shows this session: each whole, and
/// once. A message this person was already shown whole, by any path (another
/// of their sessions' hooks, a push, an MCP reply: the `mcpmsg:` mark), is
/// not shown again in any form; v76 named it as "- [id] from X (shown to
/// you earlier)", a line about a message the session had never read (26 a
/// run in the scaling study). Past `budget` characters a message waits,
/// unmarked, for the next answer instead of being named. Each line says who
/// Cork says sent it (`agenttools::vouch`; `scope` is the reader's repo).
/// Returns the lines and the messages they delivered.
pub fn hook_lines(
    store: &Store, scope: &str, user_id: &str, session: &str, messages: &[Value], budget: usize,
) -> (Vec<String>, Vec<Value>) {
    let mut budget = budget;
    let (mut lines, mut reached) = (Vec::new(), Vec::new());
    for message in messages {
        let id = text(message, "id");
        let told = format!("briefmsg:{session}:{id}");
        if id.is_empty() || store.eph_get(&told).is_some() {
            continue;
        }
        // a note from another of this person's own sessions: every session
        // is told it once (`told`), the first one does not use it up for
        // the rest, and the one that sent it is never handed it back
        let own_note = crate::agenttools::is_own_session_note(message, user_id);
        if own_note && text(message, "from_session") == session {
            continue;
        }
        let shown = format!("mcpmsg:{user_id}:{id}");
        if !own_note && store.eph_get(&shown).is_some() {
            // delivered once already: settled for this session too, silently
            let _ = store.eph_set(&told, &json!({"ts": now()}), Some(crate::access::TOLD_ONCE_TTL_S));
            continue;
        }
        if budget < 200 {
            break; // the rest ride the next hook answer, whole
        }
        let _ = store.eph_set(&told, &json!({"ts": now()}), Some(crate::access::TOLD_ONCE_TTL_S));
        if !own_note {
            let _ = store.eph_set(&shown, &json!({"ts": now()}), Some(3600.0));
        }
        // a handoff's spec is the work itself: whole, even past the budget
        let limit = if crate::agenttools::is_handoff(message) { budget.max(crate::agenttools::HANDOFF_CHARS) } else { budget };
        let mut vouched = message.clone();
        crate::agenttools::vouch(store, scope, &mut vouched, user_id, session);
        let line = crate::agenttools::message_line_within(&vouched, limit);
        budget = budget.saturating_sub(line.chars().count());
        lines.push(line);
        reached.push(message.clone());
    }
    (lines, reached)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("store")
    }

    fn file(store: &Store, scope: &str, to: &str, id: &str, ago: f64) {
        let record = json!({"id": id, "to": to, "from": "bob@x.com", "message": "please look", "ts": now() - ago});
        store.kv_put("inbox", &format!("{scope}:{to}:{id}"), &record, now()).unwrap();
    }

    fn ask<'a>(taker: Taker, scope: &'a str, session: &'a str, visible: &'a BTreeSet<String>, checkouts: &'a [(String, String)]) -> Ask<'a> {
        Ask { taker, workspace: "ws", user_id: "kevin@x.com", scope, session, visible, checkouts }
    }

    #[test]
    fn a_session_in_the_repo_takes_a_message_once_and_the_hooks_then_name_it() {
        let store = store();
        file(&store, "ws:app", "kevin@x.com", "m1", 1.0);
        let visible = BTreeSet::from(["ws:app".to_string()]);
        let got = take(&store, &ask(Taker::Session, "ws:app", "s1", &visible, &[]), |m| m);
        assert_eq!(got.len(), 1);
        assert_eq!(text(&got[0], "repo_id"), "app");
        assert!(store.eph_get("briefmsg:s1:m1").is_some());
        assert!(take(&store, &ask(Taker::Session, "ws:app", "s2", &visible, &[]), |m| m).is_empty());
    }

    #[test]
    fn another_repo_waits_its_grace_and_the_wake_watcher_waits_longer() {
        let store = store();
        file(&store, "ws:app", "kevin@x.com", "m1", 5.0);
        let visible = BTreeSet::from(["ws:app".to_string(), "ws:web".to_string()]);
        let checkouts = vec![("ws:app".to_string(), "github.com/acme/app".to_string())];
        assert!(take(&store, &ask(Taker::Session, "ws:web", "s1", &visible, &[]), |m| m).is_empty());
        assert!(take(&store, &ask(Taker::Wake, "", "", &visible, &checkouts), |m| m).is_empty());

        let store = self::store();
        file(&store, "ws:app", "kevin@x.com", "m2", WAKE_GRACE_S + 1.0);
        let got = take(&store, &ask(Taker::Wake, "", "", &visible, &checkouts), |m| m);
        assert_eq!(text(&got[0], "repo_id"), "github.com/acme/app");
    }

    #[test]
    fn the_wake_watcher_leaves_a_message_to_a_live_agent_a_missing_checkout_or_an_old_one() {
        let store = store();
        let visible = BTreeSet::from(["ws:app".to_string(), "ws:web".to_string()]);
        let checkouts = vec![("ws:app".to_string(), "app".to_string())];
        file(&store, "ws:web", "kevin@x.com", "elsewhere", 100.0);
        file(&store, "ws:app", "kevin@x.com", "stale", MAX_AGE_S + 10.0);
        assert!(take(&store, &ask(Taker::Wake, "", "", &visible, &checkouts), |m| m).is_empty());

        file(&store, "ws:app", "kevin@x.com", "live", 100.0);
        store.eph_set("focus:ws:kevin@x.com#s9", &json!({"repo_id": "app"}), Some(60.0)).unwrap();
        assert!(take(&store, &ask(Taker::Wake, "", "", &visible, &checkouts), |m| m).is_empty());
    }

    #[test]
    fn a_note_to_everyone_a_routed_one_and_collides_own_ride_the_hooks_instead() {
        let store = store();
        let visible = BTreeSet::from(["ws:app".to_string()]);
        for (id, extra) in [("b", json!({"broadcast": true})), ("r", json!({"routed": true})), ("c", json!({"from": "collide"}))] {
            let mut record = json!({"id": id, "to": "kevin@x.com", "from": "bob@x.com", "message": "fyi", "ts": now()});
            for (k, v) in extra.as_object().unwrap() {
                record[k] = v.clone();
            }
            store.kv_put("inbox", &format!("ws:app:kevin@x.com:{id}"), &record, now()).unwrap();
        }
        assert!(take(&store, &ask(Taker::Session, "ws:app", "s1", &visible, &[]), |m| m).is_empty());
    }

    #[test]
    fn a_note_to_my_own_agents_reaches_every_other_session_once_and_never_the_sender() {
        let store = store();
        // session s1 of kevin writes to kevin's agents
        let sent = crate::agenttools::send_agent_message_in(
            &store, "ws:app", "kevin@x.com", "s1", "kevin@x.com", "rebase on main first", "claude", false);
        assert_eq!(sent["ok"], true, "{sent}");
        let inbox = crate::agenttools::inbox_ack(&store, "ws:app", "kevin@x.com", &BTreeSet::new(), &[]);
        let messages = inbox["messages"].as_array().cloned().unwrap();
        assert_eq!(messages.len(), 1);
        // the sender's own hooks never hand it back
        let (lines, _) = hook_lines(&store, "ws:app", "kevin@x.com", "s1", &messages, 7000);
        assert!(lines.is_empty(), "{lines:?}");
        // every other session hears it, each once: the first does not use it up
        let (lines, reached) = hook_lines(&store, "ws:app", "kevin@x.com", "s2", &messages, 7000);
        assert_eq!((lines.len(), reached.len()), (1, 1));
        let (lines, _) = hook_lines(&store, "ws:app", "kevin@x.com", "s3", &messages, 7000);
        assert_eq!(lines.len(), 1, "a third session is told too");
        let (lines, _) = hook_lines(&store, "ws:app", "kevin@x.com", "s2", &messages, 7000);
        assert!(lines.is_empty(), "once a session");
        // a listening session takes its own copy; the sender's listener none
        let visible = BTreeSet::from(["ws:app".to_string()]);
        assert!(take(&store, &ask(Taker::Session, "ws:app", "s1", &visible, &[]), |m| m).is_empty());
        assert_eq!(take(&store, &ask(Taker::Session, "ws:app", "s4", &visible, &[]), |m| m).len(), 1);
        assert_eq!(take(&store, &ask(Taker::Session, "ws:app", "s5", &visible, &[]), |m| m).len(), 1);
        assert!(take(&store, &ask(Taker::Session, "ws:app", "s2", &visible, &[]), |m| m).is_empty(), "s2's hooks told it");
        assert!(take(&store, &ask(Taker::Wake, "", "", &visible, &[("ws:app".into(), "app".into())]), |m| m).is_empty());
        // a teammate's note is still the person's, whichever session reads first
        file(&store, "ws:app", "kevin@x.com", "team", 1.0);
        let inbox = crate::agenttools::inbox_ack(&store, "ws:app", "kevin@x.com", &BTreeSet::new(), &[]);
        let team: Vec<Value> = inbox["messages"].as_array().unwrap().iter().filter(|m| m["id"] == "team").cloned().collect();
        assert_eq!(hook_lines(&store, "ws:app", "kevin@x.com", "s6", &team, 7000).0.len(), 1);
        assert!(hook_lines(&store, "ws:app", "kevin@x.com", "s7", &team, 7000).0.is_empty());
    }

    #[test]
    fn a_message_older_than_a_week_is_no_longer_handed_over() {
        let store = store();
        file(&store, "ws:app", "kevin@x.com", "old", crate::agenttools::MESSAGE_TTL_S + 60.0);
        file(&store, "ws:app", "kevin@x.com", "new", 60.0);
        let inbox = crate::agenttools::inbox_ack(&store, "ws:app", "kevin@x.com", &BTreeSet::new(), &[]);
        let ids: Vec<&str> = inbox["messages"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
        assert_eq!(ids, vec!["new"]);
        let (status, _, _) = crate::agenttools::delivery_status(None, false, now() - crate::agenttools::MESSAGE_TTL_S - 60.0, true);
        assert_eq!(status, "expired");
        let (status, _, _) = crate::agenttools::delivery_status(None, false, now() - 60.0, true);
        assert_eq!(status, "pending");
    }

    #[test]
    fn a_message_already_shown_whole_is_not_pushed() {
        let store = store();
        file(&store, "ws:app", "kevin@x.com", "m1", 1.0);
        store.eph_set("mcpmsg:kevin@x.com:m1", &json!({}), Some(60.0)).unwrap();
        let visible = BTreeSet::from(["ws:app".to_string()]);
        assert!(take(&store, &ask(Taker::Session, "ws:app", "s1", &visible, &[]), |m| m).is_empty());
    }

    #[test]
    fn a_hook_shows_a_message_once_and_never_names_it_again() {
        let store = store();
        let m = json!({"id": "m1", "from": "bob@x.com", "message": "renamed price_for", "broadcast": true, "ts": now()});
        let (a, reached) = hook_lines(&store, "ws:app", "kevin@x.com", "sA", &[m.clone()], 7000);
        assert_eq!(a.len(), 1);
        assert!(a[0].contains("renamed price_for"), "{a:?}");
        assert_eq!(reached.len(), 1);
        // the same session: nothing, ever again
        assert!(hook_lines(&store, "ws:app", "kevin@x.com", "sA", &[m.clone()], 7000).0.is_empty());
        // another session of the same person: delivered once already, so no
        // "(shown to you earlier)" line about a message it never read
        let (b, reached) = hook_lines(&store, "ws:app", "kevin@x.com", "sB", &[m.clone()], 7000);
        assert!(b.is_empty(), "{b:?}");
        assert!(reached.is_empty());
        assert!(hook_lines(&store, "ws:app", "kevin@x.com", "sB", &[m], 7000).0.is_empty());
    }

    #[test]
    fn a_pushed_message_is_not_shown_again_by_the_hooks() {
        let store = store();
        file(&store, "ws:app", "kevin@x.com", "m1", 1.0);
        let visible = BTreeSet::from(["ws:app".to_string()]);
        let got = take(&store, &ask(Taker::Session, "ws:app", "s1", &visible, &[]), |m| m);
        assert_eq!(got.len(), 1);
        assert!(hook_lines(&store, "ws:app", "kevin@x.com", "s1", &got, 7000).0.is_empty());
        assert!(hook_lines(&store, "ws:app", "kevin@x.com", "s2", &got, 7000).0.is_empty());
    }

    #[test]
    fn past_the_budget_a_message_waits_unmarked_for_the_next_answer() {
        let store = store();
        let m = json!({"id": "m2", "from": "b", "message": "x", "ts": now()});
        assert!(hook_lines(&store, "ws:app", "kevin@x.com", "s1", &[m.clone()], 100).0.is_empty());
        assert!(store.eph_get("briefmsg:s1:m2").is_none());
        assert_eq!(hook_lines(&store, "ws:app", "kevin@x.com", "s1", &[m], 7000).0.len(), 1);
    }
}
