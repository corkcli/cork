//! Trunk mode: one branch every agent's work lands on, so the person can run
//! it and see everything.
//!
//! Agents make a branch for every feature (their defaults say "never commit
//! to the default branch", each parallel worktree needs a branch of its own,
//! branches get stacked on unmerged branches) and leave the merging to the
//! person, who then cannot run one branch and see the work. With trunk mode
//! on, a repo's agents work on its trunk (`dev`), and a branch an agent does
//! make is landed into the trunk once its task is done, by the hooks
//! (cork-hooks trunk.rs). Promotion to `main` stays a pull request a person
//! merges. It changes where work lands, never how many agents run.
//!
//! The server keeps, per repo scope:
//!
//! - the setting (`trunk`): on or off, the trunk branch ("" lets the hooks
//!   pick `dev` when origin has it, else the default branch), the promotion
//!   target, and the landing policy (`auto`, `pr`, `ask`). Every machine
//!   and teammate reads the same one, with each briefing (`attach`);
//! - the branch registry (`trunkbranch`): the branches agents made, whose,
//!   on which machine, from which base, what for, and how far they got;
//! - where each landing target last moved (`trunkhead`), so a session on
//!   the trunk elsewhere fetches when it moved, not on a timer.
//!
//! The dashboard's Land and Delete set a branch's `want`; the owner's hooks
//! do it at their next prompt or session start, through the same push turn
//! as every landing (land.rs).

use serde_json::{json, Map, Value};

use crate::store::Store;

pub const BUCKET: &str = "trunk";
pub const BRANCH_BUCKET: &str = "trunkbranch";
const HEAD_BUCKET: &str = "trunkhead";
pub const POLICIES: [&str; 3] = ["auto", "pr", "ask"];
pub const STATUSES: [&str; 4] = ["open", "landed", "abandoned", "conflicted"];
const MAX_NAME: usize = 120;
const MAX_LISTED: usize = 200;
/// Landing targets remembered per repo; a briefing carries the trunk's.
const MAX_HEADS: usize = 20;
/// A person's own branches a briefing names at most.
const MAX_MINE: usize = 10;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// A branch name as git allows one, or why not. "" stays "".
pub fn ref_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    let name = name.strip_prefix("refs/heads/").or_else(|| name.strip_prefix("origin/")).unwrap_or(name);
    if name.is_empty() {
        return Ok(String::new());
    }
    let bad = name.chars().count() > MAX_NAME
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with(".lock")
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("@{")
        || name.chars().any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c));
    if bad {
        return Err(format!("{name} is not a branch name git allows"));
    }
    Ok(name.to_string())
}

fn branch_key(scope: &str, name: &str) -> String {
    format!("{scope}|{name}")
}

/// The repo's setting, with the defaults where it never chose: off, the
/// hooks' pick of trunk, promotion to `main`, landing `auto`.
pub fn setting(store: &Store, scope: &str) -> Value {
    let saved = store.kv_get(BUCKET, scope).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let promote = text(&saved, "promote_to");
    let policy = text(&saved, "policy");
    json!({
        "on": saved.get("on").and_then(Value::as_bool).unwrap_or(false),
        "branch": text(&saved, "branch"),
        "promote_to": if promote.is_empty() { "main".to_string() } else { promote },
        "policy": if POLICIES.contains(&policy.as_str()) { policy } else { "auto".to_string() },
    })
}

/// Merge what `body` names (`on`, `branch`, `promote_to`, `policy`) into the
/// repo's setting; the setting afterwards, or why not.
pub fn set(store: &Store, scope: &str, by: &str, body: &Value, now: f64) -> Result<Value, String> {
    let mut current = setting(store, scope);
    let mut any = false;
    match body.get("on") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(on)) => {
            current["on"] = json!(on);
            any = true;
        }
        Some(_) => return Err("on is true or false".into()),
    }
    for key in ["branch", "promote_to"] {
        match body.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::String(raw)) => {
                let name = ref_name(raw)?;
                current[key] = json!(if key == "promote_to" && name.is_empty() { "main".to_string() } else { name });
                any = true;
            }
            Some(_) => return Err(format!("{key} is a branch name")),
        }
    }
    match body.get("policy") {
        None | Some(Value::Null) => {}
        Some(Value::String(policy)) if POLICIES.contains(&policy.as_str()) => {
            current["policy"] = json!(policy);
            any = true;
        }
        Some(_) => return Err("policy is auto, pr or ask".into()),
    }
    if !any {
        return Err("name at least one of on, branch, promote_to, policy".into());
    }
    let mut saved = current.clone();
    saved["by"] = json!(by);
    saved["ts"] = json!(now);
    store.kv_put(BUCKET, scope, &saved, now).map_err(|e| e.to_string())?;
    Ok(current)
}

/// A landing moved `target` (land.rs, `release`): sessions on it elsewhere
/// hear at their next prompt that it moved.
pub fn note_landed(store: &Store, scope: &str, target: &str, commit: &str, by: &str, now: f64) {
    let Ok(target) = ref_name(target) else { return };
    if target.is_empty() {
        return;
    }
    let mut heads = store.kv_get(HEAD_BUCKET, scope).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let Some(map) = heads.as_object_mut() else { return };
    map.insert(target, json!({"commit": commit.chars().take(40).collect::<String>(), "ts": now, "by": by}));
    if map.len() > MAX_HEADS {
        let mut by_age: Vec<(String, f64)> =
            map.iter().map(|(k, v)| (k.clone(), v.get("ts").and_then(Value::as_f64).unwrap_or(0.0))).collect();
        by_age.sort_by(|a, b| a.1.total_cmp(&b.1));
        for (old, _) in by_age.into_iter().take(map.len() - MAX_HEADS) {
            map.remove(&old);
        }
    }
    let _ = store.kv_put(HEAD_BUCKET, scope, &heads, now);
}

/// Where the repo's landing targets last moved, target to `{commit, ts, by}`.
pub fn heads(store: &Store, scope: &str) -> Map<String, Value> {
    store.kv_get(HEAD_BUCKET, scope).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

/// Every branch on the registry, most recently updated first.
pub fn branches(store: &Store, scope: &str) -> Vec<Value> {
    let mut rows: Vec<Value> = store.kv_list(BRANCH_BUCKET, &format!("{scope}|")).into_iter().map(|(_, v)| v).collect();
    rows.sort_by(|a, b| {
        let at = |v: &Value| v.get("updated").and_then(Value::as_f64).unwrap_or(0.0);
        at(b).total_cmp(&at(a))
    });
    rows.truncate(MAX_LISTED);
    rows
}

/// What a hook says about a branch an agent made, as an upsert. The first
/// session to report a branch owns it; later reports say how far it got. A
/// name used again after it landed or was abandoned is a new branch.
pub fn record(store: &Store, scope: &str, user: &str, session: &str, body: &Value, now: f64) -> Result<Value, String> {
    let name = ref_name(&text(body, "branch"))?;
    if name.is_empty() {
        return Err("branch is required".into());
    }
    let status = text(body, "status");
    if !status.is_empty() && !STATUSES.contains(&status.as_str()) {
        return Err("status is open, landed, abandoned or conflicted".into());
    }
    let key = branch_key(scope, &name);
    let fresh = || json!({"name": name, "user": user, "session": session, "created": now, "status": "open"});
    let mut row = store.kv_get(BRANCH_BUCKET, &key).filter(Value::is_object).unwrap_or_else(fresh);
    if status == "open" && matches!(text(&row, "status").as_str(), "landed" | "abandoned") {
        row = fresh();
    }
    let short = |v: String, n: usize| v.chars().take(n).collect::<String>();
    for (field, cap) in [("machine", 64), ("base", MAX_NAME), ("purpose", 200)] {
        let value = text(body, field);
        if !value.is_empty() {
            row[field] = json!(short(value, cap));
        }
    }
    // said with any report: a problem cleared is "", a PR is its address
    for (field, cap) in [("problem", 400), ("pr", 300), ("commit", 40)] {
        if let Some(value) = body.get(field).and_then(Value::as_str) {
            row[field] = json!(short(value.to_string(), cap));
        }
    }
    if !status.is_empty() {
        row["status"] = json!(status);
    }
    if let Some(ahead) = body.get("ahead").and_then(Value::as_u64) {
        row["ahead"] = json!(ahead.min(100_000));
    }
    if let Some(pushed) = body.get("pushed").and_then(Value::as_bool) {
        row["pushed"] = json!(pushed);
    }
    if body.get("done").and_then(Value::as_bool) == Some(true) {
        if let Some(map) = row.as_object_mut() {
            for field in ["want", "want_by", "want_at"] {
                map.remove(field);
            }
        }
    }
    row["updated"] = json!(now);
    store.kv_put(BRANCH_BUCKET, &key, &row, now).map_err(|e| e.to_string())?;
    Ok(row)
}

/// The dashboard's Land or Delete, queued for the owner's hooks. A branch
/// with commits not on the trunk is never deleted: it is landed first.
pub fn want(store: &Store, scope: &str, name: &str, action: &str, by: &str, now: f64) -> Result<Value, String> {
    let name = ref_name(name)?;
    let key = branch_key(scope, &name);
    let Some(mut row) = store.kv_get(BRANCH_BUCKET, &key).filter(Value::is_object) else {
        return Err(format!("no branch {name} on this repo's list"));
    };
    let status = text(&row, "status");
    let ahead = row.get("ahead").and_then(Value::as_u64).unwrap_or(0);
    match action {
        "land" if status == "landed" => return Err(format!("{name} has already landed")),
        "land" => {}
        "delete" if ahead > 0 && !matches!(status.as_str(), "landed" | "abandoned") => {
            return Err(format!("{name} has {ahead} commit(s) not on the trunk: land it first"));
        }
        "delete" => {}
        _ => return Err("action is land or delete".into()),
    }
    row["want"] = json!(action);
    row["want_by"] = json!(by);
    row["want_at"] = json!(now);
    row["updated"] = json!(now);
    store.kv_put(BRANCH_BUCKET, &key, &row, now).map_err(|e| e.to_string())?;
    Ok(row)
}

/// A branch as a briefing names it to its owner.
fn compact(row: &Value) -> Value {
    let mut out = json!({"name": text(row, "name"), "status": text(row, "status")});
    for field in ["want", "problem", "pr", "machine", "base"] {
        let value = text(row, field);
        if !value.is_empty() {
            out[field] = json!(value);
        }
    }
    out
}

/// The `trunk` every briefing carries (main.rs, `brief_endpoint`), off or
/// on, so a hook that cached "on" hears it was switched off. When on: where
/// the trunk last moved, and the caller's own branches that wait on them
/// (the dashboard asked to land or delete one, one could not land, a PR
/// opened for one).
pub fn attach(store: &Store, scope: &str, user: &str, answer: &mut Value) {
    let Some(map) = answer.as_object_mut() else { return };
    let mut trunk = setting(store, scope);
    if trunk.get("on").and_then(Value::as_bool) == Some(true) {
        let all = heads(store, scope);
        let named = text(&trunk, "branch");
        let shown: Map<String, Value> =
            all.into_iter().filter(|(target, _)| named.is_empty() || *target == named).collect();
        if !shown.is_empty() {
            trunk["heads"] = Value::Object(shown);
        }
        let mine: Vec<Value> = branches(store, scope)
            .iter()
            .filter(|row| text(row, "user") == user)
            .filter(|row| {
                !text(row, "want").is_empty()
                    || !text(row, "problem").is_empty()
                    || (!text(row, "pr").is_empty() && text(row, "status") == "open")
            })
            .take(MAX_MINE)
            .map(compact)
            .collect();
        if !mine.is_empty() {
            trunk["mine"] = json!(mine);
        }
    }
    map.insert("trunk".into(), trunk);
}

/// What the dashboard's repo settings show: the setting, the registry with
/// each branch's age, and where the targets last moved.
pub fn view(store: &Store, scope: &str, now: f64) -> Value {
    let rows: Vec<Value> = branches(store, scope)
        .into_iter()
        .map(|mut row| {
            let created = row.get("created").and_then(Value::as_f64).unwrap_or(now);
            row["age_s"] = json!((now - created).max(0.0).round());
            row
        })
        .collect();
    json!({"trunk": setting(store, scope), "branches": rows, "heads": heads(store, scope)})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn the_setting_defaults_to_off_and_takes_only_what_git_and_the_policies_allow() {
        let db = db();
        let scope = "w:shop";
        assert_eq!(setting(&db, scope), json!({"on": false, "branch": "", "promote_to": "main", "policy": "auto"}));
        let on = set(&db, scope, "ana", &json!({"on": true, "branch": "origin/dev"}), 1.0).unwrap();
        assert_eq!(on, json!({"on": true, "branch": "dev", "promote_to": "main", "policy": "auto"}));
        assert!(set(&db, scope, "ana", &json!({"policy": "yolo"}), 2.0).is_err());
        assert!(set(&db, scope, "ana", &json!({"branch": "bad name"}), 2.0).is_err());
        assert!(set(&db, scope, "ana", &json!({"branch": "a..b"}), 2.0).is_err());
        assert!(set(&db, scope, "ana", &json!({}), 2.0).is_err(), "nothing named");
        let pr = set(&db, scope, "ana", &json!({"policy": "pr", "promote_to": ""}), 3.0).unwrap();
        assert_eq!(pr["policy"], "pr");
        assert_eq!(pr["promote_to"], "main", "an empty target is main");
        assert_eq!(setting(&db, scope)["branch"], "dev", "kept");
        assert_eq!(setting(&db, "w:other")["on"], false, "per repo");
    }

    #[test]
    fn every_briefing_carries_the_setting_and_its_owner_hears_of_their_own_branches() {
        let db = db();
        let scope = "w:shop";
        let mut answer = json!({"ok": true, "text": ""});
        attach(&db, scope, "ana", &mut answer);
        assert_eq!(answer["trunk"]["on"], false, "off is said too, so a cached on is cleared");
        set(&db, scope, "ana", &json!({"on": true, "branch": "dev"}), 1.0).unwrap();
        note_landed(&db, scope, "dev", "abc1234", "bob", 5.0);
        note_landed(&db, scope, "feat/x", "def5678", "bob", 6.0);
        record(&db, scope, "ana", "s1", &json!({"branch": "feat/a", "base": "dev", "ahead": 2}), 7.0).unwrap();
        record(&db, scope, "ana", "s1", &json!({"branch": "feat/b", "problem": "conflicts in a.py"}), 8.0).unwrap();
        record(&db, scope, "bob", "s2", &json!({"branch": "feat/c", "problem": "tests failed"}), 9.0).unwrap();
        let mut answer = json!({"ok": true});
        attach(&db, scope, "ana", &mut answer);
        let trunk = &answer["trunk"];
        assert_eq!(trunk["on"], true);
        assert_eq!(trunk["heads"]["dev"]["commit"], "abc1234");
        assert!(trunk["heads"].get("feat/x").is_none(), "only the trunk's");
        let mine = trunk["mine"].as_array().unwrap();
        assert_eq!(mine.len(), 1, "{mine:?}");
        assert_eq!(mine[0]["name"], "feat/b");
        assert_eq!(mine[0]["problem"], "conflicts in a.py");
    }

    #[test]
    fn the_registry_keeps_the_first_owner_and_never_queues_deleting_unlanded_work() {
        let db = db();
        let scope = "w:shop";
        let row = record(&db, scope, "ana", "s1", &json!({"branch": "feat/a", "base": "dev", "machine": "m1", "purpose": "Add refunds", "ahead": 3}), 1.0).unwrap();
        assert_eq!((row["user"].as_str(), row["status"].as_str(), row["ahead"].as_u64()), (Some("ana"), Some("open"), Some(3)));
        let row = record(&db, scope, "bob", "s9", &json!({"branch": "feat/a", "ahead": 4}), 2.0).unwrap();
        assert_eq!(row["user"], "ana", "the first session owns it");
        assert_eq!(row["purpose"], "Add refunds");
        assert!(record(&db, scope, "ana", "s1", &json!({"branch": "feat/a", "status": "merged"}), 3.0).is_err());
        assert!(record(&db, scope, "ana", "s1", &json!({"status": "open"}), 3.0).is_err());

        assert!(want(&db, scope, "feat/a", "delete", "owner", 4.0).unwrap_err().contains("land it first"));
        let queued = want(&db, scope, "feat/a", "land", "owner", 4.0).unwrap();
        assert_eq!(queued["want"], "land");
        assert!(want(&db, scope, "nope", "land", "owner", 4.0).is_err());
        assert!(want(&db, scope, "feat/a", "merge", "owner", 4.0).is_err());
        // the owner's hooks did it: landed, the request cleared
        let done = record(&db, scope, "ana", "s1", &json!({"branch": "feat/a", "status": "landed", "ahead": 0, "commit": "abc", "done": true}), 5.0).unwrap();
        assert!(done.get("want").is_none());
        assert!(want(&db, scope, "feat/a", "land", "owner", 6.0).is_err(), "already landed");
        assert_eq!(want(&db, scope, "feat/a", "delete", "owner", 6.0).unwrap()["want"], "delete");
        // the name used again is a new branch, owned by whoever made it now
        let again = record(&db, scope, "bob", "s3", &json!({"branch": "feat/a", "status": "open"}), 7.0).unwrap();
        assert_eq!((again["user"].as_str(), again.get("commit")), (Some("bob"), None));

        record(&db, scope, "ana", "s1", &json!({"branch": "feat/b"}), 8.0).unwrap();
        let listed: Vec<String> = branches(&db, scope).iter().map(|r| text(r, "name")).collect();
        assert_eq!(listed, vec!["feat/b", "feat/a"], "newest first");
        assert!(branches(&db, "w:other").is_empty());
        let shown = view(&db, scope, 10.0);
        assert_eq!(shown["branches"][0]["age_s"], 2.0);
        assert_eq!(shown["trunk"]["on"], false);
    }
}
