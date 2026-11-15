//! Worktrees: why an agent may not need one here, and tidying the ones
//! agent tools leave behind.
//!
//! Agent tools give a session (or an isolated sub-agent) its own worktree
//! so parallel agents don't overwrite each other: Claude Code under the
//! repo's `.claude/worktrees/`, Cursor's parallel agents under
//! `~/.cursor/worktrees/`, Codex and Conductor in their own folders (see
//! [`AGENT_HOMES`]; a repo or machine adds others with `agent_worktree_dirs`
//! / `CORK_AGENT_WORKTREE_DIRS`). With Cork in a checkout, that is
//! already handled whichever agent is writing: every write is
//! checked against teammates' live changes before it lands. So at session
//! start, in a repo that has such worktrees, the agent is told so — as a
//! recommendation it can check, never a rule: a worktree is still the right
//! call for a multi-step feature built and tested on its own branch.
//!
//! An agent has no reason yet to trust Cork with this, so the tidying is
//! built to be safe to ignore: it only ever removes a worktree that
//!
//! - an agent tool made (in one of the agent worktree folders: never one a
//!   person made by hand somewhere else),
//! - is not locked,
//! - is on a branch (or a detached commit) already merged into the main
//!   checkout's branch or the remote's default branch,
//! - has no changes, staged, unstaged or untracked, and
//! - nobody has touched for [`QUIET_S`] (a fresh one is someone's next task),
//!
//! with plain `git worktree remove` (which refuses a dirty one by itself),
//! keeps every branch, and says what it removed and how to bring it back.
//! It runs detached, re-checking every condition at removal time, so
//! session start does not wait on it. `"tidy_worktrees": false` in
//! `.collide/config.json` or `CORK_TIDY_WORKTREES=0` turns it off.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use crate::config::{self, Env};

/// Where agent tools keep the worktrees they make: `repo:` relative to the
/// main checkout, `home:` relative to the user's home folder.
pub const AGENT_HOMES: [&str; 6] = [
    "repo:.claude/worktrees",    // Claude Code
    "repo:.codex/worktrees",     // Codex (a repo-local layout)
    "home:.codex/worktrees",     // Codex app
    "home:.cursor/worktrees",    // Cursor's parallel agents
    "home:conductor/workspaces", // Conductor
    "repo:.agent-worktrees",     // a neutral name for any other tool
];

/// A worktree untouched for less than this is in use, or about to be.
pub const QUIET_S: u64 = 24 * 3600;
/// The guidance is said at most this often per repo.
const SAY_EVERY_S: u64 = 24 * 3600;
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default)]
pub struct Worktree {
    pub path: PathBuf,
    /// `refs/heads/<name>` stripped to the name; empty when detached
    pub branch: String,
    pub head: String,
    pub locked: bool,
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    crate::git::git(root, args, GIT_TIMEOUT)
}

/// Every worktree of the repo, the main checkout first.
pub fn list(root: &Path) -> Vec<Worktree> {
    let Some(out) = git(root, &["worktree", "list", "--porcelain"]) else { return Vec::new() };
    let mut all = Vec::new();
    let mut current: Option<Worktree> = None;
    for line in out.lines().chain(std::iter::once("")) {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(Worktree { path: PathBuf::from(path.trim()), ..Worktree::default() });
        } else if let Some(wt) = current.as_mut() {
            if let Some(head) = line.strip_prefix("HEAD ") {
                wt.head = head.trim().to_string();
            } else if let Some(branch) = line.strip_prefix("branch ") {
                wt.branch = branch.trim().trim_start_matches("refs/heads/").to_string();
            } else if line.starts_with("locked") {
                wt.locked = true;
            } else if line.is_empty() {
                all.push(current.take().unwrap_or_default());
            }
        }
    }
    all
}

/// The folders agent tools keep worktrees in, for this repo: the built-in
/// [`AGENT_HOMES`] plus `agent_worktree_dirs` from `.collide/config.json`
/// and `CORK_AGENT_WORKTREE_DIRS` (`:`-separated; absolute, `~/…`, or
/// relative to the main checkout).
pub fn agent_homes(main: &Path, env: &Env) -> Vec<PathBuf> {
    // git reports worktrees by their real path (on a Mac /var is
    // /private/var), so every folder is compared the same way
    let real = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    let home = real(PathBuf::from(config::get(env, "HOME")));
    let resolve = |spec: &str| -> Option<PathBuf> {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        Some(if let Some(rest) = spec.strip_prefix("repo:") {
            main.join(rest)
        } else if let Some(rest) = spec.strip_prefix("home:").or_else(|| spec.strip_prefix("~/")) {
            home.join(rest)
        } else if Path::new(spec).is_absolute() {
            PathBuf::from(spec)
        } else {
            main.join(spec)
        })
    };
    let mut out: Vec<PathBuf> = AGENT_HOMES.iter().filter_map(|s| resolve(s)).collect();
    if let Some(extra) = config::repo_config(&main).get("agent_worktree_dirs").and_then(Value::as_array) {
        out.extend(extra.iter().filter_map(Value::as_str).filter_map(resolve));
    }
    out.extend(config::get(env, "CORK_AGENT_WORKTREE_DIRS").split(':').filter_map(resolve));
    let mut out: Vec<PathBuf> = out.into_iter().map(real).collect();
    // a bare home folder would claim every worktree under it
    out.retain(|p| p != &home && p.parent().is_some());
    out.dedup();
    out
}

/// The worktrees agent tools made: in one of [`agent_homes`].
pub fn agents(all: &[Worktree], env: &Env) -> Vec<Worktree> {
    let Some(main) = all.first() else { return Vec::new() };
    let homes = agent_homes(&main.path, env);
    all.iter().skip(1).filter(|w| homes.iter().any(|h| w.path.starts_with(h))).cloned().collect()
}

/// What "merged" is measured against: the main checkout's HEAD, and the
/// remote's default branch when there is one.
fn bases(main: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(head) = git(main, &["rev-parse", "HEAD"]) {
        out.push(head.trim().to_string());
    }
    if let Some(remote) = git(main, &["rev-parse", "--verify", "--quiet", "refs/remotes/origin/HEAD"]) {
        out.push(remote.trim().to_string());
    }
    out.retain(|s| !s.is_empty());
    out.dedup();
    out
}

/// The newest moment anyone touched the worktree: its folder, its `.git`
/// link, and git's own record of it (HEAD, index, reflog).
fn last_touched(main: &Path, wt: &Worktree) -> SystemTime {
    let name = wt.path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    let gitdir = main.join(".git").join("worktrees").join(name);
    [
        wt.path.clone(),
        wt.path.join(".git"),
        gitdir.join("HEAD"),
        gitdir.join("index"),
        gitdir.join("logs").join("HEAD"),
    ]
    .iter()
    .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
    .max()
    .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Why this worktree stays, or `None` when it is finished and may go.
pub fn keep_reason(main: &Path, wt: &Worktree, bases: &[String], quiet_s: u64, now: SystemTime) -> Option<&'static str> {
    if wt.locked {
        return Some("locked");
    }
    if !wt.path.is_dir() {
        return Some("missing");
    }
    let age = now.duration_since(last_touched(main, wt)).unwrap_or_default();
    if age < Duration::from_secs(quiet_s) {
        return Some("recently used");
    }
    match git(&wt.path, &["status", "--porcelain", "--untracked-files=normal"]) {
        Some(status) if status.trim().is_empty() => {}
        _ => return Some("has changes"),
    }
    let tip = if wt.branch.is_empty() { wt.head.clone() } else { wt.branch.clone() };
    if tip.is_empty() {
        return Some("no commit");
    }
    let merged = bases.iter().any(|base| {
        std::process::Command::new("git")
            .args(["merge-base", "--is-ancestor", &tip, base])
            .current_dir(main)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    });
    if !merged {
        return Some("not merged");
    }
    None
}

/// The finished worktrees: what [`tidy`] would remove now.
pub fn finished(root: &Path, env: &Env, quiet_s: u64, now: SystemTime) -> Vec<Worktree> {
    let all = list(root);
    let Some(main) = all.first().map(|w| w.path.clone()) else { return Vec::new() };
    let bases = bases(&main);
    if bases.is_empty() {
        return Vec::new();
    }
    agents(&all, env).into_iter().filter(|wt| keep_reason(&main, wt, &bases, quiet_s, now).is_none()).collect()
}

/// Remove the finished worktrees (re-checked here, at removal time) and
/// forget the ones whose folder is already gone. Branches stay. Returns the
/// names removed.
pub fn tidy(root: &Path, env: &Env, quiet_s: u64, now: SystemTime) -> Vec<String> {
    let mut removed = Vec::new();
    for wt in finished(root, env, quiet_s, now) {
        let path = wt.path.to_string_lossy().to_string();
        // no --force: git itself refuses a worktree with changes
        if git(root, &["worktree", "remove", &path]).is_some() {
            let name = wt.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(path);
            removed.push(if wt.branch.is_empty() { name } else { format!("{name} ({})", wt.branch) });
        }
    }
    let _ = git(root, &["worktree", "prune"]);
    removed
}

pub fn enabled(root: &Path, env: &Env) -> bool {
    if config::get(env, "CORK_TIDY_WORKTREES").trim() == "0" {
        return false;
    }
    config::repo_config(&root)
        .get("tidy_worktrees")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

// ------------------------------------------------------------ the record

/// `~/.collide/worktrees/<repo>.json`: when the guidance was last said and
/// what the last detached tidy removed (said once, then cleared).
fn record_path(main: &Path, env: &Env) -> PathBuf {
    let key = format!("{:x}", md5_like(main.to_string_lossy().as_bytes()));
    config::data_dir_in(Path::new(config::get(env, "HOME"))).join("worktrees").join(format!("{key}.json"))
}

/// A short stable hash for a file name (not security).
fn md5_like(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3))
}

fn now_s(now: SystemTime) -> u64 {
    now.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn read_record(path: &Path) -> Value {
    std::fs::read_to_string(path).ok().and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_else(|| json!({}))
}

fn write_record(path: &Path, record: &Value) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, record.to_string());
}

/// `collide-hook tidy-worktrees <root>`: the detached half. Removes what is
/// finished and leaves the names for the next session start to say.
pub fn tidy_command(root: &str, env: &Env) -> i32 {
    let root = PathBuf::from(root);
    if !enabled(&root, env) {
        return 0;
    }
    let now = SystemTime::now();
    let removed = tidy(&root, env, QUIET_S, now);
    if removed.is_empty() {
        return 0;
    }
    let Some(main) = list(&root).first().map(|w| w.path.clone()) else { return 0 };
    let path = record_path(&main, env);
    let mut record = read_record(&path);
    let mut all: Vec<Value> = record.get("removed").and_then(Value::as_array).cloned().unwrap_or_default();
    all.extend(removed.into_iter().map(Value::String));
    record["removed"] = Value::Array(all);
    record["removed_at"] = json!(now_s(now));
    write_record(&path, &record);
    0
}

/// What session start says about worktrees, if anything, and the detached
/// tidy it starts. Said when the repo has Claude worktrees (the guidance, at
/// most daily) or a tidy removed some since the last session (always, once).
pub fn session_start(root: &Path, env: &Env) -> Option<String> {
    let all = list(root);
    let main = all.first()?.path.clone();
    let mine = agents(&all, env);
    let path = record_path(&main, env);
    let mut record = read_record(&path);
    let now = SystemTime::now();
    let mut said = Vec::new();

    let removed: Vec<String> = record
        .get("removed")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if !removed.is_empty() {
        said.push(format!(
            "Cork removed {} finished agent worktree(s): {}. Each was merged, had no changes and sat untouched for \
a day; the branches are kept (`git worktree add <folder> <branch>` brings one back).",
            removed.len(),
            removed.join(", ")
        ));
        record["removed"] = json!([]);
    }

    let last_said = record.get("said_at").and_then(Value::as_u64).unwrap_or(0);
    if !mine.is_empty() && now_s(now).saturating_sub(last_said) >= SAY_EVERY_S {
        said.push(format!(
            "Cork: this repo has {} agent worktree(s). You don't need one to keep from overwriting other agents, \
whichever tool runs them: Cork checks every write in this checkout against their live changes before it lands, and stops only a real \
collision, naming who and what (you will see it in the tool result if it happens). Prefer working in the main \
checkout; a worktree is still right for a multi-step feature you build and test on its own branch, and Cork \
follows you there too. Finished worktrees (merged, no changes, untouched for a day) are tidied for you; one with \
work in it is never touched, and branches are kept.",
            mine.len()
        ));
        record["said_at"] = json!(now_s(now));
    }
    if !said.is_empty() {
        write_record(&path, &record);
    }

    // the tidy itself, detached: a big worktree takes a while to delete
    if !mine.is_empty() && enabled(root, env) {
        if let Ok(exe) = std::env::current_exe() {
            let mut command = std::process::Command::new(exe);
            command
                .arg("tidy-worktrees")
                .arg(&main)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            let _ = command.spawn();
        }
    }
    (!said.is_empty()).then(|| said.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
    }

    /// A repo on `main` with agent worktrees: Claude Code's in the repo (one
    /// merged and clean, one merged with a stray file, one with an unmerged
    /// commit, one locked), Cursor's under the home folder (merged, clean),
    /// and one a person made by hand somewhere else.
    fn repo() -> PathBuf {
        let base = std::env::temp_dir().join(format!("collide-wt-{}-{}", std::process::id(), rand_suffix()));
        let _ = std::fs::remove_dir_all(&base);
        let main = base.join("main");
        std::fs::create_dir_all(&main).unwrap();
        run(&main, &["init", "-q", "-b", "main"]);
        run(&main, &["config", "user.email", "t@t"]);
        run(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join("a.txt"), "a").unwrap();
        run(&main, &["add", "."]);
        run(&main, &["commit", "-qm", "one"]);
        for (name, branch) in [("done", "feat/done"), ("stray", "feat/stray"), ("open", "feat/open"), ("held", "feat/held")] {
            let path = main.join(".claude/worktrees").join(name);
            run(&main, &["worktree", "add", "-q", "-b", branch, &path.to_string_lossy()]);
        }
        run(&main, &["worktree", "add", "-q", "-b", "feat/elsewhere", &base.join("elsewhere").to_string_lossy()]);
        let cursor = base.join("home/.cursor/worktrees/main/quick");
        std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
        run(&main, &["worktree", "add", "-q", "-b", "cursor/quick", &cursor.to_string_lossy()]);
        let open = main.join(".claude/worktrees/open");
        std::fs::write(open.join("b.txt"), "b").unwrap();
        run(&open, &["add", "."]);
        run(&open, &["commit", "-qm", "unmerged"]);
        std::fs::write(main.join(".claude/worktrees/stray/notes.txt"), "mine").unwrap();
        run(&main, &["worktree", "lock", &main.join(".claude/worktrees/held").to_string_lossy()]);
        main
    }

    /// Unique per test in this run: tests run in parallel, and two that
    /// start in the same instant must not share a repo.
    fn rand_suffix() -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().subsec_nanos() as u64 * 1000 + n
    }

    fn env_for(main: &Path) -> Env {
        let mut env = Env::default();
        env.insert("HOME".into(), main.parent().unwrap().join("home").to_string_lossy().to_string());
        env
    }

    #[test]
    fn only_merged_clean_quiet_agent_worktrees_are_removed_whichever_tool_made_them() {
        let main = repo();
        let env = env_for(&main);
        let later = SystemTime::now() + Duration::from_secs(2 * QUIET_S);
        // just made: everything is recently used, nothing goes
        assert!(finished(&main, &env, QUIET_S, SystemTime::now()).is_empty());
        let mut names: Vec<String> = finished(&main, &env, QUIET_S, later)
            .iter()
            .map(|w| w.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["done", "quick"], "Claude Code's and Cursor's; stray has a file, open is unmerged, held is locked, elsewhere is a person's");
        let mut removed = tidy(&main, &env, QUIET_S, later);
        removed.sort();
        assert_eq!(removed, vec!["done (feat/done)", "quick (cursor/quick)"]);
        assert!(!main.join(".claude/worktrees/done").exists());
        for kept in ["stray", "open", "held"] {
            assert!(main.join(".claude/worktrees").join(kept).exists(), "{kept} stays");
        }
        assert!(main.parent().unwrap().join("elsewhere").exists());
        let branches = Command::new("git").args(["branch", "--list", "feat/done"]).current_dir(&main).output().unwrap();
        assert!(String::from_utf8_lossy(&branches.stdout).contains("feat/done"), "the branch is kept");
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    #[test]
    fn session_start_says_why_once_a_day_and_reports_what_the_tidy_removed_once() {
        let main = repo();
        let mut env = env_for(&main);
        env.insert("CORK_TIDY_WORKTREES".into(), "0".into());
        let first = session_start(&main, &env).expect("said");
        assert!(first.contains("this repo has 5 agent worktree(s)"), "{first}");
        assert!(first.contains("whichever tool runs them"), "{first}");
        assert!(first.contains("one with work in it is never touched"), "{first}");
        assert!(session_start(&main, &env).is_none(), "not again the same day");
        // what a detached tidy left behind is said once
        let path = record_path(&list(&main)[0].path, &env);
        let mut record = read_record(&path);
        record["removed"] = json!(["done (feat/done)"]);
        write_record(&path, &record);
        let told = session_start(&main, &env).expect("told");
        assert!(told.contains("Cork removed 1 finished agent worktree(s): done (feat/done)"), "{told}");
        assert!(session_start(&main, &env).is_none());
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    #[test]
    fn another_tools_folder_counts_once_named_and_a_bare_home_never_does() {
        let main = repo();
        let mut env = env_for(&main);
        let homes = agent_homes(&main, &env);
        assert!(homes.contains(&std::fs::canonicalize(main.join(".claude/worktrees")).unwrap()));
        assert!(homes.iter().any(|h| h.ends_with(".cursor/worktrees")));
        env.insert("CORK_AGENT_WORKTREE_DIRS".into(), format!("~/:{}", main.parent().unwrap().display()));
        let homes = agent_homes(&main, &env);
        assert!(
            !homes.contains(&std::fs::canonicalize(main.parent().unwrap().join("home")).unwrap()),
            "the home folder itself is never an agent folder"
        );
        // naming the folder a tool uses brings its worktrees in
        let names: Vec<String> = agents(&list(&main), &env).iter().map(|w| w.path.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert!(names.contains(&"elsewhere".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    #[test]
    fn off_switches_and_a_repo_without_agent_worktrees_say_nothing() {
        let base = std::env::temp_dir().join(format!("collide-wt-none-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&base).unwrap();
        run(&base, &["init", "-q", "-b", "main"]);
        let mut env = Env::default();
        env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
        assert!(session_start(&base, &env).is_none());
        std::fs::create_dir_all(base.join(".collide")).unwrap();
        std::fs::write(base.join(".collide/config.json"), r#"{"tidy_worktrees": false}"#).unwrap();
        assert!(!enabled(&base, &env));
        let _ = std::fs::remove_dir_all(&base);
    }
}
