//! The engine, end to end through frames: sequencing, every table cell,
//! rekey chains, batches, leases and grace, subscriptions over the graph,
//! promotion, idempotency, the log chain, snapshots, and a speed check.

use std::time::Instant;

use cork_protocol::*;
use cork_protocol_conformance::Harness;
use serde_json::{json, Value};

const SCOPE: &str = "repo";

fn hello(session: &str, person: &str) -> ClientFrame {
    ClientFrame::Hello(Hello {
        proto: PROTO_VERSION,
        session: session.into(),
        agent_id: format!("agent-{session}"),
        person: person.into(),
        scope: SCOPE.into(),
        token: None,
        resume: false,
    })
}

fn resume(session: &str, person: &str) -> ClientFrame {
    match hello(session, person) {
        ClientFrame::Hello(mut h) => {
            h.resume = true;
            ClientFrame::Hello(h)
        }
        _ => unreachable!(),
    }
}

fn mk_op(id: &str, key: &str, kind: &str, base: u64, public: Value) -> Op {
    Op {
        op_id: id.into(),
        key: key.into(),
        base: Base::at(base, String::new()),
        kind: kind.into(),
        public: public.as_object().cloned().unwrap_or_default(),
        payload: Some(hash::base64_encode(id.as_bytes())),
        ..Default::default()
    }
}

fn op(id: &str, key: &str, kind: &str, base: u64) -> ClientFrame {
    ClientFrame::Op(mk_op(id, key, kind, base, json!({})))
}

fn op_p(id: &str, key: &str, kind: &str, base: u64, public: Value) -> ClientFrame {
    ClientFrame::Op(mk_op(id, key, kind, base, public))
}

/// A harness with sessions `a`, `b`, `c` connected (a and b are people
/// alice and bob; c is alice's second agent).
fn three() -> Harness {
    let mut h = Harness::new(Config::default());
    for (s, p) in [("a", "alice"), ("b", "bob"), ("c", "alice")] {
        let out = h.send(s, hello(s, p), 0);
        assert!(matches!(out[0].1, ServerFrame::Welcome(_)));
    }
    h
}

fn only<'a>(out: &'a [(String, ServerFrame)], conn: &str) -> Vec<&'a ServerFrame> {
    out.iter().filter(|(c, _)| c == conn).map(|(_, f)| f).collect()
}

fn ack(out: &[(String, ServerFrame)], conn: &str) -> Ack {
    match only(out, conn).first() {
        Some(ServerFrame::Ack(a)) => a.clone(),
        other => panic!("expected an ack for {conn}, got {other:?} in {out:?}"),
    }
}

fn reject(out: &[(String, ServerFrame)], conn: &str) -> Reject {
    match only(out, conn).first() {
        Some(ServerFrame::Reject(r)) => r.clone(),
        other => panic!("expected a reject for {conn}, got {other:?} in {out:?}"),
    }
}

fn deltas(out: &[(String, ServerFrame)], conn: &str) -> Vec<Delta> {
    only(out, conn)
        .into_iter()
        .filter_map(|f| match f {
            ServerFrame::Delta(d) => Some(d.clone()),
            _ => None,
        })
        .collect()
}

const F: &str = "code:src/cli.py::parse_config";

// ------------------------------------------------------------------ sequencing

#[test]
fn per_key_versions_and_hashes() {
    let mut h = three();
    let a1 = ack(&h.send("a", op("a.1", F, "modify_body", 0), 1), "a");
    assert_eq!((a1.version, a1.seq), (1, 1));
    assert_eq!(a1.hash.len(), 64);
    let mut o = mk_op("a.2", F, "modify_body", 1, json!({}));
    o.base.hash = a1.hash.clone();
    let a2 = ack(&h.send("a", ClientFrame::Op(o), 2), "a");
    assert_eq!(a2.version, 2);
    assert_ne!(a2.hash, a1.hash);
    // another key starts at 1, independent
    let g = ack(&h.send("b", op("b.1", "code:src/cli.py::other", "modify_body", 0), 3), "b");
    assert_eq!(g.version, 1);
    let st = h.engine.key_state(SCOPE, F).unwrap();
    assert_eq!((st.version, st.hash.as_str()), (2, a2.hash.as_str()));
}

#[test]
fn base_ahead_or_wrong_hash_is_invalid() {
    let mut h = three();
    let r = reject(&h.send("a", op("a.1", F, "modify_body", 3), 1), "a");
    assert_eq!(r.reason, RejectReason::Invalid);
    ack(&h.send("a", op("a.2", F, "modify_body", 0), 1), "a");
    let mut o = mk_op("b.1", F, "modify_body", 1, json!({}));
    o.base.hash = "not-it".into();
    let r = reject(&h.send("b", ClientFrame::Op(o), 2), "b");
    assert_eq!(r.reason, RejectReason::Invalid);
    assert_eq!(r.current.version, 1);
}

#[test]
fn malformed_ops_are_invalid() {
    let mut h = three();
    for (f, why) in [
        (op("bad id!", F, "modify_body", 0), "op id"),
        (op("a.1", "zzz:x", "modify_body", 0), "no domain"),
        (op("a.2", F, "teleport", 0), "unknown kind"),
        (op("a.3", "file:Cargo.lock", "modify_body", 0), "kind on wrong prefix"),
        (op("a.4", F, "rename", 0), "missing public"),
        (op_p("a.5", F, "rename", 0, json!({"new_name": "parse_config"})), "rename onto itself"),
        (op("a.6", "code:../x.py::f", "modify_body", 0), "path leaves the repo"),
    ] {
        let r = reject(&h.send("a", f, 1), "a");
        assert_eq!(r.reason, RejectReason::Invalid, "{why}: {r:?}");
    }
    assert_eq!(h.engine.head().0, 0, "nothing logged");
}

#[test]
fn keys_are_normalized() {
    let mut h = three();
    let a = ack(&h.send("a", op("a.1", "code:./src//cli.py::parse_config", "modify_body", 0), 1), "a");
    assert_eq!(a.key, F);
}

#[test]
fn own_pipelined_ops_never_conflict() {
    let mut h = three();
    ack(&h.send("a", op("a.1", F, "modify_body", 0), 1), "a");
    let a2 = ack(&h.send("a", op("a.2", F, "modify_body", 0), 2), "a");
    assert_eq!(a2.version, 2);
    assert!(a2.rebased_over.is_empty());
}

#[test]
fn resending_an_acked_op_returns_the_same_ack() {
    let mut h = three();
    let first = ack(&h.send("a", op("a.1", F, "modify_body", 0), 1), "a");
    let again = ack(&h.send("a", op("a.1", F, "modify_body", 0), 50), "a");
    assert_eq!(first, again);
    assert_eq!(h.engine.key_state(SCOPE, F).unwrap().version, 1);
    assert_eq!(h.log.lock().unwrap().entries.len(), 1);
}

// ------------------------------------------------------------------ the tables, cell by cell

fn public_for(domain: &str, kind: &str) -> Value {
    match (domain, kind) {
        ("code", "rename") => json!({"new_name": "renamed_by_them"}),
        ("code", "move") => json!({"new_path": "src/moved.py"}),
        ("code", "signature_change") => json!({"new_signature": "(path, strict=False)"}),
        ("notes", "retitle") => json!({"new_slug": "retitled-by-them"}),
        ("notes", "link") | ("notes", "unlink") => json!({"target": "note:glossary"}),
        ("tasks", "set_status") => json!({"status": "review"}),
        ("tasks", "depend") | ("tasks", "undepend") => json!({"target": "task:eng-1"}),
        ("tasks", "merge") => json!({"into": "eng-7"}),
        _ => json!({}),
    }
}

fn mine_public(domain: &str, kind: &str) -> Value {
    match (domain, kind) {
        ("code", "rename") => json!({"new_name": "renamed_by_me"}),
        ("code", "move") => json!({"new_path": "src/mine.py"}),
        ("notes", "retitle") => json!({"new_slug": "retitled-by-me"}),
        ("tasks", "set_status") => json!({"status": "blocked"}),
        ("tasks", "merge") => json!({"into": "eng-8"}),
        _ => public_for(domain, kind),
    }
}

/// For every (mine, theirs) cell that can meet on one key: bob writes theirs,
/// then alice writes mine on the base before it. The outcome must be exactly
/// what the table says.
#[test]
fn every_table_cell_behaves_as_declared() {
    let mut checked = 0;
    for (domain, table) in [("code", cork_protocol_code::table()), ("notes", cork_protocol_notes::table()), ("tasks", cork_protocol_tasks::table())] {
        for mine in table.kinds.keys() {
            for theirs in table.kinds.keys() {
                let key = table
                    .prefixes
                    .iter()
                    .map(|p| match p.as_str() {
                        "code:" => "code:src/cli.py::parse_config",
                        "file:" => "file:config/app.toml",
                        "task:" => "task:eng-42",
                        _ => "note:release-plan",
                    })
                    .find(|k| table.allows(mine, k) && table.allows(theirs, k));
                let Some(key) = key else { continue };
                let mut h = three();
                let t = ack(&h.send("b", op_p("b.1", key, theirs, 0, public_for(domain, theirs)), 1), "b");
                let out = h.send("a", op_p("a.1", key, mine, 0, mine_public(domain, mine)), 2);
                let rel = table.relation(mine, theirs).unwrap();
                let cell = format!("{domain}: {mine} x {theirs}");
                match rel {
                    Relation::Commute => {
                        let a = ack(&out, "a");
                        assert_eq!(a.key, key, "{cell}");
                        assert_eq!(a.version, t.version + 1, "{cell}");
                        assert_eq!(a.rebased_over.len(), 1, "{cell}");
                        assert_eq!(a.rebased_over[0].op_id, "b.1", "{cell}");
                    }
                    Relation::Rekey => {
                        let a = ack(&out, "a");
                        let target = t.moved_to.as_ref().expect("theirs moved the key").key.clone();
                        assert_eq!(a.key, target, "{cell}");
                        assert_eq!(a.rekeyed_to.as_deref(), Some(target.as_str()), "{cell}");
                    }
                    Relation::Conflict => {
                        let r = reject(&out, "a");
                        assert_eq!(r.reason, RejectReason::Conflict, "{cell}");
                        assert_eq!(r.theirs[0].op_id, "b.1", "{cell}");
                        assert_eq!(r.holder.as_ref().unwrap().person, "bob", "{cell}");
                    }
                }
                checked += 1;
            }
        }
    }
    // code: 6x6 symbol kinds share code:, plus delete/move/file_replace on file: (9 more, minus overlaps)
    assert!(checked >= 36 + 36 + 81, "only {checked} cells reachable");
}

// ------------------------------------------------------------------ rekey

#[test]
fn rename_then_modify_follows_the_key() {
    let mut h = three();
    ack(&h.send("a", op("a.1", F, "modify_body", 0), 1), "a");
    let r = ack(&h.send("b", op_p("b.1", F, "rename", 1, json!({"new_name": "load_config"})), 2), "b");
    let new_key = "code:src/cli.py::load_config";
    assert_eq!(r.moved_to.as_ref().unwrap().key, new_key);
    assert_eq!(r.moved_to.as_ref().unwrap().version, 1);
    // alice still thinks it's parse_config@1
    let a = ack(&h.send("a", op("a.2", F, "modify_body", 1), 3), "a");
    assert_eq!(a.key, new_key);
    assert_eq!(a.rekeyed_to.as_deref(), Some(new_key));
    assert_eq!(a.version, 2);
    // a writer who saw the rename and still names the old key also follows it
    // c saw the rename (base 2 on the old key) but not alice's edit at the new key
    let r = reject(&h.send("c", op_p("c.1", F, "signature_change", 2, json!({"new_signature": "(path)"})), 4), "c");
    assert_eq!((r.reason, r.key.as_str(), r.theirs[0].op_id.as_str()), (RejectReason::Conflict, new_key, "a.2"));
    // with the edit in hand it lands
    let c = ack(&h.send("c", op_p("c.2", new_key, "signature_change", 2, json!({"new_signature": "(path)"})), 5), "c");
    assert_eq!((c.key.as_str(), c.version), (new_key, 3));
}

#[test]
fn rekey_chains_follow_every_hop_and_stop_at_a_conflict() {
    let mut h = three();
    ack(&h.send("a", op("a.1", F, "modify_body", 0), 1), "a");
    ack(&h.send("b", op_p("b.1", F, "rename", 1, json!({"new_name": "load_config"})), 2), "b");
    let k2 = "code:src/cli.py::load_config";
    ack(&h.send("b", op_p("b.2", k2, "move", 1, json!({"new_path": "src/config.py"})), 3), "b");
    let k3 = "code:src/config.py::load_config";
    let a = ack(&h.send("a", op("a.2", F, "modify_body", 1), 4), "a");
    assert_eq!(a.key, k3, "two hops: rename then move");
    // now someone edits the body at k3; a stale writer following the chain conflicts there
    ack(&h.send("b", op("b.3", k3, "modify_body", 2), 5), "b");
    let mut h2 = three();
    ack(&h2.send("a", op("a.1", F, "modify_body", 0), 1), "a");
    ack(&h2.send("b", op_p("b.1", F, "rename", 1, json!({"new_name": "load_config"})), 2), "b");
    ack(&h2.send("b", op("b.2", k2, "modify_body", 1), 3), "b");
    let r = reject(&h2.send("a", op("a.2", F, "modify_body", 1), 4), "a");
    assert_eq!(r.reason, RejectReason::Conflict);
    assert_eq!(r.key, k2, "the conflict is reported where it happened");
    assert_eq!(r.theirs[0].op_id, "b.2");
}

#[test]
fn a_writer_who_saw_the_move_is_still_redirected() {
    let mut h = three();
    ack(&h.send("b", op_p("b.1", F, "rename", 0, json!({"new_name": "load_config"})), 1), "b");
    let a = ack(&h.send("a", op("a.1", F, "modify_body", 1), 2), "a");
    assert_eq!(a.key, "code:src/cli.py::load_config");
}

#[test]
fn insert_after_a_rename_reuses_the_freed_name() {
    let mut h = three();
    ack(&h.send("b", op_p("b.1", F, "rename", 0, json!({"new_name": "load_config"})), 1), "b");
    let a = ack(&h.send("a", op("a.1", F, "insert", 0), 2), "a");
    assert_eq!(a.key, F);
    assert_eq!(a.version, 2);
    assert!(h.engine.key_state(SCOPE, F).unwrap().moved_to.is_none(), "the old key lives again");
}

#[test]
fn writing_onto_a_key_something_moved_into_conflicts() {
    let mut h = three();
    ack(&h.send("b", op_p("b.1", F, "rename", 0, json!({"new_name": "load_config"})), 1), "b");
    let r = reject(&h.send("a", op("a.1", "code:src/cli.py::load_config", "insert", 0), 2), "a");
    assert_eq!(r.reason, RejectReason::Conflict);
}

// ------------------------------------------------------------------ batches

#[test]
fn atomic_batch_is_all_or_nothing() {
    let mut h = three();
    let k = |s: &str| format!("code:src/cli.py::{s}");
    ack(&h.send("b", op("b.1", &k("two"), "modify_body", 0), 1), "b");
    let before = h.engine.head().0;
    let batch = ClientFrame::Batch(Batch {
        batch_id: "a.b1".into(),
        atomic: true,
        ops: vec![
            mk_op("a.1", &k("one"), "signature_change", 0, json!({"new_signature": "(x)"})),
            mk_op("a.2", &k("two"), "modify_body", 0, json!({})),
            mk_op("a.3", &k("three"), "modify_body", 0, json!({})),
        ],
    });
    let out = h.send("a", batch, 2);
    let rs: Vec<Reject> = only(&out, "a")
        .into_iter()
        .map(|f| match f {
            ServerFrame::Reject(r) => r.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(rs.len(), 3);
    assert_eq!(rs[0].op_id, "a.2");
    assert_eq!(rs[0].reason, RejectReason::Conflict);
    assert!(rs[1..].iter().all(|r| r.reason == RejectReason::BatchAborted && r.batch_id.as_deref() == Some("a.b1")));
    assert!(h.engine.key_state(SCOPE, &k("one")).is_none(), "nothing applied");
    assert!(h.engine.key_state(SCOPE, &k("three")).is_none());
    assert_eq!(h.engine.head().0, before, "nothing logged");
    assert!(deltas(&out, "b").is_empty());
}

#[test]
fn atomic_batch_lands_as_one_log_entry() {
    let mut h = three();
    h.send("b", ClientFrame::Subscribe(Subscribe { req: None, keys: vec!["code:src/cli.py::*".into()], depth: 0 }), 0);
    let ops: Vec<Op> = (0..5)
        .map(|i| mk_op(&format!("a.{i}"), &format!("code:src/cli.py::f{i}"), "rename", 0, json!({"new_name": format!("g{i}")})))
        .collect();
    let out = h.send("a", ClientFrame::Batch(Batch { batch_id: "a.rn".into(), atomic: true, ops }), 1);
    let acks: Vec<&ServerFrame> = only(&out, "a");
    assert_eq!(acks.len(), 5);
    assert!(acks.iter().all(|f| matches!(f, ServerFrame::Ack(a) if a.seq == 1 && a.batch_id.as_deref() == Some("a.rn"))));
    assert_eq!(deltas(&out, "b").len(), 5);
    let log = h.log.lock().unwrap();
    assert_eq!(log.entries.len(), 1);
    assert!(matches!(&log.entries[0].record, LogRecord::Batch { accepted, .. } if accepted.len() == 5));
    // resending the batch: the same acks, nothing new
    drop(log);
    let ops: Vec<Op> = (0..5)
        .map(|i| mk_op(&format!("a.{i}"), &format!("code:src/cli.py::f{i}"), "rename", 0, json!({"new_name": format!("g{i}")})))
        .collect();
    let again = h.send("a", ClientFrame::Batch(Batch { batch_id: "a.rn".into(), atomic: true, ops }), 2);
    assert_eq!(only(&again, "a").len(), 5);
    assert_eq!(h.log.lock().unwrap().entries.len(), 1);
}

#[test]
fn atomic_batch_may_not_touch_a_key_twice() {
    let mut h = three();
    let ops = vec![mk_op("a.1", F, "modify_body", 0, json!({})), mk_op("a.2", F, "modify_body", 0, json!({}))];
    let out = h.send("a", ClientFrame::Batch(Batch { batch_id: "a.b".into(), atomic: true, ops }), 1);
    let rs = only(&out, "a");
    assert!(matches!(rs[0], ServerFrame::Reject(r) if r.reason == RejectReason::Invalid && r.op_id == "a.2"));
    assert!(matches!(rs[1], ServerFrame::Reject(r) if r.reason == RejectReason::BatchAborted && r.op_id == "a.1"));
    assert!(h.engine.key_state(SCOPE, F).is_none());
}

#[test]
fn non_atomic_batch_decides_each_op_alone() {
    let mut h = three();
    ack(&h.send("b", op("b.1", F, "modify_body", 0), 1), "b");
    let ops = vec![mk_op("a.1", F, "modify_body", 0, json!({})), mk_op("a.2", "code:src/x.py::g", "modify_body", 0, json!({}))];
    let out = h.send("a", ClientFrame::Batch(Batch { batch_id: "a.b".into(), atomic: false, ops }), 2);
    let rs = only(&out, "a");
    assert!(matches!(rs[0], ServerFrame::Reject(r) if r.reason == RejectReason::Conflict));
    assert!(matches!(rs[1], ServerFrame::Ack(a) if a.version == 1));
}

// ------------------------------------------------------------------ leases

fn lease(keys: &[&str], kinds: &[&str], intent: &str) -> ClientFrame {
    ClientFrame::Lease(Lease {
        req: Some("L".into()),
        keys: keys.iter().map(|s| s.to_string()).collect(),
        kinds: kinds.iter().map(|s| s.to_string()).collect(),
        intent: Some(Intent { id: Some("i-42".into()), text: Some(intent.into()) }),
    })
}

#[test]
fn a_lease_blocks_conflicting_writes_and_admits_compatible_ones() {
    let mut h = three();
    let g = h.send("a", lease(&[F], &["modify_body"], "tighten validation"), 1);
    assert!(matches!(only(&g, "a")[0], ServerFrame::LeaseGranted(x) if x.keys == vec![F.to_string()] && x.req.as_deref() == Some("L")));
    let r = reject(&h.send("b", op("b.1", F, "modify_body", 0), 2), "b");
    assert_eq!(r.reason, RejectReason::Leased);
    assert_eq!(r.holder.as_ref().unwrap().session, "a");
    assert_eq!(r.their_intent.as_ref().unwrap().text.as_deref(), Some("tighten validation"));
    // the holder writes freely
    ack(&h.send("a", op("a.1", F, "modify_body", 0), 3), "a");
    // a rename composes with a body edit: allowed, and the lease follows the key
    let out = h.send("b", op_p("b.2", F, "rename", 1, json!({"new_name": "load_config"})), 4);
    ack(&out, "b");
    assert_eq!(deltas(&out, "a").len(), 1, "the holder hears about it");
    let nk = "code:src/cli.py::load_config";
    let d = h.send("c", lease(&[nk], &["modify_body"], "mine"), 5);
    assert!(matches!(only(&d, "c")[0], ServerFrame::LeaseDenied(x) if x.holder.session == "a"));
}

#[test]
fn leases_of_one_session_are_reentrant_and_compatible_leases_coexist() {
    let mut h = three();
    h.send("a", lease(&[F], &["modify_body"], "x"), 1);
    let again = h.send("a", lease(&[F], &["modify_body", "signature_change"], "x"), 2);
    assert!(matches!(only(&again, "a")[0], ServerFrame::LeaseGranted(_)));
    // rename is compatible with modify_body, signature_change (both orders)
    let r = h.send("b", lease(&[F], &["rename"], "y"), 3);
    assert!(matches!(only(&r, "b")[0], ServerFrame::LeaseGranted(_)));
}

#[test]
fn lease_survives_a_reconnect_within_grace_and_expires_after() {
    let mut h = three();
    h.send("a", lease(&[F], &["modify_body"], "refactor"), 0);
    h.disconnect("a", 100);
    // still held while away: bob is denied and becomes a waiter
    let d = h.send("b", lease(&[F], &["modify_body"], "mine"), 200);
    assert!(matches!(only(&d, "b")[0], ServerFrame::LeaseDenied(_)));
    assert_eq!(h.engine.next_deadline(), Some(5_100));
    assert!(h.tick(5_000).is_empty());
    // alice comes back with resume inside the grace
    h.connect("a", 3_000);
    let w = h.send("a", resume("a", "alice"), 3_000);
    match &w[0].1 {
        ServerFrame::Welcome(w) => {
            assert!(w.resumed);
            assert_eq!(w.resumed_leases.len(), 1);
            assert_eq!(w.resumed_leases[0].key, F);
        }
        other => panic!("{other:?}"),
    }
    // and leaves for good
    h.disconnect("a", 4_000);
    assert!(h.tick(8_999).is_empty());
    let out = h.tick(9_000);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, "b", "the waiter is told");
    assert!(matches!(&out[0].1, ServerFrame::LeaseReleased(r) if r.reason == ReleaseReason::Expired && r.keys == vec![F.to_string()]));
    ack(&h.send("b", op("b.1", F, "modify_body", 0), 9_001), "b");
    assert_eq!(h.engine.next_deadline(), None);
}

#[test]
fn hello_without_resume_starts_over_and_releases() {
    let mut h = three();
    h.send("b", ClientFrame::Subscribe(Subscribe { req: None, keys: vec![F.into()], depth: 0 }), 0);
    h.send("a", lease(&[F], &["modify_body"], "x"), 0);
    h.disconnect("a", 10);
    h.connect("a", 20);
    let out = h.send("a", hello("a", "alice"), 20);
    assert!(out.iter().any(|(c, f)| c == "b" && matches!(f, ServerFrame::LeaseReleased(r) if r.reason == ReleaseReason::Superseded)));
    assert!(matches!(&out.last().unwrap().1, ServerFrame::Welcome(w) if !w.resumed && w.resumed_leases.is_empty()));
}

#[test]
fn writing_under_a_lost_lease_is_stale_lease() {
    let mut h = three();
    h.send("a", lease(&[F], &["modify_body"], "x"), 0);
    let mut o = mk_op("a.1", F, "modify_body", 0, json!({}));
    o.leased = true;
    ack(&h.send("a", ClientFrame::Op(o.clone()), 1), "a");
    h.disconnect("a", 2);
    h.tick(10_000);
    h.connect("a", 10_001);
    h.send("a", hello("a", "alice"), 10_001);
    o.op_id = "a.2".into();
    o.base.version = 1;
    let r = reject(&h.send("a", ClientFrame::Op(o), 10_002), "a");
    assert_eq!(r.reason, RejectReason::StaleLease);
}

#[test]
fn release_tells_waiters_and_subscribers() {
    let mut h = three();
    h.send("c", ClientFrame::Subscribe(Subscribe { req: None, keys: vec![F.into()], depth: 0 }), 0);
    h.send("a", lease(&[F], &["modify_body"], "x"), 0);
    h.send("b", lease(&[F], &["modify_body"], "y"), 1);
    let out = h.send("a", ClientFrame::Release(Release { req: Some("R".into()), keys: vec![F.into()] }), 2);
    let names: Vec<&str> = out.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
    assert!(out.iter().all(|(_, f)| matches!(f, ServerFrame::LeaseReleased(r) if r.reason == ReleaseReason::Released)));
}

// ------------------------------------------------------------------ subscriptions

#[test]
fn deltas_reach_subscribers_through_the_graph_up_to_their_depth() {
    let mut h = three();
    h.send(
        "d",
        ClientFrame::Hello(Hello {
            proto: 1,
            session: "d".into(),
            agent_id: "x".into(),
            person: "dee".into(),
            scope: SCOPE.into(),
            token: None,
            resume: false,
        }),
        0,
    );
    let k = |s: &str| format!("code:src/app.py::{s}");
    // grand -> caller -> callee
    h.code_graph.add_edge(SCOPE, &k("caller"), &k("callee"));
    h.code_graph.add_edge(SCOPE, &k("grand"), &k("caller"));
    let sub = |key: String, depth| ClientFrame::Subscribe(Subscribe { req: None, keys: vec![key], depth });
    h.send("b", sub(k("grand"), 2), 0);
    h.send("c", sub(k("caller"), 0), 0);
    h.send("d", sub(k("grand"), 1), 0);
    let out = h.send("a", op("a.1", &k("callee"), "signature_change", 0), 1);
    // signature_change needs new_signature: invalid, nobody hears
    assert_eq!(reject(&out, "a").reason, RejectReason::Invalid);
    let out = h.send("a", op_p("a.2", &k("callee"), "signature_change", 0, json!({"new_signature": "(x, y)"})), 2);
    ack(&out, "a");
    let db = deltas(&out, "b");
    assert_eq!(db.len(), 1);
    assert_eq!(db[0].via.as_deref(), Some(&[k("grand"), k("caller"), k("callee")][..]));
    assert!(deltas(&out, "c").is_empty(), "depth 0 hears only its own key");
    assert!(deltas(&out, "d").is_empty(), "two hops away is past depth 1");
    assert!(deltas(&out, "a").is_empty(), "the writer gets an ack, not a delta");
    let out = h.send("a", op("a.3", &k("caller"), "modify_body", 0), 3);
    assert_eq!(deltas(&out, "c")[0].via, None);
    assert_eq!(deltas(&out, "d")[0].via.as_deref(), Some(&[k("grand"), k("caller")][..]));
    assert_eq!(deltas(&out, "b").len(), 1);
    // unsubscribe stops them
    h.send("b", ClientFrame::Unsubscribe(Unsubscribe { req: None, keys: vec![k("grand")] }), 4);
    let out = h.send("a", op("a.4", &k("callee"), "modify_body", 1), 5);
    assert!(deltas(&out, "b").is_empty());
}

#[test]
fn prefix_subscriptions_and_cross_scope_isolation() {
    let mut h = three();
    h.send("b", ClientFrame::Subscribe(Subscribe { req: None, keys: vec!["code:src/*".into()], depth: 0 }), 0);
    h.send(
        "x",
        ClientFrame::Hello(Hello {
            proto: 1,
            session: "x".into(),
            agent_id: "x".into(),
            person: "xi".into(),
            scope: "other-repo".into(),
            token: None,
            resume: false,
        }),
        0,
    );
    h.send("x", ClientFrame::Subscribe(Subscribe { req: None, keys: vec!["code:src/*".into()], depth: 0 }), 0);
    let out = h.send("a", op("a.1", F, "modify_body", 0), 1);
    assert_eq!(deltas(&out, "b").len(), 1);
    assert!(deltas(&out, "x").is_empty(), "scopes never see each other");
    let out = h.send("a", op("a.2", "code:lib/z.py::q", "modify_body", 0), 2);
    assert!(deltas(&out, "b").is_empty());
}

#[test]
fn notes_links_feed_the_graph() {
    let mut h = three();
    h.send("b", ClientFrame::Subscribe(Subscribe { req: None, keys: vec!["note:plan".into()], depth: 1 }), 0);
    ack(&h.send("a", op_p("a.1", "note:plan", "link", 0, json!({"target": "note:glossary"})), 1), "a");
    let out = h.send("a", op("a.2", "note:glossary", "append", 0), 2);
    assert_eq!(deltas(&out, "b")[0].via.as_deref(), Some(&["note:plan".to_string(), "note:glossary".to_string()][..]));
}

#[test]
fn concurrent_appends_commute() {
    let mut h = three();
    let mut last = 0;
    for (i, s) in ["a", "b", "c", "a", "b"].iter().enumerate() {
        let a = ack(&h.send(s, op(&format!("{s}.{i}"), "note:standup", "append", 0), i as u64), s);
        assert!(a.version > last);
        last = a.version;
    }
    assert_eq!(last, 5);
}

// ------------------------------------------------------------------ check, promote, messages

#[test]
fn check_answers_the_gate_question() {
    let mut h = three();
    ack(&h.send("b", op("b.1", F, "modify_body", 0), 1), "b");
    h.send("c", lease(&["code:src/cli.py::leased"], &["modify_body"], "busy"), 1);
    let check = ClientFrame::Check(Check {
        req: Some("q".into()),
        keys: vec![
            CheckKey { key: F.into(), base: Some(Base::at(1, String::new())), kind: Some("modify_body".into()) },
            CheckKey { key: F.into(), base: Some(Base::at(0, String::new())), kind: Some("modify_body".into()) },
            CheckKey { key: F.into(), base: Some(Base::at(0, String::new())), kind: Some("rename".into()) },
            CheckKey { key: "code:src/cli.py::leased".into(), base: None, kind: Some("modify_body".into()) },
            CheckKey { key: "code:src/cli.py::fresh".into(), base: None, kind: None },
        ],
    });
    let out = h.send("a", check, 2);
    let ServerFrame::CheckResult(r) = only(&out, "a")[0] else { panic!() };
    assert_eq!(r.req.as_deref(), Some("q"));
    let s: Vec<(CheckStatus, Option<Would>)> = r.results.iter().map(|e| (e.status, e.would)).collect();
    assert_eq!(
        s,
        vec![
            (CheckStatus::Ok, Some(Would::Accept)),
            (CheckStatus::Stale, Some(Would::Conflict)),
            (CheckStatus::Stale, Some(Would::Rebase)),
            (CheckStatus::Leased, Some(Would::Accept)),
            (CheckStatus::Ok, None),
        ]
    );
    assert_eq!(r.results[1].theirs[0].op_id, "b.1");
    assert_eq!(r.results[3].holder.as_ref().unwrap().session, "c");
    assert_eq!(r.results[3].intent.as_ref().unwrap().text.as_deref(), Some("busy"));
    assert_eq!(h.engine.head().0, 1, "check changes nothing");
}

/// A check without a kind is a reader's: a caller asks what its copy of a
/// callee missed, and hears the signature change, and the rename after it on
/// the key the subject moved to. Its own ops are not news to it.
#[test]
fn a_readers_check_lists_what_its_copy_missed() {
    let mut h = three();
    let k = "code:src/tax.py::compute_tax";
    ack(&h.send("b", op_p("b.1", k, "signature_change", 0, json!({"new_signature": "def compute_tax(amount, region)"})), 1), "b");
    let moved = ack(&h.send("b", op_p("b.2", k, "rename", 1, json!({"new_name": "tax_of"})), 2), "b");
    let landed = moved.moved_to.clone().expect("a rename moves the key");
    assert_eq!(landed.key, "code:src/tax.py::tax_of");
    ack(&h.send("c", op("c.1", "code:src/tax.py::tax_of", "modify_body", landed.version), 3), "c");
    let read = |h: &mut Harness, who: &str, version: u64| {
        let check = ClientFrame::Check(Check { req: None, keys: vec![CheckKey { key: k.into(), base: Some(Base::at(version, String::new())), kind: None }] });
        let out = h.send(who, check, 4);
        let ServerFrame::CheckResult(r) = only(&out, who)[0] else { panic!() };
        r.results[0].clone()
    };
    let e = read(&mut h, "a", 0);
    assert_eq!(e.status, CheckStatus::Stale);
    let kinds: Vec<(&str, &str)> = e.theirs.iter().map(|t| (t.op_id.as_str(), t.kind.as_str())).collect();
    assert_eq!(kinds, vec![("b.1", "signature_change"), ("b.2", "rename"), ("c.1", "modify_body")]);
    assert_eq!(e.rekeyed_to.as_deref(), Some("code:src/tax.py::tax_of"));
    assert!(e.would.is_none());
    // the author's own ops are not news to it; a current copy has nothing to hear
    let mine = read(&mut h, "b", 0);
    assert_eq!(mine.theirs.iter().map(|t| t.op_id.as_str()).collect::<Vec<_>>(), vec!["c.1"]);
    assert!(read(&mut h, "a", 2).theirs.is_empty());
    assert_eq!(h.engine.head().0, 3, "check changes nothing");
}

#[test]
fn promote_marks_ops_and_tells_subscribers() {
    let mut h = three();
    h.send("b", ClientFrame::Subscribe(Subscribe { req: None, keys: vec![F.into()], depth: 0 }), 0);
    ack(&h.send("a", op("a.1", F, "modify_body", 0), 1), "a");
    ack(&h.send("a", op("a.2", F, "modify_body", 1), 2), "a");
    ack(&h.send("b", op("b.1", "code:src/x.py::g", "modify_body", 0), 2), "b");
    let p = ClientFrame::Promote(Promote {
        req: Some("p".into()),
        op_ids: vec!["a.1".into(), "a.2".into(), "b.1".into(), "nope".into()],
        evidence: json!({"tests": "passed", "commit": "abc123"}),
    });
    // c is alice's other agent: it may promote alice's ops
    let out = h.send("c", p, 3);
    let ServerFrame::Promoted(pr) = only(&out, "c")[0] else { panic!("{out:?}") };
    assert_eq!(pr.op_ids, vec!["a.1", "a.2"]);
    assert_eq!(pr.unknown, vec!["b.1", "nope"], "bob's op is not alice's to promote");
    assert_eq!(pr.keys, vec![PromotedKey { key: F.into(), promoted_version: 2 }]);
    let ds = deltas(&out, "b");
    assert_eq!(ds.len(), 2);
    assert!(ds.iter().all(|d| d.layer == Layer::Promoted));
    let st = h.engine.key_state(SCOPE, F).unwrap();
    assert_eq!((st.version, st.promoted_version), (2, 2));
    assert!(st.history.iter().all(|e| e.promoted));
    assert!(matches!(&h.log.lock().unwrap().entries.last().unwrap().record, LogRecord::Promote { .. }));
}

#[test]
fn messages_route_by_anchor_and_by_name() {
    let mut h = three();
    h.send("b", lease(&[F], &["modify_body"], "x"), 0);
    let msg = |to: Option<&str>, anchor: Option<&str>| {
        ClientFrame::Message(SendMessage { req: None, to: to.map(Into::into), anchor: anchor.map(Into::into), text: "heads up".into() })
    };
    let out = h.send("a", msg(None, Some(F)), 1);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, "b");
    assert!(matches!(&out[0].1, ServerFrame::Message(m) if m.from.session == "a" && m.anchor.as_deref() == Some(F)));
    let out = h.send("b", msg(Some("alice"), None), 2);
    let to: Vec<&str> = out.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(to, vec!["a", "c"]);
    let out = h.send("a", msg(None, None), 3);
    assert_eq!(out.len(), 2, "broadcast to the scope");
    let out = h.send("a", msg(Some("nobody"), None), 4);
    assert!(matches!(&out[0].1, ServerFrame::Error(e) if e.code == "undelivered"));
}

// ------------------------------------------------------------------ hello & errors

#[test]
fn hello_rules() {
    let mut h = Harness::new(Config::default());
    let out = h.send("a", op("a.1", F, "modify_body", 0), 0);
    assert!(matches!(&out[0].1, ServerFrame::Error(e) if e.code == "hello_required" && !e.fatal));
    let mut bad = hello("a", "alice");
    if let ClientFrame::Hello(x) = &mut bad {
        x.proto = 99;
    }
    let out = h.send("a", bad, 0);
    assert!(matches!(&out[0].1, ServerFrame::Error(e) if e.code == "proto" && e.fatal));
    let out = h.send("a", hello("a", "alice"), 0);
    let ServerFrame::Welcome(w) = &out[0].1 else { panic!() };
    assert_eq!(w.domains.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), vec!["code", "notes", "tasks"]);
    assert_eq!(w.domains[1].coalesce_ms, 100);
    let out = h.send("a", hello("a", "alice"), 0);
    assert!(matches!(&out[0].1, ServerFrame::Error(e) if e.code == "already_hello"));
    let out = h.engine.handle_text(1, "{not json", 0);
    assert!(matches!(&out[0].frame, ServerFrame::Error(e) if e.code == "bad_frame"));
}

#[test]
fn a_second_connection_supersedes_the_first() {
    let mut h = three();
    h.connect("a2", 5);
    let out = h.send("a2", resume("a", "alice"), 5);
    assert!(matches!(&out[0], (c, ServerFrame::Error(e)) if c == "a" && e.code == "superseded" && e.fatal));
    assert!(matches!(&out[1], (c, ServerFrame::Welcome(w)) if c == "a2" && w.resumed));
}

#[test]
fn authorizer_decides_identity() {
    let mut e = Engine::builder()
        .domain(cork_protocol_code::CodeDomain::new())
        .authorize(|h| match h.token.as_deref() {
            Some("good") => Ok(Peer { session: String::new(), agent_id: h.agent_id.clone(), person: "verified@x".into() }),
            _ => Err("bad token".into()),
        })
        .build();
    let out = e.handle(1, hello("a", "claimed"), 0);
    assert!(matches!(&out[0].frame, ServerFrame::Error(x) if x.code == "unauthorized" && x.fatal));
    let mut ok = hello("a", "claimed");
    if let ClientFrame::Hello(x) = &mut ok {
        x.token = Some("good".into());
    }
    e.handle(2, ok, 0);
    let out = e.handle(2, op("a.1", F, "modify_body", 0), 1);
    assert!(matches!(&out[0].frame, ServerFrame::Ack(_)));
    assert_eq!(e.key_state(SCOPE, F).unwrap().history[0].author.person, "verified@x", "author comes from the session, not the client");
}

#[test]
fn a_log_failure_acks_nothing_and_changes_nothing() {
    let mut h = three();
    h.log.lock().unwrap().fail_next = 1;
    let out = h.send("a", op("a.1", F, "modify_body", 0), 1);
    assert!(matches!(&out[0].1, ServerFrame::Error(e) if e.code == "log" && e.req.as_deref() == Some("a.1")));
    assert!(h.engine.key_state(SCOPE, F).is_none());
    assert_eq!(h.engine.head().0, 0);
    let a = ack(&h.send("a", op("a.1", F, "modify_body", 0), 2), "a");
    assert_eq!((a.version, a.seq), (1, 1));
}

#[test]
fn server_stamps_are_strictly_increasing_even_against_future_clients() {
    let mut h = three();
    let mut prev = Hlc::default();
    for i in 0..200u64 {
        let mut o = mk_op(&format!("a.{i}"), &format!("code:src/m.py::f{}", i % 7), "insert", 0, json!({}));
        o.base.version = h.engine.key_state(SCOPE, &o.key).map(|s| s.version).unwrap_or(0);
        o.kind = "modify_body".into();
        o.hlc = Hlc::new(if i % 3 == 0 { 10_000 + i } else { 5 }, 9);
        let a = ack(&h.send("a", ClientFrame::Op(o), 1_000 - i), "a");
        assert!(a.hlc > prev, "{:?} after {:?}", a.hlc, prev);
        prev = a.hlc;
    }
}

// ------------------------------------------------------------------ log, snapshot, replay

#[test]
fn the_log_is_a_verifiable_hash_chain() {
    let mut h = three();
    for i in 0..20 {
        let key = format!("code:src/m.py::f{}", i % 4);
        let base = h.engine.key_state(SCOPE, &key).map(|s| s.version).unwrap_or(0);
        ack(&h.send("a", op(&format!("a.{i}"), &key, "modify_body", base), i), "a");
    }
    h.send("a", ClientFrame::Promote(Promote { req: None, op_ids: vec!["a.3".into()], evidence: json!({}) }), 30);
    let log = h.log.lock().unwrap();
    assert_eq!(log.entries.len(), 21);
    log.verify().unwrap();
    let mut tampered = log.entries.clone();
    if let LogRecord::Accept(a) = &mut tampered[5].record {
        a.op.kind = "delete".into();
    }
    assert_eq!(verify_chain(&tampered, hash::ZERO_HASH, 1), Err(ChainError::BadHash { seq: 6 }));
    let mut swapped = log.entries.clone();
    swapped.swap(3, 4);
    assert!(verify_chain(&swapped, hash::ZERO_HASH, 1).is_err());
    let mut dropped = log.entries.clone();
    dropped.remove(7);
    assert!(matches!(verify_chain(&dropped, hash::ZERO_HASH, 1), Err(ChainError::Gap { .. })));
}

#[test]
fn snapshot_plus_replay_rebuilds_the_same_state() {
    let snap_log = std::sync::Arc::new(std::sync::Mutex::new(MemLog { every: Some(10), ..Default::default() }));
    let mut e = Engine::builder().domain(cork_protocol_code::CodeDomain::new()).log(snap_log.clone()).build();
    e.handle(1, hello("a", "alice"), 0);
    for i in 0..25u64 {
        let key = format!("code:src/m.py::f{}", i % 3);
        let base = e.key_state(SCOPE, &key).map(|s| s.version).unwrap_or(0);
        let kind = if i == 12 { "rename" } else { "modify_body" };
        let f = if kind == "rename" {
            op_p(&format!("a.{i}"), &key, kind, base, json!({"new_name": "renamed"}))
        } else {
            op(&format!("a.{i}"), &key, kind, base)
        };
        let out = e.handle(1, f, i);
        assert!(matches!(out[0].frame, ServerFrame::Ack(_)), "{out:?}");
    }
    let log = snap_log.lock().unwrap();
    assert_eq!(log.snapshots.len(), 2, "every 10 entries");
    let snap = log.snapshots[1].clone();
    assert_eq!(snap.seq, 20);
    let mut restored = Engine::builder().domain(cork_protocol_code::CodeDomain::new()).snapshot(snap).build();
    restored.replay(&log.entries[20..]).unwrap();
    assert_eq!(restored.head(), e.head());
    assert_eq!(restored.snapshot().keys, e.snapshot().keys);
    // and from genesis
    let mut fresh = Engine::builder().domain(cork_protocol_code::CodeDomain::new()).build();
    fresh.replay(&log.entries).unwrap();
    assert_eq!(fresh.snapshot().keys, e.snapshot().keys);
    // a resent op after restore still gets its ack
    restored.handle(9, hello("a", "alice"), 100);
    let out = restored.handle(9, op("a.24", "code:src/m.py::f0", "modify_body", 0), 101);
    assert!(matches!(&out[0].frame, ServerFrame::Ack(a) if a.op_id == "a.24"));
}

// ------------------------------------------------------------------ speed

#[test]
fn hundred_sessions_distinct_keys_and_a_hot_key_in_milliseconds() {
    let mut e =
        Engine::builder().domain(cork_protocol_code::CodeDomain::new()).domain(cork_protocol_notes::NotesDomain::new()).build();
    let sessions = 100u64;
    for s in 0..sessions {
        e.handle(s, hello(&format!("s{s}"), &format!("p{s}")), 0);
        e.handle(s, ClientFrame::Subscribe(Subscribe { req: None, keys: vec!["note:hot".into()], depth: 0 }), 0);
    }
    let start = Instant::now();
    let (mut distinct, mut hot) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    let rounds = 20u64;
    for round in 0..rounds {
        for s in 0..sessions {
            // own key: always fresh
            let t = Instant::now();
            let k = format!("code:src/s{s}.py::f{}", round % 5);
            let base = e.key_state(SCOPE, &k).map(|x| x.version).unwrap_or(0);
            let out = e.handle(s, op(&format!("s{s}.{round}"), &k, "modify_body", base), round);
            assert!(matches!(out[0].frame, ServerFrame::Ack(_)));
            distinct += t.elapsed();
            // the hot key: everyone appends from a base 30 versions stale (all
            // commute), and every append fans out to all 100 subscribers
            let t = Instant::now();
            let out = e.handle(
                s,
                op(
                    &format!("s{s}.h{round}"),
                    "note:hot",
                    "append",
                    e.key_state(SCOPE, "note:hot").map(|x| x.version.saturating_sub(30)).unwrap_or(0),
                ),
                round,
            );
            assert!(matches!(out[0].frame, ServerFrame::Ack(_)), "{:?}", out[0]);
            assert_eq!(out.len(), sessions as usize);
            hot += t.elapsed();
        }
    }
    let el = start.elapsed();
    let n = (sessions * rounds) as f64;
    eprintln!(
        "{sessions} sessions x {rounds} rounds in {el:?}: distinct keys {:.1} us/op; hot key (30-op rebase + {}-way fan-out) {:.1} us/op",
        distinct.as_secs_f64() * 1e6 / n,
        sessions - 1,
        hot.as_secs_f64() * 1e6 / n
    );
    assert_eq!(e.key_state(SCOPE, "note:hot").unwrap().version, sessions * 20);
    // generous for debug builds on a busy CI box; release is ~10x faster
    assert!(el.as_millis() < 5_000, "too slow: {el:?}");
}

// ------------------------------------------------------------------ content-hash bases

fn content_op(id: &str, key: &str, base_content: &str, after: &str) -> ClientFrame {
    let mut o = mk_op(id, key, "modify_body", 0, json!({}));
    o.base = Base::of_content(base_content);
    o.content_hash = Some(after.into());
    ClientFrame::Op(o)
}

#[test]
fn content_bases_map_to_the_version_that_produced_them() {
    let mut h = three();
    // alice edits from the content on disk (H0) to H1: the first op records H0 as version 0
    let a = ack(&h.send("a", content_op("a.1", F, "H0", "H1"), 1), "a");
    assert_eq!(a.version, 1);
    assert_eq!(h.engine.key_state(SCOPE, F).unwrap().content0.as_deref(), Some("H0"));
    // bob still has H0: that is version 0, so he conflicts with alice's edit
    let r = reject(&h.send("b", content_op("b.1", F, "H0", "H2"), 2), "b");
    assert_eq!((r.reason, r.theirs[0].op_id.as_str()), (RejectReason::Conflict, "a.1"));
    // carol pulled alice's change (H1): that is version 1, so she lands
    let c = ack(&h.send("c", content_op("c.1", F, "H1", "H3"), 3), "c");
    assert_eq!(c.version, 2);
    // the same content named the other way, {version: 0, hash: C}, means the same
    let mut o = mk_op("b.2", F, "modify_body", 0, json!({}));
    o.base.hash = "H0".into();
    assert_eq!(reject(&h.send("b", ClientFrame::Op(o), 3), "b").reason, RejectReason::Conflict);
    // content nobody reported is unknown_base
    let r = reject(&h.send("b", content_op("b.3", F, "edited-by-hand", "H9"), 3), "b");
    assert_eq!((r.reason, r.theirs.len()), (RejectReason::UnknownBase, 2));
    // a chain hash, when given, wins over content
    let mut o = mk_op("a.2", F, "modify_body", 2, json!({}));
    o.base = Base { version: 2, hash: c.hash.clone(), content: Some("nonsense".into()) };
    assert_eq!(ack(&h.send("a", ClientFrame::Op(o), 4), "a").version, 3);
}

#[test]
fn check_reports_unknown_content_bases() {
    let mut h = three();
    ack(&h.send("a", content_op("a.1", F, "H0", "H1"), 1), "a");
    let ck =
        |key: &str, content: &str| CheckKey { key: key.into(), base: Some(Base::of_content(content)), kind: Some("modify_body".into()) };
    let out = h.send(
        "b",
        ClientFrame::Check(Check {
            req: None,
            keys: vec![ck(F, "H1"), ck(F, "H0"), ck(F, "never-seen"), ck("code:src/cli.py::fresh", "anything")],
        }),
        2,
    );
    let ServerFrame::CheckResult(r) = only(&out, "b")[0] else { panic!() };
    assert_eq!(r.results[2].theirs[0].op_id, "a.1", "unknown_base lists the drafts the copy may lack");
    let s: Vec<(CheckStatus, Option<Would>)> = r.results.iter().map(|e| (e.status, e.would)).collect();
    assert_eq!(
        s,
        vec![
            (CheckStatus::Ok, Some(Would::Accept)),
            (CheckStatus::Stale, Some(Would::Conflict)),
            (CheckStatus::UnknownBase, None),
            (CheckStatus::Ok, Some(Would::Accept)),
        ]
    );
}

// ------------------------------------------------------------------ the same result, and landed history

fn result_op(id: &str, key: &str, kind: &str, base: u64, after: &str) -> ClientFrame {
    let mut o = mk_op(id, key, kind, base, json!({}));
    o.content_hash = Some(after.into());
    ClientFrame::Op(o)
}

fn subscribe(keys: &[&str]) -> ClientFrame {
    ClientFrame::Subscribe(Subscribe { req: None, keys: keys.iter().map(|k| k.to_string()).collect(), depth: 0 })
}

#[test]
fn the_same_result_twice_is_acked_never_a_conflict() {
    let mut h = three();
    h.send("a", subscribe(&[F]), 0);
    let first = ack(&h.send("a", result_op("a.1", F, "insert", 0, "H1"), 1), "a");
    // bob inserts the very same function: nothing to apply, nobody to tell
    let out = h.send("b", result_op("b.1", F, "insert", 0, "H1"), 2);
    let same = ack(&out, "b");
    assert_eq!((same.version, same.hash.as_str(), same.same_as.as_deref()), (1, first.hash.as_str(), Some("a.1")));
    assert!(deltas(&out, "a").is_empty(), "no news");
    assert_eq!(h.engine.key_state(SCOPE, F).unwrap().version, 1);
    // a body edit to the same content, from a stale base, is the same
    ack(&h.send("a", result_op("a.2", F, "modify_body", 1, "H2"), 3), "a");
    assert_eq!(ack(&h.send("b", result_op("b.2", F, "modify_body", 1, "H2"), 4), "b").same_as.as_deref(), Some("a.2"));
    // a different result still conflicts, and so does a writer's return to
    // an older version: for a session only the head counts
    assert_eq!(reject(&h.send("b", result_op("b.3", F, "insert", 0, "H9"), 5), "b").reason, RejectReason::Conflict);
    assert_eq!(reject(&h.send("b", result_op("b.4", F, "modify_body", 1, "H1"), 5), "b").reason, RejectReason::Conflict);
    // the ack is not kept: there is nothing of bob's to promote
    let out = h.send("b", ClientFrame::Promote(Promote { req: None, op_ids: vec!["b.2".into()], evidence: json!({}) }), 6);
    let ServerFrame::Promoted(p) = only(&out, "b")[0] else { panic!("{out:?}") };
    assert_eq!(p.unknown, vec!["b.2"]);
    // in batches too: acked with the batch, beside what does apply
    let batch = |id: &str, atomic: bool, ops: Vec<ClientFrame>| {
        let ops = ops.into_iter().map(|f| if let ClientFrame::Op(o) = f { o } else { unreachable!() }).collect();
        ClientFrame::Batch(Batch { batch_id: id.into(), atomic, ops })
    };
    let g = "code:src/cli.py::other";
    for (atomic, n) in [(true, "x"), (false, "y")] {
        let out = h.send("b", batch(&format!("b.{n}"), atomic, vec![result_op(&format!("b.{n}1"), F, "modify_body", 2, "H2"),
            result_op(&format!("b.{n}2"), &format!("{g}{n}"), "modify_body", 0, "G1")]), 7);
        let acks: Vec<&Ack> = only(&out, "b").into_iter().filter_map(|f| if let ServerFrame::Ack(a) = f { Some(a) } else { None }).collect();
        assert_eq!(acks.len(), 2, "{out:?}");
        assert!(acks.iter().all(|a| a.batch_id.as_deref() == Some(format!("b.{n}").as_str())));
        assert_eq!(acks.iter().filter(|a| a.same_as.is_some()).count(), 1);
    }
    assert_eq!(h.engine.key_state(SCOPE, F).unwrap().version, 2, "nothing applied to F");
}

#[test]
fn landed_history_goes_over_drafts_promoted_and_is_never_a_conflict() {
    let mut h = three();
    h.send("a", subscribe(&[F]), 0);
    // alice's draft from the content on disk
    ack(&h.send("a", content_op("a.1", F, "H0", "H1"), 1), "a");
    // a commit landed upstream changed the same function another way: it
    // was a conflict; it is the truth now, promoted, and alice hears it
    let out = h.send("b", content_op("git.abc.0", F, "H0", "H5"), 2);
    let landed = ack(&out, "b");
    assert_eq!((landed.version, landed.same_as.as_deref()), (2, None));
    let told = deltas(&out, "a");
    assert_eq!((told.len(), told[0].layer), (1, Layer::Promoted));
    let st = h.engine.key_state(SCOPE, F).unwrap();
    assert_eq!(st.promoted_version, 2);
    assert!(st.history.back().unwrap().promoted && !st.history.front().unwrap().promoted);
    // alice writing on her older draft meets it the normal way: stale
    let r = reject(&h.send("a", content_op("a.2", F, "H1", "H6"), 3), "a");
    assert_eq!((r.reason, r.theirs[0].op_id.as_str()), (RejectReason::Conflict, "git.abc.0"));
    // content the key already went through is acked: the head descends from it
    ack(&h.send("a", content_op("a.3", F, "H5", "H7"), 4), "a");
    assert_eq!(ack(&h.send("b", content_op("git.def.0", F, "H0", "H5"), 5), "b").same_as.as_deref(), Some("git.abc.0"));
    // a lease, or a base the server never saw, does not hold history back
    h.send("a", lease(&[F], &["modify_body"], "mine"), 6);
    assert_eq!(ack(&h.send("b", content_op("git.fed.0", F, "edited-by-hand", "H8"), 7), "b").version, 4);
    // and rebuilt from the log alone, the same state
    let entries = h.log.lock().unwrap().entries.clone();
    let mut fresh = Engine::builder().domain(cork_protocol_code::CodeDomain::new()).build();
    fresh.replay(&entries).unwrap();
    assert_eq!(fresh.snapshot().keys, h.engine.snapshot().keys);
    assert!(is_landed("git.abc.0") && !is_landed("a.1") && !is_landed("digit.1"));
}

// ------------------------------------------------------------------ base by hash, integrator reads

#[test]
fn a_base_named_by_content_hash_survives_replay() {
    let mut h = three();
    let mut o = mk_op("a.1", F, "modify_body", 0, json!({}));
    o.base.hash = "disk-0".into();
    o.content_hash = Some("disk-1".into());
    assert_eq!(ack(&h.send("a", ClientFrame::Op(o), 1), "a").version, 1);
    // rebuilt from the log alone, the content hash still names version 1
    let entries = h.log.lock().unwrap().entries.clone();
    let mut fresh = Engine::builder().domain(cork_protocol_code::CodeDomain::new()).build();
    fresh.replay(&entries).unwrap();
    let st = fresh.key_state(SCOPE, F).unwrap();
    assert_eq!(st.version_of_hash("disk-1"), Some(1));
    assert_eq!(st.version_of_hash("never-seen"), None);
    // and an unknown hash is unknown_base, listing the others' drafts
    let mut o = mk_op("b.1", F, "modify_body", 0, json!({}));
    o.base.hash = "edited-by-hand".into();
    let r = reject(&h.send("b", ClientFrame::Op(o), 2), "b");
    assert_eq!(r.reason, RejectReason::UnknownBase);
    assert_eq!(r.theirs.iter().map(|t| t.op_id.as_str()).collect::<Vec<_>>(), vec!["a.1"]);
}

#[test]
fn integrators_read_leases_keys_and_deliver_messages() {
    let mut h = three();
    let lease = ClientFrame::Lease(Lease {
        req: None,
        keys: vec![F.into()],
        kinds: vec!["modify_body".into()],
        intent: Some(Intent { id: None, text: Some("tighten".into()) }),
    });
    assert!(matches!(only(&h.send("a", lease, 1), "a")[0], ServerFrame::LeaseGranted(_)));
    ack(&h.send("a", op("a.1", F, "modify_body", 0), 2), "a");
    let leases = h.engine.leases_in(SCOPE);
    assert_eq!(leases.len(), 1);
    assert_eq!((leases[0].key.as_str(), leases[0].holder.session.as_str()), (F, "a"));
    assert_eq!(
        h.engine.keys_in(SCOPE),
        vec![(F.to_string(), Head { version: 1, hash: h.engine.key_state(SCOPE, F).unwrap().hash.clone(), promoted_version: 0 })]
    );
    assert!(h.engine.keys_in("elsewhere").is_empty());
    let (scope, peer, attached) = h.engine.session_info("b").unwrap();
    assert_eq!((scope.as_str(), peer.person.as_str(), attached), (SCOPE, "bob", true));
    // a message from outside any session reaches one attached session only
    let from = Peer { session: String::new(), agent_id: "dashboard".into(), person: "carol".into() };
    let outs = h.engine.deliver_message(SCOPE, "b", from.clone(), "look at parse_config".into(), 3).unwrap();
    assert_eq!(outs.len(), 1);
    assert!(matches!(&outs[0].frame, ServerFrame::Message(m) if m.text == "look at parse_config" && m.from.person == "carol"));
    assert!(h.engine.deliver_message("elsewhere", "b", from.clone(), "x".into(), 3).is_none());
    h.disconnect("b", 4);
    assert!(h.engine.deliver_message(SCOPE, "b", from, "x".into(), 5).is_none());
}
