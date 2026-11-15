//! What counts as an update (and what never does), once per file per turn,
//! messages to you and not from you, the roll-up from branch to repo to
//! workspace, every kind of look, the window, and the routes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::*;
use crate::store::Store;

const WS: &str = "w1";
const REPO: &str = "github.com/acme/api";
const WEB: &str = "github.com/acme/web";

fn scope(repo: &str) -> String {
    format!("{WS}:{repo}")
}

/// `w1` (acme), owned by alice, with bob in it, and two repos.
fn store() -> Store {
    let s = Store::open(std::path::Path::new(":memory:")).unwrap();
    s.kv_put("ws", WS, &json!({"id": WS, "name": "acme", "owner": "alice", "created": 0.0}), 0.0).unwrap();
    for (uid, role) in [("alice", "owner"), ("bob", "member")] {
        s.kv_put("member", &format!("{WS}/{uid}"), &json!({
            "uid": uid, "email": format!("{uid}@x.dev"), "name": uid, "role": role, "access": "write", "repos": "*", "added": 0.0,
        }), 0.0).unwrap();
        s.kv_put("uws", &format!("{uid}/{WS}"), &json!({"workspace": WS}), 0.0).unwrap();
    }
    for repo in [REPO, WEB] {
        s.kv_put("ghrepo", &format!("{WS}:{repo}"), &json!({"repo_id": repo, "by": "alice", "updated": 0.0}), 0.0).unwrap();
    }
    s
}

fn bob() -> Viewer {
    Viewer { uid: "bob".into(), email: "bob@x.dev".into() }
}

/// Bob's looks, counting from `since`.
fn seen_from(since: f64) -> Seen {
    Seen { since, ..Seen::default() }
}

fn row(s: &Store, repo: &str, kind: &str, payload: Value, ts: f64) {
    s.ledger_append(&scope(repo), kind, &payload, ts).unwrap();
}

fn edit(s: &Store, repo: &str, user: &str, session: &str, turn: &str, path: &str, branch: &str, ts: f64) {
    let mut p = json!({"user": user, "session": session, "path": path, "auto": true, "events": [], "hunks": []});
    if !turn.is_empty() {
        p["turn_id"] = json!(turn);
    }
    if !branch.is_empty() {
        p["branch"] = json!(branch);
    }
    row(s, repo, "edit_reported", p, ts);
}

fn count(s: &Store, repo: &str, seen: &Seen, at: f64) -> RepoCount {
    count_scope(s, &scope(repo), &bob(), seen, at)
}

// ------------------------------------------------------------ counting rules

#[test]
fn every_kind_of_update_counts_once() {
    let s = store();
    let t = now();
    let p = |user: &str| json!({"user": user, "session": "s1"});
    for kind in [
        "intent_declared", "intent_completed", "gate_block", "op_conflict", "claim_conflict", "collision_prevented",
        "claim_prevented", "landed", "decision_retried",
    ] {
        row(&s, REPO, kind, p("alice@x.dev"), t - 50.0);
    }
    edit(&s, REPO, "alice@x.dev", "s1", "turn1", "a.py", "", t - 40.0);
    row(&s, REPO, "agent_message", json!({"to": "bob@x.dev", "from": "alice@x.dev", "id": "m1"}), t - 30.0);
    assert_eq!(count(&s, REPO, &seen_from(t - 100.0), t).total, 11);
}

/// A commit's ops refused over the drafts that wrote that code, and the
/// same content refused, were never a collision: no news either.
#[test]
fn a_refusal_that_was_never_a_clash_is_no_update() {
    let s = store();
    let t = now();
    row(&s, REPO, "op_conflict", json!({"op_id": "git.b266fed3d7a5721e.4", "writer": "alice@x.dev", "path": "a.tsx"}), t - 50.0);
    row(&s, REPO, "op_conflict", json!({"op_id": "s1.7.0", "writer": "alice@x.dev", "path": "a.tsx", "identical": true}), t - 45.0);
    row(&s, REPO, "op_conflict", json!({"op_id": "s1.8.0", "writer": "alice@x.dev", "path": "a.tsx", "identical": false}), t - 40.0);
    assert_eq!(count(&s, REPO, &seen_from(t - 100.0), t).total, 1, "only the real one");
}

#[test]
fn not_every_call_is_an_update() {
    let s = store();
    let t = now();
    // reads, searches, commands, briefings, checks and presence write these
    // (or nothing): none of them is news
    for kind in [
        "check_performed", "brief_outcome", "map_answered", "turn_usage", "hooks_alive", "deltas_delivered", "batch_applied",
        "handoff_shown", "example_shown", "block_outcome", "tokens_saved", "memory_saved", "git_signal", "interface_verified",
    ] {
        row(&s, REPO, kind, json!({"user": "alice@x.dev", "session": "s1", "path": "a.py"}), t - 50.0);
    }
    assert_eq!(count(&s, REPO, &seen_from(t - 100.0), t).total, 0);
}

#[test]
fn an_edit_counts_once_per_file_per_agent_turn() {
    let s = store();
    let t = now();
    // five writes to one file in one turn: one update
    for i in 0..5 {
        edit(&s, REPO, "alice@x.dev", "s1", "turn1", "a.py", "", t - 90.0 + i as f64);
    }
    assert_eq!(count(&s, REPO, &seen_from(t - 100.0), t).total, 1);
    // that turn wrote four more files: one each
    for path in ["b.py", "c.py", "d.py", "e.py"] {
        edit(&s, REPO, "alice@x.dev", "s1", "turn1", path, "", t - 80.0);
    }
    assert_eq!(count(&s, REPO, &seen_from(t - 100.0), t).total, 5);
    // the same file in the next turn is news again
    edit(&s, REPO, "alice@x.dev", "s1", "turn2", "a.py", "", t - 70.0);
    // and another agent's turn on it is its own
    edit(&s, REPO, "carol@x.dev", "s9", "turn1", "a.py", "", t - 60.0);
    assert_eq!(count(&s, REPO, &seen_from(t - 100.0), t).total, 7);
}

#[test]
fn an_edit_with_no_turn_id_counts_once_per_file_per_ten_minutes() {
    let s = store();
    let t = (now() / 600.0).floor() * 600.0 + 1.0;
    for i in 0..4 {
        edit(&s, REPO, "alice@x.dev", "s1", "", "a.py", "", t + 10.0 * i as f64);
    }
    assert_eq!(count(&s, REPO, &seen_from(t - 100.0), t + 100.0).total, 1);
}

#[test]
fn a_message_counts_when_it_is_to_you_and_not_from_you() {
    let s = store();
    let t = now();
    let msg = |to: &str, from: &str, ts: f64| row(&s, REPO, "agent_message", json!({"to": to, "from": from, "id": format!("{ts}")}), ts);
    msg("bob@x.dev", "alice@x.dev", t - 50.0);
    msg("bob@x.dev#s2", "alice@x.dev#s1", t - 49.0);
    // to a teammate: not yours to read
    msg("alice@x.dev", "carol@x.dev", t - 48.0);
    // to yourself (your own agents): not news
    msg("bob@x.dev#s3", "bob@x.dev", t - 47.0);
    msg("bob@x.dev", "bob@x.dev#s1", t - 46.0);
    assert_eq!(count(&s, REPO, &seen_from(t - 100.0), t).total, 2);
}

#[test]
fn your_own_agents_work_counts_too() {
    let s = store();
    let t = now();
    edit(&s, REPO, "bob@x.dev", "s1", "turn1", "a.py", "", t - 50.0);
    row(&s, REPO, "landed", json!({"user": "bob@x.dev", "session": "s1"}), t - 40.0);
    assert_eq!(count(&s, REPO, &seen_from(t - 100.0), t).total, 2);
}

// ------------------------------------------------------------------ roll-up

#[test]
fn branches_roll_into_their_repo_and_repos_into_their_workspace() {
    let s = store();
    let t = now();
    edit(&s, REPO, "alice@x.dev", "s1", "t1", "a.py", "main", t - 50.0);
    edit(&s, REPO, "alice@x.dev", "s1", "t1", "b.py", "main", t - 49.0);
    // a row without a branch takes its agent's
    row(&s, REPO, "intent_declared", json!({"user": "alice@x.dev", "session": "s1"}), t - 48.0);
    edit(&s, REPO, "carol@x.dev", "s2", "t1", "c.py", "dev", t - 47.0);
    // an agent nothing says the branch of: in the repo's count only
    row(&s, REPO, "gate_block", json!({"user": "dan@x.dev", "session": "s3"}), t - 46.0);
    edit(&s, WEB, "alice@x.dev", "s5", "t1", "x.ts", "main", t - 45.0);

    let repo = count(&s, REPO, &seen_from(t - 100.0), t);
    assert_eq!(repo.total, 5);
    assert_eq!(repo.branches.get("main"), Some(&3));
    assert_eq!(repo.branches.get("dev"), Some(&1));

    let aliases = crate::repo::Aliases::new();
    let counts = compute(&s, &aliases, &bob(), &seen_from(t - 100.0), t);
    assert_eq!(counts.repos[WS][REPO].total, 5);
    assert_eq!(counts.repos[WS][WEB].total, 1);
    assert_eq!(counts.workspaces[WS], 6, "the workspace is its repos' sum");
    let shown = answer(&counts, WS, None);
    assert_eq!(shown["workspaces"][WS], json!(6));
    assert_eq!(shown["repos"][REPO], json!(5));
    assert_eq!(shown["branches"][REPO]["main"], json!(3));
    assert_eq!(shown["total"], json!(6));
}

#[test]
fn counts_fall_on_repos_and_branches_only() {
    // a worktree is a folder on someone's disk: Cork counts the repo and
    // the branch, and the answer names nothing else
    let s = store();
    let t = now();
    edit(&s, REPO, "alice@x.dev", "s1", "t1", "a.py", "auth", t - 50.0);
    let repo = count(&s, REPO, &seen_from(t - 100.0), t);
    assert_eq!((repo.total, repo.branches.get("auth")), (1, Some(&1)));
    let counts = compute(&s, &crate::repo::Aliases::new(), &bob(), &seen_from(t - 100.0), t);
    assert!(answer(&counts, WS, None).get("worktrees").is_none());
}

#[test]
fn a_repo_you_may_not_see_is_never_counted() {
    let s = store();
    let t = now();
    s.kv_put("member", &format!("{WS}/bob"), &json!({
        "uid": "bob", "email": "bob@x.dev", "role": "member", "access": "write", "repos": [REPO], "added": 0.0,
    }), 0.0).unwrap();
    edit(&s, WEB, "alice@x.dev", "s5", "t1", "x.ts", "main", t - 45.0);
    let counts = compute(&s, &crate::repo::Aliases::new(), &bob(), &seen_from(t - 100.0), t);
    assert!(!counts.repos[WS].contains_key(WEB));
    assert_eq!(counts.workspaces[WS], 0);
}

// -------------------------------------------------------------------- looks

#[test]
fn counting_begins_the_first_time_and_never_reaches_past_two_weeks() {
    let s = store();
    let t = now();
    edit(&s, REPO, "alice@x.dev", "s1", "t1", "a.py", "", t - 3600.0);
    // first look: two weeks of history is not news
    let first = load(&s, "bob", t);
    assert_eq!(first.since, t);
    assert_eq!(count(&s, REPO, &first, t).total, 0);
    assert_eq!(load(&s, "bob", t + 60.0).since, t, "kept on the account");
    edit(&s, REPO, "alice@x.dev", "s1", "t2", "a.py", "", t + 10.0);
    assert_eq!(count(&s, REPO, &load(&s, "bob", t + 60.0), t + 60.0).total, 1);
    // however long ago counting began, only the last 14 days are read
    edit(&s, REPO, "alice@x.dev", "s1", "t3", "old.py", "", t - 15.0 * 86_400.0);
    assert_eq!(count(&s, REPO, &seen_from(t - 30.0 * 86_400.0), t).total, 2);
}

#[test]
fn viewing_a_repo_marks_it_seen_and_a_branch_only_that_branch() {
    let s = store();
    let t = now();
    load(&s, "bob", t - 1000.0);
    edit(&s, REPO, "alice@x.dev", "s1", "t1", "a.py", "main", t - 50.0);
    edit(&s, REPO, "carol@x.dev", "s2", "t1", "c.py", "dev", t - 40.0);
    assert_eq!(count(&s, REPO, &load(&s, "bob", t), t).total, 2);

    // a branch picked: that branch is seen, the other still counts
    assert!(mark(&s, "bob", &[Look::Branch(WS.into(), scope(REPO), "main".into())], t, true));
    let after = count(&s, REPO, &load(&s, "bob", t), t);
    assert_eq!((after.total, after.branches.get("main"), after.branches.get("dev")), (1, None, Some(&1)));

    // the whole repo
    assert!(mark(&s, "bob", &[Look::Repo(WS.into(), scope(REPO))], t, true));
    assert_eq!(count(&s, REPO, &load(&s, "bob", t), t).total, 0);
    // new work after the look is unread again
    edit(&s, REPO, "carol@x.dev", "s2", "t2", "c.py", "dev", t + 5.0);
    assert_eq!(count(&s, REPO, &load(&s, "bob", t + 6.0), t + 6.0).total, 1);
    // a look while viewing is written at most every 30 seconds
    assert!(!mark(&s, "bob", &[Look::Repo(WS.into(), scope(REPO))], t + 10.0, true));
    assert!(mark(&s, "bob", &[Look::Repo(WS.into(), scope(REPO))], t + 10.0, false), "an explicit look always is");
    assert!(mark(&s, "bob", &[Look::Repo(WS.into(), scope(REPO))], t + 45.0, true));
}

#[test]
fn mark_all_read_and_a_workspace_look_cover_every_repo_in_them() {
    let s = store();
    let t = now();
    load(&s, "bob", t - 1000.0);
    edit(&s, REPO, "alice@x.dev", "s1", "t1", "a.py", "main", t - 50.0);
    edit(&s, WEB, "alice@x.dev", "s5", "t1", "x.ts", "main", t - 45.0);
    let aliases = crate::repo::Aliases::new();
    assert_eq!(compute(&s, &aliases, &bob(), &load(&s, "bob", t), t).workspaces[WS], 2);
    mark(&s, "bob", &[Look::Workspace(WS.into())], t, false);
    assert_eq!(compute(&s, &aliases, &bob(), &load(&s, "bob", t), t).workspaces[WS], 0);
    edit(&s, WEB, "alice@x.dev", "s5", "t2", "x.ts", "main", t + 1.0);
    assert_eq!(compute(&s, &aliases, &bob(), &load(&s, "bob", t + 2.0), t + 2.0).workspaces[WS], 1);
    mark(&s, "bob", &[Look::All], t + 2.0, false);
    let seen = load(&s, "bob", t + 3.0);
    assert!(seen.marks.is_empty(), "everything is covered by the one look");
    assert_eq!(compute(&s, &aliases, &bob(), &seen, t + 3.0).workspaces[WS], 0);
}

#[test]
fn what_is_on_screen_reads_as_seen_in_the_answer() {
    let s = store();
    let t = now();
    edit(&s, REPO, "alice@x.dev", "s1", "t1", "a.py", "main", t - 50.0);
    edit(&s, REPO, "carol@x.dev", "s2", "t1", "c.py", "dev", t - 40.0);
    edit(&s, WEB, "alice@x.dev", "s5", "t1", "x.ts", "main", t - 45.0);
    let counts = compute(&s, &crate::repo::Aliases::new(), &bob(), &seen_from(t - 100.0), t);
    let whole = Viewing { workspace: WS.into(), repo: REPO.into(), ..Viewing::default() };
    let shown = answer(&counts, WS, Some((&whole, &scope(REPO))));
    assert_eq!((shown["repos"][REPO].clone(), shown["repos"][WEB].clone(), shown["workspaces"][WS].clone()), (json!(0), json!(1), json!(1)));
    let on_dev = Viewing { workspace: WS.into(), repo: REPO.into(), branch: "dev".into() };
    let shown = answer(&counts, WS, Some((&on_dev, &scope(REPO))));
    assert_eq!(shown["repos"][REPO], json!(1));
    assert_eq!(shown["branches"][REPO], json!({"main": 1}));
    assert_eq!(shown["workspaces"][WS], json!(2));
}

#[test]
fn the_bells_update_lines_name_the_repo_and_branch() {
    let s = store();
    let t = now();
    edit(&s, REPO, "alice@x.dev", "s1", "t1", "a.py", "main", t - 50.0);
    edit(&s, REPO, "alice@x.dev", "s1", "t1", "b.py", "main", t - 49.0);
    row(&s, REPO, "gate_block", json!({"user": "dan@x.dev", "session": "s3"}), t - 46.0);
    let counts = compute(&s, &crate::repo::Aliases::new(), &bob(), &seen_from(t - 100.0), t);
    let lines = update_lines(&counts, 8);
    let texts: Vec<&str> = lines.iter().map(|l| l["text"].as_str().unwrap()).collect();
    assert_eq!(texts, vec!["2 updates in api on main", "1 update in api"]);
    assert!(lines[0]["link"].as_str().unwrap().contains("branch=main"));
}

// ------------------------------------------------------------------- routes

fn app(store: Store) -> Arc<App> {
    let store = Arc::new(store);
    let protocol = crate::protocol_ws::Hub::open(Arc::clone(&store), "");
    Arc::new(App {
        store,
        aliases: crate::repo::Aliases::new(),
        started: std::time::Instant::now(),
        dashboard_url: String::new(),
        public_url: "https://api.example.test".into(),
        mcp_url: String::new(),
        dashboard_auth: crate::auth::DashboardAuth { test_mode: true, firebase_project_id: String::new(), firebase_emulator: false },
        hot_ttl_s: 900.0,
        idle_after_s: 600.0,
        intent_ttl_s: 900.0,
        protocol,
    })
}

async fn call(app: &Arc<App>, method: Method, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let router = crate::build_router(app);
    let mut request = Request::builder().method(method).uri(path).header("host", "api.example.test");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())).unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn the_route_counts_marks_what_is_viewed_and_marks_all_read() {
    let s = store();
    let t = now();
    // bob's counting began a while ago (the test principal has no email: his uid labels him)
    s.kv_put(BUCKET, "bob", &json!({"since": t - 1000.0, "workspace": WS, "repo": REPO}), t).unwrap();
    edit(&s, REPO, "alice@x.dev", "s1", "t1", "a.py", "main", t - 50.0);
    edit(&s, WEB, "alice@x.dev", "s5", "t1", "x.ts", "main", t - 45.0);
    row(&s, WEB, "agent_message", json!({"to": "bob", "from": "alice@x.dev", "id": "m1"}), t - 40.0);
    let app = app(s);

    let (status, _) = call(&app, Method::GET, "/api/unread", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = call(&app, Method::GET, "/api/unread", Some("test:bob"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workspace"], json!(WS), "the workspace last open");
    assert_eq!((body["repos"][REPO].clone(), body["repos"][WEB].clone(), body["workspaces"][WS].clone()), (json!(1), json!(2), json!(3)));

    // viewing api marks it seen, and it stays seen once the page is gone
    let path = format!("/api/unread?workspace={WS}&repo={}", enc(REPO));
    let (_, body) = call(&app, Method::GET, &path, Some("test:bob"), None).await;
    assert_eq!((body["repos"][REPO].clone(), body["workspaces"][WS].clone()), (json!(0), json!(2)));
    let (_, body) = call(&app, Method::GET, "/api/unread", Some("test:bob"), None).await;
    assert_eq!(body["repos"][REPO], json!(0));
    assert!(app.store.kv_get(BUCKET, "bob").is_some_and(|r| r["repo"] == json!(REPO)), "the last-open record is kept");

    // a menu's "Mark all read"
    let (status, body) = call(&app, Method::POST, "/api/unread/seen", Some("test:bob"), Some(json!({"workspace": WS}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workspaces"][WS], json!(0));
    let (status, _) = call(&app, Method::POST, "/api/unread/seen", Some("test:bob"), Some(json!({"workspace": "elsewhere"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
