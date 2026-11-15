//! Branches hidden from a repo's lists. Each member's hooks report the
//! branches of their checkout at session start (`repogit:` markers); the
//! dashboard's switchers and Settings list them. The owner may hide one in
//! Settings: it leaves the lists, and it stays hidden when a hook reports it
//! again (it still exists in someone's checkout) until it is shown again.
//! Nothing in anyone's checkout changes.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::store::{now, Store};

pub const BUCKET: &str = "hidden_refs";

fn names(value: &Value, key: &str) -> BTreeSet<String> {
    value.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

/// The branches hidden in this repo's scope. A record written before
/// Cork stopped tracking worktrees may still hold a `worktrees` list:
/// nothing reads it.
pub fn hidden(store: &Store, scope: &str) -> BTreeSet<String> {
    let record = store.kv_get(BUCKET, scope).unwrap_or_else(|| json!({}));
    names(&record, "branches")
}

/// The branches the members' hooks report for this scope.
pub fn reported(store: &Store, scope: &str) -> BTreeSet<String> {
    let mut branches = BTreeSet::new();
    for (_key, record) in store.eph_scan(&format!("repogit:{scope}:")) {
        branches.extend(names(&record, "branches"));
    }
    branches
}

/// What Settings shows: the reported ones still listed, and the hidden ones
/// (reported now or not).
pub fn view(store: &Store, scope: &str) -> Value {
    let (branches, hidden_branches) = (reported(store, scope), hidden(store, scope));
    json!({
        "branches": branches.difference(&hidden_branches).collect::<Vec<_>>(),
        "hidden": {"branches": hidden_branches},
    })
}

/// Hide one (`hide`) or show it again; `kind` is "branch", the only kind
/// there is. The view afterwards, or why not.
pub fn set(store: &Store, scope: &str, kind: &str, name: &str, hide: bool) -> Result<Value, String> {
    if kind != "branch" {
        return Err("kind must be branch".to_string());
    }
    let field = "branches";
    let name = name.trim();
    if name.is_empty() {
        return Err("name is required".to_string());
    }
    let mut record = store.kv_get(BUCKET, scope).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let mut listed = names(&record, field);
    if hide {
        listed.insert(name.to_string());
    } else {
        listed.remove(name);
    }
    record[field] = json!(listed);
    let _ = store.kv_put(BUCKET, scope, &record, now());
    Ok(view(store, scope))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hidden_branch_leaves_the_lists_and_stays_hidden_when_reported_again() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "ws:shop";
        let report = |db: &Store, who: &str| {
            let _ = db.eph_set(&format!("repogit:{scope}:{who}"),
                &json!({"branches": ["main", "old-spike"], "worktrees": ["shop", "shop-hotfix"]}), Some(600.0));
        };
        report(&db, "ana");
        let shown = set(&db, scope, "branch", "old-spike", true).unwrap();
        assert_eq!(shown["branches"], json!(["main"]));
        assert_eq!(shown["hidden"]["branches"], json!(["old-spike"]));
        // another member's hook reports it again: still hidden
        report(&db, "ben");
        assert_eq!(view(&db, scope)["branches"], json!(["main"]));
        let back = set(&db, scope, "branch", "old-spike", false).unwrap();
        assert_eq!(back["branches"], json!(["main", "old-spike"]));
        assert!(set(&db, scope, "tag", "v1", true).is_err());
        // a worktree is a folder on someone's disk, not something Cork
        // lists: refused like any other kind, and an older hook's list of
        // them never reaches a reply
        assert!(set(&db, scope, "worktree", "shop-hotfix", true).is_err());
        assert!(view(&db, scope).get("worktrees").is_none());
        assert!(view(&db, scope)["hidden"].get("worktrees").is_none());
    }
}
