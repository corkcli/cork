//! Who moved a checkout to another branch. An agent shares its folder with
//! the person and with any other agent in it, and `git checkout` in one of
//! them moves all of them: an agent that is not told keeps writing, now on
//! a branch it never chose.
//!
//! Each session keeps the branch it last saw here. When the folder's branch
//! differs at its next step, the session either switched it itself (that
//! step was a `git checkout`/`git switch`: recorded, nothing said), or
//! another agent session did (that session's own step recorded it), or
//! someone outside the agents did (you, an editor, GitHub Desktop). The
//! last two are said to the session once, in one line.
//!
//! Layout: `journal/<checkout>/branch.json` (the folder's last switch:
//! `{branch, from, by, at}`) and `journal/<checkout>/branch-seen/<session>`.

use std::path::Path;

use serde_json::{json, Value};

use crate::config::Env;

/// Does this command move the folder to another branch?
pub fn switches_branch(command: &str) -> bool {
    let words: Vec<&str> = command.split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')')).filter(|w| !w.is_empty()).collect();
    words.windows(2).any(|pair| matches!(pair, ["git", "checkout" | "switch"]))
        || words.windows(3).any(|three| matches!(three, ["gh", "pr", "checkout"]))
}

fn short(session: &str) -> String {
    session.chars().take(8).collect()
}

/// The line for this session when its folder's branch moved since its
/// last step, and not by this step; empty otherwise. `command` is the step's
/// shell command (empty for any other tool).
pub fn observe(root: &Path, session: &str, command: &str, env: &Env) -> String {
    let branch = crate::git::branch(root);
    if branch.is_empty() || session.is_empty() {
        return String::new();
    }
    let dir = crate::journal::journal_dir(root, env);
    let seen_path = dir.join("branch-seen").join(crate::journal::session_tag(session));
    let seen = std::fs::read_to_string(&seen_path).map(|s| s.trim().to_string()).unwrap_or_default();
    if seen != branch {
        let _ = std::fs::create_dir_all(dir.join("branch-seen"));
        let _ = std::fs::write(&seen_path, &branch);
    }
    // a session's first step here, or nothing moved
    if seen.is_empty() || seen == branch {
        return String::new();
    }
    let record_path = dir.join("branch.json");
    let record = crate::config::load_json(&record_path);
    let field = |key: &str| record.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let write = |by: &str| {
        let body = json!({"branch": branch, "from": seen, "by": by, "at": crate::journal::now_s()});
        let _ = std::fs::write(&record_path, body.to_string());
    };
    if switches_branch(command) {
        write(session);
        return String::new();
    }
    let by = field("by");
    if field("branch") == branch && !by.is_empty() && by != session {
        return format!(
            "Δ This folder is now on branch `{branch}` (it was on `{seen}`): another agent session ({}) switched it. \
Your edits from here on land on `{branch}`; if they belong on `{seen}`, say so before writing.",
            short(&by)
        );
    }
    if field("branch") != branch {
        write("");
    }
    format!(
        "Δ This folder is now on branch `{branch}` (it was on `{seen}`): it was switched outside your agents \
(the person, an editor or GitHub Desktop). Your edits from here on land on `{branch}`; if they belong on `{seen}`, say so before writing."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn repo(name: &str) -> (std::path::PathBuf, Env) {
        let base = std::env::temp_dir().join(format!("cork-switch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "dev"]);
        git(&root, &["-c", "user.email=a@x", "-c", "user.name=a", "commit", "-q", "--allow-empty", "-m", "start"]);
        git(&root, &["branch", "feature"]);
        let env: Env = [("CORK_HOME".to_string(), base.join("home").to_string_lossy().to_string())].into_iter().collect();
        (root, env)
    }

    #[test]
    fn a_switch_is_told_to_the_others_and_not_to_the_switcher() {
        let (root, env) = repo("agents");
        // both sessions have seen dev
        assert_eq!(observe(&root, "lead-1", "", &env), "");
        assert_eq!(observe(&root, "help-2", "", &env), "");
        // the lead switches: its own step says nothing
        git(&root, &["checkout", "-q", "feature"]);
        assert_eq!(observe(&root, "lead-1", "git checkout feature", &env), "");
        // the other session hears it once, named as another agent's
        let told = observe(&root, "help-2", "", &env);
        assert!(told.contains("now on branch `feature`") && told.contains("another agent session (lead-1)"), "{told}");
        assert_eq!(observe(&root, "help-2", "", &env), "");
    }

    #[test]
    fn a_switch_no_agent_made_is_said_to_come_from_outside() {
        let (root, env) = repo("outside");
        assert_eq!(observe(&root, "s1", "", &env), "");
        git(&root, &["checkout", "-q", "feature"]);
        let told = observe(&root, "s1", "ls", &env);
        assert!(told.contains("switched outside your agents") && told.contains("was on `dev`"), "{told}");
    }

    #[test]
    fn what_counts_as_a_branch_switch() {
        assert!(switches_branch("git checkout feature"));
        assert!(switches_branch("cd x && git switch -c new"));
        assert!(switches_branch("gh pr checkout 12"));
        assert!(!switches_branch("git status && git log"));
        assert!(!switches_branch("echo checkout"));
    }
}
