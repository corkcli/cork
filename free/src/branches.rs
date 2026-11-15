//! Which branch a session's work lands on, and whether two sessions share one.
//!
//! The gate used to treat every live session in a repo as if it wrote onto
//! the same branch. Two agents on different feature branches are not about
//! to overwrite each other: their changes meet only when the branches merge.
//! Blocking one over the other's rename stopped work that git would only
//! have to reconcile weeks later, if ever.
//!
//! So each session's branch is kept, as its hook names it with the gate and
//! the report: the branch it is on, and (when its pushes land through
//! `cork land`) the branch they land onto. Everything landing onto one
//! branch is on that branch: an agent in a worktree branch that lands onto
//! `main` is on `main` here. The base branch everyone lands onto is that
//! landing target; a session that does not land is on the branch it is on.
//!
//! Two sessions are *apart* only when both branches are known and differ.
//! Anything unknown (an older hook, a detached HEAD mid-rebase, a record
//! that expired) is today's behaviour: the same branch.

use serde_json::{json, Value};

use crate::codegraph::path_key;
use crate::store::{now, Store};

/// As long as a copy fingerprint: a session quiet longer is not live.
const TTL_S: f64 = 7_200.0;
/// A branch note is said once per (reader, file, teammate session) this long.
const NOTE_TTL_S: f64 = 86_400.0;
const MAX_BRANCH: usize = 120;

fn key(scope: &str, user: &str, session: &str) -> String {
    format!("branchof:{scope}:{user}:{session}")
}

/// A branch name as kept; a detached HEAD (the hooks send its commit, as
/// mid-rebase) is no branch: unknown.
fn clean(name: &str) -> String {
    let name = name.trim();
    let detached = (7..=40).contains(&name.len()) && name.chars().all(|c| c.is_ascii_hexdigit());
    if detached { String::new() } else { name.chars().take(MAX_BRANCH).collect() }
}

/// The branch a session's work lands on: the landing target when it lands,
/// else the branch it is on ("" when unknown).
pub fn effective(branch: &str, target: &str) -> String {
    let target = clean(target);
    if target.is_empty() { clean(branch) } else { target }
}

/// What a hook said about the session's branch (`branch`, and `target` when
/// its pushes land through Cork). A call that names none changes nothing.
pub fn note(store: &Store, scope: &str, user: &str, session: &str, branch: &str, target: &str) {
    let on = effective(branch, target);
    if session.is_empty() || on.is_empty() {
        return;
    }
    let key = key(scope, user, session);
    let record = json!({"user": user, "session": session, "on": on, "branch": clean(branch)});
    if store.eph_get(&key).as_ref() != Some(&record) {
        let _ = store.eph_set(&key, &record, Some(TTL_S));
    }
}

/// The branch `note` kept for a session ("" when unknown). A helper
/// (`session/worker`, presence::agent_session) works in a checkout of its
/// own, often a worktree on a branch of its own: its record, else its
/// session's, as before helpers had one.
pub fn of(store: &Store, scope: &str, user: &str, session: &str) -> String {
    if session.is_empty() {
        return String::new();
    }
    let kept = |session: &str| {
        store.eph_get(&key(scope, user, session)).and_then(|r| r.get("on").and_then(Value::as_str).map(str::to_string))
    };
    let (lead, worker) = crate::presence::split_agent(session);
    kept(session).or_else(|| (!worker.is_empty()).then(|| kept(lead)).flatten()).unwrap_or_default()
}

/// The branch `them` is on when it is known to differ from `mine`: their
/// work meets this session's only when the branches merge. `None` is the
/// same branch, or not known.
pub fn apart(store: &Store, scope: &str, mine: &str, them: (&str, &str)) -> Option<String> {
    if mine.is_empty() {
        return None;
    }
    let theirs = of(store, scope, them.0, them.1);
    (!theirs.is_empty() && theirs != mine).then_some(theirs)
}

/// Every live session on another branch than `mine`, session to branch: what
/// the hook needs to tell a protocol block from another branch.
pub fn others_apart(store: &Store, scope: &str, mine: &str, me: (&str, &str)) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    if mine.is_empty() {
        return out;
    }
    for (_, record) in store.eph_scan(&format!("branchof:{scope}:")) {
        let text = |k: &str| record.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let (user, session, on) = (text("user"), text("session"), text("on"));
        if session.is_empty() || on.is_empty() || on == mine || (user == me.0 && session == me.1) {
            continue;
        }
        out.insert(session, json!(on));
        if out.len() >= 50 {
            break;
        }
    }
    out
}

/// True the first time this reader hears about work on `path` from
/// `branch`: a branch note is said once per file and branch, however many
/// teammates are at work there.
pub fn first_note(store: &Store, scope: &str, reader: (&str, &str), branch: &str, path: &str) -> bool {
    let key = format!(
        "branchnote:{scope}:{}:{}:{}",
        crate::presence::agent_id(reader.0, reader.1),
        path_key(path),
        crate::hashing::sha256_hex(branch).get(..16).unwrap_or("")
    );
    if store.eph_get(&key).is_some() {
        return false;
    }
    let _ = store.eph_set(&key, &json!({"ts": now()}), Some(NOTE_TTL_S));
    true
}

/// The note: who is changing what, on which branch, and when it meets this
/// work. `what` is "`create_booking`" or "" for the file as a whole.
pub fn note_line(who: &str, what: &str, path: &str, branch: &str) -> String {
    let what = if what.is_empty() { path.to_string() } else { format!("{what} in {path}") };
    format!("Cork: {who} is changing {what} on branch {branch}; this will conflict when the branches merge. No need to stop now.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_apart_only_when_both_branches_are_known_and_differ() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        note(&db, scope, "alice", "a1", "feat/x", "");
        note(&db, scope, "bob", "b1", "worktree-agent-1", "main"); // lands onto main
        note(&db, scope, "carol", "c1", "main", "");
        assert_eq!(of(&db, scope, "bob", "b1"), "main", "landing onto a branch is being on it");
        assert_eq!(apart(&db, scope, "main", ("alice", "a1")).as_deref(), Some("feat/x"));
        assert_eq!(apart(&db, scope, "main", ("bob", "b1")), None, "both land onto main");
        assert_eq!(apart(&db, scope, "main", ("dave", "d1")), None, "unknown is the same branch");
        assert_eq!(apart(&db, scope, "", ("alice", "a1")), None, "so is not knowing mine");
        note(&db, scope, "erin", "e1", "a1b2c3d4e5f6", "");
        assert_eq!(of(&db, scope, "erin", "e1"), "", "a detached HEAD is no branch");
        let others = others_apart(&db, scope, "main", ("carol", "c1"));
        assert_eq!(others.len(), 1);
        assert_eq!(others["a1"], json!("feat/x"));
        // said once per file pair
        assert!(first_note(&db, scope, ("carol", "c1"), "feat/x", "booking.py"));
        assert!(!first_note(&db, scope, ("carol", "c1"), "feat/x", "booking.py"));
        assert!(first_note(&db, scope, ("carol", "c1"), "feat/x", "slots.py"));
        assert!(first_note(&db, scope, ("carol", "c1"), "feat/y", "booking.py"));
        assert!(!note_line("alice", "`create_booking`", "booking.py", "feat/x").to_lowercase().contains("pull"));
    }
}
