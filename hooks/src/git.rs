//! Git facts the hooks read cheaply, plus the continuous auto-pull.
//!
//! Branch and origin come straight out of `.git/` with no subprocess — free
//! inside the hook's budget. The sync does shell out, but the network fetch is
//! throttled to once per interval and every call is time-boxed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{json, Value};

use crate::config::load_json;

pub const SYNC_INTERVAL_S: f64 = 15.0;
/// With landing on, the one git-sync notice a wave of teammates' landings gets.
pub const SYNC_BURST_S: f64 = 600.0;
pub const FETCH_TIMEOUT_S: u64 = 4;

/// The repo's current branch, read straight from `.git/HEAD`.
pub fn branch(root: &Path) -> String {
    // a linked worktree's .git is a file naming its own git dir
    let dot_git = root.join(".git");
    let git_dir = match std::fs::read_to_string(&dot_git) {
        Ok(text) => match text.trim().strip_prefix("gitdir:") {
            Some(dir) => {
                let dir = Path::new(dir.trim());
                if dir.is_absolute() { dir.to_path_buf() } else { root.join(dir) }
            }
            None => dot_git,
        },
        Err(_) => dot_git,
    };
    let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) else {
        return String::new();
    };
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(name) => name.chars().take(80).collect(),
        None => head.chars().take(12).collect(),
    }
}

fn origin_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)\[remote "origin"\][^\[]*?\burl\s*=\s*(\S+)"#).expect("origin pattern")
    })
}

/// A git remote URL (or repo id) reduced to host/org/name, lowercased — the
/// shape a Cork repo id takes.
pub fn normalize_repo_url(url: &str) -> String {
    let mut s = url.trim().to_string();
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("git@") {
        s = rest.to_string();
    }
    s = s.replace(':', "/");
    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    s.to_lowercase()
}

/// The repo id implied by git's `origin`, read from `.git/config`. Gates a
/// server-driven rename: a config is only repointed when this agrees.
pub fn origin_repo_id(root: &Path) -> String {
    // a worktree's .git is a file pointing at the main repository: git
    // itself knows where its config is
    let text = match std::fs::read_to_string(root.join(".git").join("config")) {
        Ok(text) => text,
        Err(_) if root.join(".git").is_file() => {
            let url = std::process::Command::new("git")
                .args(["-C", &root.to_string_lossy(), "config", "--get", "remote.origin.url"])
                .output()
                .ok()
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .unwrap_or_default();
            if url.is_empty() {
                return String::new();
            }
            format!("[remote \"origin\"]\n\turl = {url}\n")
        }
        Err(_) => return String::new(),
    };
    origin_url_re()
        .captures(&text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .filter(|url| !is_local_origin(url))
        .map(normalize_repo_url)
        .unwrap_or_default()
}

/// An origin that is a folder on this machine (a clone of a local repo, a
/// bare repo beside it) names no hosted repo: it is never a rename.
pub fn is_local_origin(url: &str) -> bool {
    let url = url.trim();
    let b = url.as_bytes();
    url.starts_with('/') || url.starts_with("./") || url.starts_with("../") || url.starts_with('~')
        || url.starts_with("file://") || url.starts_with('\\')
        || (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

fn sha1_hex12(text: &str) -> String {
    // The stamp path only has to be stable and collision-free per checkout;
    // FNV-1a keyed on the full path is plenty and keeps the dependency list
    // at zero. Distinct from the Python stamp on purpose: the two
    // implementations must not share a throttle file.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:012x}")
}

/// Per-checkout scratch file for notes that must not repeat on every edit.
pub fn stamp_path(root: &Path, kind: &str) -> PathBuf {
    let digest = sha1_hex12(&root.to_string_lossy());
    std::env::temp_dir().join(format!("collide-rs-{kind}-{digest}.json"))
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// After a background fetch timed out or was refused its credentials, the
/// hooks leave the network alone for this long in that checkout: a stuck
/// keychain must not get one more helper per hook event.
pub const FETCH_BACKOFF_S: f64 = 1800.0;

/// Run `command` in a process group of its own, so a timeout can end
/// everything it started. `git fetch` over https runs `git-remote-https`,
/// which runs the credential helper (`git-credential-osxkeychain get`);
/// killing `git` alone re-parented those to launchd, where a hung keychain
/// lookup lived forever (117 of them in a morning on one Mac).
pub(crate) fn own_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
}

/// Kill a child started with `own_group` and everything in its group, then
/// reap it.
pub(crate) fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let pid = child.id() as i32;
        if pid > 0 {
            // SAFETY: kill(2) with a negative pid signals the process group
            // `own_group` made for this child; it touches no memory.
            unsafe {
                kill(-pid, 9);
            }
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// What a network git command came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fetched {
    Ok,
    Failed,
    /// refused credentials, or would have had to ask for them
    Auth,
    TimedOut,
    /// skipped: this checkout is backing off after a timeout or refusal
    BackingOff,
}

/// Whether git's error output says the remote wanted credentials it could
/// not get without asking.
fn auth_refused(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    [
        "authentication failed",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
        "permission denied (publickey",
        "invalid username or password",
        "http basic: access denied",
        "the requested url returned error: 401",
        "the requested url returned error: 403",
        "host key verification failed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// A git command that goes to the network, on its own process group and
/// bounded by `timeout`. `background` (the hooks' own fetch, nobody waiting
/// on it) makes every credential lookup non-interactive: no terminal prompt,
/// no askpass program, no GUI credential manager. A cached credential or an
/// SSH key still works; anything that would have to ask fails instead.
pub(crate) fn network_git(program: &str, root: &Path, args: &[&str], timeout: Duration, background: bool) -> Fetched {
    let mut command = Command::new(program);
    if background {
        // understood by git 2.46+ and its credential helpers; ignored before
        command.args(["-c", "credential.interactive=false"]);
        command
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .env("GCM_INTERACTIVE", "never");
    }
    command
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    own_group(&mut command);
    let Ok(mut child) = command.spawn() else { return Fetched::Failed };
    // drained as it comes: a full pipe must not hold git up
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
            buf
        })
    });
    let deadline = SystemTime::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if SystemTime::now() >= deadline => break None,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break None,
        }
    };
    let Some(status) = status else {
        kill_group(&mut child);
        // the reader is not waited for: a helper that left the group would
        // hold the pipe open, and the hook must not wait on it
        drop(stderr);
        return Fetched::TimedOut;
    };
    let err = stderr.and_then(|reader| reader.join().ok()).unwrap_or_default();
    if status.success() {
        Fetched::Ok
    } else if auth_refused(&String::from_utf8_lossy(&err)) {
        Fetched::Auth
    } else {
        Fetched::Failed
    }
}

/// The hooks' own `git fetch`, with back-off: after a timeout or a refusal,
/// this checkout is not fetched again for FETCH_BACKOFF_S.
pub(crate) fn background_fetch(root: &Path) -> Fetched {
    background_fetch_with("git", root, Duration::from_secs(FETCH_TIMEOUT_S))
}

pub(crate) fn background_fetch_with(program: &str, root: &Path, timeout: Duration) -> Fetched {
    let stamp = stamp_path(root, "fetch-backoff");
    let now = now_secs();
    if load_json(&stamp).get("until").and_then(Value::as_f64).unwrap_or(0.0) > now {
        return Fetched::BackingOff;
    }
    let outcome = network_git(program, root, &["fetch", "origin", "--quiet"], timeout, true);
    match outcome {
        Fetched::TimedOut | Fetched::Auth => {
            let why = if outcome == Fetched::Auth { "auth" } else { "timeout" };
            let _ = std::fs::write(&stamp, json!({"until": now + FETCH_BACKOFF_S, "why": why}).to_string());
        }
        Fetched::Ok => {
            let _ = std::fs::remove_file(&stamp);
        }
        _ => {}
    }
    outcome
}

pub(crate) fn git(root: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = SystemTime::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    use std::io::Read;
                    let _ = stdout.read_to_string(&mut out);
                }
                return if status.success() { Some(out) } else { None };
            }
            Ok(None) => {
                if SystemTime::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

/// What git says of the files a session changed, for its handoff: the ones
/// with changes not committed, the ones in commits not yet on the upstream,
/// and whether there is an upstream at all. Paths only, never content; at
/// most 40. None when git does not answer within `budget` (the three calls
/// together): a guess would be worse than saying nothing.
pub fn files_state(root: &Path, paths: &[String], budget: Duration) -> Option<Value> {
    if paths.is_empty() {
        return None;
    }
    let deadline = std::time::Instant::now() + budget;
    let left = || deadline.checked_duration_since(std::time::Instant::now()).filter(|d| !d.is_zero());
    let paths: Vec<&str> = paths.iter().take(40).map(String::as_str).collect();
    let mut args = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    args.extend(paths.iter().copied());
    let status = git(root, &args, left()?)?;
    // `XY path`, or `XY old -> new` for a rename
    let uncommitted: Vec<String> = status
        .lines()
        .filter_map(|line| line.get(3..))
        .map(|path| path.rsplit(" -> ").next().unwrap_or(path).trim_matches('"').to_string())
        .collect();
    // no upstream fails this call too: only a call that ran out of time is no answer
    let upstream = git(root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"], left()?).is_some();
    left()?;
    let mut unpushed: Vec<String> = Vec::new();
    if upstream {
        let mut args = vec!["log", "--name-only", "--format=", "@{u}..HEAD", "--"];
        args.extend(paths.iter().copied());
        for path in git(root, &args, left()?)?.lines().map(str::trim).filter(|p| !p.is_empty()) {
            if !unpushed.iter().any(|p| p == path) {
                unpushed.push(path.to_string());
            }
        }
    }
    Some(json!({"uncommitted": uncommitted, "unpushed": unpushed, "upstream": upstream}))
}

/// Whether git IGNORES this path.
///
/// The session-start index walks `git ls-files`, so it never sees an ignored
/// file. The per-edit report had no such check: whatever the agent wrote, it
/// posted, and a gitignored scratch file entered the map for good. On this
/// repo that was nine phantom Python files out of 113, every one under a
/// throwaway harness directory.
///
/// `check-ignore` without `--no-index` says nothing about TRACKED files, which
/// is exactly right: a tracked file that happens to match a pattern is still
/// the user's code. Only 0 means ignored; 1 is "not ignored", 128 is an error,
/// and both of those fail toward reporting — being one file too generous is a
/// far smaller problem than a real edit going unobserved.
pub fn is_ignored(root: &Path, rel: &str) -> bool {
    let Ok(mut child) = Command::new("git")
        .args(["check-ignore", "-q", "--", rel])
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = SystemTime::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code() == Some(0),
            Ok(None) => {
                if SystemTime::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return false,
        }
    }
}

/// Whether this checkout asked Cork to fast-forward it: `"auto_pull":
/// true` in the committed `.cork/config.json`, or `CORK_AUTO_PULL=1`.
/// Off by default — changing someone's working tree is theirs to switch on.
pub fn auto_pull_enabled(root: &Path, env_value: &str) -> bool {
    if env_value.trim() == "1" {
        return true;
    }
    std::fs::read_to_string(crate::config::repo_config_path(root))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|config| config.get("auto_pull").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// Files the incoming commits change that this checkout changed too: in its
/// working tree (uncommitted or untracked) or in commits not yet pushed.
fn overlap(root: &Path) -> Vec<String> {
    let names = |args: &[&str]| -> Vec<String> {
        git(root, args, Duration::from_secs(2))
            .map(|out| out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
            .unwrap_or_default()
    };
    let incoming = names(&["diff", "--name-only", "HEAD...@{u}"]);
    if incoming.is_empty() {
        return Vec::new();
    }
    let mut mine: Vec<String> = names(&["diff", "--name-only", "@{u}...HEAD"]);
    mine.extend(names(&["diff", "--name-only", "HEAD"]));
    mine.extend(names(&["ls-files", "--others", "--exclude-standard"]));
    incoming.into_iter().filter(|path| mine.contains(path)).collect()
}

/// Git sync. Once per interval the network fetch runs synchronously
/// (bounded) so this hook fire sees teammates' just-pushed commits. By
/// default Cork only TELLS the agent the checkout is behind, once per
/// behind-count; the working tree is never touched. With auto-pull on it
/// fast-forwards — `--ff-only` is the safety: a dirty tree or a diverged
/// branch aborts untouched and downgrades to advising the agent.
/// Whether a path holds tests, by convention (the server's
/// `graphview::is_test_path`, twin): a tests/ directory above it, pytest's
/// `test_*.py` and `conftest.py`, `*_test.go`, `*_spec.rb`, `*.test.ts`,
/// `FooTest.java`.
pub fn is_test_path(path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    let name = parts.pop().unwrap_or("");
    if parts.iter().any(|part| ["tests", "test", "__tests__", "spec", "specs", "testing"].contains(part)) || name == "conftest.py" {
        return true;
    }
    let stem = name.split_once('.').map(|(head, _)| head).unwrap_or(name);
    stem.starts_with("test_") || stem.starts_with("test-") || stem.ends_with("_test") || stem.ends_with("-test")
        || stem.ends_with("_spec") || stem.ends_with("-spec") || name.contains(".test.") || name.contains(".spec.")
        || stem.ends_with("Test") || stem.ends_with("Tests") || stem.ends_with("Spec") || stem.ends_with("Specs")
}

pub fn sync_note(root: &Path, auto_pull: bool) -> String {
    sync_note_with(root, auto_pull, false)
}

/// The old-side line ranges (`start..end`, merge-base or HEAD numbering)
/// each hunk of `git diff -U0 <args>` touches, per file.
fn hunk_ranges(root: &Path, args: &[&str]) -> Vec<(String, usize, usize)> {
    let mut full: Vec<&str> = vec!["diff", "-U0", "--no-color"];
    full.extend_from_slice(args);
    let Some(out) = git(root, &full, Duration::from_secs(2)) else { return Vec::new() };
    let mut file = String::new();
    let mut ranges = Vec::new();
    for line in out.lines() {
        if let Some(name) = line.strip_prefix("--- a/") {
            file = name.to_string();
        } else if let Some(name) = line.strip_prefix("+++ b/") {
            if file.is_empty() {
                file = name.to_string();
            }
        } else if line.starts_with("--- ") {
            file.clear();
        } else if let Some(rest) = line.strip_prefix("@@ -") {
            let old = rest.split_whitespace().next().unwrap_or("");
            let mut parts = old.split(',');
            let start: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let count: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            // an insertion (count 0) sits between two lines: it conflicts with
            // an edit to either, as git's own merge does
            ranges.push((file.clone(), start, start + count.max(1)));
        }
    }
    ranges
}

/// Whether the incoming commits change the same lines (or the lines next to
/// them) as this checkout's own changes, in any of `files`: where a rebase
/// would stop on a conflict. Everything else landing rebases cleanly.
fn hunks_overlap(root: &Path, files: &[String]) -> bool {
    let mut incoming_args: Vec<&str> = vec!["HEAD...@{u}", "--"];
    incoming_args.extend(files.iter().map(String::as_str));
    let incoming = hunk_ranges(root, &incoming_args);
    let mut mine = {
        let mut args: Vec<&str> = vec!["@{u}...HEAD", "--"];
        args.extend(files.iter().map(String::as_str));
        hunk_ranges(root, &args)
    };
    let mut args: Vec<&str> = vec!["HEAD", "--"];
    args.extend(files.iter().map(String::as_str));
    mine.extend(hunk_ranges(root, &args));
    incoming.iter().any(|(f, a, b)| mine.iter().any(|(g, c, d)| f == g && *a <= *d && *c <= *b))
}

/// `sync_note`, for a checkout whose pushes land through Cork (`lands`):
/// a teammate's commits on origin are then something landing rebases onto,
/// and the agent is told to pull only when they change the very lines it
/// changed too (Study 8: "your copy is behind; pull and re-read" was the
/// largest reaction cost at 16 agents, and landing rebased anyway).
pub fn sync_note_with(root: &Path, auto_pull: bool, lands: bool) -> String {
    let stamp = stamp_path(root, "sync");
    let mut state = load_json(&stamp);
    let now = now_secs();
    let last = state.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    if now - last >= SYNC_INTERVAL_S {
        if let Some(map) = state.as_object_mut() {
            map.insert("ts".into(), json!(now));
        }
        let _ = std::fs::write(&stamp, state.to_string());
        let _ = background_fetch(root);
    }

    let behind: i64 = git(root, &["rev-list", "--count", "HEAD..@{u}"], Duration::from_secs(2))
        .and_then(|out| out.trim().parse().ok())
        .unwrap_or(0);
    if behind <= 0 {
        return String::new();
    }

    // who pushed the incoming commits, captured BEFORE the ff-merge while the
    // range still names exactly the pulled commits
    let mut authors = String::new();
    if let Some(log) = git(
        root,
        &["log", "HEAD..@{u}", "--format=%an", "-n", "30"],
        Duration::from_secs(2),
    ) {
        let mut names: Vec<String> = Vec::new();
        for name in log.lines() {
            let name = name.trim();
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        if !names.is_empty() {
            let shown = names.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
            let ellipsis = if names.len() > 4 { "…" } else { "" };
            authors = format!(" from {shown}{ellipsis}");
        }
    }

    if !auto_pull {
        // only when the incoming commits change a file this checkout changed:
        // anything else the push's rebase takes in, and a teammate's change
        // to code the agent reads arrives as a delta. Study 5's agents pulled
        // on every nudge before they had edited anything.
        let shared = overlap(root);
        if shared.is_empty() {
            return String::new();
        }
        // landing rebases onto them: only lines both sides changed are news,
        // said once per set of files (not again as each new commit arrives),
        // and never as "pull" (Study 8, second study: 23-27 pull nudges per
        // round drove 34 manual pulls and most of the remaining conflicts)
        if lands {
            if !hunks_overlap(root, &shared) {
                return String::new();
            }
            let files = shared.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
            // once per landing burst: teammates land in waves, and each
            // commit of a wave is not news again (Study 8, second rerun: 17
            // git-sync notices a run)
            let last = state.get("told_at").and_then(Value::as_f64).unwrap_or(0.0);
            if state.get("told_files").and_then(Value::as_str) == Some(files.as_str()) || now - last < SYNC_BURST_S {
                return String::new();
            }
            if let Some(map) = state.as_object_mut() {
                map.insert("told_files".into(), json!(files));
                map.insert("told_at".into(), json!(now));
            }
            let _ = std::fs::write(&stamp, state.to_string());
            let ellipsis = if shared.len() > 3 { "…" } else { "" };
            return format!(
                "Cork git-sync: {behind} commit(s) on origin{authors} change the same lines of {files}{ellipsis} that you \
changed. Landing will rebase onto them when you push and stop on any line both sides changed: keep your edit to \
your own part and carry on."
            );
        }
        if state.get("told").and_then(Value::as_i64) == Some(behind) {
            return String::new(); // told about this state already
        }
        if let Some(map) = state.as_object_mut() {
            map.insert("told".into(), json!(behind));
        }
        let _ = std::fs::write(&stamp, state.to_string());
        let ellipsis = if shared.len() > 3 { "…" } else { "" };
        let files = shared.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
        return format!(
            "Cork git-sync: origin is {behind} commit(s) ahead{authors}, changing {files}{ellipsis} \
that you changed here too. Commit, `git pull --rebase`, and re-read those files before you \
go on; or set \"auto_pull\": true in .cork/config.json to have Cork fast-forward this \
checkout for you."
        );
    }
    if git(root, &["merge", "--ff-only", "@{u}"], Duration::from_secs(5)).is_some() {
        if lands {
            return format!(
                "Cork git-sync: fast-forwarded this checkout by {behind} commit(s){authors}. Files may have changed on \
disk: re-read anything you loaded before this point instead of editing from memory."
            );
        }
        return format!(
            "Cork git-sync: auto-pulled {behind} commit(s){authors} into this checkout. \
Files may have changed on disk — re-read anything you loaded before this point instead of \
editing from memory, and check since_your_last_call/get_briefing for who landed what (each \
carries a resolved identity: nickname / @username / email)."
        );
    }

    // landing rebases what the fast-forward could not take in
    if lands {
        return String::new();
    }
    if state.get("warned").and_then(Value::as_i64) == Some(behind) {
        return String::new(); // same stuck state: one warning, not one per tool call
    }
    if let Some(map) = state.as_object_mut() {
        map.insert("warned".into(), json!(behind));
    }
    let _ = std::fs::write(&stamp, state.to_string());
    format!(
        "Cork git-sync: origin is {behind} commit(s) ahead but auto-pull could not \
fast-forward (uncommitted overlapping changes or a diverged branch). Reconcile at your next \
safe moment: commit or stash, then `git pull --ff-only`."
    )
}

#[cfg(test)]
mod ignore_tests {
    use super::*;

    fn sh(root: &Path, args: &[&str]) {
        let status = Command::new("git").args(args).current_dir(root)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .status().expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn ignored_untracked_and_tracked_paths_are_told_apart() {
        let root = std::env::temp_dir().join(format!("collide-ignore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("scratch")).unwrap();
        sh(&root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "scratch/\n").unwrap();
        std::fs::write(root.join("kept.py"), "x = 1\n").unwrap();
        std::fs::write(root.join("new.py"), "y = 2\n").unwrap();
        std::fs::write(root.join("scratch").join("junk.py"), "z = 3\n").unwrap();
        sh(&root, &["add", ".gitignore", "kept.py"]);

        assert!(is_ignored(&root, "scratch/junk.py"), "a gitignored path is ignored");
        assert!(!is_ignored(&root, "kept.py"), "a tracked path is never ignored");
        assert!(!is_ignored(&root, "new.py"), "untracked but not ignored is still the user's code");
        assert!(!is_ignored(&root, "does/not/exist.py"), "an absent path fails toward reporting");
        let _ = std::fs::remove_dir_all(&root);
        // no repository at all: check-ignore fails, which must read as "report it"
        let bare = std::env::temp_dir().join(format!("collide-ignore-none-{}", std::process::id()));
        std::fs::create_dir_all(&bare).unwrap();
        assert!(!is_ignored(&bare, "anything.py"));
        let _ = std::fs::remove_dir_all(&bare);
    }

    #[test]
    fn branch_and_origin_come_straight_out_of_dot_git() {
        let root = std::env::temp_dir().join(format!("collide-head-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/feat/gutters\n").unwrap();
        assert_eq!(branch(&root), "feat/gutters");
        std::fs::write(root.join(".git").join("HEAD"), "a1b2c3d4e5f6a7b8c9d0\n").unwrap(); // detached
        assert_eq!(branch(&root), "a1b2c3d4e5f6");
        assert_eq!(branch(&root.join("nogit")), "");
        for url in ["https://github.com/lithometric/cork.git", "git@github.com:lithometric/cork.git"] {
            std::fs::write(root.join(".git").join("config"),
                           format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {url}\n")).unwrap();
            assert_eq!(origin_repo_id(&root), "github.com/lithometric/cork", "{url}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Opted in, the next hook call fast-forwards the checkout and says so;
    /// overlapping local changes make ff-only abort with the tree untouched,
    /// and that is said once, not on every tool call.
    #[test]
    fn auto_pull_fast_forwards_or_warns_once() {
        let base = std::env::temp_dir().join(format!("collide-autopull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, &["init", "-q", "--bare", "origin.git"]);
        sh(&base, &["clone", "-q", "origin.git", "seed"]);
        let seed = base.join("seed");
        sh(&seed, &["config", "user.email", "t@t"]);
        sh(&seed, &["config", "user.name", "t"]);
        std::fs::write(seed.join("a.txt"), "one\n").unwrap();
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-qm", "one"]);
        sh(&seed, &["push", "-q", "-u", "origin", "HEAD"]);
        sh(&base, &["clone", "-q", "origin.git", "clone"]);
        let clone = base.join("clone");
        let _ = std::fs::remove_file(stamp_path(&clone, "sync"));
        assert_eq!(sync_note(&clone, true), "", "current: quiet");

        std::fs::write(seed.join("a.txt"), "two\n").unwrap();
        sh(&seed, &["commit", "-qam", "two"]);
        sh(&seed, &["push", "-q", "origin", "HEAD"]);
        sh(&clone, &["fetch", "-q", "origin"]);
        let note = sync_note(&clone, true);
        assert!(note.contains("auto-pulled 1 commit(s)") && note.contains("re-read"), "{note}");
        assert_eq!(std::fs::read_to_string(clone.join("a.txt")).unwrap(), "two\n");

        std::fs::write(seed.join("a.txt"), "three\n").unwrap();
        sh(&seed, &["commit", "-qam", "three"]);
        sh(&seed, &["push", "-q", "origin", "HEAD"]);
        sh(&clone, &["fetch", "-q", "origin"]);
        std::fs::write(clone.join("a.txt"), "local dirt\n").unwrap();
        let note = sync_note(&clone, true);
        assert!(note.contains("could not fast-forward"), "{note}");
        assert_eq!(std::fs::read_to_string(clone.join("a.txt")).unwrap(), "local dirt\n");
        assert_eq!(sync_note(&clone, true), "", "no repeat nag");
        let _ = std::fs::remove_file(stamp_path(&clone, "sync"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// With landing on, a teammate's push to a file this checkout changed
    /// too is no pull-now unless it changes the same lines: landing rebases
    /// onto it (Study 8's largest reaction cost at 16 agents).
    #[test]
    fn with_landing_on_origin_ahead_is_told_only_when_the_same_lines_changed() {
        let base = std::env::temp_dir().join(format!("collide-sync-land-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, &["init", "-q", "--bare", "origin.git"]);
        sh(&base, &["clone", "-q", "origin.git", "seed"]);
        let seed = base.join("seed");
        sh(&seed, &["config", "user.email", "t@t"]);
        sh(&seed, &["config", "user.name", "t"]);
        let lines: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(seed.join("a.txt"), &lines).unwrap();
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-qm", "one"]);
        sh(&seed, &["push", "-q", "-u", "origin", "HEAD"]);
        sh(&base, &["clone", "-q", "origin.git", "clone"]);
        let clone = base.join("clone");
        // a teammate changes line 2 and pushes; this checkout changes line 18
        std::fs::write(seed.join("a.txt"), lines.replace("line 2\n", "line two\n")).unwrap();
        sh(&seed, &["commit", "-qam", "two"]);
        sh(&seed, &["push", "-q", "origin", "HEAD"]);
        sh(&clone, &["fetch", "-q", "origin"]);
        std::fs::write(clone.join("a.txt"), lines.replace("line 18\n", "line eighteen\n")).unwrap();
        let _ = std::fs::remove_file(stamp_path(&clone, "sync"));
        assert_eq!(sync_note_with(&clone, false, true), "", "landing rebases onto it: nothing to say");
        // without landing the same state is a pull nudge
        let note = sync_note_with(&clone, false, false);
        assert!(note.contains("git pull --rebase"), "{note}");
        // the same lines: said once, and as "landing will rebase", never "pull"
        std::fs::write(clone.join("a.txt"), lines.replace("line 2\n", "line deux\n")).unwrap();
        let _ = std::fs::remove_file(stamp_path(&clone, "sync"));
        let note = sync_note_with(&clone, false, true);
        assert!(note.contains("Landing will rebase onto them") && !note.to_lowercase().contains("pull"), "{note}");
        assert_eq!(sync_note_with(&clone, false, true), "", "said once");
        let _ = std::fs::remove_file(stamp_path(&clone, "sync"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn origin_ahead_is_told_only_when_it_touches_what_this_checkout_changed() {
        let base = std::env::temp_dir().join(format!("collide-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, &["init", "-q", "--bare", "origin.git"]);
        sh(&base, &["clone", "-q", "origin.git", "seed"]);
        let seed = base.join("seed");
        sh(&seed, &["config", "user.email", "t@t"]);
        sh(&seed, &["config", "user.name", "t"]);
        std::fs::write(seed.join("a.txt"), "one\n").unwrap();
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-qm", "one"]);
        sh(&seed, &["push", "-q", "-u", "origin", "HEAD"]);
        sh(&base, &["clone", "-q", "origin.git", "clone"]);
        let clone = base.join("clone");
        std::fs::write(seed.join("a.txt"), "two\n").unwrap();
        sh(&seed, &["commit", "-qam", "two"]);
        sh(&seed, &["push", "-q", "origin", "HEAD"]);
        sh(&clone, &["fetch", "-q", "origin"]);
        let _ = std::fs::remove_file(stamp_path(&clone, "sync"));

        std::fs::write(clone.join("b.txt"), "mine\n").unwrap();
        assert_eq!(sync_note(&clone, false), "", "nothing incoming touches what this checkout changed");
        std::fs::write(clone.join("a.txt"), "one, edited here\n").unwrap();
        let note = sync_note(&clone, false);
        assert!(note.contains("origin is 1 commit(s) ahead") && note.contains("changing a.txt"), "{note}");
        assert_eq!(sync_note(&clone, false), "", "told once");
        let _ = std::fs::remove_file(stamp_path(&clone, "sync"));
        let _ = std::fs::remove_dir_all(&base);
    }
}

fn git_lines(root: &Path, args: &[&str]) -> Vec<String> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// The repo's branch names, its own and the remote's (origin), for the
/// dashboard's branch switcher.
pub fn branches(root: &Path) -> Vec<String> {
    // `lstrip=2` names refs/heads/x as x; `lstrip=3` names
    // refs/remotes/origin/x as x too, so a branch both here and on the
    // remote is listed once. The `worktree-…` branches agent tools make for
    // a worktree of their own (Claude Code's worktree-agent-…, a workflow's
    // worktree-wf_…) are scratch, dozens a day: left off the list (an agent
    // on one still shows it, from its own row)
    let local = git_lines(root, &["for-each-ref", "--count=200", "--format=%(refname:lstrip=2)", "refs/heads"]);
    let remote = git_lines(root, &["for-each-ref", "--count=200", "--format=%(refname:lstrip=3)", "refs/remotes/origin"]);
    let mut names: Vec<String> = Vec::new();
    for name in local.into_iter().chain(remote) {
        let name: String = name.trim().chars().take(80).collect();
        if !name.is_empty() && name != "HEAD" && !name.starts_with("worktree-") && !names.contains(&name) {
            names.push(name);
        }
    }
    names.truncate(200);
    names
}

#[cfg(all(test, unix))]
mod fetch_tests {
    //! The background fetch on a Mac whose keychain helper hangs: nothing it
    //! started outlives its timeout, and the checkout backs off.
    use super::*;

    /// A fake `git` (a shell script) and a checkout of its own: the back-off
    /// stamp is per checkout.
    fn fake(name: &str, body: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("collide-fetch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("checkout")).unwrap();
        let script = dir.join("git");
        std::fs::write(&script, format!("#!/bin/sh\necho run >> \"$0.calls\"\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = dir.join("checkout");
        let _ = std::fs::remove_file(stamp_path(&root, "fetch-backoff"));
        (script, root)
    }

    fn calls(script: &Path) -> usize {
        std::fs::read_to_string(script.with_extension("calls")).map(|s| s.lines().count()).unwrap_or(0)
    }

    fn alive(pid: &str) -> bool {
        Command::new("kill").args(["-0", pid]).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
    }

    #[test]
    fn a_hung_credential_helper_dies_with_the_fetch_and_the_checkout_backs_off() {
        // git-remote-https, then git-credential-osxkeychain get, which hangs.
        // The fake git waits until its helper is running, and the timeout
        // leaves room for that on a loaded machine: with 400 ms, a busy CI
        // runner sometimes killed the group before the helper had started,
        // and there was no helper left to check.
        let (script, root) = fake(
            "hang",
            "sh -c 'echo $$ > \"$1\"; exec sleep 300' helper \"$0.pid\" &\nwhile [ ! -s \"$0.pid\" ]; do sleep 0.02; done\nwait",
        );
        let outcome = background_fetch_with(script.to_str().unwrap(), &root, Duration::from_secs(3));
        assert_eq!(outcome, Fetched::TimedOut);
        let pid = std::fs::read_to_string(script.with_extension("pid")).expect("the helper started").trim().to_string();
        // killed with its group, and reaped by init once its parent is gone
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive(&pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(&pid), "the credential helper {pid} outlived the fetch's timeout");
        // and the next hook events leave the network alone
        for _ in 0..3 {
            assert_eq!(background_fetch_with(script.to_str().unwrap(), &root, Duration::from_millis(400)), Fetched::BackingOff);
        }
        assert_eq!(calls(&script), 1, "one helper per back-off window, not one per hook event");
        let stamp = load_json(&stamp_path(&root, "fetch-backoff"));
        assert_eq!(stamp["why"], "timeout");
        assert!(stamp["until"].as_f64().unwrap() >= now_secs() + FETCH_BACKOFF_S - 5.0);
    }

    #[test]
    fn refused_credentials_back_off_and_a_plain_failure_does_not() {
        let (script, root) = fake(
            "auth",
            "echo \"fatal: could not read Username for 'https://github.com': terminal prompts disabled\" >&2\nexit 128",
        );
        assert_eq!(background_fetch_with(script.to_str().unwrap(), &root, Duration::from_secs(5)), Fetched::Auth);
        assert_eq!(background_fetch_with(script.to_str().unwrap(), &root, Duration::from_secs(5)), Fetched::BackingOff);
        assert_eq!(calls(&script), 1);

        let (script, root) = fake("fail", "echo \"fatal: couldn't find remote ref main\" >&2\nexit 1");
        assert_eq!(background_fetch_with(script.to_str().unwrap(), &root, Duration::from_secs(5)), Fetched::Failed);
        assert_eq!(background_fetch_with(script.to_str().unwrap(), &root, Duration::from_secs(5)), Fetched::Failed);
        assert_eq!(calls(&script), 2, "a failure that is not about credentials is tried again");
    }

    #[test]
    fn a_background_fetch_never_asks_and_a_working_one_clears_the_back_off() {
        let (script, root) = fake("ok", "echo \"$@\" > \"$0.args\"\nenv > \"$0.env\"\nexit 0");
        // a stale back-off from an hour ago does not hold
        std::fs::write(stamp_path(&root, "fetch-backoff"), json!({"until": now_secs() - 3600.0}).to_string()).unwrap();
        assert_eq!(background_fetch_with(script.to_str().unwrap(), &root, Duration::from_secs(5)), Fetched::Ok);
        assert!(!stamp_path(&root, "fetch-backoff").exists());
        let args = std::fs::read_to_string(script.with_extension("args")).unwrap();
        assert!(args.starts_with("-c credential.interactive=false fetch origin"), "{args}");
        let env: Vec<String> = std::fs::read_to_string(script.with_extension("env")).unwrap().lines().map(str::to_string).collect();
        for want in ["GIT_TERMINAL_PROMPT=0", "GIT_ASKPASS=", "SSH_ASKPASS=", "GCM_INTERACTIVE=never"] {
            assert!(env.iter().any(|line| line == want), "{want} not set for a background fetch");
        }
    }
}
