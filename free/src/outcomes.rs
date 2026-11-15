//! What an agent did after Cork stopped it.
//!
//! Every block or hold opens a pending outcome for that agent (person and
//! session) and those paths; what the session sends next settles it, once,
//! as a `block_outcome` ledger row:
//!
//! - `adapted`: it wrote the path again with the teammate's change in its
//!   copy. For a teammate's rename, removal or signature change: the symbols
//!   it wrote reflect that change ([`crate::gate::reflected`]). Otherwise
//!   (a stale copy, a protocol reject): it read the file again after the
//!   block and before that write. For a numbered file: it wrote the number
//!   it was moved to.
//! - `overrode`: it wrote the path anyway without that (a gate that failed
//!   open, a write from the shell, a retry on the old copy); for a lease, it
//!   wrote while the holder was still editing the file; for a numbered file,
//!   it wrote the number it was moved off.
//! - `waited`: a plan_work hold was released and the session then wrote one
//!   of the held files; a lease it hit was gone (the holder no longer
//!   editing the file) when it wrote.
//! - for a hold on an owner's standing decision (`decision_hold`): a write
//!   of the path after it is `overrode` (the agent went ahead), none is
//!   `abandoned` (it left that code alone).
//! - `abandoned`: no write to those paths by that session within
//!   [`SETTLE_AFTER_S`], or the session ended first.
//!
//! One pending outcome per agent, kind and set of paths: a block retried
//! three times is one outcome, settled by what finally happened.

use serde_json::{json, Value};

use crate::store::{now, Store};

pub const BUCKET: &str = "outcome_pending";
/// No write this long after a block or hold: abandoned.
pub const SETTLE_AFTER_S: f64 = 1800.0;
/// A protocol reject and the write it refused arrive in either order: a
/// write this soon after the reject is that same write, not the next one.
const PROTOCOL_GRACE_S: f64 = 10.0;
/// When the waste report's newer counters (stale-copy blocks, every
/// protocol reject, outcomes) started being written: weeks before it are
/// undercounted.
pub const INSTRUMENTED_BUCKET: &str = "instrumented";
pub const INSTRUMENTED_KEY: &str = "waste_v2";

pub const OUTCOMES: [&str; 4] = ["adapted", "waited", "overrode", "abandoned"];

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn num(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Record, once per server, when these counters started.
pub fn mark_instrumented(store: &Store) {
    let stamp = now();
    let _ = store.kv_put_if_absent(INSTRUMENTED_BUCKET, INSTRUMENTED_KEY, &json!({"ts": stamp}), stamp);
}

pub fn instrumented_at(store: &Store) -> Option<f64> {
    store.kv_get(INSTRUMENTED_BUCKET, INSTRUMENTED_KEY).and_then(|v| v.get("ts").and_then(Value::as_f64))
}

fn agent_prefix(scope: &str, user: &str, session: &str) -> String {
    format!("{scope}|{}|", crate::presence::agent_id(user, session))
}

/// One agent's pending outcome of one kind on one set of paths.
fn pending_key(scope: &str, kind: &str, user: &str, session: &str, paths: &[String]) -> String {
    let mut sorted: Vec<String> = paths.to_vec();
    sorted.sort();
    format!(
        "{}{kind}|{}",
        agent_prefix(scope, user, session),
        crate::hashing::sha256_hex(&sorted.join("\n")).get(..16).unwrap_or("")
    )
}

/// Forget a pending outcome that was never a block (a commit's ops a
/// server refused before it took landed history as the truth): nothing is
/// settled or written. Whether there was one.
pub fn drop_pending(store: &Store, scope: &str, kind: &str, user: &str, session: &str, paths: &[String]) -> bool {
    let key = pending_key(scope, kind, user, session, paths);
    store.kv_get(BUCKET, &key).is_some() && store.kv_delete(BUCKET, &key).is_ok()
}

/// Open (or join) the pending outcome of a block or hold. `kind` is the
/// waste report's part (`stale_copy`, `teammate_change`, `protocol_stale`,
/// `protocol_unknown_base`, `protocol_stale_lease`, `protocol_check`,
/// `protocol_lease`, `plan_hold`, `number_claim`); `detail` is what settling
/// it needs (the blocking events, the holder, the number moved to).
#[allow(clippy::too_many_arguments)]
pub fn open(store: &Store, scope: &str, kind: &str, user: &str, session: &str, paths: &[String], detail: Value, ts: f64) {
    if session.is_empty() || paths.is_empty() {
        return;
    }
    let key = pending_key(scope, kind, user, session, paths);
    let mut record = match store.kv_get(BUCKET, &key) {
        Some(mut existing) => {
            existing["blocks"] = json!(existing.get("blocks").and_then(Value::as_u64).unwrap_or(1) + 1);
            existing["last"] = json!(ts);
            // a retried block may name more of the teammate's changes
            if let (Some(have), Some(more)) = (
                existing.get_mut("events").and_then(Value::as_array_mut),
                detail.get("events").and_then(Value::as_array),
            ) {
                for event in more {
                    if !have.contains(event) {
                        have.push(event.clone());
                    }
                }
            }
            existing
        }
        None => json!({"kind": kind, "user": user, "session": session, "paths": paths, "opened": ts, "last": ts, "blocks": 1}),
    };
    if let (Some(map), Some(extra)) = (record.as_object_mut(), detail.as_object()) {
        for (k, v) in extra {
            if k != "events" || !map.contains_key("events") {
                map.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    let _ = store.kv_put(BUCKET, &key, &record, ts);
    sweep(store, scope, ts);
}

fn settle(store: &Store, scope: &str, key: &str, record: &Value, outcome: &str, by: &str, at: f64) {
    let _ = store.kv_delete(BUCKET, key);
    let _ = store.ledger_append(scope, "block_outcome", &json!({
        "kind": text(record, "kind"), "outcome": outcome, "settled_by": by,
        "user": text(record, "user"), "session": text(record, "session"),
        "paths": record.get("paths").cloned().unwrap_or(json!([])),
        "opened": num(record, "opened"), "blocks": record.get("blocks").cloned().unwrap_or(json!(1)),
        "settled_at": at,
    }), now());
}

/// When this session last read `path` (its copy's fingerprint), read before
/// a report replaces it with the write's own.
pub fn copy_seen_at(store: &Store, scope: &str, user: &str, session: &str, path: &str) -> f64 {
    crate::freshness::copy_seen_at(store, scope, user, session, path).unwrap_or(0.0)
}

/// Whether the lease holder is still editing `path` (its focus marker).
fn still_editing(store: &Store, scope: &str, holder: &str, holder_session: &str, path: &str) -> bool {
    if holder.is_empty() {
        return false;
    }
    store
        .eph_get(&crate::presence::focus_key(scope, holder, holder_session))
        .is_some_and(|f| text(&f, "action") == "editing" && text(&f, "path") == path)
}

/// A write of `path` by this session: settles every pending outcome of the
/// session that names it. `copy_before` is [`copy_seen_at`] from before the
/// report; `symbols` the written file's symbols when it parsed.
pub fn on_write(
    store: &Store, scope: &str, user: &str, session: &str, path: &str, copy_before: f64,
    symbols: Option<&std::collections::BTreeMap<String, String>>,
) {
    if session.is_empty() {
        return;
    }
    let stamp = now();
    for (key, record) in store.kv_list(BUCKET, &agent_prefix(scope, user, session)) {
        let paths: Vec<String> = record.get("paths").and_then(Value::as_array).into_iter().flatten()
            .filter_map(Value::as_str).map(str::to_string).collect();
        if !paths.iter().any(|p| p == path) {
            continue;
        }
        let kind = text(&record, "kind");
        let opened = num(&record, "opened");
        let last = num(&record, "last").max(opened);
        let reread = copy_before > last;
        let outcome = match kind.as_str() {
            "teammate_change" => {
                let events: Vec<Value> = record.get("events").and_then(Value::as_array).cloned().unwrap_or_default();
                match symbols {
                    Some(symbols) if !events.is_empty() => {
                        if events.iter().all(|e| crate::gate::reflected(e, symbols)) { "adapted" } else { "overrode" }
                    }
                    _ => if reread { "adapted" } else { "overrode" },
                }
            }
            "stale_copy" => if reread { "adapted" } else { "overrode" },
            "number_claim" => {
                // [asked for, moved to]
                if paths.get(1).map(String::as_str) == Some(path) { "adapted" } else { "overrode" }
            }
            "plan_hold" => "waited",
            // held once on an owner's standing decision: a write there after
            // the hold is the agent going ahead anyway (decisions.rs)
            "decision_hold" => "overrode",
            "protocol_lease" => {
                if stamp - opened < PROTOCOL_GRACE_S {
                    continue;
                }
                if still_editing(store, scope, &text(&record, "holder"), &text(&record, "holder_session"), path) { "overrode" } else { "waited" }
            }
            k if k.starts_with("protocol_") => {
                if stamp - opened < PROTOCOL_GRACE_S {
                    continue;
                }
                if reread { "adapted" } else { "overrode" }
            }
            _ => continue,
        };
        settle(store, scope, &key, &record, outcome, "write", stamp);
    }
}

/// The session ended: what it never wrote, it abandoned.
pub fn session_ended(store: &Store, scope: &str, user: &str, session: &str) {
    let stamp = now();
    for (key, record) in store.kv_list(BUCKET, &agent_prefix(scope, user, session)) {
        settle(store, scope, &key, &record, "abandoned", "session_end", stamp);
    }
}

/// Every pending outcome of the repo past [`SETTLE_AFTER_S`] with no write:
/// abandoned. Run when a block opens and before the waste report reads.
pub fn sweep(store: &Store, scope: &str, at: f64) -> usize {
    let mut settled = 0;
    for (key, record) in store.kv_list(BUCKET, &format!("{scope}|")) {
        let last = num(&record, "last").max(num(&record, "opened"));
        if at - last >= SETTLE_AFTER_S {
            settle(store, scope, &key, &record, "abandoned", "timeout", last + SETTLE_AFTER_S);
            settled += 1;
        }
    }
    settled
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const S: &str = "w:org/app";

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("store")
    }

    fn outcomes(db: &Store) -> Vec<(String, String)> {
        db.ledger_since_kinds(S, 0.0, &["block_outcome"])
            .into_iter()
            .map(|r| (text(&r.payload, "kind"), text(&r.payload, "outcome")))
            .collect()
    }

    fn sigs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_write_that_carries_the_teammates_rename_adapted_one_that_does_not_overrode() {
        let db = store();
        let t = now();
        let rename = json!({"kind": "renamed", "symbol": "total", "new_name": "sum_prices"});
        open(&db, S, "teammate_change", "a@x", "s1", &["calc.py".into()], json!({"events": [rename]}), t);
        // blocked again on retry: still one outcome
        open(&db, S, "teammate_change", "a@x", "s1", &["calc.py".into()], json!({"events": [rename]}), t + 1.0);
        open(&db, S, "teammate_change", "b@x", "s2", &["calc.py".into()], json!({"events": [rename]}), t);
        // another file of a's is not settled by this write
        open(&db, S, "stale_copy", "a@x", "s1", &["other.py".into()], json!({}), t);
        on_write(&db, S, "a@x", "s1", "calc.py", 0.0, Some(&sigs(&[("sum_prices", "def sum_prices(p)")])));
        on_write(&db, S, "b@x", "s2", "calc.py", 0.0, Some(&sigs(&[("total", "def total(p)")])));
        let got = outcomes(&db);
        assert_eq!(got, vec![("teammate_change".into(), "adapted".into()), ("teammate_change".into(), "overrode".into())]);
        let row = &db.ledger_since_kinds(S, 0.0, &["block_outcome"])[0].payload;
        assert_eq!(row["blocks"], 2);
        assert_eq!(db.kv_list(BUCKET, &format!("{S}|")).len(), 1, "other.py still pending");
    }

    #[test]
    fn a_stale_copy_adapted_only_when_read_again_after_the_block() {
        let db = store();
        let t = now() - 100.0;
        open(&db, S, "stale_copy", "a@x", "s1", &["x.py".into()], json!({}), t);
        open(&db, S, "stale_copy", "b@x", "s2", &["x.py".into()], json!({}), t);
        on_write(&db, S, "a@x", "s1", "x.py", t + 5.0, None);
        on_write(&db, S, "b@x", "s2", "x.py", t - 5.0, None);
        assert_eq!(outcomes(&db), vec![("stale_copy".into(), "adapted".into()), ("stale_copy".into(), "overrode".into())]);
    }

    #[test]
    fn holds_wait_claims_follow_the_number_and_silence_is_abandoned() {
        let db = store();
        let t = now() - 100.0;
        open(&db, S, "plan_hold", "a@x", "s1", &["a.py".into(), "b.py".into()], json!({}), t);
        open(&db, S, "number_claim", "a@x", "s1", &["db/006_x.sql".into(), "db/007_x.sql".into()], json!({}), t);
        open(&db, S, "number_claim", "b@x", "s2", &["db/006_y.sql".into(), "db/008_y.sql".into()], json!({}), t);
        open(&db, S, "stale_copy", "c@x", "s3", &["z.py".into()], json!({}), t);
        open(&db, S, "stale_copy", "d@x", "s4", &["z.py".into()], json!({}), t - SETTLE_AFTER_S - 1.0);
        on_write(&db, S, "a@x", "s1", "b.py", 0.0, None);
        on_write(&db, S, "a@x", "s1", "db/007_x.sql", 0.0, None);
        on_write(&db, S, "b@x", "s2", "db/006_y.sql", 0.0, None);
        session_ended(&db, S, "c@x", "s3");
        // d's block is past the window: the next sweep settles it
        assert_eq!(sweep(&db, S, now()), 1);
        let want: Vec<(String, String)> = [
            ("plan_hold", "waited"), ("number_claim", "adapted"), ("number_claim", "overrode"),
            ("stale_copy", "abandoned"), ("stale_copy", "abandoned"),
        ].iter().map(|(k, o)| (k.to_string(), o.to_string())).collect();
        assert_eq!(outcomes(&db), want);
        let swept = db.ledger_since_kinds(S, 0.0, &["block_outcome"]).pop().unwrap().payload;
        assert_eq!(swept["settled_by"], "timeout");
        assert!(db.kv_list(BUCKET, &format!("{S}|")).is_empty());
    }

    #[test]
    fn a_protocol_reject_waits_out_its_own_write_and_a_lease_checks_the_holder() {
        let db = store();
        let t = now();
        open(&db, S, "protocol_stale", "a@x", "s1", &["p.py".into()], json!({}), t);
        // the write the reject refused, arriving just after it: not the outcome
        on_write(&db, S, "a@x", "s1", "p.py", 0.0, None);
        assert!(outcomes(&db).is_empty());
        let old = t - 60.0;
        open(&db, S, "protocol_lease", "b@x", "s2", &["q.py".into()], json!({"holder": "h@x", "holder_session": "hs"}), old);
        open(&db, S, "protocol_lease", "c@x", "s3", &["r.py".into()], json!({"holder": "h@x", "holder_session": "hs"}), old);
        db.eph_set(&crate::presence::focus_key(S, "h@x", "hs"), &json!({"action": "editing", "path": "q.py"}), Some(60.0)).unwrap();
        on_write(&db, S, "b@x", "s2", "q.py", 0.0, None);
        on_write(&db, S, "c@x", "s3", "r.py", 0.0, None);
        assert_eq!(outcomes(&db), vec![("protocol_lease".into(), "overrode".into()), ("protocol_lease".into(), "waited".into())]);
    }
}
