//! The gate and landing features, end to end: the real hook program, run the
//! way Claude Code runs it, against the real Collide server on this machine
//! (`cork-server local`), with agents in clones of one origin.
//!
//! - a block carries its fix: the exact lines of the pending write that a
//!   teammate's landed rename makes wrong, and what each becomes;
//! - `collide land` prints a predicted conflict with a teammate's in-flight
//!   change to the same function, and lands anyway;
//! - two agents on different branches: never a block, one line, once;
//! - two agents on the same branch: still held, once.

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

/// The server, built: `cork-server` from ../cork-rs in Cork's own repo,
/// `cork` from ../free in the public one (lithometric/cork).
fn collide_server() -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (rs, bin) = if here.join("../cork-rs").exists() { (here.join("../cork-rs"), "cork-server") } else { (here.join("../free"), "cork") };
    let built = Command::new("cargo").args(["build", "--release", "-q"]).current_dir(&rs).status().expect("cargo runs");
    assert!(built.success(), "{bin} builds");
    target_dir_of(&rs).join("release").join(format!("{bin}{}", std::env::consts::EXE_SUFFIX))
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

fn sh(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
    assert!(ok.success(), "git {args:?} in {}", dir.display());
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Machine {
    base: PathBuf,
    home: PathBuf,
    _server: Proc,
}

impl Machine {
    fn new(name: &str) -> Machine {
        let base = std::env::temp_dir().join(format!("collide-gland-{name}-{}", std::process::id()));
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
        Machine { base, home, _server: Proc(child) }
    }

    fn origin(&self, name: &str, files: &[(&str, &str)]) {
        let bare = self.base.join(format!("{name}.git"));
        sh(&self.base, &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
        let seed = self.clone_of(name, &format!("{name}-seed"));
        for (path, text) in files {
            std::fs::write(seed.join(path), text).unwrap();
        }
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-qm", "seed"]);
        sh(&seed, &["push", "-q", "-u", "origin", "HEAD"]);
    }

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
}

struct Agent<'a> {
    m: &'a Machine,
    root: PathBuf,
    session: String,
}

impl<'a> Agent<'a> {
    fn new(m: &'a Machine, root: &Path, session: &str) -> Agent<'a> {
        Agent { m, root: root.to_path_buf(), session: session.into() }
    }

    fn run(&self, args: &[&str], input: Value) -> (i32, String, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cork-hook"))
            .args(args)
            .env_clear()
            .envs(self.m.env())
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
    }

    fn input(&self, event: &str, tool: &str, tool_input: Value) -> Value {
        json!({"hook_event_name": event, "session_id": self.session, "cwd": self.root.to_string_lossy(),
               "tool_name": tool, "tool_input": tool_input, "tool_response": {}})
    }

    fn read(&self, rel: &str) -> String {
        self.run(&["report"], self.input("PostToolUse", "Read", json!({"file_path": self.root.join(rel).to_string_lossy()}))).1
    }

    /// An Edit: the gate, the write if allowed, the report.
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

    /// An Edit that may be held once: the retry goes through.
    fn edit_through(&self, rel: &str, old: &str, new: &str) -> String {
        match self.edit(rel, old, new) {
            Ok(said) => said,
            Err(_) => self.edit(rel, old, new).expect("a hold is once"),
        }
    }

    fn land(&self) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_cork-hook"))
            .args(["land", "--session", &self.session, "--test", "true"])
            .env_clear()
            .envs(self.m.env())
            .current_dir(&self.root)
            .output()
            .unwrap();
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    }
}

const TOTAL: &str = "def amount_due(lines):\n    return sum(lines)\n\n\ndef invoice(lines):\n    return amount_due(lines) * 2\n";

/// Alice renames `amount_due` and lands it; Bob, on an older copy, edits the
/// line that calls it. The block names that line of HIS write and what it
/// becomes, and the declaration his copy still has: no re-read needed.
#[test]
fn a_block_lists_the_exact_lines_of_the_pending_write() {
    let m = Machine::new("fix");
    m.origin("till", &[("total.py", TOTAL)]);
    let (a, b) = (m.clone_of("till", "alice"), m.clone_of("till", "bob"));
    let alice = Agent::new(&m, &a, "alice-1");
    let bob = Agent::new(&m, &b, "bob-1");
    alice.read("total.py");
    bob.read("total.py");

    alice.edit("total.py", "def amount_due(lines):", "def checkout_total(lines):").expect("alice renames");
    alice.edit_through("total.py", "return amount_due(lines) * 2", "return checkout_total(lines) * 2");
    sh(&a, &["commit", "-qam", "rename amount_due"]);
    let (landed, said) = alice.land();
    assert!(landed, "{said}");
    sh(&b, &["fetch", "-q", "origin"]);

    let stopped = bob.edit("total.py", "return amount_due(lines) * 2", "return amount_due(lines) * 3").unwrap_err();
    eprintln!("{stopped}");
    assert!(stopped.contains("In this write (total.py):"), "{stopped}");
    let fix: Vec<&str> = stopped.lines().skip_while(|l| !l.starts_with("In this write")).skip(1).take_while(|l| l.starts_with("  ")).collect();
    assert_eq!(fix, vec![
        "  line 6: `amount_due(lines)` → `checkout_total(lines)`",
        "  line 1: `def amount_due(lines):` → `def checkout_total(lines):`",
    ], "the write's own line first: {stopped}");
    assert!(!stopped.to_lowercase().contains("pull"), "landing is on: {stopped}");
    bob.edit("total.py", "return amount_due(lines) * 2", "return amount_due(lines) * 3").expect("held once");
    let _ = std::fs::remove_dir_all(&m.base);
}

const BOOKINGS: &str = "def list_bookings(store):\n    return list(store)\n\n\ndef create_booking(store, booking):\n    store.append(booking)\n    return booking\n";

/// Bob re-signatures `create_booking` and has not landed. Alice changes its
/// body, commits and lands: land says Bob's change will conflict with hers,
/// at her line, and lands all the same.
#[test]
fn land_prints_a_predicted_conflict_and_still_lands() {
    let m = Machine::new("preview");
    m.origin("book", &[("bookings.py", BOOKINGS)]);
    let (a, b) = (m.clone_of("book", "alice"), m.clone_of("book", "bob"));
    let alice = Agent::new(&m, &a, "alice-1");
    let bob = Agent::new(&m, &b, "bob-1");
    alice.read("bookings.py");
    bob.read("bookings.py");
    bob.edit_through("bookings.py", "def create_booking(store, booking):", "def create_booking(store, booking, day):");

    alice.edit_through("bookings.py", "    store.append(booking)\n", "    store.append(dict(booking))\n");
    sh(&a, &["commit", "-qam", "copy the booking"]);
    let (landed, said) = alice.land();
    eprintln!("{said}");
    assert!(landed, "a prediction never stops a landing: {said}");
    let preview = said.lines().find(|l| l.starts_with("Cork merge preview:")).unwrap_or_else(|| panic!("predicted: {said}"));
    assert!(preview.contains("in-flight change to `create_booking` (signature, now `def create_booking(store, booking, day)`)"), "{preview}");
    assert!(preview.contains("at bookings.py line 5") && preview.contains("Landing anyway"), "{preview}");
    assert!(!said.to_lowercase().contains("pull"), "{said}");
    assert!(said.contains("Cork landed 1 commit(s)"), "{said}");
    let _ = std::fs::remove_dir_all(&m.base);
}

/// Alice renames `amount_due` on her feature branch; Bob, on main, changes
/// the same function. Their work meets when the branches merge: Bob is never
/// held, and hears it in one line, once. The same edit on the same branch is
/// held, once.
#[test]
fn different_branches_never_block_and_the_same_branch_holds_once() {
    let m = Machine::new("branch");
    m.origin("till", &[("total.py", TOTAL)]);
    let (a, b, c) = (m.clone_of("till", "alice"), m.clone_of("till", "bob"), m.clone_of("till", "carol"));
    sh(&a, &["checkout", "-q", "-b", "feat/x"]);
    let alice = Agent::new(&m, &a, "alice-1");
    let bob = Agent::new(&m, &b, "bob-1");
    let carol = Agent::new(&m, &c, "carol-1");
    for agent in [&alice, &bob, &carol] {
        agent.read("total.py");
    }
    alice.edit("total.py", "def amount_due(", "def checkout_total(").expect("alice renames on feat/x");

    // carol is on feat/x too: the same branch as alice, held once
    sh(&c, &["checkout", "-q", "-b", "feat/x"]);
    let stopped = carol.edit("total.py", "return sum(lines)", "return sum(lines) * 1").unwrap_err();
    assert!(stopped.contains("amount_due") && stopped.contains("not pushed"), "{stopped}");
    carol.edit("total.py", "return sum(lines)", "return sum(lines) * 1").expect("the retry goes through");

    // bob, on main: never held, told once which branch it is on, and the
    // lint does not say it again
    let said = bob.edit("total.py", "return sum(lines)", "return sum(lines) + 0").expect("another branch never blocks");
    eprintln!("{said}");
    assert!(said.contains("is changing `amount_due` in total.py on branch feat/x; this will conflict when the branches merge"), "{said}");
    assert_eq!(said.matches("feat/x").count(), 1, "one line, for both teammates there: {said}");
    assert!(!said.contains("write against"), "{said}");
    let again = bob.edit("total.py", "return sum(lines) + 0", "return sum(lines) + 1").expect("still never blocks");
    assert!(!again.contains("feat/x") && !again.contains("checkout_total"), "once per file and branch: {again}");
    let _ = std::fs::remove_dir_all(&m.base);
}
