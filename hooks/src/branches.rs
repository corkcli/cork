//! The branch this checkout's work lands on, read straight out of `.git/`
//! (no subprocess: the gate's budget is 200ms).
//!
//! The server keeps it per session (cork-rs branches.rs) and never holds
//! a write over a teammate's work on another branch. Two numbers matter:
//!
//! - `branch`: the branch checked out ("" for a detached HEAD, as mid-rebase:
//!   unknown, so today's behaviour);
//! - `target`: where a push lands when `cork land` does it, the branch's
//!   upstream (`branch.<name>.merge`). Everything landing onto one branch is
//!   on that branch, so an agent in its own worktree branch that lands onto
//!   `main` is on `main`.
//!
//! A branch in a worktree of its own (an agent tool's `worktree-agent-…`, a
//! workflow's, one a person made with `git worktree add`) that does not land
//! through Cork goes back into the branch it was made from, almost
//! always, and soon: work there meets that branch's live work at the merge.
//! So its target is that base branch (`base`), and the server checks it
//! against the agents on the base like any other agent there. A branch in
//! the main checkout that does not land sends no target: a feature branch
//! there may live for weeks, and stays on its own.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::config::Env;

/// (this checkout's git dir, the repository's common dir).
fn git_dirs(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = root.join(".git");
    let git_dir = match std::fs::read_to_string(&dot_git) {
        Ok(text) => {
            let dir = text.trim().strip_prefix("gitdir:")?.trim().to_string();
            let dir = PathBuf::from(dir);
            if dir.is_absolute() { dir } else { root.join(dir) }
        }
        Err(_) if dot_git.is_dir() => dot_git,
        Err(_) => return None,
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => {
            let rel = PathBuf::from(rel.trim());
            if rel.is_absolute() { rel } else { git_dir.join(rel) }
        }
        Err(_) => git_dir.clone(),
    };
    Some((git_dir, common))
}

/// The branch checked out, "" when HEAD is detached or unreadable.
pub fn current(root: &Path) -> String {
    let Some((git_dir, _)) = git_dirs(root) else { return String::new() };
    std::fs::read_to_string(git_dir.join("HEAD"))
        .ok()
        .and_then(|head| head.trim().strip_prefix("ref: refs/heads/").map(|b| b.chars().take(120).collect()))
        .unwrap_or_default()
}

/// The branch `branch` pushes to (its upstream's name on the remote), from
/// the repository's config; "" when it tracks none.
pub fn upstream(root: &Path, branch: &str) -> String {
    if branch.is_empty() {
        return String::new();
    }
    let Some((_, common)) = git_dirs(root) else { return String::new() };
    let Ok(config) = std::fs::read_to_string(common.join("config")) else { return String::new() };
    let header = format!("[branch \"{branch}\"]");
    let mut inside = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == header;
            continue;
        }
        if inside {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "merge" {
                    let value = value.trim();
                    return value.strip_prefix("refs/heads/").unwrap_or(value).to_string();
                }
            }
        }
    }
    String::new()
}

/// (branch, landing target) for this checkout: the upstream when its pushes
/// land through Cork, else, in a worktree of its own, the branch it was
/// made from.
pub fn of(root: &Path, env: &Env) -> (String, String) {
    let branch = current(root);
    if branch.is_empty() {
        return (branch, String::new());
    }
    let mut target = if crate::land::lands(root, env) { upstream(root, &branch) } else { String::new() };
    if target.is_empty() {
        target = base(root, &branch, env);
    }
    (branch, target)
}

/// A branch name as a ref names it: `refs/heads/dev`, `origin/dev` and
/// `refs/remotes/origin/dev` are all `dev`.
fn short(name: &str) -> &str {
    let name = name.trim();
    ["refs/heads/", "refs/remotes/origin/", "origin/"].iter().find_map(|p| name.strip_prefix(p)).unwrap_or(name)
}

/// The branch a ref file points at (`ref: refs/heads/dev`), "" otherwise.
fn points_at(path: &Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().strip_prefix("ref: ").map(|r| short(r).to_string()))
        .unwrap_or_default()
}

/// The branch a branch in a linked worktree goes back into. An agent tool's
/// scratch branch (`worktree-…`) goes back into the session that made it,
/// which works in the main checkout: the main checkout's branch, whatever
/// it was made from (Claude Code makes its worktrees from `origin/main`
/// while the session is on `dev`). Any other: the branch its reflog's first
/// line names ("branch: Created from dev"), else, when that names a commit
/// or HEAD, the branch holding that commit, the main checkout's first, then
/// the remote's default, then any other that is not scratch. "" in the main
/// checkout. Kept per branch, so git runs once and the gate's later calls
/// read a file.
pub fn base(root: &Path, branch: &str, env: &Env) -> String {
    let Some((git_dir, common)) = git_dirs(root) else { return String::new() };
    if crate::config::normpath(&git_dir) == crate::config::normpath(&common) {
        return String::new();
    }
    let cache = crate::config::data_dir(env).join("check").join(format!(
        "base-{}.json",
        &crate::report::sha256_hex(format!("{}\n{branch}", common.display()).as_bytes())[..16]
    ));
    if let Some(known) = crate::config::load_json(&cache).get("base").and_then(Value::as_str) {
        return known.to_string();
    }
    let main_branch = points_at(&common.join("HEAD"));
    let first = std::fs::read_to_string(common.join("logs").join("refs").join("heads").join(branch))
        .ok()
        .and_then(|log| log.lines().next().map(str::to_string))
        .unwrap_or_default();
    let (fields, message) = first.split_once('\t').unwrap_or((first.as_str(), ""));
    let start = fields.split_whitespace().nth(1).unwrap_or("").to_string();
    let named = message.trim().strip_prefix("branch: Created from ").map(|from| from.trim_end_matches("^{commit}")).unwrap_or("");
    let is_commit = |name: &str| name.len() >= 7 && name.chars().all(|c| c.is_ascii_hexdigit());
    let found = if branch.starts_with("worktree-") {
        main_branch.clone()
    } else if !named.is_empty() && named != "HEAD" && !is_commit(named) {
        short(named).to_string()
    } else {
        let holding = if is_commit(&start) {
            std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["for-each-ref", "--contains", &start, "--format=%(refname:lstrip=2)", "refs/heads"])
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().to_string()).collect::<Vec<_>>())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let holding: Vec<String> = holding.into_iter().filter(|b| b != branch && !b.starts_with("worktree-")).collect();
        let default = points_at(&common.join("refs").join("remotes").join("origin").join("HEAD"));
        let preferred = [main_branch.clone(), default].into_iter().find(|b| !b.is_empty() && holding.contains(b));
        preferred.or_else(|| holding.first().cloned()).unwrap_or_else(|| main_branch.clone())
    };
    let found = if found == branch { String::new() } else { found };
    if let Some(dir) = cache.parent() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(&cache, json!({"branch": branch, "base": found}).to_string());
    }
    found
}

/// Adds `branch` and `target` to a payload, each only when known.
pub fn add_to(map: &mut Map<String, Value>, root: &Path, env: &Env) {
    let (branch, target) = of(root, env);
    if !branch.is_empty() {
        map.insert("branch".into(), json!(branch));
    }
    if !target.is_empty() {
        map.insert("target".into(), json!(target));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        assert!(ok.success(), "git {args:?}");
    }

    #[test]
    fn the_branch_and_where_it_lands_come_out_of_dot_git() {
        let base = std::env::temp_dir().join(format!("collide-branches-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        sh(&base, &["clone", "-q", "origin.git", "a"]);
        let a = base.join("a");
        sh(&a, &["config", "user.email", "a@x"]);
        sh(&a, &["config", "user.name", "a"]);
        std::fs::write(a.join("x.py"), "x = 1\n").unwrap();
        sh(&a, &["add", "."]);
        sh(&a, &["commit", "-qm", "seed"]);
        sh(&a, &["push", "-q", "-u", "origin", "main"]);
        assert_eq!(current(&a), "main");
        assert_eq!(upstream(&a, "main"), "main");
        // a worktree on its own branch that tracks main lands onto main
        sh(&a, &["worktree", "add", "-q", "-b", "agent-1", base.join("wt").to_str().unwrap()]);
        let wt = base.join("wt");
        sh(&wt, &["branch", "-q", "--set-upstream-to", "origin/main"]);
        assert_eq!(current(&wt), "agent-1");
        assert_eq!(upstream(&wt, "agent-1"), "main");
        // a feature branch that tracks nothing
        sh(&wt, &["checkout", "-q", "-b", "feat/x"]);
        assert_eq!((current(&wt).as_str(), upstream(&wt, "feat/x").as_str()), ("feat/x", ""));
        // detached: unknown
        sh(&wt, &["checkout", "-q", "--detach"]);
        assert_eq!(current(&wt), "");
        assert_eq!(current(&base.join("nowhere")), "");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_worktree_branch_is_on_the_branch_it_was_made_from_and_the_main_checkout_on_its_own() {
        let base_dir = std::env::temp_dir().join(format!("collide-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();
        let env: Env = [("CORK_HOME".to_string(), base_dir.join("home").display().to_string()), ("CORK_LAND".to_string(), "0".to_string())]
            .into_iter()
            .collect();
        sh(&base_dir, &["init", "-q", "-b", "main", "repo"]);
        let repo = base_dir.join("repo");
        sh(&repo, &["config", "user.email", "a@x"]);
        sh(&repo, &["config", "user.name", "a"]);
        std::fs::write(repo.join("x.py"), "x = 1\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-qm", "seed"]);
        sh(&repo, &["checkout", "-q", "-b", "dev"]);
        std::fs::write(repo.join("y.py"), "y = 1\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-qm", "dev work"]);
        // the main checkout, on a branch that tracks nothing: on its own
        assert_eq!(of(&repo, &env), ("dev".to_string(), String::new()));
        // made from a branch by name: the reflog says so
        sh(&repo, &["worktree", "add", "-q", "-b", "feat-a", base_dir.join("a").to_str().unwrap(), "main"]);
        assert_eq!(of(&base_dir.join("a"), &env), ("feat-a".to_string(), "main".to_string()));
        // a named branch made from a commit: the branch holding it, the main
        // checkout's first (main holds the seed too, dev is preferred)
        let seed = String::from_utf8(Command::new("git").args(["rev-parse", "main"]).current_dir(&repo).output().unwrap().stdout).unwrap();
        sh(&repo, &["worktree", "add", "-q", "-b", "feat-b", base_dir.join("b").to_str().unwrap(), seed.trim()]);
        assert_eq!(of(&base_dir.join("b"), &env).1, "dev", "the main checkout's branch holds the seed too");
        // an agent tool's scratch branch, made from main as Claude Code makes
        // them: it goes back into the session's branch, the main checkout's
        sh(&repo, &["worktree", "add", "-q", "-b", "worktree-agent-1", base_dir.join("c").to_str().unwrap(), "main"]);
        assert_eq!(of(&base_dir.join("c"), &env).1, "dev");
        // kept: the answer stands once found
        sh(&repo, &["checkout", "-q", "main"]);
        assert_eq!(of(&base_dir.join("c"), &env).1, "dev");
        let _ = std::fs::remove_dir_all(&base_dir);
    }
}
