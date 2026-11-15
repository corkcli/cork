//! The token-scaling study's fixes (Study 8), end to end: the real hook
//! program, run the way Claude Code runs it, against the real Collide server
//! on this machine (`cork-server local`, the free version's server), with
//! agents in clones of one origin. Each agent session is its own identity
//! there, as on a team.
//!
//! - two agents adding their own tests to one file both write unblocked;
//!   two changing the same test are still stopped, once;
//! - a teammate's rename that is only in their checkout is "not pushed yet:
//!   write against `checkout_total`", never "pull";
//! - a session in repo B hears nothing from repo A, even when its session
//!   started in A; an anchorless message reaches everyone at work in A;
//! - a teammate's change reaches only the session whose working set calls
//!   or touches it, and only once.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

type Env = BTreeMap<String, String>;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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

/// The server, built: `cork-server` from ../cork-rs in Cork's own repo,
/// `cork` from ../free in the public one (lithometric/cork).
fn collide_server() -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (rs, bin) = if here.join("../cork-rs").exists() { (here.join("../cork-rs"), "cork-server") } else { (here.join("../free"), "cork") };
    let built = Command::new("cargo").args(["build", "--release", "-q"]).current_dir(&rs).status().expect("cargo runs");
    assert!(built.success(), "{bin} builds");
    target_dir_of(&rs).join("release").join(format!("{bin}{}", std::env::consts::EXE_SUFFIX))
}

fn sh(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
    assert!(ok.success(), "git {args:?} in {}", dir.display());
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// One developer machine: a home with Collide installed (the free version),
/// its local server, and the repos' origins.
struct Machine {
    base: PathBuf,
    home: PathBuf,
    url: String,
    token: String,
    _server: Proc,
}

impl Machine {
    fn new(name: &str) -> Machine {
        let base = std::env::temp_dir().join(format!("collide-scaling-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        std::fs::create_dir_all(home.join(".collide")).unwrap();
        std::fs::write(home.join(".collide/machine.json"), json!({"mode": "local"}).to_string()).unwrap();
        let port = free_port();
        let log = std::fs::File::create(base.join("server.log")).unwrap();
        let child = Command::new(collide_server())
            .args(["local", "--port", &port.to_string()])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("CORK_HOME", &home)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("the local server starts");
        let token_file = home.join(".collide/local/token");
        let started = Instant::now();
        while !(token_file.exists() && std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()) {
            assert!(started.elapsed() < Duration::from_secs(20), "the local server listens");
            std::thread::sleep(Duration::from_millis(50));
        }
        let token = std::fs::read_to_string(&token_file).unwrap().trim().to_string();
        Machine { base, home, url: format!("http://127.0.0.1:{port}"), token, _server: Proc(child) }
    }

    /// A repo's origin, `github.com/acme/<name>` by name, seeded with `files`.
    fn origin(&self, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let bare = self.base.join(format!("{name}.git"));
        sh(&self.base, &["init", "-q", "--bare", bare.to_str().unwrap()]);
        let seed = self.clone_of(name, &format!("{name}-seed"));
        for (path, text) in files {
            let full = seed.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-qm", "seed"]);
        sh(&seed, &["push", "-q", "-u", "origin", "HEAD"]);
        bare
    }

    /// An agent's checkout: its origin reads as the hosted repo (the id
    /// Collide keys it by), and fetches and pushes go to the bare one here.
    fn clone_of(&self, name: &str, dir: &str) -> PathBuf {
        let bare = self.base.join(format!("{name}.git"));
        let path = self.base.join(dir);
        sh(&self.base, &["clone", "-q", bare.to_str().unwrap(), path.to_str().unwrap()]);
        let hosted = format!("https://github.com/acme/{name}.git");
        sh(&path, &["config", "remote.origin.url", &hosted]);
        sh(&path, &["config", &format!("url.{}.insteadOf", bare.display()), &hosted]);
        sh(&path, &["config", "user.email", &format!("{dir}@x.com")]);
        sh(&path, &["config", "user.name", dir]);
        path
    }

    fn env(&self) -> Env {
        let mut env = Env::new();
        for (k, v) in [
            ("HOME", self.home.to_string_lossy().to_string()),
            ("CORK_HOME", self.home.to_string_lossy().to_string()),
            ("PATH", std::env::var("PATH").unwrap_or_default()),
            ("CORK_NATIVE_INSTALL", "0".into()),
            ("CORK_NO_BROWSER", "1".into()),
            ("CORK_LISTEN", "0".into()),
        ] {
            env.insert(k.into(), v);
        }
        env
    }

    /// The hook program, as the harness runs it: (exit code, stdout, stderr).
    fn hook(&self, args: &[&str], cwd: &Path, input: &Value, extra: &[(&str, &str)]) -> (i32, String, String) {
        let mut env = self.env();
        for (k, v) in extra {
            env.insert(k.to_string(), v.to_string());
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_cork-hook"))
            .args(args)
            .env_clear()
            .envs(&env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
    }
}

/// One agent session in one checkout.
struct Agent<'a> {
    m: &'a Machine,
    root: PathBuf,
    session: String,
    extra: Vec<(String, String)>,
}

impl<'a> Agent<'a> {
    fn new(m: &'a Machine, root: &Path, session: &str) -> Agent<'a> {
        Agent { m, root: root.to_path_buf(), session: session.into(), extra: Vec::new() }
    }

    fn run(&self, args: &[&str], input: Value) -> (i32, String, String) {
        let extra: Vec<(&str, &str)> = self.extra.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        self.m.hook(args, &self.root, &input, &extra)
    }

    fn input(&self, event: &str, tool: &str, tool_input: Value) -> Value {
        json!({"hook_event_name": event, "session_id": self.session, "cwd": self.root.to_string_lossy(),
               "tool_name": tool, "tool_input": tool_input, "tool_response": {}})
    }

    fn start(&self) -> String {
        let input = json!({"hook_event_name": "SessionStart", "session_id": self.session, "cwd": self.root.to_string_lossy(), "source": "startup"});
        self.run(&["report"], input).1
    }

    /// A Read, as its PostToolUse hook reports it: what the hook said.
    fn read(&self, rel: &str) -> String {
        self.run(&["report"], self.input("PostToolUse", "Read", json!({"file_path": self.root.join(rel).to_string_lossy()}))).1
    }

    /// An Edit: the gate, the write if allowed, the report. `Ok(what the
    /// gate said)` or `Err(the block)`.
    fn edit(&self, rel: &str, old: &str, new: &str) -> Result<String, String> {
        let target = self.root.join(rel);
        let tool_input = json!({"file_path": target.to_string_lossy(), "old_string": old, "new_string": new});
        let (code, out, err) = self.run(&["gate"], self.input("PreToolUse", "Edit", tool_input.clone()));
        if code == 2 {
            return Err(err);
        }
        assert_eq!(code, 0, "{err}");
        let content = std::fs::read_to_string(&target).unwrap();
        assert!(content.contains(old), "{old:?} in {rel}");
        std::fs::write(&target, content.replacen(old, new, 1)).unwrap();
        let (_, reported, _) = self.run(&["report"], self.input("PostToolUse", "Edit", tool_input));
        Ok(format!("{out}{reported}"))
    }
}

const FEATURES: &str = "def test_a():\n    assert 1\n";

/// Study 8: at 16 agents, 589k of the 802k tokens stalled at the gate were
/// blocks on tests/test_features.py, where each agent was adding its own
/// test. Now both pass, with a line; editing the same test is still stopped,
/// once.
#[test]
fn two_agents_adding_tests_to_one_file_both_write_unblocked() {
    let m = Machine::new("tests");
    m.origin("shop", &[("tests/test_features.py", FEATURES), ("tests/__init__.py", "")]);
    let (a, b, c) = (m.clone_of("shop", "alice"), m.clone_of("shop", "bob"), m.clone_of("shop", "carol"));
    let alice = Agent::new(&m, &a, "alice-1");
    let bob = Agent::new(&m, &b, "bob-1");
    let carol = Agent::new(&m, &c, "carol-1");
    for agent in [&alice, &bob, &carol] {
        agent.read("tests/test_features.py");
    }

    alice.edit("tests/test_features.py", "    assert 1\n", "    assert 1\n\n\ndef test_b():\n    assert 2\n").expect("alice adds test_b");
    let said = bob.edit("tests/test_features.py", "    assert 1\n", "    assert 1\n\n\ndef test_c():\n    assert 3\n")
        .expect("bob adds test_c while alice's test_b is not pushed: not blocked");
    assert!(said.contains("is also adding to tests/test_features.py (`test_b`") && said.contains("landing merges both"), "{said}");
    assert!(!said.to_lowercase().contains("pull"), "{said}");

    // the same test, changed by both: stopped once, and never "pull"
    alice.edit("tests/test_features.py", "    assert 1\n", "    assert 10\n").expect("alice changes test_a");
    let stopped = carol.edit("tests/test_features.py", "    assert 1\n", "    assert 11\n").unwrap_err();
    assert!(stopped.contains("both of you change `test_a`") && stopped.contains("not pushed"), "{stopped}");
    assert!(!stopped.to_lowercase().contains("git pull"), "{stopped}");
    carol.edit("tests/test_features.py", "    assert 1\n", "    assert 11\n").expect("the retry goes through");
    let _ = std::fs::remove_dir_all(&m.base);
}

const BOOKINGS: &str = "def list_bookings(store):\n    return list(store)\n\n\ndef create_booking(store, booking):\n    store.append(booking)\n    return booking\n";

impl<'a> Agent<'a> {
    /// A shell command, as the gate sees it before it runs: its exit code
    /// (2 when held) and what it said. The command is not run.
    fn bash_gate(&self, command: &str) -> (i32, String) {
        let (code, out, err) = self.run(&["gate"], self.input("PreToolUse", "Bash", json!({"command": command})));
        (code, format!("{out}{err}"))
    }

    fn write_whole(&self, rel: &str, content: &str) -> Result<String, String> {
        let target = self.root.join(rel);
        let tool_input = json!({"file_path": target.to_string_lossy(), "content": content});
        let (code, out, err) = self.run(&["gate"], self.input("PreToolUse", "Write", tool_input.clone()));
        if code == 2 {
            return Err(err);
        }
        std::fs::write(&target, content).unwrap();
        let (_, reported, _) = self.run(&["report"], self.input("PostToolUse", "Write", tool_input));
        Ok(format!("{out}{reported}"))
    }
}

/// Study 8, second study (n16-collide-r1, a1): a teammate pushed a new
/// signature for `list_bookings`; the agent rebased onto it, hit a conflict
/// in the same file, and the gate held it fifteen times while it resolved
/// its own conflict: its Write, `cat >`, Edit, `echo >>`, `git checkout
/// --theirs` and `mv` were each stopped with "pull that change", which no
/// read of a file full of conflict markers could clear. It hit its turn cap
/// and used 12.8M tokens. Now the teammate's change holds the agent once,
/// and a file in conflict, and git's own conflict commands, are never held.
#[test]
fn an_agent_resolving_its_own_rebase_conflict_is_held_at_most_once() {
    let m = Machine::new("rebase");
    m.origin("book", &[("bookings.py", BOOKINGS)]);
    let (b, a) = (m.clone_of("book", "bob"), m.clone_of("book", "alice"));
    let bob = Agent::new(&m, &b, "bob-1");
    let alice = Agent::new(&m, &a, "alice-1");
    bob.read("bookings.py");
    alice.read("bookings.py");

    // bob re-signatures list_bookings and lands it
    bob.edit("bookings.py", "def list_bookings(store):", "def list_bookings(store, day=None):").expect("bob's change");
    sh(&b, &["commit", "-qam", "list_bookings takes a day"]);
    let landed = Command::new(env!("CARGO_BIN_EXE_cork-hook")).args(["land", "--session", "bob-1", "--test", "true"])
        .env_clear().envs(m.env()).current_dir(&b).output().unwrap();
    eprintln!("land: {}{}", String::from_utf8_lossy(&landed.stdout), String::from_utf8_lossy(&landed.stderr));
    assert!(landed.status.success(), "{}", String::from_utf8_lossy(&landed.stdout));

    let mut held: Vec<String> = Vec::new();
    // alice's checkout has fetched it (git-sync does, now and then), so the
    // gate knows it is on origin
    sh(&a, &["fetch", "-q", "origin"]);
    // alice, on her stale copy, changes the same function: held once, never "pull"
    let mine = ("def list_bookings(store):", "def list_bookings(store, member_id=None):");
    let reason = alice.edit("bookings.py", mine.0, mine.1).expect_err("bob's pushed signature holds her stale write");
    assert!(reason.contains("list_bookings") && !reason.to_lowercase().contains("pull"), "landing is on: {reason}");
    held.push(reason);
    alice.edit("bookings.py", mine.0, mine.1).expect("the retry goes through");
    sh(&a, &["commit", "-qam", "list_bookings by member"]);
    // she rebases onto bob's push by hand and the same lines conflict
    sh(&a, &["fetch", "-q", "origin"]);
    let rebased = Command::new("git").args(["rebase", "origin/HEAD"]).current_dir(&a).output().unwrap();
    assert!(!rebased.status.success(), "the rebase stops on a conflict");
    assert!(std::fs::read_to_string(a.join("bookings.py")).unwrap().contains("<<<<<<< "));

    // everything she does to resolve it goes through
    let attempts: Vec<Result<String, String>> = vec![
        alice.write_whole("bookings.py", &BOOKINGS.replace("def list_bookings(store):", "def list_bookings(store, day=None, member_id=None):")),
        alice.edit("bookings.py", "member_id=None):", "member_id=None):  # merged"),
    ];
    for attempt in attempts {
        if let Err(reason) = attempt {
            held.push(reason);
        }
    }
    for command in [
        "git checkout --theirs -- bookings.py && echo OK",
        "mv /tmp/bookings_resolved.py bookings.py && echo MOVED_OK",
        "echo \"# test\" >> bookings.py && echo WROTE_OK",
        "cat > bookings.py << 'PYEOF'\ndef list_bookings(store, day=None):\n    return list(store)\nPYEOF",
        "git add bookings.py && git rebase --continue",
    ] {
        let (code, said) = alice.bash_gate(command);
        if code == 2 {
            held.push(said);
        }
    }
    assert!(held.len() <= 1, "held {} times: {held:#?}", held.len());
    let _ = std::fs::remove_dir_all(&m.base);
}

const TOTAL: &str = "def amount_due(lines):\n    return sum(lines)\n";

/// A teammate renames `amount_due` in their checkout and has not pushed:
/// the gate never says "pull" (there is nothing on origin to pull), and
/// says what to write against.
#[test]
fn an_unpushed_rename_gives_not_pushed_yet_never_pull() {
    let m = Machine::new("rename");
    m.origin("till", &[("total.py", TOTAL)]);
    let (a, b, d) = (m.clone_of("till", "alice"), m.clone_of("till", "bob"), m.clone_of("till", "dana"));
    let alice = Agent::new(&m, &a, "alice-1");
    let bob = Agent::new(&m, &b, "bob-1");
    let dana = Agent::new(&m, &d, "dana-1");
    alice.read("total.py");
    bob.read("total.py");
    alice.edit("total.py", "def amount_due(", "def checkout_total(").expect("alice renames");

    // bob changes the very function: stopped once, told what to write against
    let stopped = bob.edit("total.py", "return sum(lines)", "return sum(lines) + 0").unwrap_err();
    assert!(stopped.contains("not pushed"), "{stopped}");
    assert!(stopped.contains("write against `checkout_total`"), "{stopped}");
    assert!(!stopped.to_lowercase().contains("pull"), "{stopped}");
    bob.edit("total.py", "return sum(lines)", "return sum(lines) + 0").expect("the retry goes through");

    // dana read the file after the rename (her copy lacks it all the same)
    dana.read("total.py");
    let said = dana.edit("total.py", "return sum(lines)", "return sum(lines) * 1").expect("never a block");
    assert!(said.contains("not pushed yet): write against `checkout_total`; landing will rebase"), "{said}");
    assert!(!said.to_lowercase().contains("pull"), "{said}");

    // alice pushes it with plain git (no landing): the server still knows it
    // only as unpushed, but origin has it: held once, and with landing on it
    // says what to write against, never "pull"
    let erin = Agent::new(&m, &m.clone_of("till", "erin"), "erin-1");
    sh(&a, &["commit", "-qam", "rename amount_due"]);
    sh(&a, &["push", "-q", "origin", "HEAD"]);
    sh(&erin.root, &["fetch", "-q", "origin"]);
    erin.read("total.py");
    let stopped = erin.edit("total.py", "return sum(lines)", "return sum(lines) - 0").unwrap_err();
    assert!(stopped.contains("it is on origin") && stopped.contains("landing will rebase") && !stopped.to_lowercase().contains("pull"), "{stopped}");
    erin.edit("total.py", "return sum(lines)", "return sum(lines) - 0").expect("blocked once");
    let _ = std::fs::remove_dir_all(&m.base);
}

/// The leak the user reported: agents working in other repos received
/// notices about this one. A session in repo B, even one whose Claude Code
/// session started in repo A (`CLAUDE_PROJECT_DIR`), hears nothing of A:
/// not A's messages to everyone at work there, not A's changes. A message
/// to everyone (no recipient, no anchor) reaches the agents at work in A;
/// one anchored to code nobody is on reaches nobody, and its sender is told
/// so (since v110: it used to go to everyone at work, as noise).
#[test]
fn a_session_in_repo_b_hears_nothing_from_repo_a() {
    let m = Machine::new("leak");
    m.origin("a", &[("a.py", "def alpha():\n    return 1\n")]);
    m.origin("b", &[("b.py", "def beta():\n    return 2\n")]);
    let (a1, a2, b1) = (m.clone_of("a", "a-one"), m.clone_of("a", "a-two"), m.clone_of("b", "b-one"));
    // repo A still carries a per-repo setup of its own (the retired one):
    // looking for that above every candidate first rooted B's session in A
    for root in [&a1, &a2] {
        std::fs::create_dir_all(root.join(".collide")).unwrap();
        std::fs::write(root.join(".collide/config.json"), json!({"server_url": m.url, "repo_id": "github.com/acme/a"}).to_string()).unwrap();
    }
    std::fs::write(m.home.join(".collide/credentials.json"), json!({m.url.clone(): m.token}).to_string()).unwrap();
    let sa1 = Agent::new(&m, &a1, "sa-1");
    let sa2 = Agent::new(&m, &a2, "sa-2");
    let mut sb1 = Agent::new(&m, &b1, "sb-1");
    // B's agent was started from a session in repo A
    sb1.extra.push(("CLAUDE_PROJECT_DIR".into(), a1.to_string_lossy().to_string()));
    sa1.read("a.py");
    sa2.read("a.py");
    sb1.read("b.py");
    sa1.edit("a.py", "return 1", "return 10").expect("a write in A");

    // a message to everyone at work in A, from A's shell
    let send = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cork-hook"));
        command.args(args).env_clear().envs(m.env()).env("CORK_SESSION", "sa-1").current_dir(&a1);
        let out = command.output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let sent = send(&["message", "news from A"]);
    assert!(sent.contains("sent to"), "{sent}");
    let anchored = send(&["message", "--about", "nowhere.py::f", "heads up on nowhere"]);
    assert!(anchored.contains("nobody is working on nowhere.py; not delivered"), "nobody on that code: nobody hears it: {anchored}");

    // A's other agent hears the message to everyone, on its next step, and
    // nothing of the one about code nobody is on
    let heard = sa2.read("a.py");
    assert!(heard.contains("news from A") && !heard.contains("heads up on nowhere"), "{heard}");
    // B's agent hears nothing of A, at its next read or its next write
    let in_b = format!("{}{}", sb1.read("b.py"), sb1.edit("b.py", "return 2", "return 20").expect("b writes"));
    for leaked in ["news from A", "heads up", "a.py", "acme/a"] {
        assert!(!in_b.contains(leaked), "repo B heard {leaked:?} from repo A: {in_b}");
    }
    let _ = std::fs::remove_dir_all(&m.base);
}

const MONEY: &str = "def fmt(x):\n    return f\"${x}\"\n";
const APP: &str = "from money import fmt\n\n\ndef tile(x):\n    return fmt(x)\n";
const ORDERS: &str = "def orders():\n    return []\n";

/// Study 8: the same `Δ fmt signature …` line reached most of 16 agents,
/// some of them several times. A teammate's new signature reaches the
/// session that calls it (it read app.py), once; a session that never went
/// near the code hears nothing.
#[test]
fn a_delta_reaches_only_the_session_whose_working_set_touches_it_and_only_once() {
    let m = Machine::new("delta");
    m.origin("tiles", &[("money.py", MONEY), ("app.py", APP), ("orders.py", ORDERS)]);
    let (a, b, c) = (m.clone_of("tiles", "alice"), m.clone_of("tiles", "bob"), m.clone_of("tiles", "carol"));
    let alice = Agent::new(&m, &a, "alice-1");
    let bob = Agent::new(&m, &b, "bob-1");
    let carol = Agent::new(&m, &c, "carol-1");
    alice.start();
    bob.start();
    carol.start();
    alice.read("money.py");
    bob.read("app.py");
    carol.read("orders.py");
    alice.edit("money.py", "def fmt(x):", "def fmt(x, currency):").expect("alice re-signatures fmt");

    let told = bob.read("orders.py");
    assert!(told.contains("fmt") && told.contains("def fmt(x, currency)"), "the caller hears it: {told}");
    let again = format!("{}{}", bob.read("orders.py"), bob.read("app.py"));
    assert!(!again.contains("def fmt(x, currency)"), "said once: {again}");
    let carol_heard = format!("{}{}", carol.read("orders.py"), carol.read("orders.py"));
    assert!(!carol_heard.contains("fmt"), "carol's working set never touched fmt: {carol_heard}");
    let _ = std::fs::remove_dir_all(&m.base);
}

impl<'a> Agent<'a> {
    /// Commit what it wrote and push: the gate rewrites `git push` into
    /// `collide land`, as it does for Claude Code; run here directly.
    fn land(&self, message: &str) -> String {
        sh(&self.root, &["add", "-A"]);
        sh(&self.root, &["commit", "-qm", message]);
        let out = Command::new(env!("CARGO_BIN_EXE_cork-hook"))
            .args(["land", "--session", &self.session, "--test", "true"])
            .env_clear().envs(self.m.env()).current_dir(&self.root).output().unwrap();
        let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "{said}");
        said
    }
}

const SHOP: &[(&str, &str)] = &[
    ("api/router.py", "def handle(req):\n    return req\n\n\ndef route_b(req):\n    return 2\n"),
    ("billing/pricing.py", "def price_for(x):\n    return x\n"),
    ("tests/test_api.py", "def test_handle():\n    assert 1\n"),
];

/// Everything one session heard over its next steps: what its hooks printed.
fn heard(agent: &Agent) -> String {
    format!("{}{}{}", agent.read("billing/pricing.py"), agent.read("billing/pricing.py"), agent.read("api/router.py"))
}

/// Study 8, second rerun: about 35 manual pulls a round, each after a
/// change notice, a landing notice or git-sync. A teammate lands work on
/// files this session never wrote: it hears nothing of it (landing takes
/// it in when it pushes), and nothing it hears says "pull".
#[test]
fn a_teammates_landing_elsewhere_is_no_notice_and_no_pull_cue() {
    let m = Machine::new("land-quiet");
    m.origin("shop", SHOP);
    let (a, b) = (m.clone_of("shop", "alice"), m.clone_of("shop", "bob"));
    let alice = Agent::new(&m, &a, "alice-1");
    let bob = Agent::new(&m, &b, "bob-1");
    bob.read("api/router.py");
    bob.edit("billing/pricing.py", "return x", "return x * 2").expect("bob works on pricing");
    alice.read("api/router.py");
    alice.edit("api/router.py", "return 2", "return 3").expect("alice changes a route");
    alice.land("route_b returns 3");
    let said = heard(&bob);
    assert!(!said.to_lowercase().contains("pull"), "{said}");
    assert!(!said.contains("landed") && !said.contains("route_b"), "not bob's code: {said}");
    let _ = std::fs::remove_dir_all(&m.base);
}

/// A teammate lands a change to a function this session changed too: it is
/// told its next land rebases onto it, never to pull.
#[test]
fn a_teammates_landing_on_a_file_this_session_edits_says_landing_will_rebase() {
    let m = Machine::new("land-same");
    m.origin("shop", SHOP);
    let (a, b) = (m.clone_of("shop", "alice"), m.clone_of("shop", "bob"));
    let alice = Agent::new(&m, &a, "alice-1");
    let bob = Agent::new(&m, &b, "bob-1");
    bob.read("api/router.py");
    bob.edit("api/router.py", "    return req\n", "    return req or {}\n").expect("bob edits handle");
    alice.read("api/router.py");
    alice.edit("api/router.py", "def handle(req):", "def handle(req, user=None):").expect("alice changes handle");
    alice.land("handle takes a user");
    let said = heard(&bob);
    assert!(said.contains("Your next land rebases onto it") || said.contains("landing rebases onto them when you push"), "{said}");
    assert!(!said.to_lowercase().contains("pull"), "{said}");
    let _ = std::fs::remove_dir_all(&m.base);
}

/// A teammate adds tests to the shared test file this session also added a
/// test to: no delta, no landing notice, no git-sync.
#[test]
fn a_teammate_adding_tests_to_a_shared_test_file_is_not_news() {
    let m = Machine::new("land-tests");
    m.origin("shop", SHOP);
    let (a, b) = (m.clone_of("shop", "alice"), m.clone_of("shop", "bob"));
    let alice = Agent::new(&m, &a, "alice-1");
    let bob = Agent::new(&m, &b, "bob-1");
    bob.read("tests/test_api.py");
    bob.edit("tests/test_api.py", "    assert 1\n", "    assert 1\n\n\ndef test_bob():\n    assert 2\n").expect("bob adds a test");
    alice.read("tests/test_api.py");
    alice.edit("tests/test_api.py", "def test_handle():", "def test_alice():\n    assert 3\n\n\ndef test_handle():").expect("alice adds a test");
    alice.land("a test of alice's");
    let said = format!("{}{}", bob.read("tests/test_api.py"), heard(&bob));
    assert!(!said.contains("test_alice") && !said.contains("landed") && !said.contains("git-sync"), "{said}");
    assert!(!said.to_lowercase().contains("pull"), "{said}");
    let _ = std::fs::remove_dir_all(&m.base);
}

/// A manual `git pull` mid-task, with landing on: it runs, with one short
/// line, once a session. Never a block.
#[test]
fn a_manual_pull_mid_task_gets_the_note_once() {
    let m = Machine::new("land-pull");
    m.origin("shop", SHOP);
    let b = m.clone_of("shop", "bob");
    let bob = Agent::new(&m, &b, "bob-1");
    bob.read("api/router.py");
    let (code, said) = bob.bash_gate("git pull --rebase origin main");
    assert_eq!(code, 0, "never a block: {said}");
    assert!(said.contains("Cork lands your push and rebases for you; a manual pull now can conflict. Carry on and push when done."), "{said}");
    let (code, again) = bob.bash_gate("git fetch origin && git rebase origin/main");
    assert_eq!(code, 0);
    assert!(!again.contains("Cork lands your push"), "once a session: {again}");
    // resolving a conflict is not a manual pull
    let carol = Agent::new(&m, &b, "carol-1");
    let (_, resolving) = carol.bash_gate("git rebase --continue");
    assert!(!resolving.contains("Cork lands your push"), "{resolving}");
    let _ = std::fs::remove_dir_all(&m.base);
}
