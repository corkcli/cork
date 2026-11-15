//! The gate holds a write once, then lets it through.
//!
//! Every block the gate can give (a teammate's live change to the file, a
//! rename or new signature it has not taken in, a lease, a standing
//! decision) ends the same way: "make your edit fit theirs, then retry and it
//! goes through". The second study at 16 agents showed the promise broken:
//! each retry carried different content, or another teammate's change had
//! arrived, so the gate stopped it again. One agent was stopped fifteen
//! times on one file while resolving its own rebase conflict, hit its turn
//! cap and used 12.8M tokens, a third of the round.
//!
//! So the promise is kept here, on this machine, whatever the server says:
//!
//! - a file with an unresolved merge or rebase conflict is never held: the
//!   agent is resolving it, and only it can (unmerged entries in git's index,
//!   conflict markers in the file, or a merge or rebase in progress);
//! - a file is held at most once per teammate change (the block's words,
//!   ages aside), and after any hold, the next write to that file goes
//!   through, whatever it is.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::config::{self, Env};

/// How long a hold is remembered: a session's working day.
const KEEP_S: f64 = 12.0 * 3600.0;

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn path_of(env: &Env, session: &str) -> PathBuf {
    crate::proto::session_dir(env, session).join("holds.json")
}

/// The block's words with the parts that change from retry to retry taken
/// out (ages, line numbers): the same teammate change reads the same.
pub fn key_of(reason: &str) -> String {
    use sha2::{Digest, Sha256};
    static AGE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let age = AGE.get_or_init(|| regex::Regex::new(r"just now|\d+\s*[smhd] ago|\d+").expect("age pattern"));
    let first = reason.lines().next().unwrap_or("").to_lowercase();
    let normal = age.replace_all(&first, "#");
    Sha256::digest(normal.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The git directory of the checkout at `root` (a linked worktree's `.git`
/// is a file naming it).
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    if dot.is_dir() {
        return Some(dot);
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let named = text.trim().strip_prefix("gitdir:")?.trim();
    let dir = PathBuf::from(named);
    Some(if dir.is_absolute() { dir } else { root.join(dir) })
}

/// Whether this checkout is in the middle of a merge, rebase or cherry-pick
/// (`REBASE_HEAD` is left behind after a rebase finishes: not a sign).
pub fn rebase_in_progress(root: &Path) -> bool {
    git_dir(root).is_some_and(|dir| {
        ["MERGE_HEAD", "CHERRY_PICK_HEAD", "rebase-merge", "rebase-apply"].iter().any(|name| dir.join(name).exists())
    })
}

/// Whether `rel` is in the middle of a conflict this checkout is resolving:
/// a merge, rebase or cherry-pick in progress, conflict markers in the file,
/// or unmerged entries for it in git's index.
pub fn in_conflict(root: &Path, rel: &str) -> bool {
    if rebase_in_progress(root) {
        return true;
    }
    let marked = std::fs::read_to_string(root.join(rel))
        .map(|text| {
            let mut opened = false;
            for line in text.lines() {
                if line.starts_with("<<<<<<< ") {
                    opened = true;
                } else if opened && line.starts_with(">>>>>>> ") {
                    return true;
                }
            }
            false
        })
        .unwrap_or(false);
    if marked {
        return true;
    }
    crate::git::git(root, &["ls-files", "-u", "--", rel], Duration::from_millis(300)).is_some_and(|out| !out.trim().is_empty())
}

/// Whether the gate may hold this write to `rel` with `reason`. False (let
/// it through) when the file is mid-conflict, when this teammate change
/// already held it once, or when the last write to it was held. Remembers
/// the hold when it says yes. A write with no session is held as before.
pub fn may_hold(env: &Env, session: &str, root: &Path, rel: &str, reason: &str) -> bool {
    if in_conflict(root, rel) {
        return false;
    }
    if session.is_empty() {
        return true;
    }
    let path = path_of(env, session);
    let cutoff = now_s() - KEEP_S;
    let mut all: Map<String, Value> = config::load_json(&path)
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, v)| v.get("ts").and_then(Value::as_f64).unwrap_or(0.0) >= cutoff)
        .collect();
    let key = key_of(reason);
    let entry = all.entry(rel.to_string()).or_insert_with(|| json!({"pending": false, "keys": []}));
    let pending = entry.get("pending").and_then(Value::as_bool).unwrap_or(false);
    let seen = entry.get("keys").and_then(Value::as_array).is_some_and(|k| k.iter().any(|x| x.as_str() == Some(key.as_str())));
    let hold = !pending && !seen;
    entry["ts"] = json!(now_s());
    entry["pending"] = json!(hold);
    if hold {
        if let Some(keys) = entry.get_mut("keys").and_then(Value::as_array_mut) {
            keys.push(json!(key));
        }
    }
    save(&path, &Value::Object(all));
    hold
}

/// A write to these files went through: the hold, if any, is spent.
pub fn passed(env: &Env, session: &str, rels: &[String]) {
    if session.is_empty() || rels.is_empty() {
        return;
    }
    let path = path_of(env, session);
    let mut all = config::load_json(&path);
    let Some(map) = all.as_object_mut() else { return };
    let mut changed = false;
    for rel in rels {
        if let Some(entry) = map.get_mut(rel) {
            if entry.get("pending").and_then(Value::as_bool) == Some(true) {
                entry["pending"] = json!(false);
                changed = true;
            }
        }
    }
    if changed {
        save(&path, &all);
    }
}

fn save(path: &Path, value: &Value) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, value.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn env_in(name: &str) -> (PathBuf, Env) {
        let dir = std::env::temp_dir().join(format!("collide-hold-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), dir.join("home").to_string_lossy().to_string());
        (dir, env)
    }

    #[test]
    fn a_file_is_held_once_per_teammate_change_and_the_next_write_goes_through() {
        let (dir, env) = env_in("once");
        let root = dir.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.py"), "def f():\n    pass\n").unwrap();
        let theirs = "Another session of yours is changing a.py in their own checkout (just now); it is not pushed";
        assert!(may_hold(&env, "s1", &root, "a.py", theirs), "the first time");
        // the retry, with other content, and a new teammate change on top: through
        assert!(!may_hold(&env, "s1", &root, "a.py", theirs.replace("just now", "2m ago").as_str()));
        assert!(!may_hold(&env, "s1", &root, "a.py", theirs), "the same change never holds again");
        // a new teammate change, after a write went through, holds once
        let other = "bob renamed `f` to `g` in a.py and pushed it";
        assert!(may_hold(&env, "s1", &root, "a.py", other));
        assert!(!may_hold(&env, "s1", &root, "a.py", "carol is editing f right now"), "the next write always passes");
        // another session is its own
        assert!(may_hold(&env, "s2", &root, "a.py", theirs));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_in_conflict_is_never_held() {
        let (dir, env) = env_in("conflict");
        let root = dir.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let ok = Command::new("git").args(["init", "-q"]).current_dir(&root).stdout(Stdio::null()).status().unwrap();
        assert!(ok.success());
        std::fs::write(root.join("a.py"), "<<<<<<< HEAD\nx = 1\n=======\nx = 2\n>>>>>>> theirs\n").unwrap();
        assert!(in_conflict(&root, "a.py"));
        assert!(!may_hold(&env, "s1", &root, "a.py", "anything"));
        std::fs::write(root.join("b.py"), "x = 1\n").unwrap();
        assert!(!in_conflict(&root, "b.py"));
        // a rebase in progress: everything it touches is being resolved
        std::fs::create_dir_all(root.join(".git/rebase-merge")).unwrap();
        assert!(in_conflict(&root, "b.py"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
