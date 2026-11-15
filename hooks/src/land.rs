//! `collide-hook land`: get the agent's commits into origin without the model.
//!
//! Study 5 measured where a concurrent agent's session goes once Cork has
//! cut the searching: over half its tool calls were landing. Push, rejected,
//! pull --rebase, rerun the tests, push, rejected again: each lap a message
//! that replays the whole context. This runs that loop as a subprocess and
//! answers once:
//!
//! 1. take the repo's push turn from the server, so concurrent agents queue
//!    instead of racing (fail open: no server, no turn, land anyway);
//! 2. fetch; when teammates' commits came in, rebase onto them. A conflict
//!    aborts cleanly and hands back only the conflicting hunks;
//! 3. adapt the agent's own commits to a teammate's rename that has landed
//!    (the lint's registry names it; git confirms origin no longer uses the
//!    old name), as its own commit;
//! 4. when anything came in, run the repo's tests: the agent tested its own
//!    tree already, not the merge;
//! 5. push; a lost race goes round again, up to four times.
//!
//! The gate turns an agent's plain `git push` into this (`rewrite`), so no
//! agent has to learn a command. Native only: the Python fallback hook never
//! rewrites, and a push there stays a push.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::http;
use crate::report::user_agent;

pub(crate) const ATTEMPTS: usize = 4;
const MAX_TURN_WAIT_S: f64 = 240.0;
pub(crate) const TEST_MAX_S: f64 = 600.0;
const CONFLICT_LINES: usize = 40;
pub(crate) const TEST_TAIL_LINES: usize = 40;

/// (ok, stdout, stderr) of one git command, bounded.
pub(crate) fn git(root: &Path, args: &[&str], timeout_s: u64) -> (bool, String, String) {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // a fetch or push runs a remote helper and a credential helper: on a
    // timeout the whole group goes, never just git (crate::git::own_group)
    crate::git::own_group(&mut command);
    let child = command.spawn();
    let Ok(mut child) = child else { return (false, String::new(), "git could not start".into()) };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > Duration::from_secs(timeout_s) => {
                crate::git::kill_group(&mut child);
                return (false, String::new(), format!("git {} timed out", args.join(" ")));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return (false, String::new(), String::new()),
        }
    }
    match child.wait_with_output() {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Err(_) => (false, String::new(), String::new()),
    }
}

pub(crate) fn git_out(root: &Path, args: &[&str]) -> String {
    let (ok, out, _) = git(root, args, 60);
    if ok { out.trim().to_string() } else { String::new() }
}

pub(crate) fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

pub(crate) fn names(items: &[String]) -> String {
    let shown: Vec<&str> = items.iter().take(4).map(String::as_str).collect();
    let more = if items.len() > 4 { format!(" and {} more", items.len() - 4) } else { String::new() };
    format!("{}{more}", shown.join(", "))
}

pub(crate) struct Server {
    pub(crate) cfg: config::Config,
    pub(crate) session: String,
}

impl Server {
    pub(crate) fn post(&self, payload: Value) -> Option<Value> {
        if !self.cfg.usable() {
            return None;
        }
        let mut body = payload;
        body["repo_id"] = json!(self.cfg.repo_id);
        body["session"] = json!(self.session);
        body["agent"] = json!("collide-hook land");
        http::post(&self.cfg.server, "/land", &self.cfg.token, &user_agent(), &body, Duration::from_secs(5)).ok()
    }
}

/// The repo's test command: `--test`, then `land_test` or `verify` in
/// .collide/config.json, then the runner the repo plainly uses.
pub fn test_command(root: &Path, explicit: &str) -> String {
    if !explicit.trim().is_empty() {
        return explicit.trim().to_string();
    }
    let cfg = config::repo_config(&root);
    for key in ["land_test", "verify"] {
        if let Some(cmd) = cfg.get(key).and_then(Value::as_str).filter(|c| !c.trim().is_empty()) {
            return cmd.trim().to_string();
        }
    }
    let exists = |p: &str| root.join(p).exists();
    let pyproject = std::fs::read_to_string(root.join("pyproject.toml")).unwrap_or_default();
    if exists("pytest.ini") || exists("conftest.py") || pyproject.contains("[tool.pytest") || (exists("tests") && has_python_tests(&root.join("tests"))) {
        let python = [".venv/bin/python", "venv/bin/python"]
            .iter()
            .map(|c| root.join(c))
            .find(|p| p.exists())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "python3".to_string());
        return format!("{} -m pytest -q -p no:cacheprovider --no-header", shell_quote(&python));
    }
    if exists("package.json") {
        let manifest = config::load_json(&root.join("package.json"));
        let script = manifest.get("scripts").and_then(|s| s.get("test")).and_then(Value::as_str).unwrap_or("");
        if !script.is_empty() && !script.contains("no test specified") {
            return "npm test --silent".to_string();
        }
    }
    if exists("Cargo.toml") {
        return "cargo test -q".to_string();
    }
    if exists("go.mod") {
        return "go test ./...".to_string();
    }
    String::new()
}

fn has_python_tests(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".py")))
        .unwrap_or(false)
}

/// The conflicting hunks, marker to marker with a little context, capped.
fn conflict_excerpt(root: &Path, files: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    for file in files.iter().take(3) {
        let Ok(body) = std::fs::read_to_string(root.join(file)) else { continue };
        let all: Vec<&str> = body.lines().collect();
        let mut i = 0;
        while i < all.len() && out.len() < CONFLICT_LINES {
            if all[i].starts_with("<<<<<<<") {
                out.push(format!("{file}:{}", i + 1));
                while i < all.len() && out.len() < CONFLICT_LINES {
                    out.push(format!("  {}", all[i]));
                    if all[i].starts_with(">>>>>>>") {
                        break;
                    }
                    i += 1;
                }
            }
            i += 1;
        }
    }
    out.join("\n")
}

/// What a landing that stopped at a conflict says: the rebase is left
/// stopped in this checkout, so finishing it is the whole job, with nothing
/// to pull.
pub fn conflict_message(incoming: usize, upstream: &str, authors: &[String], files: &[String], excerpt: &str) -> String {
    format!(
        "Cork land: your commits and {incoming} new commit(s) on {upstream} from {} change the same lines in {}. \
Nothing was pushed. The rebase onto them is stopped at that conflict in this checkout: keep both sides' work in those \
files, `git add` them, `git rebase --continue`, then push again (Cork lands it).\n{excerpt}",
        names(authors), names(files)
    )
}

/// What a push says while a rebase is still stopped in this checkout.
pub fn in_progress_message(files: &[String]) -> String {
    if files.is_empty() {
        return "Cork land: a rebase is still in progress here. Run `git rebase --continue`, then push again.".into();
    }
    format!(
        "Cork land: a rebase is still stopped at a conflict in {}. Keep both sides' work there, `git add` them, \
`git rebase --continue`, then push again.",
        names(files)
    )
}

/// Rewrite the agent's own commits for teammates' renames that have landed:
/// the new name is on origin and nothing on origin still uses the old one,
/// but a file this branch changed does. Returns (renames applied, files).
fn adapt_renames(root: &Path, server: &Server, upstream: &str, skip: &[String]) -> (Vec<String>, Vec<String>) {
    let Some(answer) = server.post(json!({"action": "renames"})) else { return (Vec::new(), Vec::new()) };
    let Some(renames) = answer.get("renames").and_then(Value::as_object) else { return (Vec::new(), Vec::new()) };
    adapt_to(root, renames, upstream, skip)
}

/// `skip`: files another session has uncommitted work in, never rewritten.
fn adapt_to(root: &Path, renames: &serde_json::Map<String, Value>, upstream: &str, skip: &[String]) -> (Vec<String>, Vec<String>) {
    let range = format!("{upstream}..HEAD");
    let mine: Vec<String> = lines(&git_out(root, &["diff", "--name-only", "--diff-filter=AMR", &range]))
        .into_iter().filter(|f| !skip.contains(f)).collect();
    let mut applied: Vec<String> = Vec::new();
    let mut touched: BTreeSet<String> = BTreeSet::new();
    for (old, new) in renames {
        let Some(new) = new.as_str() else { continue };
        let ident = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("static regex");
        if !ident.is_match(old) || !ident.is_match(new) {
            continue;
        }
        let landed = git(root, &["grep", "-q", "-w", "-e", new, upstream, "--"], 30).0;
        let old_on_origin = git(root, &["grep", "-q", "-w", "-e", old, upstream, "--"], 30).0;
        if !landed || old_on_origin {
            continue;
        }
        let word = Regex::new(&format!(r"\b{}\b", regex::escape(old))).expect("escaped regex");
        let mut hit = false;
        for file in &mine {
            let path = root.join(file);
            let Ok(body) = std::fs::read_to_string(&path) else { continue };
            if !word.is_match(&body) {
                continue;
            }
            if std::fs::write(&path, word.replace_all(&body, new).as_ref()).is_ok() {
                touched.insert(file.clone());
                hit = true;
            }
        }
        if hit {
            applied.push(format!("{old} → {new}"));
        }
    }
    if !touched.is_empty() {
        let files: Vec<&str> = touched.iter().map(String::as_str).collect();
        let mut add = vec!["add", "--"];
        add.extend(files.iter());
        git(root, &add, 30);
        let message = format!("Adapt to teammates' renames: {}", applied.join(", "));
        // only these files: nothing else anyone staged rides along
        let mut commit = vec!["commit", "-q", "-m", &message, "--"];
        commit.extend(files.iter());
        git(root, &commit, 60);
    }
    (applied, touched.into_iter().collect())
}

/// Rebase this branch's commits onto `upstream` without touching the
/// working tree: each commit is replayed with `git merge-tree`, then the
/// branch moves with `reset --keep`, which updates only the files the
/// incoming commits change and refuses if any of those has local changes.
/// `kept` (other sessions' uncommitted files) stays byte for byte as it was.
fn rebase_keeping(root: &Path, upstream: &str, kept: &[String]) -> Result<(), String> {
    let incoming: BTreeSet<String> = lines(&git_out(root, &["diff", "--name-only", &format!("HEAD...{upstream}")])).into_iter().collect();
    let overlap: Vec<String> = kept.iter().filter(|f| incoming.contains(*f)).cloned().collect();
    if !overlap.is_empty() {
        return Err(format!(
            "new commits on {upstream} change {}, which other sessions here have uncommitted work in; land again once they commit.",
            names(&overlap)
        ));
    }
    let commits = lines(&git_out(root, &["rev-list", "--reverse", "--no-merges", &format!("{upstream}..HEAD")]));
    let mut base = git_out(root, &["rev-parse", upstream]);
    for commit in &commits {
        let parent = format!("{commit}^");
        let merge_base = format!("--merge-base={parent}");
        let (ok, out, err) = git(root, &["merge-tree", "--write-tree", "--name-only", "--no-messages", &merge_base, &base, commit], 60);
        let tree = out.lines().next().unwrap_or("").trim().to_string();
        if !ok {
            if tree.len() >= 40 && tree.chars().all(|c| c.is_ascii_hexdigit()) {
                let files: Vec<String> = out.lines().skip(1).map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
                return Err(format!(
                    "your commits conflict with new commits on {upstream} in {}. Other sessions' uncommitted work is here, so it did not rebase in place: \
once they commit, push again and Cork lands it.",
                    names(&files)
                ));
            }
            return Err(format!("this git cannot rebase around other sessions' uncommitted work ({}); land again once they commit.", err.trim()));
        }
        // a commit the incoming ones already contain is dropped, as rebase does
        if git_out(root, &["rev-parse", &format!("{base}^{{tree}}")]) == tree {
            continue;
        }
        let meta = git_out(root, &["log", "-1", "--format=%an%x00%ae%x00%ad%x00%B", "--date=raw", commit]);
        let parts: Vec<&str> = meta.splitn(4, '\0').collect();
        let [name, email, date, message] = parts.as_slice() else { return Err("could not read a commit to replay.".into()) };
        let child = Command::new("git")
            .args(["commit-tree", &tree, "-p", &base, "-m", message])
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_AUTHOR_DATE", date)
            .stdin(Stdio::null())
            .output();
        match child {
            Ok(out) if out.status.success() => base = String::from_utf8_lossy(&out.stdout).trim().to_string(),
            _ => return Err("could not replay a commit.".into()),
        }
    }
    let (ok, _, err) = git(root, &["reset", "-q", "--keep", &base], 60);
    if !ok {
        return Err(format!("could not move the branch around other sessions' uncommitted work ({}).", err.trim()));
    }
    Ok(())
}

/// Files and symbols a merge preview looks at, at most.
const PREVIEW_FILES: usize = 30;
const PREVIEW_SYMBOLS: usize = 200;
const PREVIEW_LINES: usize = 5;

/// The symbols this branch's commits change (`upstream...HEAD`, from the
/// merge base), by name: `(path, symbol, kind, line in HEAD)`. Read with
/// git and parsed here; only the names leave.
pub fn changed_symbols(root: &Path, upstream: &str) -> Vec<(String, String, String, usize)> {
    let base = git_out(root, &["merge-base", upstream, "HEAD"]);
    if base.is_empty() {
        return Vec::new();
    }
    let files = lines(&git_out(root, &["diff", "--name-only", "--diff-filter=AMR", &format!("{base}..HEAD")]));
    let mut out = Vec::new();
    for rel in files.into_iter().take(PREVIEW_FILES) {
        let show = |rev: &str| {
            let (ok, text, _) = git(root, &["show", &format!("{rev}:{rel}")], 30);
            (ok && text.len() <= 512 * 1024).then_some(text)
        };
        let Some(after) = show("HEAD") else { continue };
        let before = show(&base);
        let spans: Vec<(String, usize)> = cork_core::local::symbols(&rel, &after)
            .map(|p| p.symbols.into_iter().map(|s| (s.name, s.span.0 as usize)).collect())
            .unwrap_or_default();
        let named = |key: &str| {
            let short = crate::proto::key_symbol(key);
            spans.iter().find(|(n, _)| n.split_whitespace().collect::<Vec<_>>().join("_") == short).cloned()
        };
        let planned = crate::proto::plan(&rel, &crate::proto::shape_of(&rel, before.as_deref()), &crate::proto::shape_of(&rel, Some(&after)));
        for op in planned {
            let kind = match op.kind {
                "modify_body" => "body",
                "signature_change" => "signature",
                "rename" => "renamed",
                "insert" => "added",
                // gone from HEAD: no line to name, and nothing of it to land
                _ => continue,
            };
            // a rename is keyed by the old name, which is what a teammate
            // changes; its line is the new name's
            let (symbol, line) = match named(&op.key) {
                Some((name, line)) => (name, line),
                None if kind == "renamed" => {
                    let new = op.public.get("new_name").and_then(Value::as_str).unwrap_or("");
                    (crate::proto::key_symbol(&op.key), spans.iter().find(|(n, _)| n == new).map(|(_, l)| *l).unwrap_or(0))
                }
                None => continue,
            };
            out.push((rel.clone(), symbol, kind.to_string(), line));
            if out.len() >= PREVIEW_SYMBOLS {
                return out;
            }
        }
    }
    out
}

/// What the server predicts about landing these changes (merge.rs): a line
/// per symbol another live session on the same branch is changing too and
/// has not landed. Advice: the landing goes ahead whatever it says, and it
/// fails open (no server, no answer, no lines).
fn merge_preview(root: &Path, server: &Server, upstream: &str, env: &Env) -> Vec<String> {
    let changed = changed_symbols(root, upstream);
    if changed.is_empty() {
        return Vec::new();
    }
    let names: Vec<Value> = changed.iter().map(|(p, s, k, _)| json!({"path": p, "symbol": s, "kind": k})).collect();
    let mut body = json!({"action": "preview", "changed": names});
    if let Some(map) = body.as_object_mut() {
        crate::branches::add_to(map, root, env);
    }
    let Some(answer) = server.post(body) else { return Vec::new() };
    preview_lines(&answer, &changed)
}

/// The server's predicted conflicts as land prints them.
pub fn preview_lines(answer: &Value, changed: &[(String, String, String, usize)]) -> Vec<String> {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let conflicts: Vec<&Value> = answer.get("conflicts").and_then(Value::as_array).into_iter().flatten().collect();
    let mut out = Vec::new();
    for c in conflicts.iter().take(PREVIEW_LINES) {
        let (path, symbol, kind, now) = (text(c, "path"), text(c, "symbol"), text(c, "kind"), text(c, "now"));
        let what = match kind.as_str() {
            "renamed" if !now.is_empty() => format!("rename to `{now}`"),
            "signature" if !now.is_empty() => format!("signature, now `{now}`"),
            "signature" => "signature".to_string(),
            "removed" => "removal".to_string(),
            "added" => "adds its own".to_string(),
            _ => "body".to_string(),
        };
        let at = changed
            .iter()
            .find(|(p, s, _, _)| *p == path && *s == symbol)
            .map(|(_, _, _, line)| *line)
            .filter(|l| *l > 0)
            .map(|l| format!("{path} line {l}"))
            .unwrap_or_else(|| path.clone());
        out.push(format!(
            "Cork merge preview: {}'s in-flight change to `{symbol}` ({what}) will conflict with yours at {at} when they land. \
Landing anyway; align with them (`cork message`) if one of you should give way.",
            text(c, "who")
        ));
    }
    let more = answer.get("more").and_then(Value::as_u64).unwrap_or(0) as usize + conflicts.len().saturating_sub(PREVIEW_LINES);
    if more > 0 && !out.is_empty() {
        out.push(format!("Cork merge preview: {more} more symbol(s) predicted to conflict."));
    }
    out
}

/// Queue for the repo's push turn behind whoever is landing now: who held
/// it, and the seconds waited. No server, no turn: it returns at once, and
/// a stuck turn never stops a push.
pub(crate) fn take_turn(server: &Server) -> (String, f64) {
    let mut waited_for = String::new();
    let turn_started = Instant::now();
    loop {
        let Some(answer) = server.post(json!({"action": "acquire"})) else { break };
        if answer.get("held").and_then(Value::as_bool).unwrap_or(true) {
            break;
        }
        waited_for = answer.get("holder").and_then(Value::as_str).unwrap_or("a teammate").to_string();
        if turn_started.elapsed().as_secs_f64() > MAX_TURN_WAIT_S {
            break;
        }
        let wait = answer.get("wait_s").and_then(Value::as_f64).unwrap_or(2.0).clamp(0.5, 2.0);
        std::thread::sleep(Duration::from_secs_f64(wait));
    }
    (waited_for, turn_started.elapsed().as_secs_f64())
}

struct Args {
    session: String,
    test: String,
}

fn parse_args(args: &[String]) -> Args {
    let mut out = Args { session: String::new(), test: String::new() };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                out.session = args.get(i + 1).cloned().unwrap_or_default();
                i += 1;
            }
            "--test" => {
                out.test = args.get(i + 1).cloned().unwrap_or_default();
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

pub fn run(raw_args: &[String], env: &Env) -> i32 {
    run_in(&std::env::current_dir().unwrap_or_default(), raw_args, env)
}

pub fn run_in(cwd: &Path, raw_args: &[String], env: &Env) -> i32 {
    let started = Instant::now();
    let args = parse_args(raw_args);
    let top = git_out(cwd, &["rev-parse", "--show-toplevel"]);
    if top.is_empty() {
        println!("Cork land: this is not a git repository.");
        return 1;
    }
    let root = PathBuf::from(top);
    // a rebase left stopped at a conflict (by this command, or by hand):
    // finishing it is the next step, not another land
    if crate::hold::rebase_in_progress(&root) {
        let files = lines(&git_out(&root, &["diff", "--name-only", "--diff-filter=U"]));
        println!("{}", in_progress_message(&files));
        return 1;
    }
    // trunk mode: a branch of the agent's own lands into the trunk (trunk.rs)
    if let Some(trunk) = crate::trunk::active(&root, env) {
        let branch = crate::branches::current(&root);
        if crate::trunk::lands_into_trunk(&root, &branch, &trunk) {
            let options = crate::trunk::LandOptions { session: args.session.clone(), test: args.test.clone(), ..Default::default() };
            let (code, said) = crate::trunk::land_branch(&root, &branch, &trunk, &options, env);
            println!("{said}");
            return code;
        }
    }
    let server = Server { cfg: config::config(Some(&root), env), session: args.session.clone() };

    // uncommitted work blocks a landing unless every change in it belongs
    // to other sessions at work in this checkout: theirs to commit, and a
    // push of what is already committed leaves it as it is
    // tracked files changed, staged or not (untracked ones never block)
    let dirty = lines(&git_out(&root, &["diff", "HEAD", "--name-only", "--no-renames"]));
    let mut kept: Vec<String> = Vec::new();
    if !dirty.is_empty() {
        let files: Vec<String> = dirty.clone();
        let me = if args.session.is_empty() { crate::machine::running_session(env) } else { args.session.clone() };
        let blocking: Vec<String> = if me.is_empty() {
            files.clone()
        } else {
            let entries = crate::journal::load(&root, env);
            files.iter()
                .filter_map(|f| crate::journal::blocking_reason(&root, f, &me, &entries, env).map(|why| format!("{f} ({why})")))
                .collect()
        };
        if !blocking.is_empty() {
            println!(
                "Cork land: commit your changes first, then land again. Not committed: {}. \
`{} -m \"...\"` commits only your lines.",
                names(&blocking), crate::journal::commit_hint(env)
            );
            return 1;
        }
        kept = files;
    }
    let branch = git_out(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let upstream = git_out(&root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]);
    if upstream.is_empty() {
        // no upstream to rebase onto: a plain first push, told as it is
        let (ok, out, err) = git(&root, &["push", "-u", "origin", "HEAD"], 120);
        print!("{out}{err}");
        return if ok { 0 } else { 1 };
    }
    let remote = upstream.split('/').next().unwrap_or("origin").to_string();
    git(&root, &["fetch", &remote, "--quiet"], 60);
    let ahead: usize = git_out(&root, &["rev-list", "--count", &format!("{upstream}..HEAD")]).parse().unwrap_or(0);
    if ahead == 0 {
        println!("Cork land: nothing to push; {branch} is already on {upstream}.");
        return 0;
    }

    // what landing this will meet: teammates' in-flight work on the same
    // symbols, said before the push and never a reason not to push
    for line in merge_preview(&root, &server, &upstream, env) {
        println!("{line}");
    }

    // the push turn: queue behind whoever is landing now
    let (waited_for, waited_s) = take_turn(&server);
    let release = |outcome: &str, extra: Value| {
        let mut body = json!({"action": "release", "outcome": outcome, "branch": branch});
        if let (Some(map), Some(more)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in more {
                map.insert(k.clone(), v.clone());
            }
        }
        server.post(body);
    };

    let test_cmd = test_command(&root, &args.test);
    let mut rebases = 0u64;
    let mut over = 0usize;
    let mut authors: Vec<String> = Vec::new();
    let mut renamed: Vec<String> = Vec::new();
    let mut adapted: BTreeSet<String> = BTreeSet::new();
    let mut tested: Option<bool> = None;
    // each lap as it went: what it rebased over, its test run's command (no
    // output), time and verdict, and how its push went (the waste report)
    let mut laps: Vec<Value> = Vec::new();
    let mut push_rejects = 0u64;
    let short = |cmd: &str| cmd.chars().take(120).collect::<String>();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            git(&root, &["fetch", &remote, "--quiet"], 60);
        }
        let incoming: usize = git_out(&root, &["rev-list", "--count", &format!("HEAD..{upstream}")]).parse().unwrap_or(0);
        let mut lap = json!({"incoming": incoming, "test": "", "seconds": 0.0, "ok": null, "timed_out": false, "push": ""});
        if incoming > 0 {
            for name in lines(&git_out(&root, &["log", &format!("HEAD..{upstream}"), "--format=%an"])) {
                if !authors.contains(&name) {
                    authors.push(name);
                }
            }
            over += incoming;
            if !kept.is_empty() {
                // other sessions' uncommitted files stay exactly as they are:
                // the commits are replayed without the working tree, and the
                // branch moves with `reset --keep`, which touches only files
                // the incoming commits change
                if let Err(problem) = rebase_keeping(&root, &upstream, &kept) {
                    release("conflict", json!({}));
                    println!("Cork land: {problem} Nothing was pushed and your branch is as it was.");
                    return 1;
                }
            }
            let (ok, _, _) = if kept.is_empty() { git(&root, &["rebase", &upstream], 120) } else { (true, String::new(), String::new()) };
            if !ok {
                let files = lines(&git_out(&root, &["diff", "--name-only", "--diff-filter=U"]));
                let excerpt = conflict_excerpt(&root, &files);
                release("conflict", json!({"push_rejects": push_rejects}));
                // the rebase stays stopped at the conflict, here: resolving it
                // is all that is left. Aborting it and saying "run `git pull
                // --rebase`" sent the agent to redo the same rebase by hand
                // (Study 8, second rerun: 20 of 68 manual pulls a run).
                println!("{}", conflict_message(incoming, &upstream, &authors, &files, &excerpt));
                return 1;
            }
            rebases += 1;
            let (applied, files) = adapt_renames(&root, &server, &upstream, &kept);
            renamed.extend(applied);
            adapted.extend(files);
            if !test_cmd.is_empty() {
                let test_started = Instant::now();
                let (ok, output, _, timed_out) = crate::check::run_verify(&root, &test_cmd, TEST_MAX_S);
                lap["test"] = json!(short(&test_cmd));
                lap["seconds"] = json!(test_started.elapsed().as_secs_f64());
                lap["ok"] = json!(ok);
                lap["timed_out"] = json!(timed_out);
                tested = Some(ok);
                if !ok {
                    release("tests_failed", json!({"push_rejects": push_rejects}));
                    let tail: Vec<&str> = output.lines().rev().take(TEST_TAIL_LINES).collect::<Vec<_>>().into_iter().rev().collect();
                    println!(
                        "Cork land: rebased onto {over} new commit(s) from {}, then `{test_cmd}` {}. Nothing was pushed; \
your commits now sit on top of theirs, so fix it here, commit, and push again.\n{}",
                        names(&authors), if timed_out { "timed out" } else { "failed" }, tail.join("\n")
                    );
                    return 1;
                }
            }
        }
        // the commit names the ops it carries (`Cork-Ops:`); it is not
        // pushed yet, so amending its message rewrites nothing anyone has
        crate::proto::stamp_trailer(&root, env, &args.session, &upstream);
        let carried = crate::proto::trailer_ids(&root, &format!("{upstream}..HEAD"));
        let (ok, _, err) = git(&root, &["push", "--quiet", &remote, &format!("HEAD:{}", upstream.split_once('/').map(|x| x.1).unwrap_or(&branch))], 120);
        lap["push"] = json!(if ok { "landed" } else { "rejected" });
        laps.push(lap);
        if ok {
            let sha = git_out(&root, &["rev-parse", "HEAD"]);
            let tests = match tested { Some(true) => "passed", Some(false) => "failed", None => "untested" };
            crate::proto::promote(&root, &server.cfg, env, &args.session, carried, json!({"commit": sha, "tests": tests}));
            let commit = git_out(&root, &["rev-parse", "--short", "HEAD"]);
            let adapted_list: Vec<String> = adapted.iter().cloned().collect();
            release("landed", json!({
                "commit": commit, "target": upstream.split_once('/').map(|x| x.1).unwrap_or(&branch),
                "commits": ahead, "rebases": rebases, "over": over, "authors": authors,
                "renames_applied": renamed, "adapted": adapted_list,
                "tests": test_cmd, "tests_ok": tested, "seconds": started.elapsed().as_secs_f64(),
                "laps": laps, "push_rejects": push_rejects,
            }));
            let mut parts = vec![format!("Cork landed {ahead} commit(s) on {upstream} ({commit}).")];
            if waited_s >= 1.0 && !waited_for.is_empty() {
                parts.push(format!("Waited {}s for {waited_for}'s push.", waited_s.round() as i64));
            }
            if over > 0 {
                parts.push(format!("Rebased onto {over} new commit(s) from {}.", names(&authors)));
            }
            if !renamed.is_empty() {
                parts.push(format!("Adapted to renames that landed ({}) in {}, as its own commit.", renamed.join(", "), names(&adapted_list)));
            }
            match tested {
                Some(true) => parts.push(format!("`{test_cmd}` passed on the rebased tree.")),
                None if over > 0 => parts.push("No test command found, so the rebased tree was not tested.".to_string()),
                _ => {}
            }
            if !kept.is_empty() {
                parts.push(format!("Other sessions' uncommitted work stayed as it was ({}).", names(&kept)));
            }
            parts.push("It is pushed; nothing else to do.".to_string());
            println!("{}", parts.join(" "));
            return 0;
        }
        let lost_race = ["rejected", "non-fast-forward", "fetch first", "stale info"].iter().any(|w| err.contains(w));
        if lost_race {
            push_rejects += 1;
        }
        if !lost_race {
            if let Some(last) = laps.last_mut() {
                last["push"] = json!("failed");
            }
            release("failed", json!({"push_rejects": push_rejects}));
            println!("Cork land: git push failed:\n{}", err.trim());
            return 1;
        }
    }
    release("gave_up", json!({"push_rejects": push_rejects}));
    println!("Cork land: origin kept moving; {ATTEMPTS} pushes lost the race. Your commits are rebased onto the latest; push again.");
    1
}

fn push_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            // whatever runs first (a cd, git add, a commit with a heredoc
            // message, the tests), then the push as the LAST step
            r#"^(?P<pre>[\s\S]*(?:&&|;|\n)\s*)?git\s+push(?P<args>(?:[ \t]+[^\s;&|<>]+)*?)[ \t]*(?:2>&1)?[ \t]*(?:\|[ \t]*(?:tail|head)(?:[ \t]+-n)?[ \t]+-?\d+)?\s*$"#,
        )
        .expect("static regex")
    })
}

/// A plain `git push` of the current branch, as a PreToolUse hook sees it,
/// becomes `collide-hook land` for this session, also when it ends a chain
/// (`git add … && git commit … && git push`: what runs before it is kept).
/// A force, a tag, another branch, or a push that is not the last step is
/// left alone. Claude Code only: it is the
/// harness whose hooks can replace a command.
/// Whether a `git push` in this repo lands through Cork (`rewrite` turns
/// it into `cork land`): then a teammate's push is something landing
/// rebases onto, and no notice tells the agent to pull mid-task.
pub fn lands(root: &Path, env: &Env) -> bool {
    if crate::harness::current() != "claude" || config::get(env, "CORK_LAND") == "0" {
        return false;
    }
    let own = config::repo_config_path(root);
    if own.exists() {
        return config::load_json(&own).get("land").and_then(Value::as_bool) != Some(false);
    }
    crate::machine::repo_config(root, env).is_some()
}

pub fn rewrite(stdin_data: &str, env: &Env) -> Option<String> {
    if crate::harness::current() != "claude" || config::get(env, "CORK_LAND") == "0" {
        return None;
    }
    let input: Value = serde_json::from_str(stdin_data).ok()?;
    if input.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let tool_input = input.get("tool_input")?.clone();
    let command = tool_input.get("command").and_then(Value::as_str)?.to_string();
    let caps = push_re().captures(&command)?;
    let pre = caps.name("pre").map(|m| m.as_str()).unwrap_or("");
    let push_args: Vec<&str> = caps.name("args").map(|m| m.as_str().split_whitespace().collect()).unwrap_or_default();

    let cwd = input.get("cwd").and_then(Value::as_str).map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let dir = {
        static CD: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        let cd = CD.get_or_init(|| Regex::new(r#"^\s*cd\s+("[^"]*"|'[^']*'|[^\s;&|]+)\s*(?:&&|;)"#).expect("static regex"));
        let target = cd.captures(pre).and_then(|c| c.get(1)).map(|m| m.as_str().trim_matches(|c| c == '"' || c == '\'')).unwrap_or("");
        if target.is_empty() { cwd.clone() } else { cwd.join(target) }
    };
    let root = PathBuf::from(git_out(&dir, &["rev-parse", "--show-toplevel"]));
    if root.as_os_str().is_empty() {
        return None;
    }
    // a repo set up with its own config, or any repo on a machine with
    // Cork installed for every repo (the free version lands too)
    let own = config::has_repo_config(&root);
    if !own && crate::machine::repo_config(&root, env).is_none() {
        return None;
    }
    if own && config::repo_config(&root).get("land").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let branch = git_out(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let upstream = git_out(&root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]);
    let (remote, remote_branch) = upstream.split_once('/')?;
    // trunk mode: a push of a branch of the agent's own publishes it, as
    // git does; `cork land` (or its task ending) lands it into the trunk
    if crate::trunk::active(&root, env).is_some_and(|trunk| crate::trunk::lands_into_trunk(&root, &branch, &trunk)) {
        return None;
    }
    // only a push of this branch to its own upstream
    let mut positional: Vec<&str> = Vec::new();
    for arg in &push_args {
        match *arg {
            "-q" | "--quiet" => {}
            a if a.starts_with('-') => return None,
            a => positional.push(a),
        }
    }
    match positional.as_slice() {
        [] => {}
        [r] if *r == remote => {}
        [r, b] if *r == remote && (*b == branch || *b == remote_branch || *b == "HEAD" || *b == format!("HEAD:{remote_branch}")) => {}
        _ => return None,
    }

    let exe = std::env::current_exe().ok()?;
    let session = input.get("session_id").and_then(Value::as_str).unwrap_or("");
    let home = if own { String::new() } else { crate::machine::home_prefix(env) };
    let landed = format!("{pre}{home}{} land --session {}", shell_quote(&exe.to_string_lossy()), shell_quote(session));
    let mut updated = tool_input;
    updated["command"] = json!(landed);
    // the push was the agent's to make; in a session that asks before
    // running commands, it still asks, now about the landing
    let decision = if input.get("permission_mode").and_then(Value::as_str) == Some("bypassPermissions") { "allow" } else { "ask" };
    Some(
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": "Cork lands this push: it rebases onto teammates' new commits, runs the tests and pushes, in one step.",
            "updatedInput": updated,
        }})
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(root: &Path, args: &[&str]) {
        let (ok, _, err) = git(root, args, 60);
        assert!(ok, "git {args:?}: {err}");
    }

    fn repo(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("collide-land-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        sh(&base, &["clone", "-q", "origin.git", "a"]);
        let a = base.join("a");
        for (k, v) in [("user.email", "a@x"), ("user.name", "alice")] {
            sh(&a, &["config", k, v]);
        }
        std::fs::write(a.join("lib.py"), "def amount_due(x):\n    return x\n").unwrap();
        std::fs::write(a.join("notes.txt"), "one\n").unwrap();
        sh(&a, &["add", "."]);
        sh(&a, &["commit", "-qm", "seed"]);
        sh(&a, &["push", "-q", "-u", "origin", "main"]);
        sh(&base, &["clone", "-q", "origin.git", "b"]);
        let b = base.join("b");
        for (k, v) in [("user.email", "b@x"), ("user.name", "bob")] {
            sh(&b, &["config", k, v]);
        }
        (base, a, b)
    }

    fn land_in(dir: &Path) -> i32 {
        run_in(dir, &["--test".into(), "true".into()], &Env::new())
    }

    #[test]
    fn it_rebases_onto_a_teammate_and_pushes() {
        let (base, a, b) = repo("rebase");
        // a teammate lands first
        std::fs::write(a.join("notes.txt"), "one\ntwo\n").unwrap();
        sh(&a, &["commit", "-qam", "teammate"]);
        sh(&a, &["push", "-q"]);
        // this agent committed on the old tip: a plain push would be rejected
        std::fs::write(b.join("extra.py"), "def extra():\n    return 1\n").unwrap();
        sh(&b, &["add", "."]);
        sh(&b, &["commit", "-qm", "mine"]);
        assert_eq!(land_in(&b), 0);
        sh(&a, &["pull", "-q"]);
        assert!(a.join("extra.py").exists() && std::fs::read_to_string(a.join("notes.txt")).unwrap().contains("two"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A stand-in server that answers every POST `{"ok": true, "held": true}`
    /// and keeps each body it was sent.
    fn capture_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut got: Vec<u8> = Vec::new();
                let mut buffer = [0u8; 8192];
                let body = loop {
                    let Ok(n) = stream.read(&mut buffer) else { break String::new() };
                    if n == 0 {
                        break String::new();
                    }
                    got.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&got).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length: usize = text[..end].lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                            .unwrap_or(0);
                        if got.len() >= end + 4 + length {
                            break text[end + 4..].to_string();
                        }
                    }
                };
                if let Ok(value) = serde_json::from_str::<Value>(&body) {
                    kept.lock().unwrap().push(value);
                }
                let answer = json!({"ok": true, "held": true}).to_string();
                let _ = write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}", answer.len());
            }
        });
        (base, seen)
    }

    #[test]
    fn a_landing_reports_each_laps_test_time_and_verdict_but_no_output() {
        let (base, a, b) = repo("laps");
        std::fs::write(a.join("notes.txt"), "one\ntwo\n").unwrap();
        sh(&a, &["commit", "-qam", "teammate"]);
        sh(&a, &["push", "-q"]);
        std::fs::write(b.join("extra.py"), "def extra():\n    return 1\n").unwrap();
        sh(&b, &["add", "."]);
        sh(&b, &["commit", "-qm", "mine"]);
        let (server, seen) = capture_server();
        let mut env = Env::new();
        env.insert("CORK_SERVER_URL".into(), server);
        env.insert("CORK_TOKEN".into(), "cat_test".into());
        env.insert("CORK_REPO_ID".into(), "github.com/acme/laps".into());
        env.insert("HOME".into(), base.to_string_lossy().to_string());
        assert_eq!(run_in(&b, &["--test".into(), "echo SECRET-OUTPUT && sleep 0.2".into()], &env), 0);
        let bodies = seen.lock().unwrap().clone();
        let landed = bodies.iter().find(|v| v["outcome"] == "landed").unwrap_or_else(|| panic!("{bodies:?}"));
        let laps = landed["laps"].as_array().unwrap();
        assert_eq!(laps.len(), 1, "{landed}");
        assert_eq!(laps[0]["incoming"], 1);
        assert_eq!(laps[0]["test"], "echo SECRET-OUTPUT && sleep 0.2");
        assert!(laps[0]["seconds"].as_f64().unwrap() >= 0.2);
        assert_eq!(laps[0]["ok"], true);
        assert_eq!(laps[0]["push"], "landed");
        assert_eq!(landed["push_rejects"], 0);
        assert!(!landed.to_string().contains("SECRET-OUTPUT\n"), "the test's output never leaves");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    /// A conflict pushes nothing and leaves the rebase stopped at it, so the
    /// agent resolves it in place instead of redoing the rebase with a manual
    /// `git pull --rebase` (Study 8, second rerun: 20 of 68 manual pulls a
    /// run followed this message). The next push, once it is resolved, lands.
    fn a_conflict_pushes_nothing_and_leaves_the_rebase_stopped_at_it() {
        let (base, a, b) = repo("conflict");
        std::fs::write(a.join("notes.txt"), "theirs\n").unwrap();
        sh(&a, &["commit", "-qam", "teammate"]);
        sh(&a, &["push", "-q"]);
        std::fs::write(b.join("notes.txt"), "mine\n").unwrap();
        sh(&b, &["commit", "-qam", "mine"]);
        let pushed = git_out(&a, &["rev-parse", "HEAD"]);
        assert_eq!(land_in(&b), 1);
        assert!(crate::hold::rebase_in_progress(&b), "stopped at the conflict");
        assert!(std::fs::read_to_string(b.join("notes.txt")).unwrap().contains("<<<<<<< "));
        // landing again before it is resolved says how to finish, and pushes nothing
        assert_eq!(land_in(&b), 1);
        sh(&a, &["fetch", "-q"]);
        assert_eq!(git_out(&a, &["rev-parse", "origin/main"]), pushed, "nothing was pushed");
        // resolved: the next land pushes
        std::fs::write(b.join("notes.txt"), "theirs\nmine\n").unwrap();
        sh(&b, &["add", "notes.txt"]);
        let ok = Command::new("git").args(["rebase", "--continue"]).env("GIT_EDITOR", "true").current_dir(&b)
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        assert!(ok.success());
        assert_eq!(land_in(&b), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_rename_that_landed_is_applied_to_this_branch_only_once_it_is_on_origin() {
        let (base, a, b) = repo("renames");
        // this agent calls amount_due in a new test
        std::fs::write(b.join("test_x.py"), "from lib import amount_due\n\n\ndef test_x():\n    assert amount_due(1) == 1\n").unwrap();
        sh(&b, &["add", "."]);
        sh(&b, &["commit", "-qm", "mine"]);
        let renames: serde_json::Map<String, Value> = [("amount_due".to_string(), json!("checkout_total"))].into_iter().collect();
        // not on origin yet: nothing is touched
        sh(&b, &["fetch", "-q"]);
        assert!(adapt_to(&b, &renames, "origin/main", &[]).0.is_empty());
        // the teammate's rename lands; this branch rebases onto it
        std::fs::write(a.join("lib.py"), "def checkout_total(x):\n    return x\n").unwrap();
        sh(&a, &["commit", "-qam", "rename"]);
        sh(&a, &["push", "-q"]);
        sh(&b, &["fetch", "-q"]);
        sh(&b, &["rebase", "-q", "origin/main"]);
        let (applied, files) = adapt_to(&b, &renames, "origin/main", &[]);
        assert_eq!(applied, vec!["amount_due → checkout_total".to_string()]);
        assert_eq!(files, vec!["test_x.py".to_string()]);
        let body = std::fs::read_to_string(b.join("test_x.py")).unwrap();
        assert!(body.contains("import checkout_total") && !body.contains("amount_due"), "{body}");
        assert!(git_out(&b, &["status", "--porcelain"]).is_empty(), "committed");
        assert!(git_out(&b, &["log", "-1", "--format=%s"]).starts_with("Adapt to teammates' renames"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_push_lands_in_any_repo_on_a_machine_with_collide_installed() {
        let (base, _a, b) = repo("machine");
        let input = json!({"tool_name": "Bash", "tool_input": {"command": "git push"}, "cwd": b.to_string_lossy(),
                           "session_id": "s1", "permission_mode": "bypassPermissions"}).to_string();
        let home = base.join("home");
        let mut env = Env::new();
        env.insert("HOME".into(), base.join("elsewhere").to_string_lossy().to_string());
        env.insert("CORK_HOME".into(), home.to_string_lossy().to_string());
        assert!(rewrite(&input, &env).is_none(), "not installed: the push is left alone");
        assert!(crate::machine::save_settings(&env, &json!({"mode": "local"})));
        let out: Value = serde_json::from_str(&rewrite(&input, &env).expect("installed: the push lands")).unwrap();
        let rewritten = out["hookSpecificOutput"]["updatedInput"]["command"].as_str().unwrap();
        assert!(rewritten.starts_with(&format!("CORK_HOME={} ", home.to_string_lossy())), "{rewritten}");
        assert!(rewritten.ends_with(" land --session 's1'"), "{rewritten}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The merge preview: this branch's symbols by name and line, and the
    /// server's predicted conflicts as one short line each. Nothing is sent
    /// but names, and a server that predicts nothing prints nothing.
    #[test]
    fn a_merge_preview_names_the_symbol_its_line_and_whose_change() {
        let (base, _a, b) = repo("preview");
        std::fs::write(b.join("lib.py"), "def amount_due(x):\n    return x * 2\n\n\ndef fee(x):\n    return 1\n").unwrap();
        sh(&b, &["commit", "-qam", "mine"]);
        sh(&b, &["fetch", "-q"]);
        let changed = changed_symbols(&b, "origin/main");
        assert_eq!(changed, vec![
            ("lib.py".to_string(), "amount_due".to_string(), "body".to_string(), 1),
            ("lib.py".to_string(), "fee".to_string(), "added".to_string(), 5),
        ]);
        let answer = json!({"conflicts": [{"path": "lib.py", "symbol": "amount_due", "who": "Bob (bob@x.com)", "kind": "signature",
            "now": "def amount_due(x, region)"}], "more": 0});
        assert_eq!(preview_lines(&answer, &changed), vec![
            "Cork merge preview: Bob (bob@x.com)'s in-flight change to `amount_due` (signature, now `def amount_due(x, region)`) \
will conflict with yours at lib.py line 1 when they land. Landing anyway; align with them (`cork message`) if one of you should give way."
                .to_string(),
        ]);
        assert!(preview_lines(&json!({"conflicts": []}), &changed).is_empty());
        assert!(preview_lines(&json!({"ok": false}), &changed).is_empty(), "fails open");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn uncommitted_work_is_refused() {
        let (base, _a, b) = repo("dirty");
        std::fs::write(b.join("notes.txt"), "half done\n").unwrap();
        assert_eq!(land_in(&b), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A write by `session` as the hooks journal it.
    fn written_by(dir: &Path, rel: &str, body: &str, session: &str, env: &Env) {
        let path = dir.join(rel);
        let before = std::fs::read(&path).unwrap_or_default();
        std::fs::write(&path, body).unwrap();
        crate::journal::record(dir, &path, rel, session, "", "", "", Some(before), env);
    }

    #[test]
    fn another_sessions_uncommitted_work_does_not_block_a_push_but_this_ones_does() {
        let (base, a, b) = repo("guard");
        let mut env = Env::new();
        env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
        let land_as = |session: &str, env: &Env| run_in(&b, &["--test".into(), "true".into(), "--session".into(), session.into()], env);
        // this session committed its work; another session is mid-edit
        std::fs::write(b.join("extra.py"), "def extra():\n    return 1\n").unwrap();
        sh(&b, &["add", "extra.py"]);
        sh(&b, &["commit", "-qm", "mine"]);
        written_by(&b, "notes.txt", "one\nhalf done by the other session\n", "other-session-0001", &env);
        assert!(crate::journal::blocking_reason(&b, "notes.txt", "this-session-0002", &crate::journal::load(&b, &env), &env).is_none());
        assert_eq!(land_as("this-session-0002", &env), 0, "the push of committed work lands");
        sh(&a, &["pull", "-q"]);
        assert!(a.join("extra.py").exists());
        assert!(!std::fs::read_to_string(a.join("notes.txt")).unwrap().contains("half done"), "their work was not pushed");
        assert!(std::fs::read_to_string(b.join("notes.txt")).unwrap().contains("half done"), "and is still here");

        // a teammate lands meanwhile: the rebase goes around their work
        std::fs::write(a.join("lib.py"), "def amount_due(x):\n    return x * 2\n").unwrap();
        sh(&a, &["commit", "-qam", "teammate"]);
        sh(&a, &["push", "-q"]);
        std::fs::write(b.join("more.py"), "X = 1\n").unwrap();
        sh(&b, &["add", "more.py"]);
        sh(&b, &["commit", "-qm", "more"]);
        assert_eq!(land_as("this-session-0002", &env), 0);
        assert!(std::fs::read_to_string(b.join("lib.py")).unwrap().contains("x * 2"), "rebased onto theirs");
        assert!(std::fs::read_to_string(b.join("notes.txt")).unwrap().contains("half done"), "untouched");
        assert_eq!(git_out(&b, &["rev-list", "--count", "HEAD..origin/main"]), "0");

        // this session's own uncommitted work still blocks, and so does a
        // change no session wrote
        written_by(&b, "lib.py", "def amount_due(x):\n    return x * 3\n", "this-session-0002", &env);
        let why = crate::journal::blocking_reason(&b, "lib.py", "this-session-0002", &crate::journal::load(&b, &env), &env);
        assert_eq!(why.as_deref(), Some("yours"));
        std::fs::write(b.join("more.py"), "X = 2\n").unwrap();
        sh(&b, &["commit", "-qm", "again", "--allow-empty"]);
        assert_eq!(land_as("this-session-0002", &env), 1);
        sh(&b, &["checkout", "-q", "--", "lib.py"]);
        let why = crate::journal::blocking_reason(&b, "more.py", "this-session-0002", &crate::journal::load(&b, &env), &env);
        assert_eq!(why.as_deref(), Some("no session wrote it"));
        assert_eq!(land_as("this-session-0002", &env), 1);
        // without a session nothing can be told apart: it blocks as before
        sh(&b, &["checkout", "-q", "--", "more.py"]);
        assert_eq!(run_in(&b, &["--test".into(), "true".into()], &env), 1, "notes.txt is unattributable without a session");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Trunk mode (trunk.rs): a push of a branch of the agent's own publishes
    /// it as git does, a push on the trunk still lands, and `cork land` on
    /// the branch lands it into the trunk.
    #[test]
    fn in_trunk_mode_a_branch_push_stays_a_push_and_cork_land_lands_it_into_the_trunk() {
        let (base, a, b) = repo("trunk");
        std::fs::create_dir_all(b.join(".collide")).unwrap();
        std::fs::write(b.join(".collide").join("config.json"), r#"{"repo_id": "github.com/acme/trunk"}"#).unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
        sh(&b, &["checkout", "-q", "-b", "dev"]);
        sh(&b, &["push", "-q", "-u", "origin", "dev"]);
        crate::trunk::remember("github.com/acme/trunk", &json!({"trunk": {"on": true, "branch": "dev", "promote_to": "main"}}), &env);
        sh(&b, &["checkout", "-q", "-b", "feat"]);
        std::fs::write(b.join("refund.py"), "def refund():\n    return 1\n").unwrap();
        sh(&b, &["add", "refund.py"]);
        sh(&b, &["commit", "-qm", "Add refunds"]);
        sh(&b, &["push", "-q", "-u", "origin", "feat"]);
        let push = |dir: &Path| {
            let input = json!({"tool_name": "Bash", "tool_input": {"command": "git push"}, "cwd": dir.to_string_lossy(),
                               "session_id": "s1", "permission_mode": "bypassPermissions"});
            rewrite(&input.to_string(), &env)
        };
        assert!(push(&b).is_none(), "a branch of its own: a plain push");
        sh(&b, &["checkout", "-q", "dev"]);
        assert!(push(&b).is_some(), "on the trunk it lands");
        sh(&b, &["checkout", "-q", "feat"]);
        assert_eq!(run_in(&b, &["--test".into(), "true".into()], &env), 0);
        sh(&a, &["fetch", "-q", "origin"]);
        assert!(git(&a, &["cat-file", "-e", "origin/dev:refund.py"], 30).0, "landed into dev");
        assert!(!git(&a, &["cat-file", "-e", "origin/main:refund.py"], 30).0, "never into main");
        assert_eq!(crate::branches::current(&b), "dev", "the checkout is on the trunk now");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_a_plain_push_of_this_branch_is_rewritten() {
        let (base, _a, b) = repo("rewrite");
        std::fs::create_dir_all(b.join(".collide")).unwrap();
        std::fs::write(b.join(".collide").join("config.json"), "{}").unwrap();
        let ask = |command: &str, mode: &str| {
            let input = json!({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": b.to_string_lossy(),
                               "session_id": "s1", "permission_mode": mode});
            rewrite(&input.to_string(), &Env::new())
        };
        for command in ["git push", "git push origin main", "git push origin main 2>&1", "git push -q origin HEAD",
                        "git push origin main 2>&1 | tail -5"] {
            let out: Value = serde_json::from_str(&ask(command, "bypassPermissions").unwrap_or_else(|| panic!("{command}"))).unwrap();
            let rewritten = out["hookSpecificOutput"]["updatedInput"]["command"].as_str().unwrap().to_string();
            assert!(rewritten.contains(" land --session 's1'"), "{rewritten}");
            assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "allow");
        }
        let cd = format!("cd {} && git push origin main", b.to_string_lossy());
        assert!(ask(&cd, "bypassPermissions").unwrap().contains("land --session"));
        // a push at the end of a chain lands; what runs before it stays as it was
        for chained in [
            "git add a.py && git commit -m 'Fix it' && git push origin main",
            "git add -A && git commit -q -F - <<'EOF'\nFix it\n\nWith a body && more\nEOF\ngit push -q origin main",
            "python -m pytest -q && git commit -am wip; git push",
        ] {
            let out: Value = serde_json::from_str(&ask(chained, "bypassPermissions").unwrap_or_else(|| panic!("{chained}"))).unwrap();
            let rewritten = out["hookSpecificOutput"]["updatedInput"]["command"].as_str().unwrap().to_string();
            let kept = &chained[..chained.rfind("git push").unwrap()];
            assert!(rewritten.starts_with(kept) && rewritten.ends_with(" land --session 's1'"), "{rewritten}");
        }
        for command in ["git push --force", "git push origin other", "git push origin --tags", "git push && echo hi",
                        "git commit -m x && git push --force",
                        "git pull --rebase", "git push upstream main"] {
            assert!(ask(command, "bypassPermissions").is_none(), "{command}");
        }
        // a session that asks before commands still asks
        let asked: Value = serde_json::from_str(&ask("git push", "default").unwrap()).unwrap();
        assert_eq!(asked["hookSpecificOutput"]["permissionDecision"], "ask");
        // a repo that turned landing off keeps its push
        std::fs::write(b.join(".collide").join("config.json"), r#"{"land": false}"#).unwrap();
        assert!(ask("git push", "bypassPermissions").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
