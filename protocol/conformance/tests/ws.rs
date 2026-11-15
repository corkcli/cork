//! Over real sockets: the reference server (tokio) and the blocking client
//! (what the hooks use). Two agents collide on one function and resolve.

use std::time::{Duration, Instant};

use cork_protocol::ws_client::BlockingClient;
use cork_protocol::*;
use cork_protocol_code::CodeDomain;
use serde_json::{json, Map};

const F: &str = "code:src/cli.py::parse_config";
const T: Duration = Duration::from_secs(5);

fn server(grace_ms: u64) -> (String, cork_protocol::ws_server::Handle) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            let engine = Engine::builder().grace_ms(grace_ms).domain(CodeDomain::new()).build();
            let (handle, fut) = cork_protocol::ws_server::spawn(l, engine);
            tx.send((format!("ws://{addr}"), handle)).unwrap();
            let _ = fut.await;
        });
    });
    rx.recv().unwrap()
}

fn cfg(session: &str, person: &str) -> ClientConfig {
    ClientConfig {
        session: session.into(),
        agent_id: format!("agent-{session}"),
        person: person.into(),
        scope: "acme/api".into(),
        token: None,
    }
}

#[test]
fn two_agents_collide_and_resolve_over_websockets() {
    let (url, handle) = server(5_000);
    let (mut alice, w) = BlockingClient::connect(&url, cfg("alice-1", "alice"), false, T).unwrap();
    assert_eq!(w.domains[0].name, "code");
    let (mut bob, _) = BlockingClient::connect(&url, cfg("bob-1", "bob"), false, T).unwrap();
    let sub = bob.client.subscribe(&[F], 0);
    bob.send(&sub).unwrap();
    // let the subscribe land before alice writes (one socket per client: no cross-socket order)
    let gate = bob.check(&[F], Some("modify_body"), T).unwrap();
    assert_eq!(gate.results[0].status, CheckStatus::Ok);

    let started = Instant::now();
    let now = alice.now_ms();
    let op = alice.client.op(F, "modify_body", Map::new(), Some(b"alice's body"), now);
    let a = alice.write(op, T).unwrap().unwrap();
    assert_eq!(a.version, 1);

    let now = bob.now_ms();
    let op = bob.client.op(F, "modify_body", Map::new(), Some(b"bob's body"), now);
    let r = bob.write(op, T).unwrap().unwrap_err();
    assert_eq!(r.reason, RejectReason::Conflict);
    assert_eq!(r.theirs[0].author.person, "alice");
    // bob merges locally, adopts her version, writes again
    bob.client.adopt(F, Base::at(r.current.version, r.current.hash.clone()));
    let now = bob.now_ms();
    let op = bob.client.op(F, "modify_body", Map::new(), Some(b"merged"), now);
    let b = bob.write(op, T).unwrap().unwrap();
    assert_eq!(b.version, 2);
    let elapsed = started.elapsed();
    eprintln!("write, conflict, merge-write round trips over localhost: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(1));

    // bob heard alice's delta while waiting
    let inbox = bob.drain();
    assert!(inbox.iter().any(|f| matches!(f, ServerFrame::Delta(d) if d.op.author.person == "alice")));
    let pr = alice.promote(vec![a.op_id.clone()], json!({"tests": "passed"}), T).unwrap();
    assert_eq!(pr.keys[0].promoted_version, 1);
    handle.with_engine(|e| assert_eq!(e.key_state("acme/api", F).unwrap().version, 2));
}

#[test]
fn leases_ride_the_connection() {
    let (url, handle) = server(300);
    let (mut alice, _) = BlockingClient::connect(&url, cfg("alice-2", "alice"), false, T).unwrap();
    alice.lease(&[F], &["modify_body"], Some(Intent { id: None, text: Some("refactor".into()) }), T).unwrap().unwrap();
    let (mut bob, _) = BlockingClient::connect(&url, cfg("bob-2", "bob"), false, T).unwrap();
    let denied = bob.lease(&[F], &["modify_body"], None, T).unwrap().unwrap_err();
    assert_eq!(denied.holder.session, "alice-2");
    alice.close();
    // within grace: still held; then released, and bob (a waiter) is told
    let released = loop {
        match bob.recv(Duration::from_secs(3)).unwrap() {
            Some(ServerFrame::LeaseReleased(r)) => break r,
            Some(_) => continue,
            None => panic!("no release"),
        }
    };
    assert_eq!(released.reason, ReleaseReason::Expired);
    bob.lease(&[F], &["modify_body"], None, T).unwrap().unwrap();
    handle.with_engine(|e| assert_eq!(e.sessions_in("acme/api").len(), 1));
}
