//! Repo aliasing and renaming.
//!
//! A repo that gets renamed on GitHub must keep its history. The ledger is a
//! single global hash chain, so re-keying a scope would invalidate every row
//! after it — the rename therefore never moves data. It records two things
//! instead: an alias so future reports under the new id resolve onto the
//! scope that already holds the history, and a preferred id so the server can
//! hand the new name back for clients to adopt.
//!
//! Both operations refuse, rather than force, when the target already owns
//! its own history. A rename that silently buried a live scope would be
//! unrecoverable, and two repos briefly sharing a name is a normal thing that
//! must not cost anyone their record.

use serde_json::{json, Value};

use crate::repo::{normalize_repo_id, repo_key, Aliases};
use crate::store::{now, Store};

/// True when a repo id already owns a persisted tree — the signal that
/// aliasing it away would orphan real history.
pub(crate) fn scope_has_data(store: &Store, aliases: &Aliases, workspace_id: &str, repo_id: &str) -> bool {
    let scope = aliases.scope_for(workspace_id, repo_id);
    store.list_scopes(&format!("{workspace_id}:")).contains(&scope)
}

/// Point every future reference to `from_id` at the scope that holds `to_id`.
/// Idempotent.
pub fn alias_repo(
    store: &Store, aliases: &Aliases, workspace_id: &str, from_id: &str, to_id: &str,
    by: &str, force: bool,
) -> Value {
    let from_id = from_id.trim();
    let to_id = to_id.trim();
    if from_id.is_empty() || to_id.is_empty() {
        return json!({"ok": false, "error": "need both from and to repo ids"});
    }
    // resolve the target to its terminal canonical so aliases stay one hop
    let canonical = aliases.resolve(workspace_id, to_id);
    if from_id == canonical {
        return json!({"ok": false, "error": "from and to resolve to the same repo"});
    }
    if let Some(existing) = store.kv_get("repo_alias", &format!("{workspace_id}:{from_id}")) {
        if existing.get("canonical").and_then(Value::as_str) == Some(canonical.as_str()) {
            return json!({"ok": true, "from": from_id, "canonical": canonical, "already": true});
        }
    }
    if !force && scope_has_data(store, aliases, workspace_id, from_id) {
        return json!({
            "ok": false, "shadows_data": true,
            "error": format!("'{from_id}' already has its own history; pass force to alias anyway"),
        });
    }
    let stamp = now();
    let record = json!({"canonical": canonical, "from": from_id, "created": stamp, "by": by});
    let _ = store.kv_put("repo_alias", &format!("{workspace_id}:{from_id}"), &record, stamp);
    aliases.add_rename(workspace_id, from_id, &canonical);
    json!({"ok": true, "from": from_id, "canonical": canonical})
}

/// Present `new_id` as this repo's id everywhere, while its ledger stays
/// physically under the scope that already holds it. Idempotent.
pub fn rename_repo(
    store: &Store, aliases: &Aliases, workspace_id: &str, old_id: &str, new_id: &str,
    by: &str, force: bool,
) -> Value {
    let old_id = old_id.trim();
    let new_id = new_id.trim();
    if old_id.is_empty() || new_id.is_empty() {
        return json!({"ok": false, "error": "need both old and new repo ids"});
    }
    let physical = aliases.resolve(workspace_id, old_id);
    let pkey = format!("{workspace_id}:{physical}");
    if new_id == physical {
        return json!({"ok": false, "error": "new id is already the canonical scope"});
    }
    if let Some(record) = store.kv_get("repo_preferred", &pkey) {
        if record.get("preferred").and_then(Value::as_str) == Some(new_id) {
            return json!({
                "ok": true, "canonical": physical, "preferred": new_id, "already": true,
            });
        }
    }
    // both steps must hold: alias new->old so future reports land on the real
    // history, then record the preferred id so it can be handed back
    let aliased = alias_repo(store, aliases, workspace_id, new_id, old_id, by, force);
    if aliased.get("ok").and_then(Value::as_bool) != Some(true) {
        return aliased;
    }
    let stamp = now();
    let record = json!({
        "preferred": new_id, "canonical": physical, "from": old_id,
        "created": stamp, "by": by,
    });
    let _ = store.kv_put("repo_preferred", &pkey, &record, stamp);

    // also bridge the bare basename of the new name onto the same scope, so a
    // client reporting the short form does not fragment into its own scope.
    // Best-effort and never forced: if the short name already owns data, leave
    // it — workspace-wide recall keeps those notes findable anyway.
    let base = normalize_repo_id(new_id);
    if !base.is_empty() && base != new_id && base != physical {
        let _ = alias_repo(store, aliases, workspace_id, &base, old_id, by, false);
    }
    json!({"ok": true, "canonical": physical, "preferred": new_id})
}

/// The hook reports the id it was configured with AND the id its git origin
/// implies. When they differ, and the origin is not already a repo of its own
/// here, the repo was renamed on GitHub.
///
/// Throttled to one check an hour per repo pair, because the hook sends the
/// origin on every single edit and this is the only expensive path it can
/// reach. Returns the new id when something changed, else empty.
///
/// `member` is the caller's own workspace record: adopting an origin
/// pre-registers alias origin->repo, which redirects every later read on the
/// origin's id onto this repo's scope, so an origin outside the caller's
/// per-repo allowlist is refused (before the throttle, so a refused attempt
/// never blocks a legitimate one for the hour). The owner sees every repo.
pub fn adopt_origin(
    store: &Store, aliases: &Aliases, workspace_id: &str, repo_id: &str, origin: &str, by: &str,
    member: Option<&Value>,
) -> String {
    let origin = origin.trim().to_lowercase();
    let repo_id = repo_id.trim();
    // a folder on the reporting machine is no hosted repo (older hooks sent them)
    if origin.is_empty() || repo_id.is_empty() || !origin.contains('/') || origin.starts_with('/')
        || origin.starts_with('.') || origin.starts_with('~') || origin.starts_with("file")
        || origin.chars().nth(1) == Some(':')
    {
        return String::new();
    }
    if repo_key(&origin) == repo_key(repo_id) {
        return String::new();
    }
    // a #branch-scoped id keeps its suffix through the rename
    let suffix = repo_id.find('#').map(|index| &repo_id[index..]).unwrap_or("");
    let new_id = format!("{origin}{suffix}");
    if !crate::access::has_repo_access(member, &new_id) {
        return String::new();
    }
    let stamp_key = format!(
        "originchk:{workspace_id}:{}:{}", repo_key(repo_id), repo_key(&new_id));
    if store.eph_get(&stamp_key).is_some() {
        return String::new();
    }
    let _ = store.eph_set(&stamp_key, &json!({"t": now()}), Some(3600.0));

    if crate::envelope::preferred_repo_id(store, aliases, workspace_id, repo_id) == new_id {
        return new_id;
    }
    if scope_has_data(store, aliases, workspace_id, &new_id) {
        return String::new(); // two real repos, not a rename: leave both alone
    }
    let renamed = rename_repo(store, aliases, workspace_id, repo_id, &new_id, by, false);
    if renamed.get("ok").and_then(Value::as_bool) != Some(true) {
        return String::new();
    }
    let _ = store.ledger_append(
        &aliases.scope_for(workspace_id, repo_id),
        "repo_renamed",
        &json!({"user": by, "from": repo_id, "to": new_id, "source": "git-origin"}),
        now(),
    );
    new_id
}

#[cfg(test)]
mod rename_ported_tests {
    use super::*;

    fn store_with(scopes: &[&str]) -> (Store, Aliases) {
        let store = Store::open(std::path::Path::new(":memory:")).expect("in-memory store");
        for scope in scopes {
            store.put_tree(scope, "alice", &json!({"root": "r", "dirs": {}, "files": {}}), 1.0).unwrap();
        }
        let aliases = Aliases::new();
        aliases.seed(&scopes.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        (store, aliases)
    }

    #[test]
    fn seeding_prefers_the_qualified_canonical() {
        let (_, aliases) = store_with(&["ws1:github.com/acme/agentmemory"]);
        assert_eq!(aliases.resolve("ws1", "agentmemory"), "github.com/acme/agentmemory");
        assert_eq!(aliases.resolve("ws1", "github.com/acme/agentmemory"), "github.com/acme/agentmemory");
        assert_eq!(aliases.resolve("ws2", "agentmemory"), "agentmemory");
    }

    #[test]
    fn ambiguous_basenames_are_not_merged() {
        let (_, aliases) = store_with(&["ws1:github.com/acme/app", "ws1:github.com/other/app"]);
        assert_eq!(aliases.resolve("ws1", "github.com/acme/app"), "github.com/acme/app");
        assert_eq!(aliases.resolve("ws1", "github.com/other/app"), "github.com/other/app");
        assert_eq!(aliases.resolve("ws1", "app"), "app");
    }

    #[test]
    fn the_first_name_seen_claims_the_alias_at_runtime() {
        let (_, aliases) = store_with(&[]);
        assert_eq!(aliases.resolve("ws1", "widgets"), "widgets");
        assert_eq!(aliases.resolve("ws1", "github.com/acme/widgets"), "widgets");
        // branch-qualified ids are their own scope until the base is known
        assert_eq!(normalize_repo_id("github.com/acme/app#main"), "app#main");
        assert_ne!(normalize_repo_id("app#main"), normalize_repo_id("app#dev"));
    }

    #[test]
    fn an_alias_redirects_the_new_name_onto_the_old_scope() {
        let (store, aliases) = store_with(&["ws1:github.com/acme/agentmemory"]);
        let result = alias_repo(&store, &aliases, "ws1", "github.com/acme/collide", "github.com/acme/agentmemory", "alice", false);
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["canonical"], "github.com/acme/agentmemory");
        assert_eq!(aliases.resolve("ws1", "github.com/acme/collide"), "github.com/acme/agentmemory");
        assert_eq!(aliases.resolve("ws2", "github.com/acme/collide"), "github.com/acme/collide");
        // and it is stored, for the next start to re-seed
        assert!(store.kv_get("repo_alias", "ws1:github.com/acme/collide").is_some());
    }

    #[test]
    fn an_alias_refuses_to_shadow_live_history_without_force() {
        let (store, aliases) = store_with(&["ws1:github.com/acme/agentmemory", "ws1:github.com/acme/collide"]);
        let refused = alias_repo(&store, &aliases, "ws1", "github.com/acme/collide", "github.com/acme/agentmemory", "alice", false);
        assert_eq!((refused["ok"].clone(), refused["shadows_data"].clone()), (json!(false), json!(true)), "{refused}");
        assert_eq!(aliases.resolve("ws1", "github.com/acme/collide"), "github.com/acme/collide");
        let forced = alias_repo(&store, &aliases, "ws1", "github.com/acme/collide", "github.com/acme/agentmemory", "alice", true);
        assert_eq!(forced["ok"], true, "{forced}");
        assert_eq!(aliases.resolve("ws1", "github.com/acme/collide"), "github.com/acme/agentmemory");
    }

    const OLD: &str = "github.com/lithometric/agentmemory";
    const NEW: &str = "github.com/lithometric/cork";

    #[test]
    fn a_rename_points_the_new_id_at_the_old_scope_without_moving_it() {
        let (store, aliases) = store_with(&["ws1:github.com/lithometric/agentmemory"]);
        let physical = aliases.resolve("ws1", OLD);
        let result = rename_repo(&store, &aliases, "ws1", OLD, NEW, "alice", false);
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["canonical"], json!(physical));
        assert_eq!(result["preferred"], NEW);
        // the new id resolves onto the same physical scope: no orphaned history
        assert_eq!(aliases.resolve("ws1", NEW), physical);
        // and the bare basename aligns to it too
        assert_eq!(aliases.resolve("ws1", "cork"), physical);
        // the old id hands the new one back; a client already on it has nothing to adopt
        assert_eq!(crate::envelope::preferred_repo_id(&store, &aliases, "ws1", OLD), NEW);
        assert_eq!(crate::envelope::preferred_repo_id(&store, &aliases, "ws1", NEW), "");
        // idempotent
        let again = rename_repo(&store, &aliases, "ws1", OLD, NEW, "alice", false);
        assert_eq!((again["ok"].clone(), again["already"].clone()), (json!(true), json!(true)), "{again}");
    }

    #[test]
    fn a_rename_refuses_to_bury_a_repo_that_owns_history() {
        let (store, aliases) = store_with(&["ws1:github.com/lithometric/agentmemory", "ws1:github.com/lithometric/cork"]);
        let blocked = rename_repo(&store, &aliases, "ws1", OLD, NEW, "alice", false);
        assert_eq!((blocked["ok"].clone(), blocked["shadows_data"].clone()), (json!(false), json!(true)), "{blocked}");
        let forced = rename_repo(&store, &aliases, "ws1", OLD, NEW, "alice", true);
        assert_eq!(forced["ok"], true, "{forced}");
    }

    #[test]
    fn a_github_rename_seen_by_watch_is_a_full_relink() {
        let (store, aliases) = store_with(&["ws1:github.com/lithometric/agentmemory"]);
        let physical = aliases.resolve("ws1", OLD);
        let first = crate::watch::register_watch(&store, &aliases, "ws1", OLD, "12345", "alice");
        assert_eq!(first["ok"], true, "{first}");
        assert!(first.get("renamed_from").is_none());
        // the same slug again is no rename
        let same = crate::watch::register_watch(&store, &aliases, "ws1", OLD, "12345", "alice");
        assert!(same.get("renamed_from").is_none(), "{same}");
        // same numeric id, new slug: a rename
        let moved = crate::watch::register_watch(&store, &aliases, "ws1", NEW, "12345", "alice");
        assert_eq!(moved["renamed_from"], OLD, "{moved}");
        assert_eq!(aliases.resolve("ws1", NEW), physical);
        assert_eq!(crate::envelope::preferred_repo_id(&store, &aliases, "ws1", OLD), NEW);
        assert_eq!(aliases.resolve("ws1", "cork"), physical);
    }
}
