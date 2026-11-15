//! The hooks against the protocol's reference server (server/protocol):
//! two sessions in two checkouts of one repo. Alice works through her
//! session's daemon, Bob through the hooks' own short-lived connections.
//!
//! - Alice edits compute_tax; Bob, whose copy is older, is stopped at the
//!   gate with her change and her name.
//! - Alice renames rate_for; Bob's body edit to rate_for still goes in, onto
//!   the new name (the rename rekeys it), and Alice hears about it live.
//! - Alice's push promotes her drafts.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cork_hooks::config::{self, Env};
use cork_hooks::{proto, protod};
use serde_json::{json, Value};

const TAX: &str = "def compute_tax(amount, rate):\n    return round(amount * rate, 2)\n\n\ndef rate_for(region):\n    return 0.2\n";

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

/// Where this machine's cargo puts a crate's builds: a shared target dir
/// moves it out of the crate.
fn target_dir_of(crate_dir: &Path) -> PathBuf {
    let meta = Command::new("cargo").args(["metadata", "--format-version", "1", "--no-deps"]).current_dir(crate_dir).output().expect("cargo runs");
    serde_json::from_slice::<serde_json::Value>(&meta.stdout)
        .ok()
        .and_then(|v| v["target_directory"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| crate_dir.join("target"))
}

fn reference_server() -> (Server, u16) {
    let protocol = Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol");
    let built = Command::new("cargo")
        .args(["build", "--release", "-q", "-p", "cork-protocol-server"])
        .current_dir(&protocol)
        .status()
        .expect("cargo runs");
    assert!(built.success(), "the reference server builds");
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let child = Command::new(target_dir_of(&protocol).join("release/cork-protocol-server"))
        .args(["--addr", &format!("127.0.0.1:{port}"), "--grace-ms", "5000"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the reference server starts");
    let started = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(started.elapsed() < Duration::from_secs(10), "the reference server listens");
        std::thread::sleep(Duration::from_millis(20));
    }
    (Server(child), port)
}

fn sh(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
    assert!(ok.success(), "git {args:?}");
}

fn checkout(base: &Path, name: &str, email: &str) -> PathBuf {
    let dir = base.join(name);
    std::fs::create_dir_all(dir.join(".collide")).unwrap();
    std::fs::write(dir.join(".collide/config.json"), "{}").unwrap();
    std::fs::write(dir.join("tax.py"), TAX).unwrap();
    sh(&dir, &["init", "-q"]);
    sh(&dir, &["config", "user.email", email]);
    sh(&dir, &["config", "user.name", name]);
    dir
}

fn edit(root: &Path, old: &str, new: &str) -> Value {
    json!({"file_path": root.join("tax.py").to_string_lossy(), "old_string": old, "new_string": new})
}

/// A write as the hooks see it: the gate, then (if allowed) the edit, then
/// the report. Returns the gate's block reason, or the report's notice.
fn write(ctx: &proto::Ctx, input: &Value) -> Result<String, String> {
    let target = PathBuf::from(input["file_path"].as_str().unwrap());
    let targets = vec![target.clone()];
    if let Some(reason) = proto::gate(ctx, "Edit", input, ctx.root, &targets, Instant::now(), Duration::from_millis(1500)).block {
        return Err(reason);
    }
    let content = std::fs::read_to_string(&target).unwrap();
    let old = input["old_string"].as_str().unwrap();
    std::fs::write(&target, content.replacen(old, input["new_string"].as_str().unwrap(), 1)).unwrap();
    Ok(proto::report(ctx, &targets))
}

fn wait_for(what: &str, mut f: impl FnMut() -> Option<String>) -> String {
    let started = Instant::now();
    loop {
        if let Some(found) = f() {
            return found;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "waited for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn two_sessions_meet_on_compute_tax() {
    let (_server, port) = reference_server();
    let base = std::env::temp_dir().join(format!("collide-proto-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let alice_root = checkout(&base, "alice", "alice@x.com");
    let bob_root = checkout(&base, "bob", "bob@x.com");
    let mut env = Env::default();
    env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
    env.insert("CORK_SERVER_URL".into(), "http://127.0.0.1:9".into());
    env.insert("CORK_PROTOCOL_URL".into(), format!("ws://127.0.0.1:{port}"));
    env.insert("CORK_TOKEN".into(), "t".into());
    env.insert("CORK_REPO_ID".into(), "acme/tax".into());
    let alice_cfg = config::config(Some(&alice_root), &env);
    let bob_cfg = config::config(Some(&bob_root), &env);
    assert!(proto::enabled(&alice_root, &alice_cfg, &env));

    // Alice's session daemon
    let opts = protod::Options {
        root: alice_root.clone(),
        session: "alice-1".into(),
        agent: "claude".into(),
        socket: String::new(),
        socket_token: String::new(),
        owner: None,
    };
    let daemon_env = env.clone();
    let daemon = std::thread::spawn(move || protod::run(&opts, &daemon_env));
    wait_for("alice's daemon to connect", || {
        proto::ask(&env, "alice-1", &json!({"cmd": "ping"}), Duration::from_millis(200))
            .filter(|a| a["online"] == json!(true))
            .map(|_| String::new())
    });
    let alice = proto::Ctx { root: &alice_root, cfg: &alice_cfg, env: &env, session: "alice-1" };
    let bob = proto::Ctx { root: &bob_root, cfg: &bob_cfg, env: &env, session: "bob-1" };

    // 1. Alice changes compute_tax's body
    let said = write(&alice, &edit(&alice_root, "rate, 2)", "rate, 4)")).expect("alice is not blocked");
    assert_eq!(said, "", "accepted, nothing to say");
    let drafts = proto::drafts(&env, "alice-1");
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    assert_eq!(drafts[0]["key"], json!("code:tax.py::compute_tax"));

    // Bob's copy is older: the gate names Alice and her change
    let gate_started = Instant::now();
    let blocked = write(&bob, &edit(&bob_root, "return round(amount * rate, 2)", "return amount * rate")).unwrap_err();
    let direct_ms = gate_started.elapsed().as_millis();
    assert!(blocked.contains("alice@x.com (claude) is editing compute_tax right now"), "{blocked}");
    assert!(blocked.contains("compute_tax"), "{blocked}");
    assert_eq!(std::fs::read_to_string(bob_root.join("tax.py")).unwrap(), TAX, "nothing was written");

    // 2. Alice renames rate_for; Bob's body edit to it follows the rename
    write(&alice, &edit(&alice_root, "def rate_for(", "def rate_of(")).expect("a rename goes in");
    let said = write(&bob, &edit(&bob_root, "return 0.2", "return 0.25")).expect("a body edit commutes with a rename");
    assert_eq!(said, "", "accepted (rekeyed onto rate_of)");
    let bob_drafts = proto::drafts(&env, "bob-1");
    assert_eq!(bob_drafts.last().unwrap()["key"], json!("code:tax.py::rate_of"), "{bob_drafts:?}");

    // Alice hears Bob's edit to the function she just renamed, live
    let heard = wait_for("alice's delta", || Some(proto::take_notices(&env, "alice-1")).filter(|s| !s.is_empty()));
    assert!(heard.contains("Δ bob@x.com (claude) edited rate_of, which you are editing in tax.py"), "{heard}");

    // the daemon's gate is fast: a check round trip over its open socket
    let timed = Instant::now();
    let _ = proto::gate(&alice, "Edit", &edit(&alice_root, "rate, 4)", "rate, 5)"), &alice_root,
        &[alice_root.join("tax.py")], Instant::now(), Duration::from_millis(200));
    let daemon_ms = timed.elapsed().as_millis();
    eprintln!("gate latency: through the daemon {daemon_ms} ms, on its own connection {direct_ms} ms");

    // 3. Alice pushes: her drafts are promoted and leave the spool
    let ids: Vec<String> = proto::drafts(&env, "alice-1").iter().map(|d| d["op_id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 2);
    let promoted = proto::promote(&alice_root, &alice_cfg, &env, "alice-1", ids, json!({"commit": "abc", "tests": "passed"}));
    assert_eq!(promoted, 2);
    assert!(proto::drafts(&env, "alice-1").is_empty());

    // her lease went with the promotion; Bob's copy is still the old one, so
    // the gate now says what she did rather than that she is on it
    let stale = write(&bob, &edit(&bob_root, "return round(amount * rate, 2)", "return amount * rate")).unwrap_err();
    assert!(stale.contains("alice@x.com (claude) already edited compute_tax"), "{stale}");
    // told once: the next try builds on her version (the agent was told to
    // bring it in) and goes through
    let said = write(&bob, &edit(&bob_root, "rate, 2)", "rate, 3)")).expect("adopted");
    assert_eq!(said, "");
    let bob_drafts = proto::drafts(&env, "bob-1");
    assert_eq!(bob_drafts.last().unwrap()["key"], json!("code:tax.py::compute_tax"), "{bob_drafts:?}");

    proto::bye(&env, "alice-1");
    daemon.join().unwrap();
    let _ = std::fs::remove_dir_all(&base);
}

/// What the protocol adds to the gate, the hook that runs before every write:
/// through the session's daemon, on the hook's own connection, and with the
/// server unreachable (the write must go ahead within the gate's budget).
#[test]
fn the_gate_stays_fast_and_fails_open() {
    let (_server, port) = reference_server();
    let base = std::env::temp_dir().join(format!("collide-proto-lat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = checkout(&base, "carol", "carol@x.com");
    let mut env = Env::default();
    env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
    env.insert("CORK_SERVER_URL".into(), "http://127.0.0.1:9".into());
    env.insert("CORK_PROTOCOL_URL".into(), format!("ws://127.0.0.1:{port}"));
    env.insert("CORK_TOKEN".into(), "t".into());
    env.insert("CORK_REPO_ID".into(), "acme/lat".into());
    env.insert("CORK_LISTEN".into(), "0".into()); // the test starts its own daemon
    let cfg = config::config(Some(&root), &env);
    let input = edit(&root, "rate, 2)", "rate, 4)");
    let targets = vec![root.join("tax.py")];
    let gate = |ctx: &proto::Ctx| {
        let t = Instant::now();
        let said = proto::gate(ctx, "Edit", &input, &root, &targets, Instant::now(), Duration::from_millis(200)).block;
        (said, t.elapsed())
    };
    let median = |mut v: Vec<Duration>| {
        v.sort();
        v[v.len() / 2]
    };

    // no daemon: the hook's own connection
    let direct = proto::Ctx { root: &root, cfg: &cfg, env: &env, session: "carol-direct" };
    let direct_times: Vec<Duration> = (0..5).map(|_| gate(&direct).1).collect();

    // the daemon
    let opts = protod::Options { root: root.clone(), session: "carol-1".into(), agent: "claude".into(), socket: String::new(), socket_token: String::new(), owner: None };
    let daemon_env = env.clone();
    let daemon = std::thread::spawn(move || protod::run(&opts, &daemon_env));
    wait_for("the daemon", || {
        proto::ask(&env, "carol-1", &json!({"cmd": "ping"}), Duration::from_millis(200)).filter(|a| a["online"] == json!(true)).map(|_| String::new())
    });
    let ctx = proto::Ctx { root: &root, cfg: &cfg, env: &env, session: "carol-1" };
    let daemon_times: Vec<Duration> = (0..20).map(|_| gate(&ctx).1).collect();
    proto::bye(&env, "carol-1");
    daemon.join().unwrap();

    // the server is gone and there is no daemon: one bounded try, then nothing
    drop(_server);
    let mut gone_env = env.clone();
    gone_env.insert("CORK_PROTOCOL_URL".into(), "ws://10.255.255.1:9".into());
    let gone = proto::Ctx { root: &root, cfg: &cfg, env: &gone_env, session: "carol-gone" };
    let (said, first) = gate(&gone);
    assert!(said.is_none(), "fails open");
    let (said, then) = gate(&gone);
    assert!(said.is_none());
    eprintln!(
        "gate latency (median): daemon {:?}, own connection {:?}; server unreachable: first {:?}, then {:?}",
        median(daemon_times.clone()), median(direct_times), first, then
    );
    assert!(median(daemon_times) < Duration::from_millis(50));
    assert!(first <= proto::DIRECT_BUDGET + Duration::from_millis(100));
    // no second bounded try: far under the first's budget (a shared CI
    // runner once took 33 ms here, so the bound is not a tight one)
    assert!(then < Duration::from_millis(250), "{then:?}");
    let _ = std::fs::remove_dir_all(&base);
}

// ------------------------------------------------------------ semantic breaks before the write

const TAX0: &str = "def compute_tax(amount):\n    return round(amount * 0.2, 2)\n\n\ndef rate_for(region):\n    return 0.2\n";
const APP0: &str = "from tax import compute_tax, rate_for\n\n\ndef checkout(total):\n    return total + compute_tax(total)\n";
const CLI0: &str = "from tax import compute_tax\n\n\ndef main(x):\n    print(x)\n";
const JOBS0: &str = "from tax import compute_tax\n\n\ndef nightly(x):\n    return x\n";

fn caller_checkout(base: &Path, name: &str, email: &str) -> PathBuf {
    let dir = checkout(base, name, email);
    std::fs::write(dir.join("tax.py"), TAX0).unwrap();
    std::fs::write(dir.join("app.py"), APP0).unwrap();
    std::fs::write(dir.join("cli.py"), CLI0).unwrap();
    std::fs::write(dir.join("jobs.py"), JOBS0).unwrap();
    dir
}

fn edit_in(root: &Path, file: &str, old: &str, new: &str) -> Value {
    json!({"file_path": root.join(file).to_string_lossy(), "old_string": old, "new_string": new})
}

/// A write as the hooks see it, with what the gate said: `Err(block)`, or
/// the notice it let the write through with.
fn write_said(ctx: &proto::Ctx, input: &Value) -> Result<String, String> {
    let target = PathBuf::from(input["file_path"].as_str().unwrap());
    let targets = vec![target.clone()];
    let said = proto::gate(ctx, "Edit", input, ctx.root, &targets, Instant::now(), Duration::from_millis(1500));
    if let Some(reason) = said.block {
        return Err(reason);
    }
    let content = std::fs::read_to_string(&target).unwrap();
    let old = input["old_string"].as_str().unwrap();
    std::fs::write(&target, content.replacen(old, input["new_string"].as_str().unwrap(), 1)).unwrap();
    proto::report(ctx, &targets);
    Ok(said.notice)
}

fn start_daemon(root: &Path, session: &str, env: &Env) -> std::thread::JoinHandle<()> {
    let opts = protod::Options { root: root.to_path_buf(), session: session.into(), agent: "claude".into(), socket: String::new(), socket_token: String::new(), owner: None };
    let daemon_env = env.clone();
    let handle = std::thread::spawn(move || protod::run(&opts, &daemon_env));
    wait_for("the daemon", || {
        proto::ask(env, session, &json!({"cmd": "ping"}), Duration::from_millis(200)).filter(|a| a["online"] == json!(true)).map(|_| String::new())
    });
    handle
}

/// Alice changes `compute_tax(amount)` to `compute_tax(amount, region)` in
/// tax.py. Carol, about to edit cli.py (another file) to call it the old
/// way, hears it at her gate, before the write lands: who, old -> new, why,
/// and the line. Bob, who read app.py earlier, hears it pushed at his next
/// tool result, before he writes at all. Neither hears it twice; a caller
/// already passing the region hears nothing; and a write that ADDS a call
/// to a name Alice renamed is stopped once.
#[test]
fn a_caller_hears_a_signature_change_before_its_write() {
    let (_server, port) = reference_server();
    let base = std::env::temp_dir().join(format!("collide-proto-break-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let alice_root = caller_checkout(&base, "alice", "alice@x.com");
    let bob_root = caller_checkout(&base, "bob", "bob@x.com");
    let carol_root = caller_checkout(&base, "carol", "carol@x.com");
    let mut env = Env::default();
    env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
    env.insert("CORK_SERVER_URL".into(), "http://127.0.0.1:9".into());
    env.insert("CORK_PROTOCOL_URL".into(), format!("ws://127.0.0.1:{port}"));
    env.insert("CORK_TOKEN".into(), "t".into());
    env.insert("CORK_REPO_ID".into(), "acme/break".into());
    env.insert("CORK_LISTEN".into(), "0".into()); // the daemons are started here
    let (alice_cfg, bob_cfg, carol_cfg) =
        (config::config(Some(&alice_root), &env), config::config(Some(&bob_root), &env), config::config(Some(&carol_root), &env));
    let alice_daemon = start_daemon(&alice_root, "alice-1", &env);
    let bob_daemon = start_daemon(&bob_root, "bob-1", &env);
    let alice = proto::Ctx { root: &alice_root, cfg: &alice_cfg, env: &env, session: "alice-1" };
    let bob = proto::Ctx { root: &bob_root, cfg: &bob_cfg, env: &env, session: "bob-1" };
    // carol has no daemon: her gate uses a connection of its own
    let carol = proto::Ctx { root: &carol_root, cfg: &carol_cfg, env: &env, session: "carol-1" };
    proto::note_intent(&env, "alice-1", "Add regional tax rates");

    // Bob read app.py earlier: his daemon follows what it calls
    proto::follow_read(&env, "bob-1", &bob_root, "app.py", APP0);
    // (a round trip through his connection: the subscription is in before Alice writes)
    proto::ask(&env, "bob-1", &json!({"cmd": "gate", "ops": [], "deps": [{"key": "code:tax.py::compute_tax"}]}), Duration::from_secs(2)).unwrap();

    // 1. Alice re-signatures compute_tax
    let said = write_said(&alice, &edit_in(&alice_root, "tax.py", "def compute_tax(amount):", "def compute_tax(amount, region):")).unwrap();
    assert_eq!(said, "");
    assert_eq!(proto::drafts(&env, "alice-1").last().unwrap()["key"], json!("code:tax.py::compute_tax"));

    // 2. pushed: Bob hears it at his next tool result, with app.py's line, before writing anything
    let heard = wait_for("bob's push", || Some(proto::take_notices(&env, "bob-1")).filter(|s| !s.is_empty()));
    assert!(heard.contains("Δ alice@x.com (claude) changed compute_tax in tax.py: `def compute_tax(amount)` -> `def compute_tax(amount, region)` \
(their intent: Add regional tax rates). Your calls in app.py do not fit it yet (line 5 passes 1 arg; it now takes 2 (amount, region)): update them."), "{heard}");

    // 3. Carol is about to call it the old way in cli.py: the gate says so, and lets the write through
    let input = edit_in(&carol_root, "cli.py", "print(x)", "print(x, compute_tax(x))");
    let timed = Instant::now();
    let gated = proto::gate(&carol, "Edit", &input, &carol_root, &[carol_root.join("cli.py")], Instant::now(), Duration::from_millis(1500));
    let direct_ms = timed.elapsed().as_millis();
    assert!(gated.block.is_none(), "a signature change never blocks: {gated:?}");
    assert!(gated.notice.starts_with("Cork (before this write): a teammate changed code this file calls."), "{}", gated.notice);
    assert!(gated.notice.contains("Δ alice@x.com (claude) changed compute_tax in tax.py: `def compute_tax(amount)` -> `def compute_tax(amount, region)` \
(their intent: Add regional tax rates). Your calls in cli.py do not fit it yet (line 5 passes 1 arg; it now takes 2 (amount, region)): update them."), "{}", gated.notice);
    assert_eq!(std::fs::read_to_string(carol_root.join("cli.py")).unwrap(), CLI0, "the gate writes nothing");

    // 4. said once: the same write again, and another edit of that file, say nothing
    assert_eq!(write_said(&carol, &input).unwrap(), "");
    assert_eq!(write_said(&carol, &edit_in(&carol_root, "cli.py", "print(x, compute_tax(x))", "print(x, compute_tax(x) + 1)")).unwrap(), "");
    // ... nor does Bob's gate on the file he was already told about (push and gate are one channel)
    let timed = Instant::now();
    let bob_said = write_said(&bob, &edit_in(&bob_root, "app.py", "return total + compute_tax(total)", "return compute_tax(total) + total")).unwrap();
    let daemon_ms = timed.elapsed().as_millis();
    assert_eq!(bob_said, "", "told by the push already");
    // and in a file he was not told about, only that file's lines
    let bob_cli = write_said(&bob, &edit_in(&bob_root, "jobs.py", "return x", "return compute_tax(x)")).unwrap();
    assert!(bob_cli.contains("Δ compute_tax in tax.py changed (said above): Your calls in jobs.py do not fit it yet (line 5 passes 1 arg; it now takes 2 (amount, region))"), "{bob_cli}");
    assert!(!bob_cli.contains("alice@x.com"), "not the story again: {bob_cli}");

    // 5. an already-updated caller hears nothing (and is never blocked):
    //    carol-2, a new session on Carol's copy, was never told
    let fresh = proto::Ctx { root: &carol_root, cfg: &carol_cfg, env: &env, session: "carol-2" };
    let updated = write_said(&fresh, &edit_in(&carol_root, "cli.py", "compute_tax(x) + 1", "compute_tax(x, \"eu\") + 1")).unwrap();
    assert_eq!(updated, "", "the call already fits the new signature");

    // 6. Alice renames rate_for, in her checkout only (not pushed): a write that
    //    ADDS a call to the old name is told what to write against, never
    //    stopped and never sent to pull (there is nothing on origin to pull)
    write_said(&alice, &edit_in(&alice_root, "tax.py", "def rate_for(", "def rate_of(")).unwrap();
    let adds = edit_in(
        &carol_root,
        "cli.py",
        "from tax import compute_tax\n\n\ndef main(x):\n    print(x, compute_tax(x, \"eu\") + 1)",
        "from tax import compute_tax, rate_for\n\n\ndef main(x):\n    print(x, compute_tax(x, \"eu\") + 1, rate_for(\"eu\"))",
    );
    let told = proto::gate(&fresh, "Edit", &adds, &carol_root, &[carol_root.join("cli.py")], Instant::now(), Duration::from_millis(1500));
    assert!(told.block.is_none(), "{told:?}");
    assert!(told.notice.contains("Δ alice@x.com (claude) renamed rate_for -> rate_of in tax.py (their intent: Add regional tax rates)."), "{}", told.notice);
    assert!(told.notice.contains("cli.py still uses `rate_for` on lines 1, 5: not pushed yet, write it against `rate_of` (`rate_for(…)` → `rate_of(…)`); landing will rebase onto it."), "{}", told.notice);
    assert!(!told.notice.to_lowercase().contains("pull"), "{}", told.notice);
    // said once
    assert_eq!(write_said(&fresh, &adds).unwrap(), "");

    // what the dependency check costs the gate, through the daemon: the
    // write's own keys and two callees in one check frame, plus the local
    // parse of the file and of tax.py
    let probe = edit_in(&bob_root, "app.py", "return compute_tax(total) + total", "return compute_tax(total) + total + 0");
    let mut runs: Vec<Duration> = (0..15)
        .map(|_| {
            let t = Instant::now();
            let said = proto::gate(&bob, "Edit", &probe, &bob_root, &[bob_root.join("app.py")], Instant::now(), Duration::from_millis(200));
            assert!(said.block.is_none());
            t.elapsed()
        })
        .collect();
    runs.sort();
    let started = Instant::now();
    for _ in 0..15 {
        assert_eq!(cork_hooks::deps::deps_of(&bob_root, "app.py", APP0).len(), 2);
    }
    let local = started.elapsed() / 15;
    eprintln!(
        "gate with a dependency check: own connection {direct_ms} ms (first, incl. connect); through the daemon median {:?} \
(whole write incl. report {daemon_ms} ms); the local resolve alone {local:?}",
        runs[7]
    );
    assert!(runs[7] < Duration::from_millis(50), "{runs:?}");

    proto::bye(&env, "alice-1");
    proto::bye(&env, "bob-1");
    alice_daemon.join().unwrap();
    bob_daemon.join().unwrap();
    let _ = std::fs::remove_dir_all(&base);
}

/// Two clones of one origin: what the phantom-block test needs, an
/// upstream to check a teammate's change against.
fn cloned(base: &Path, origin: &Path, name: &str, email: &str) -> PathBuf {
    let dir = base.join(name);
    sh(base, &["clone", "-q", origin.to_str().unwrap(), dir.to_str().unwrap()]);
    sh(&dir, &["config", "user.email", email]);
    sh(&dir, &["config", "user.name", name]);
    dir
}

/// Study 8's phantom block, reproduced. A session's rename was recorded,
/// and even promoted (it landed, onto another clone's origin), but never
/// reached the origin this checkout pulls from. The gate told the next agent
/// to "pull it" four times; there was nothing to pull, and the agent went
/// round the gate with `sed`. Now: in the author's checkout only, it is
/// "not pushed yet: write against `rate_of`"; promoted but not on this
/// origin, it is "nothing to pull"; on origin, it blocks once, and the
/// retry goes through.
#[test]
fn an_unpushed_rename_says_not_pushed_never_pull() {
    let (_server, port) = reference_server();
    let base = std::env::temp_dir().join(format!("collide-proto-phantom-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let origin = base.join("origin.git");
    sh(&base, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    let seed = cloned(&base, &origin, "seed", "seed@x.com");
    std::fs::create_dir_all(seed.join(".collide")).unwrap();
    std::fs::write(seed.join(".collide/config.json"), "{}").unwrap();
    std::fs::write(seed.join("tax.py"), TAX).unwrap();
    sh(&seed, &["add", "."]);
    sh(&seed, &["commit", "-qm", "seed"]);
    sh(&seed, &["push", "-q", "-u", "origin", "HEAD"]);
    let alice_root = cloned(&base, &origin, "alice", "alice@x.com");
    let bob_root = cloned(&base, &origin, "bob", "bob@x.com");
    let mut env = Env::default();
    env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
    env.insert("CORK_SERVER_URL".into(), "http://127.0.0.1:9".into());
    env.insert("CORK_PROTOCOL_URL".into(), format!("ws://127.0.0.1:{port}"));
    env.insert("CORK_TOKEN".into(), "t".into());
    env.insert("CORK_REPO_ID".into(), "acme/phantom".into());
    env.insert("CORK_LISTEN".into(), "0".into());
    let (alice_cfg, bob_cfg) = (config::config(Some(&alice_root), &env), config::config(Some(&bob_root), &env));
    let alice = proto::Ctx { root: &alice_root, cfg: &alice_cfg, env: &env, session: "alice-1" };
    let rename = |root: &Path| edit_in(root, "tax.py", "def rate_for(", "def rate_of(");
    let gate = |session: &str| {
        let ctx = proto::Ctx { root: &bob_root, cfg: &bob_cfg, env: &env, session };
        proto::gate(&ctx, "Edit", &rename(&bob_root), &bob_root, &[bob_root.join("tax.py")], Instant::now(), Duration::from_millis(1500))
    };

    // 1. Alice renames rate_for in her checkout; Bob is about to rename it too
    write_said(&alice, &rename(&alice_root)).expect("alice renames");
    let said = gate("bob-1");
    assert!(said.block.is_none(), "a draft is never a block: {said:?}");
    assert!(said.notice.contains("alice@x.com (claude) renamed rate_for -> rate_of") && said.notice.contains("not pushed yet")
        && said.notice.contains("write against `rate_of`; landing will rebase onto it"), "{}", said.notice);
    assert!(!said.notice.to_lowercase().contains("pull"), "{}", said.notice);

    // 2. her ops are promoted (she landed, but not onto Bob's origin): still
    //    nothing to pull, so no block
    let ids: Vec<String> = proto::drafts(&env, "alice-1").iter().map(|d| d["op_id"].as_str().unwrap().to_string()).collect();
    assert_eq!(proto::promote(&alice_root, &alice_cfg, &env, "alice-1", ids, json!({"commit": "elsewhere", "tests": "passed"})), 1);
    let said = gate("bob-2");
    assert!(said.block.is_none(), "promoted elsewhere is not on this origin: {said:?}");
    assert!(said.notice.contains("there is nothing to bring in") && said.notice.contains("write against `rate_of`"), "{}", said.notice);
    assert!(!said.notice.contains("git pull"), "{}", said.notice);

    // 3. she pushes it to this origin and Bob's checkout fetches: one block, cleared by a retry
    sh(&alice_root, &["commit", "-qam", "rename rate_for"]);
    sh(&alice_root, &["push", "-q", "origin", "HEAD"]);
    sh(&bob_root, &["fetch", "-q", "origin"]);
    let said = gate("bob-3");
    let reason = said.block.expect("on origin: a pull brings it in");
    // this checkout lands its pushes: what to write against, never "pull"
    assert!(reason.contains("and pushed it") && reason.contains("landing will rebase onto it") && !reason.to_lowercase().contains("pull"), "{reason}");
    assert!(gate("bob-3").block.is_none(), "blocked once; the retry goes through");
    let _ = std::fs::remove_dir_all(&base);
}

fn hook(args: &[&str], env: &Env, cwd: &Path, input: &Value) -> (i32, String, String, Duration) {
    use std::io::Write;
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cork-hook"))
        .args(args)
        .env_clear()
        .envs(env)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into(), started.elapsed())
}

/// The real program, as Codex runs it: SessionStart starts the session's
/// daemon (not only Claude Code's), the gate and the report go through it,
/// SessionEnd ends it. And with the server gone, the gate allows in time.
#[test]
fn the_hook_program_starts_its_daemon_and_fails_open() {
    let (_server, port) = reference_server();
    let base = std::env::temp_dir().join(format!("collide-proto-bin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = checkout(&base, "dana", "dana@x.com");
    let mut env = Env::default();
    env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
    env.insert("CORK_SERVER_URL".into(), "http://127.0.0.1:9".into());
    env.insert("CORK_PROTOCOL_URL".into(), format!("ws://127.0.0.1:{port}"));
    env.insert("CORK_TOKEN".into(), "t".into());
    env.insert("CORK_REPO_ID".into(), "acme/bin".into());
    env.insert("CORK_NATIVE_INSTALL".into(), "0".into());
    let cwd = root.to_string_lossy().to_string();
    let start = json!({"hook_event_name": "SessionStart", "session_id": "dana-1", "cwd": cwd, "source": "startup"});
    let (code, _, _, _) = hook(&["report", "--harness", "codex"], &env, &root, &start);
    assert_eq!(code, 0);
    wait_for("the daemon SessionStart started", || {
        proto::ask(&env, "dana-1", &json!({"cmd": "ping"}), Duration::from_millis(200)).filter(|a| a["online"] == json!(true)).map(|_| String::new())
    });

    let input = edit(&root, "rate, 2)", "rate, 4)");
    let pre = json!({"hook_event_name": "PreToolUse", "session_id": "dana-1", "cwd": cwd, "tool_name": "Edit", "tool_input": input});
    let (code, _, err, _) = hook(&["gate", "--harness", "codex"], &env, &root, &pre);
    assert_eq!(code, 0, "{err}");
    let median = |env: &Env| {
        let mut runs: Vec<Duration> = (0..7).map(|_| hook(&["gate", "--harness", "codex"], env, &root, &pre).3).collect();
        runs.sort();
        runs[3]
    };
    let mut off = env.clone();
    off.insert("CORK_PROTOCOL".into(), "0".into());
    eprintln!("gate program (median of 7): protocol off {:?}, through the daemon {:?}", median(&off), median(&env));
    let content = std::fs::read_to_string(root.join("tax.py")).unwrap().replace("rate, 2)", "rate, 4)");
    std::fs::write(root.join("tax.py"), content).unwrap();
    let post = json!({"hook_event_name": "PostToolUse", "session_id": "dana-1", "cwd": cwd, "tool_name": "Edit", "tool_input": input});
    let (code, _, _, _) = hook(&["report", "--harness", "codex"], &env, &root, &post);
    assert_eq!(code, 0);
    let drafts = proto::drafts(&env, "dana-1");
    assert_eq!(drafts.len(), 1, "the report's op went through the daemon: {drafts:?}");

    let end = json!({"hook_event_name": "SessionEnd", "session_id": "dana-1", "cwd": cwd});
    hook(&["report", "--harness", "codex"], &env, &root, &end);
    wait_for("the daemon to end", || (!proto::daemon_up(&env, "dana-1")).then(String::new));

    // the server is gone: one bounded try, the write is allowed
    drop(_server);
    let mut gone = env.clone();
    gone.insert("CORK_PROTOCOL_URL".into(), "ws://10.255.255.1:9".into());
    gone.insert("CORK_LISTEN".into(), "0".into());
    let pre = json!({"hook_event_name": "PreToolUse", "session_id": "dana-2", "cwd": cwd, "tool_name": "Edit",
                     "tool_input": edit(&root, "rate, 4)", "rate, 5)")});
    let (code, _, err, took) = hook(&["gate", "--harness", "codex"], &gone, &root, &pre);
    assert_eq!(code, 0, "{err}");
    assert!(took < Duration::from_millis(2000), "{took:?}");
    let (code, _, _, again) = hook(&["gate", "--harness", "codex"], &gone, &root, &pre);
    assert_eq!(code, 0);
    eprintln!("gate program, server unreachable: first {took:?}, then {again:?}");
    let _ = std::fs::remove_dir_all(&base);
}
