//! Several sessions in ONE checkout, end to end: the real hook program run
//! the way Claude Code runs it (the gate before each tool call, the report
//! after), real commands run by a real shell in a real git repo, and a
//! stand-in server that keeps every body it is sent.
//!
//! Acme's report (hooks v88), turned into tests: one session's `npm test`
//! was credited with another session's CLAUDE.md paragraph and the commit
//! guard then blocked the real author; her `tsx` script's edits were
//! credited to nobody; and the server heard a dozen "your code imports it"
//! worth of writes she never made. Each test here fails on v88.
//!
//! `CORK_TEST_HOOK_BIN` runs the same tests against another build of the
//! program (the released one, to see them fail).
#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const EMAIL: &str = "e1a1e1a1-0000-4000-8000-000000000001";
const FOOD: &str = "f00df00d-0000-4000-8000-000000000002";
const THIRD: &str = "7h1rd000-0000-4000-8000-000000000003";

/// A server that answers every POST with `{"ok": true}` and keeps (path, body).
fn stub() -> (u16, Arc<Mutex<Vec<(String, Value)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let kept = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let kept = kept.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    return;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                    kept.lock().unwrap().push((path, v));
                }
                let text = json!({"ok": true}).to_string();
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            });
        }
    });
    (port, seen)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).stderr(Stdio::null()).output().unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn have(program: &str) -> bool {
    Command::new("sh").args(["-c", &format!("command -v {program}")]).stdout(Stdio::null()).status().is_ok_and(|s| s.success())
}

struct Checkout {
    base: PathBuf,
    home: PathBuf,
    root: PathBuf,
    path: String,
    port: u16,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    calls: std::cell::Cell<u32>,
}

impl Drop for Checkout {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// What one hook run said: exit code, stdout, stderr.
struct Said {
    code: i32,
    out: String,
    err: String,
}

impl Checkout {
    /// A repo with a committed CLAUDE.md and a few source files, the way
    /// Acme's looked, and a `tsx` on PATH (a Node runner Cork's list of
    /// write idioms never knew).
    fn new(name: &str) -> Option<Checkout> {
        if !have("node") || !have("npm") {
            eprintln!("skipped: needs node and npm");
            return None;
        }
        let base = std::env::temp_dir().join(format!("cork-shared-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (home, root, bin) = (base.join("home"), base.join("app"), base.join("bin"));
        for dir in [&home, &root.join("src"), &root.join("scripts"), &bin] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let files: [(&str, &str); 8] = [
            ("CLAUDE.md", "# App\n\nRules for agents.\n\n## Testing\n\nRun npm test.\n"),
            ("src/worker.ts", "// jobs\nexport const jobs: string[] = [];\n\nexport function run() {\n  return jobs.length;\n}\n"),
            ("src/gmailDrafts.ts", "export function draft(to: string) {\n  return { to };\n}\n"),
            ("src/foodLog.ts", "export function logFood(name: string) {\n  return name;\n}\n"),
            ("package.json", "{\"name\": \"app\", \"private\": true, \"scripts\": {\"test\": \"node test.js\"}}\n"),
            ("test.js", "const fs = require('fs');\nfor (const f of ['CLAUDE.md', 'src/worker.ts']) fs.readFileSync(f, 'utf8');\nconsole.log('ok');\n"),
            (".gitignore", "node_modules/\n"),
            ("README.md", "# app\n"),
        ];
        for (rel, text) in files {
            std::fs::write(root.join(rel), text).unwrap();
        }
        // the email session's scripts: none names on its command line what it writes
        std::fs::write(
            root.join("scripts/add-doc-note.ts"),
            "const fs = require('fs');\n\
             fs.appendFileSync('CLAUDE.md', '\\n## Email drafts\\n\\nDrafts are saved through gmailDrafts, never sent.\\n');\n\
             const w = fs.readFileSync('src/worker.ts', 'utf8');\n\
             fs.writeFileSync('src/worker.ts', w.replace('// jobs\\n', '// jobs\\nexport const draftJob = \"email\";\\n'));\n\
             const g = fs.readFileSync('src/gmailDrafts.ts', 'utf8');\n\
             fs.writeFileSync('src/gmailDrafts.ts', g + '\\nexport function send() {\\n  return false;\\n}\\n');\n",
        )
        .unwrap();
        // writes a file whose name it builds: no command and no script text names it
        std::fs::write(
            root.join("scripts/gen.ts"),
            "const fs = require('fs');\nfs.writeFileSync(['src/gen', 'erated', '.ts'].join(''), 'export const built = 1;\\n');\n",
        )
        .unwrap();
        // writes eight files
        std::fs::write(
            root.join("scripts/many.ts"),
            "const fs = require('fs');\nfor (let i = 0; i < 8; i++) fs.writeFileSync(`src/part${i}.ts`, `export const p${i} = ${i};\\n`);\n",
        )
        .unwrap();
        std::fs::write(bin.join("tsx"), "#!/bin/sh\nexec node - < \"$1\"\n").unwrap();
        Command::new("chmod").args(["+x", &bin.join("tsx").to_string_lossy()]).status().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);
        let (port, seen) = stub();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        Some(Checkout { base, home, root, path, port, seen, calls: std::cell::Cell::new(0) })
    }

    fn bin(&self) -> String {
        std::env::var("CORK_TEST_HOOK_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_cork-hook").to_string())
    }

    fn env(&self) -> cork_hooks::config::Env {
        [("HOME".to_string(), self.home.to_string_lossy().into_owned())].into_iter().collect()
    }

    fn hook(&self, sub: &str, input: Value) -> Said {
        let mut child = Command::new(self.bin())
            .arg(sub)
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", &self.home)
            .env("CORK_SERVER_URL", format!("http://127.0.0.1:{}", self.port))
            .env("CORK_TOKEN", "cat_test_token_0123456789abcdef")
            .env("CORK_REPO_ID", "github.com/acme/app")
            .env("CORK_PROTOCOL", "0")
            .env("CORK_LISTEN", "0")
            .env("CORK_NATIVE_INSTALL", "0")
            .env("CORK_NO_BROWSER", "1")
            .env("CORK_AUTO_PULL", "0")
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the hook runs");
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        Said {
            code: out.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&out.stdout).into_owned(),
            err: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn next_id(&self) -> String {
        self.calls.set(self.calls.get() + 1);
        format!("toolu_{:04}", self.calls.get())
    }

    fn event(&self, event: &str, session: &str, id: &str, tool: &str, input: Value) -> Value {
        let mut v = json!({
            "hook_event_name": event, "session_id": session, "tool_use_id": id, "tool_name": tool,
            "tool_input": input, "cwd": self.root, "transcript_path": "",
        });
        if event == "PostToolUse" {
            v["tool_response"] = json!({"stdout": "", "stderr": "", "interrupted": false});
        }
        v
    }

    /// The gate's half of a Bash call: before the command runs.
    fn bash_starts(&self, session: &str, command: &str) -> (String, Said) {
        let id = self.next_id();
        let said = self.hook("gate", self.event("PreToolUse", session, &id, "Bash", json!({"command": command})));
        (id, said)
    }

    /// The command itself, as the harness runs it.
    fn run(&self, command: &str) {
        let out = Command::new("sh").args(["-c", command]).current_dir(&self.root).env("PATH", &self.path)
            .env("HOME", &self.home).output().unwrap();
        assert!(out.status.success(), "{command}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// The report's half: after the command ran.
    fn bash_ends(&self, session: &str, id: &str, command: &str) {
        self.hook("report", self.event("PostToolUse", session, id, "Bash", json!({"command": command})));
    }

    /// A whole Bash call: gate, the command, report.
    fn bash(&self, session: &str, command: &str) {
        let (id, said) = self.bash_starts(session, command);
        assert_eq!(said.code, 0, "the gate let `{command}` run: {}{}", said.out, said.err);
        self.run(command);
        self.bash_ends(session, &id, command);
    }

    /// A whole Edit call: gate, the edit, report.
    fn edit(&self, session: &str, rel: &str, old: &str, new: &str) {
        let id = self.next_id();
        let path = self.root.join(rel);
        let input = json!({"file_path": path, "old_string": old, "new_string": new});
        let said = self.hook("gate", self.event("PreToolUse", session, &id, "Edit", input.clone()));
        assert_eq!(said.code, 0, "the gate let the edit of {rel} through: {}{}", said.out, said.err);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(old));
        std::fs::write(&path, text.replacen(old, new, 1)).unwrap();
        self.hook("report", self.event("PostToolUse", session, &id, "Edit", input));
    }

    /// The files whose lines the journal gives `session`.
    fn credited(&self, session: &str) -> BTreeSet<String> {
        cork_hooks::journal::load(&self.root, &self.env())
            .into_iter()
            .filter(|e| e.session == session && e.hunks.iter().any(|(_, add)| !add.is_empty()))
            .map(|e| e.path)
            .collect()
    }

    /// The files `session` reported to the server as written.
    fn reported(&self, session: &str) -> BTreeSet<String> {
        self.seen.lock().unwrap().iter()
            .filter(|(p, b)| p == "/report" && b["session"] == json!(session))
            .filter_map(|(_, b)| b["path"].as_str().map(str::to_string))
            .collect()
    }

    /// Whether the gate holds `session`'s commit, and why.
    fn commit_held(&self, session: &str, command: &str) -> Option<String> {
        let (_, said) = self.bash_starts(session, command);
        let text = format!("{}{}", said.out, said.err);
        (said.code != 0 || text.contains("\"deny\"") || text.contains("Cork held")).then_some(text)
    }

    /// Who owns each changed hunk of `rel`, for `session`.
    fn owners(&self, rel: &str, session: &str) -> Vec<cork_hooks::journal::Owner> {
        let only = [rel.to_string()];
        cork_hooks::journal::plan(&self.root, cork_hooks::journal::Base::Head, session, Some(&only), &self.env())
            .into_iter()
            .flat_map(|p| p.hunks.into_iter().map(|(_, o)| o))
            .collect()
    }
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Acme, exactly: the email session's `tsx` script writes a CLAUDE.md
/// paragraph, worker.ts and gmailDrafts.ts; seconds later the food-log
/// session runs `npm test`, which writes nothing. v88 credited none of it to
/// her and all of it to food-log, and then held her commit.
#[test]
fn a_test_run_right_after_a_teammates_script_takes_none_of_its_work() {
    let Some(c) = Checkout::new("acme") else { return };
    c.bash(EMAIL, "tsx scripts/add-doc-note.ts");
    c.bash(FOOD, "npm test");

    let hers = set(&["CLAUDE.md", "src/worker.ts", "src/gmailDrafts.ts"]);
    assert_eq!(c.credited(EMAIL), hers, "her script's edits are hers");
    assert!(c.credited(FOOD).is_empty(), "npm test wrote nothing: {:?}", c.credited(FOOD));
    assert_eq!(c.reported(EMAIL), hers, "the server hears her writes from her");
    assert!(c.reported(FOOD).is_empty(), "and none of them from food-log: {:?}", c.reported(FOOD));
    // her commit goes through; food-log is told those lines are not its own
    assert_eq!(c.commit_held(EMAIL, "git add CLAUDE.md src/worker.ts src/gmailDrafts.ts && git commit -m 'Document drafts'"), None);
    let held = c.commit_held(FOOD, "git commit -am 'food log'").expect("food-log's sweeping commit is held");
    assert!(held.contains("CLAUDE.md"), "{held}");
    for rel in &hers {
        assert_eq!(c.owners(rel, EMAIL), vec![cork_hooks::journal::Owner::Mine], "{rel}");
    }
}

/// The same with an Edit instead of a script: an edit, then another
/// session's command within the old 20-second window.
#[test]
fn a_command_right_after_a_teammates_edit_takes_none_of_it() {
    let Some(c) = Checkout::new("edit-then-test") else { return };
    c.edit(EMAIL, "CLAUDE.md", "## Testing\n", "## Testing\n\nDrafts never send on their own.\n");
    c.bash(FOOD, "npm test");
    c.bash(FOOD, "node -e \"require('fs').readFileSync('CLAUDE.md')\"");
    assert_eq!(c.credited(EMAIL), set(&["CLAUDE.md"]));
    assert!(c.credited(FOOD).is_empty(), "{:?}", c.credited(FOOD));
    assert!(c.reported(FOOD).is_empty(), "{:?}", c.reported(FOOD));
    assert_eq!(c.commit_held(EMAIL, "git add CLAUDE.md && git commit -m docs"), None);
}

/// A script a session runs is that session's, whatever runner it uses:
/// v88 knew `npm`, `node` and `python`, not `tsx`, so her work was nobody's.
#[test]
fn a_script_run_by_a_runner_cork_never_heard_of_is_still_its_sessions_work() {
    let Some(c) = Checkout::new("tsx") else { return };
    c.bash(EMAIL, "tsx scripts/add-doc-note.ts");
    assert_eq!(c.credited(EMAIL), set(&["CLAUDE.md", "src/worker.ts", "src/gmailDrafts.ts"]));
    // and a runner hidden behind a package script, or one cd away
    c.bash(THIRD, "cd src && cd .. && tsx scripts/gen.ts");
    assert_eq!(c.credited(THIRD), set(&["src/generated.ts"]));
}

/// Two commands at once, and a file changed: the one whose command (or
/// whose script's text) names it wrote it. The other is credited nothing.
#[test]
fn of_two_commands_running_at_once_the_one_that_names_a_file_wrote_it() {
    let Some(c) = Checkout::new("overlap") else { return };
    // food-log's test run starts, and is still running when...
    let (food_call, said) = c.bash_starts(FOOD, "npm test");
    assert_eq!(said.code, 0);
    // ...the email session's whole script call happens
    c.bash(EMAIL, "tsx scripts/add-doc-note.ts");
    c.run("npm test");
    c.bash_ends(FOOD, &food_call, "npm test");
    assert_eq!(c.credited(EMAIL), set(&["CLAUDE.md", "src/worker.ts", "src/gmailDrafts.ts"]));
    assert!(c.credited(FOOD).is_empty(), "{:?}", c.credited(FOOD));
    assert!(c.reported(FOOD).is_empty(), "{:?}", c.reported(FOOD));
    // and the other way round: hers starts first, food-log's runs inside it
    c.run("git checkout -q -- . && git clean -qfd src");
    let (email_call, _) = c.bash_starts(EMAIL, "tsx scripts/add-doc-note.ts");
    c.bash(FOOD, "npm test");
    c.run("tsx scripts/add-doc-note.ts");
    c.bash_ends(EMAIL, &email_call, "tsx scripts/add-doc-note.ts");
    assert!(c.credited(FOOD).is_empty(), "{:?}", c.credited(FOOD));
}

/// Two commands at once and a file neither names: nobody can say whose it
/// is, so it is nobody's. Unattributed never blocks a commit; a wrong
/// owner did.
#[test]
fn a_file_neither_running_command_names_is_nobodys() {
    let Some(c) = Checkout::new("nobody") else { return };
    let (food_call, _) = c.bash_starts(FOOD, "npm test");
    let (gen_call, _) = c.bash_starts(THIRD, "tsx scripts/gen.ts");
    c.run("tsx scripts/gen.ts");
    c.run("npm test");
    c.bash_ends(THIRD, &gen_call, "tsx scripts/gen.ts");
    c.bash_ends(FOOD, &food_call, "npm test");
    assert!(c.credited(THIRD).is_empty() && c.credited(FOOD).is_empty());
    assert!(c.reported(FOOD).is_empty() && c.reported(THIRD).is_empty());
    assert_eq!(c.commit_held(THIRD, "git add src/generated.ts && git commit -m gen"), None);
    assert_eq!(c.commit_held(FOOD, "git add src/generated.ts && git commit -m gen"), None);
}

/// A teammate's Edit while a command runs is the teammate's.
#[test]
fn an_edit_made_while_another_sessions_command_runs_stays_the_editors() {
    let Some(c) = Checkout::new("edit-during") else { return };
    let (food_call, _) = c.bash_starts(FOOD, "npm test");
    c.edit(EMAIL, "src/gmailDrafts.ts", "return { to };", "return { to, draft: true };");
    c.run("npm test");
    c.bash_ends(FOOD, &food_call, "npm test");
    assert_eq!(c.credited(EMAIL), set(&["src/gmailDrafts.ts"]));
    assert!(c.credited(FOOD).is_empty(), "{:?}", c.credited(FOOD));
    // even when the edit's own report comes after the command's
    let (food_call, _) = c.bash_starts(FOOD, "npm test");
    let id = c.next_id();
    let path = c.root.join("src/worker.ts");
    let input = json!({"file_path": path, "old_string": "jobs.length", "new_string": "jobs.length + 0"});
    c.hook("gate", c.event("PreToolUse", EMAIL, &id, "Edit", input.clone()));
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replacen("jobs.length", "jobs.length + 0", 1)).unwrap();
    c.bash_ends(FOOD, &food_call, "npm test");
    c.hook("report", c.event("PostToolUse", EMAIL, &id, "Edit", input));
    assert!(c.credited(FOOD).is_empty(), "{:?}", c.credited(FOOD));
    assert_eq!(c.credited(EMAIL), set(&["src/gmailDrafts.ts", "src/worker.ts"]));
}

/// The lines a command adds are its own, and nothing else in the file: a
/// change already there before it ran (here a person's, which no session
/// journaled) is not swept into its claim.
#[test]
fn a_command_is_credited_with_its_own_lines_not_a_files_older_changes() {
    let Some(c) = Checkout::new("exact-before") else { return };
    // a person's edit at the top, journaled by no one
    let text = std::fs::read_to_string(c.root.join("CLAUDE.md")).unwrap();
    std::fs::write(c.root.join("CLAUDE.md"), text.replace("Rules for agents.\n", "Rules for agents, by a person.\n")).unwrap();
    std::thread::sleep(Duration::from_millis(30));
    c.bash(EMAIL, "tsx scripts/add-doc-note.ts");
    let owners = c.owners("CLAUDE.md", EMAIL);
    assert!(owners.contains(&cork_hooks::journal::Owner::Mine), "{owners:?}");
    assert!(owners.contains(&cork_hooks::journal::Owner::Nobody), "the person's line is nobody's: {owners:?}");
}

/// A command that writes more than the five files the server hears at once:
/// every one is journaled as its own, and the rest are owed to the server.
#[test]
fn every_file_a_command_writes_is_journaled_and_the_server_is_owed_the_rest() {
    let Some(c) = Checkout::new("many") else { return };
    c.bash(EMAIL, "tsx scripts/many.ts");
    let all: BTreeSet<String> = (0..8).map(|i| format!("src/part{i}.ts")).collect();
    assert_eq!(c.credited(EMAIL), all);
    // five go live; the rest are owed, and go with what the budget leaves
    let live = c.reported(EMAIL);
    assert!(live.len() >= 5 && live.is_subset(&all), "{live:?}");
    // the session's next write sends what is owed
    c.edit(EMAIL, "README.md", "# app\n", "# app\n\nParts.\n");
    let after = c.reported(EMAIL);
    assert!(all.is_subset(&after), "the rest went with the next write: {after:?}");
}

/// A command that only reads takes no snapshot, so it can never contest a
/// teammate's file; a sweeping commit is still held for the right reason.
#[test]
fn a_read_only_command_takes_no_snapshot_and_contests_nothing() {
    let Some(c) = Checkout::new("read-only") else { return };
    let (grep_call, _) = c.bash_starts(FOOD, "git status --short && grep -rn draft src | head -5");
    let runs = cork_hooks::config::data_dir(&c.env()).join("journal");
    let snapshots = walk(&runs).into_iter().filter(|p| p.to_string_lossy().contains("/shell/runs/")).count();
    assert_eq!(snapshots, 0, "a read takes no snapshot");
    c.bash(EMAIL, "tsx scripts/gen.ts");
    c.bash_ends(FOOD, &grep_call, "git status --short && grep -rn draft src | head -5");
    assert_eq!(c.credited(EMAIL), set(&["src/generated.ts"]), "her unnamed file is still hers");
}

/// With no snapshot (a gate that never ran), a command is credited only with
/// what it names while another session works in the checkout; alone, the
/// clock's guess stands as before.
#[test]
fn without_a_snapshot_only_named_files_count_while_a_teammate_is_at_work() {
    let Some(c) = Checkout::new("fallback") else { return };
    c.edit(EMAIL, "src/gmailDrafts.ts", "return { to };", "return { to, ok: 1 };");
    // food-log's call whose gate never ran: a sed it names, and a script it doesn't
    let command = "sed -i.bak 's/name;/name.trim();/' src/foodLog.ts && rm src/foodLog.ts.bak && tsx scripts/gen.ts";
    c.run(command);
    c.bash_ends(FOOD, "toolu_nogate", command);
    assert_eq!(c.credited(FOOD), set(&["src/foodLog.ts"]), "only what it names");
    assert_eq!(c.credited(EMAIL), set(&["src/gmailDrafts.ts"]));
}

/// A background command (a dev server, a watcher) keeps running after its
/// call returns: until it is done, what changes and nobody names is nobody's.
#[test]
fn a_background_command_still_running_contests_unnamed_changes() {
    let Some(c) = Checkout::new("background") else { return };
    let id = c.next_id();
    let input = json!({"command": "npm run dev", "run_in_background": true});
    c.hook("gate", c.event("PreToolUse", FOOD, &id, "Bash", input.clone()));
    c.hook("report", c.event("PostToolUse", FOOD, &id, "Bash", input));
    c.bash(THIRD, "tsx scripts/gen.ts");
    assert!(c.credited(THIRD).is_empty(), "{:?}", c.credited(THIRD));
    // what it names is still its own
    c.bash(EMAIL, "tsx scripts/add-doc-note.ts");
    assert_eq!(c.credited(EMAIL), set(&["CLAUDE.md", "src/worker.ts", "src/gmailDrafts.ts"]));
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// The cost: the gate's snapshot on a tree with many dirty files stays well
/// inside its budget once the copies exist.
#[test]
fn the_snapshot_is_cheap_on_a_tree_with_many_dirty_files() {
    let Some(c) = Checkout::new("cost") else { return };
    for i in 0..400 {
        std::fs::write(c.root.join(format!("src/dirty{i}.ts")), format!("export const d{i} = {i};\n").repeat(40)).unwrap();
    }
    let first = Instant::now();
    c.bash_starts(EMAIL, "npm test");
    let cold = first.elapsed();
    // the best of three, so a busy machine running the other tests does not decide
    let warm = (0..3)
        .map(|_| {
            let at = Instant::now();
            c.bash_starts(EMAIL, "npm run lint");
            at.elapsed()
        })
        .min()
        .unwrap();
    eprintln!("gate with 400 dirty files: cold {cold:?}, warm {warm:?}");
    assert!(warm < Duration::from_millis(400), "warm snapshot took {warm:?}");
}
