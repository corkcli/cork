//! Landing: the rebase-test-push loop, done by `cork land` instead of
//! the model. Study 5's agents spent over half their tool calls getting work
//! into origin: push, rejected, pull --rebase, rerun the tests, push, rejected
//! again, each lap a message that replays the whole context. The hook runs
//! that loop as a subprocess; the server's part is small:
//!
//! * one push turn per repo at a time (`acquire` / `release`, a lease), so
//!   concurrent agents queue instead of racing and losing;
//! * the renames that landed, so the hook can adapt the agent's own commits
//!   to a teammate's rename after it rebases onto it (`renames`);
//! * the record: a `landed` ledger row and live event, and the messages the
//!   agent did not send, as a saving.

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use crate::store::Store;

pub const LEASE_BUCKET: &str = "landlease";
/// Long enough for a rebase and the repo's tests; a hook that dies holding
/// the turn blocks nobody for longer than this.
pub const LEASE_S: f64 = 180.0;
const RENAME_TTL_S: f64 = 7.0 * 86400.0;
/// A rebase the hook did is at least the rejected push, the pull and the test
/// run the agent would have sent.
const MESSAGES_PER_REBASE: u64 = 3;
/// A file adapted to a landed rename: the read and the edit.
const MESSAGES_PER_ADAPTED_FILE: u64 = 2;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn count(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Ask for the repo's push turn. Granted when free, expired, or already held
/// by this agent; otherwise says who has it and for how long at most.
pub fn acquire(store: &Store, scope: &str, caller: &str, holder: &str, now: f64) -> Value {
    match store.kv_lease(LEASE_BUCKET, scope, holder, LEASE_S, now) {
        Ok((true, _, until)) => json!({"ok": true, "held": true, "until": until}),
        Ok((false, current, until)) => {
            let user = current.split('#').next().unwrap_or("").to_string();
            let users: BTreeSet<String> = [user.clone()].into_iter().collect();
            let who = crate::activity::identity_labels(store, caller, &users)
                .get(&user)
                .and_then(|l| l.get("label"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(user);
            json!({"ok": true, "held": false, "holder": who, "wait_s": (until - now).max(0.0)})
        }
        // a store failure never stops a push: land without a turn
        Err(_) => json!({"ok": true, "held": true, "until": now}),
    }
}

/// Renames on the lint registry, old name to new: what the hook may rewrite
/// in the agent's own commits once the rename is on origin (it checks that).
pub fn renames(store: &Store, scope: &str, now: f64) -> Map<String, Value> {
    let registry = store.kv_get("semlint", scope).unwrap_or_else(|| json!({}));
    let mut out = Map::new();
    if let Some(entries) = registry.get("renames").and_then(Value::as_object) {
        for (key, info) in entries {
            // keyed `path::symbol` since v80; the symbol is on the entry
            let old = { let symbol = text(info, "symbol"); if symbol.is_empty() { key.clone() } else { symbol } };
            let new = text(info, "new");
            let fresh = info.get("ts").and_then(Value::as_f64).unwrap_or(0.0) >= now - RENAME_TTL_S;
            if fresh && !new.is_empty() && new != old {
                out.insert(old, json!(new));
            }
        }
    }
    out
}

/// Give the turn back, and when the push landed, record it: a ledger row, a
/// live event for the dashboard, and the messages it saved.
#[allow(clippy::too_many_arguments)]
pub fn release(
    store: &Store, scope: &str, user: &str, session: &str, agent: &str, holder: &str, body: &Value, now: f64, via: &str,
) -> Value {
    let _ = store.kv_release_lease(LEASE_BUCKET, scope, holder);
    let outcome = text(body, "outcome");
    // git's own trouble, as land met it: a rebase that conflicted, pushes
    // that lost the race (the waste report's git baseline)
    if outcome == "conflict" {
        record_signal(store, scope, user, session, "merge_conflict", "land", 1, now);
    }
    let push_rejects = count(body, "push_rejects").min(20);
    if push_rejects > 0 {
        record_signal(store, scope, user, session, "push_rejected", "land", push_rejects, now);
    }
    if outcome != "landed" {
        return json!({"ok": true, "released": true});
    }
    let rebases = count(body, "rebases").min(20);
    let adapted: Vec<Value> = body.get("adapted").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().take(20).collect();
    let renamed: Vec<Value> = body.get("renames_applied").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().take(20).collect();
    let row = json!({
        "user": user, "session": session, "agent": agent,
        "commit": text(body, "commit").chars().take(40).collect::<String>(),
        "branch": text(body, "branch").chars().take(120).collect::<String>(),
        "commits": count(body, "commits").min(1000),
        "rebases": rebases,
        "over": count(body, "over").min(1000),
        "authors": body.get("authors").cloned().unwrap_or(json!([])),
        "renames_applied": renamed, "adapted": adapted,
        "tests": text(body, "tests").chars().take(200).collect::<String>(),
        "tests_ok": body.get("tests_ok").cloned().unwrap_or(Value::Null),
        "seconds": body.get("seconds").and_then(Value::as_f64).unwrap_or(0.0),
        "laps": laps_of(body),
        "push_rejects": push_rejects,
    });
    let _ = store.ledger_append(scope, "landed", &row, now);
    // the branch it landed onto moved: sessions on it elsewhere catch up
    // at their next prompt (trunk.rs)
    let target = { let target = text(body, "target"); if target.is_empty() { text(body, "branch") } else { target } };
    crate::trunk::note_landed(store, scope, &target, row["commit"].as_str().unwrap_or(""), user, now);
    // it is on origin now: no longer in flight for anyone
    crate::inflight::landed(store, scope, user, session);
    // and whoever's copy is behind what just landed hears it on their next step
    crate::freshness::announce_landing(store, scope, user, session);
    crate::events::publish(store, scope, json!({"kind": "landed", "user": user, "agent": agent, "session": session,
        "commit": row["commit"], "over": row["over"], "rebases": rebases, "renames_applied": row["renames_applied"]}), via);
    let messages = rebases * MESSAGES_PER_REBASE + adapted.len() as u64 * MESSAGES_PER_ADAPTED_FILE;
    if messages > 0 {
        crate::briefstat::record_saving(
            store, scope, "land_saved", user, agent, session, messages,
            &json!({"rebases": rebases, "files": adapted.len(), "over": row["over"]}), now,
        );
    }
    json!({"ok": true, "released": true, "recorded": true})
}

/// Each lap of a landing, as the hook timed it: how many commits it rebased
/// over, the test command (no output, at most 120 characters), how long it
/// ran, whether it passed, and how the push went. At most 20.
fn laps_of(body: &Value) -> Vec<Value> {
    body.get("laps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(20)
        .map(|lap| {
            let push = match text(lap, "push").as_str() {
                p @ ("landed" | "rejected" | "failed") => p.to_string(),
                _ => String::new(),
            };
            json!({
                "incoming": count(lap, "incoming").min(10_000),
                "test": text(lap, "test").chars().take(120).collect::<String>(),
                "seconds": lap.get("seconds").and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, 7_200.0),
                "ok": lap.get("ok").and_then(Value::as_bool),
                "timed_out": lap.get("timed_out").and_then(Value::as_bool).unwrap_or(false),
                "push": push,
            })
        })
        .collect()
}

/// What git signals the hook may send.
pub const SIGNALS: [&str; 3] = ["merge_conflict", "push_rejected", "revert"];

#[allow(clippy::too_many_arguments)]
fn record_signal(store: &Store, scope: &str, user: &str, session: &str, signal: &str, source: &str, n: u64, now: f64) {
    let _ = store.ledger_append(scope, "git_signal", &json!({
        "user": user, "session": session, "signal": signal, "source": source, "count": n,
    }), now);
}

/// A git signal the hook saw in an agent's shell: a pull, rebase or merge
/// that hit conflicts, a push rejected, a revert. A count, never the output.
pub fn signal(store: &Store, scope: &str, user: &str, session: &str, body: &Value, now: f64) -> Value {
    let kind = text(body, "signal");
    if !SIGNALS.contains(&kind.as_str()) {
        return json!({"ok": false, "reason": "signal is merge_conflict, push_rejected or revert"});
    }
    let source = if text(body, "source") == "land" { "land" } else { "shell" };
    record_signal(store, scope, user, session, &kind, source, count(body, "count").clamp(1, 50), now);
    json!({"ok": true})
}

pub const BASELINE_BUCKET: &str = "gitbaseline";
const BASELINE_FIELDS: [&str; 3] = ["commits", "reverts", "conflict_fixes"];

/// The repo's own history, week by week, as the hook counted it from `git
/// log` on its machine: commits, reverts, and commits that say they fixed a
/// merge or resolved a conflict. Counts only. Weeks are keyed by their
/// Monday; machines with different history merge by the larger count.
pub fn baseline(store: &Store, scope: &str, user: &str, body: &Value, now: f64) -> Value {
    let Some(weeks) = body.get("weeks").and_then(Value::as_object) else {
        return json!({"ok": false, "reason": "missing weeks"});
    };
    let mut record = store.kv_get(BASELINE_BUCKET, scope).unwrap_or_else(|| json!({"weeks": {}}));
    let mut taken = 0;
    for (monday, counts) in weeks.iter().take(60) {
        let Ok(day) = chrono::NaiveDate::parse_from_str(monday, "%Y-%m-%d") else { continue };
        if chrono::Datelike::weekday(&day) != chrono::Weekday::Mon {
            continue;
        }
        let slot = &mut record["weeks"][monday.as_str()];
        if !slot.is_object() {
            *slot = json!({});
        }
        for field in BASELINE_FIELDS {
            let given = count(counts, field).min(100_000);
            let have = count(slot, field);
            slot[field] = json!(given.max(have));
        }
        taken += 1;
    }
    // when Cork started recording here, kept: the ledger may be pruned
    if let Some(first) = store.ledger_first_ts(scope) {
        let kept = record.get("adopted_at").and_then(Value::as_f64).unwrap_or(f64::MAX);
        record["adopted_at"] = json!(first.min(kept));
    }
    record["updated"] = json!(now);
    record["by"] = json!(user);
    let _ = store.kv_put(BASELINE_BUCKET, scope, &record, now);
    json!({"ok": true, "weeks": taken})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_landing_keeps_each_laps_test_time_and_land_trouble_is_a_git_signal() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        release(&db, "w:r", "a@x", "1", "hook", "a@x#1", &json!({"outcome": "landed", "commit": "abc", "rebases": 2, "push_rejects": 1,
            "laps": [{"incoming": 3, "test": "cargo test --release ".repeat(20), "seconds": 42.5, "ok": true, "push": "rejected", "output": "secret"},
                     {"incoming": 1, "test": "cargo test", "seconds": 9999.0, "ok": false, "push": "bogus"}]}), 1_000.0, "test");
        release(&db, "w:r", "a@x", "1", "hook", "a@x#1", &json!({"outcome": "conflict"}), 1_001.0, "test");
        let rows = db.ledger_since("w:r", 0.0);
        let landed = &rows.iter().find(|r| r.kind == "landed").unwrap().payload;
        let laps = landed["laps"].as_array().unwrap();
        assert_eq!(laps.len(), 2);
        assert_eq!(laps[0]["test"].as_str().unwrap().chars().count(), 120);
        assert_eq!(laps[0]["seconds"], 42.5);
        assert!(laps[0].get("output").is_none(), "no output is ever kept");
        assert_eq!(laps[1]["seconds"], 7_200.0);
        assert_eq!(laps[1]["push"], "");
        let signals: Vec<(String, String, u64)> = rows.iter().filter(|r| r.kind == "git_signal")
            .map(|r| (text(&r.payload, "signal"), text(&r.payload, "source"), count(&r.payload, "count"))).collect();
        assert_eq!(signals, vec![("push_rejected".into(), "land".into(), 1), ("merge_conflict".into(), "land".into(), 1)]);
        assert_eq!(signal(&db, "w:r", "a@x", "1", &json!({"signal": "rm -rf"}), 1.0)["ok"], false);
        assert_eq!(signal(&db, "w:r", "a@x", "1", &json!({"signal": "revert", "count": 999}), 1.0)["ok"], true);
        assert_eq!(count(&db.ledger_since_kinds("w:r", 0.0, &["git_signal"]).pop().unwrap().payload, "count"), 50);
    }

    #[test]
    fn a_baseline_keeps_counts_by_monday_and_the_larger_of_two_machines() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        baseline(&db, "w:r", "a@x", &json!({"weeks": {"2026-09-07": {"commits": 40, "reverts": 2, "conflict_fixes": 1, "subject": "x"},
            "2026-09-08": {"commits": 5}, "garbage": {"commits": 1}}}), 1.0);
        let answer = baseline(&db, "w:r", "b@x", &json!({"weeks": {"2026-09-07": {"commits": 30, "reverts": 3}}}), 2.0);
        assert_eq!(answer["weeks"], 1);
        let record = db.kv_get(BASELINE_BUCKET, "w:r").unwrap();
        let weeks = record["weeks"].as_object().unwrap();
        assert_eq!(weeks.len(), 1, "a Tuesday and a non-date are dropped: {record}");
        assert_eq!(weeks["2026-09-07"], json!({"commits": 40, "reverts": 3, "conflict_fixes": 1}));
    }

    #[test]
    fn one_turn_at_a_time_and_a_dead_holder_expires() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let a = acquire(&db, "w:r", "a@x", "a@x#1", 1_000.0);
        assert_eq!(a["held"], true);
        let b = acquire(&db, "w:r", "b@x", "b@x#2", 1_001.0);
        assert_eq!(b["held"], false, "{b}");
        assert!(b["wait_s"].as_f64().unwrap() > 170.0);
        // the holder asking again renews, it does not lock itself out
        assert_eq!(acquire(&db, "w:r", "a@x", "a@x#1", 1_002.0)["held"], true);
        // another repo is another queue
        assert_eq!(acquire(&db, "w:other", "b@x", "b@x#2", 1_002.0)["held"], true);
        // released: the next agent gets it
        release(&db, "w:r", "a@x", "1", "hook", "a@x#1", &json!({"outcome": "conflict"}), 1_003.0, "test");
        assert_eq!(acquire(&db, "w:r", "b@x", "b@x#2", 1_004.0)["held"], true);
        // a holder that died: its turn lapses
        assert_eq!(acquire(&db, "w:r", "c@x", "c@x#3", 1_004.0 + LEASE_S + 1.0)["held"], true);
        // releasing a turn you do not hold takes nothing from its holder
        release(&db, "w:r", "b@x", "2", "hook", "b@x#2", &json!({}), 1_300.0, "test");
        assert_eq!(acquire(&db, "w:r", "b@x", "b@x#2", 1_301.0)["held"], false);
    }

    #[test]
    fn a_landing_is_recorded_with_what_it_saved() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        acquire(&db, "w:r", "a@x", "a@x#1", 1_000.0);
        release(&db, "w:r", "a@x", "1", "hook", "a@x#1",
                &json!({"outcome": "landed", "commit": "abc", "rebases": 2, "over": 3, "adapted": ["t.py"]}), 1_001.0, "test");
        let rows = db.ledger_since("w:r", 0.0);
        let landed = rows.iter().find(|r| r.kind == "landed").expect("a landed row");
        assert_eq!(landed.payload["over"], 3);
        let saved = rows.iter().find(|r| r.kind == "land_saved").expect("a saving");
        // what it had in flight is not in flight any more
        let _ = db.eph_set("inflight:w:r:k:1", &json!({"user": "a@x", "session": "1", "path": "p.py"}), Some(60.0));
        let _ = db.eph_set("inflight:w:r:k:2", &json!({"user": "b@x", "session": "2", "path": "q.py"}), Some(60.0));
        let _ = db.eph_set("inflightmsg:w:r:s9:x", &json!({"text": "t", "author": "a@x", "author_session": "1"}), Some(60.0));
        acquire(&db, "w:r", "a@x", "a@x#1", 1_002.0);
        release(&db, "w:r", "a@x", "1", "hook", "a@x#1", &json!({"outcome": "landed", "commit": "def"}), 1_003.0, "test");
        assert!(db.eph_get("inflight:w:r:k:1").is_none() && db.eph_get("inflightmsg:w:r:s9:x").is_none());
        assert!(db.eph_get("inflight:w:r:k:2").is_some(), "another agent's work is still in flight");
        assert_eq!(saved.payload["messages"], 2 * MESSAGES_PER_REBASE + MESSAGES_PER_ADAPTED_FILE);
    }

    #[test]
    fn renames_come_from_the_lint_registry_while_fresh() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        db.kv_put("semlint", "w:r", &json!({"renames": {
            "amount_due": {"new": "checkout_total", "by": "a@x", "ts": 1_000_000.0},
            "old_one": {"new": "new_one", "by": "a@x", "ts": 1.0},
            "shop/price.py::price": {"symbol": "price", "path": "shop/price.py", "new": "price_for", "by": "a@x", "ts": 1_000_000.0},
        }}), 1_000.0).unwrap();
        let got = renames(&db, "w:r", 1_000_100.0);
        assert_eq!(got.get("amount_due"), Some(&json!("checkout_total")));
        assert!(!got.contains_key("old_one"), "a week-old rename has had its chance");
        assert_eq!(got.get("price"), Some(&json!("price_for")), "a v80 rule is keyed by file, renamed by symbol");
    }
}
