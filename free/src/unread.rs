//! Unread counts for the dashboard's switchers: a number beside each
//! workspace, repo and branch saying how much happened there since this
//! person last looked.
//!
//! What counts as one update, read off the ledger (never every call):
//!
//! - an edit report that landed (`edit_reported`; a draft is never on the
//!   ledger), once per file per agent turn: a turn that wrote five files is
//!   five, however many writes each took. A row with no turn id counts once
//!   per file per agent per ten minutes;
//! - an intent declared or completed;
//! - a collision, block or hold: `gate_block` (a decision's hold too),
//!   `op_conflict`, `claim_conflict`, `collision_prevented`, `claim_prevented`;
//! - a landing (`landed`: a commit or push);
//! - a decision gone past (`decision_retried`);
//! - a message to this person (`agent_message` addressed to them), never one
//!   they sent, so a note to their own agents is not news to them.
//!
//! Reads, searches, commands, briefings, gate checks and presence write
//! other kinds (or none) and never count. The person's own agents' work
//! counts like anyone's.
//!
//! "Since they last looked" is kept on the account, in the `lastopen` record
//! the dashboard already keeps there (routes_workspaces.rs), so every device
//! agrees: `since` (when counting began for them), `all` (their last "mark
//! all read") and `marks`, the last look at a workspace, a repo, or one
//! branch of a repo. An event is unread when it is newer than every look
//! that covers it. Viewing a repo with no branch picked covers the whole
//! repo; with a branch picked, only that branch, so the others keep
//! counting in the picker while you are on one.
//!
//! The bound on the work: at most the last 14 days, at most `SCAN_CAP` rows
//! per repo, read newest first through the ledger's (scope, ts) index with
//! only the few fields needed pulled out of the stored JSON, and the answer
//! cached per person for 15 seconds. A look while viewing is written at
//! most every 30 seconds.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};

use crate::store::{now, Store};
use crate::App;

/// The account's record of where the dashboard was last open; the looks
/// live beside it.
pub const BUCKET: &str = "lastopen";
/// The scan window: an update older than this is never unread.
pub const WINDOW_S: f64 = 14.0 * 86_400.0;
/// How long one person's counts are reused.
pub const CACHE_S: f64 = 15.0;
/// A look while viewing is written at most this often.
pub const MARK_EVERY_S: f64 = 30.0;
/// The most rows read per repo.
pub const SCAN_CAP: usize = 5_000;
/// The most looks kept per person (the oldest go first).
const MARKS_CAP: usize = 400;
/// An edit report without a turn id: one file, one agent, this window.
const TURNLESS_S: f64 = 600.0;

/// The ledger kinds that are updates (see the module's head).
pub const KINDS: [&str; 11] = [
    "edit_reported", "intent_declared", "intent_completed", "gate_block", "op_conflict", "claim_conflict",
    "collision_prevented", "claim_prevented", "landed", "decision_retried", "agent_message",
];
const FIELDS: [&str; 9] = ["user", "session", "path", "turn_id", "branch", "to", "from", "op_id", "identical"];

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn num(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

// -------------------------------------------------------------------- who

/// The person reading: a ledger label names them by email, or by uid when
/// the account has no email.
#[derive(Clone, Debug)]
pub struct Viewer {
    pub uid: String,
    pub email: String,
}

impl Viewer {
    /// Does a ledger label (`email`, `email#session`, a uid) name this person?
    pub fn is(&self, label: &str) -> bool {
        let who = label.split('#').next().unwrap_or("").trim();
        !who.is_empty() && ((!self.email.is_empty() && who.eq_ignore_ascii_case(&self.email)) || who == self.uid)
    }
}

// ------------------------------------------------------------------ looks

pub fn ws_key(workspace: &str) -> String {
    format!("ws:{workspace}")
}
pub fn repo_key(scope: &str) -> String {
    format!("r:{scope}")
}
pub fn branch_key(scope: &str, branch: &str) -> String {
    format!("b:{scope}|{branch}")
}

/// When this person last looked, at everything and at each part.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    /// when counting began for them: nothing before it is unread
    pub since: f64,
    /// their last "mark all read"
    pub all: f64,
    pub marks: BTreeMap<String, f64>,
}

impl Seen {
    fn mark(&self, key: &str) -> f64 {
        self.marks.get(key).copied().unwrap_or(0.0)
    }

    /// The newest look that covers a whole repo, and the window's edge.
    pub fn floor(&self, workspace: &str, scope: &str, at: f64) -> f64 {
        (at - WINDOW_S).max(self.since).max(self.all).max(self.mark(&ws_key(workspace))).max(self.mark(&repo_key(scope)))
    }
}

/// The looks, from the account's record. A person counted for the first
/// time starts now: two weeks of history is not news to them.
pub fn load(store: &Store, uid: &str, at: f64) -> Seen {
    let record = store.kv_get(BUCKET, uid).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let marks: BTreeMap<String, f64> = record
        .get("marks")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| v.as_f64().map(|t| (k.clone(), t))).collect())
        .unwrap_or_default();
    let mut seen = Seen { since: num(&record, "since"), all: num(&record, "all"), marks };
    if seen.since <= 0.0 {
        seen.since = at;
        save(store, uid, &seen, at);
    }
    seen
}

/// Write the looks back into the record, leaving the rest of it (the
/// workspace and repo last open) as it was.
fn save(store: &Store, uid: &str, seen: &Seen, at: f64) {
    let mut record = store.kv_get(BUCKET, uid).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let mut marks: Vec<(String, f64)> = seen
        .marks
        .iter()
        .filter(|(_, t)| **t > at - WINDOW_S && **t > seen.all)
        .map(|(k, t)| (k.clone(), *t))
        .collect();
    if marks.len() > MARKS_CAP {
        marks.sort_by(|a, b| b.1.total_cmp(&a.1));
        marks.truncate(MARKS_CAP);
    }
    let marks: Map<String, Value> = marks.into_iter().map(|(k, t)| (k, json!(t))).collect();
    record["since"] = json!(seen.since);
    record["all"] = json!(seen.all);
    record["marks"] = Value::Object(marks);
    let _ = store.kv_put(BUCKET, uid, &record, at);
}

/// What a look covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Look {
    /// "Mark all read": everything, everywhere
    All,
    Workspace(String),
    /// a whole repo, by its scope
    Repo(String, String),
    Branch(String, String, String),
}

/// Record looks at `at`. `throttle`: a look while viewing, written only when
/// the last one at the same thing is older than `MARK_EVERY_S`. True when
/// anything was written.
pub fn mark(store: &Store, uid: &str, looks: &[Look], at: f64, throttle: bool) -> bool {
    if uid.is_empty() || looks.is_empty() {
        return false;
    }
    let mut seen = load(store, uid, at);
    let mut changed = false;
    for look in looks {
        let key = match look {
            Look::All => {
                seen.all = at;
                seen.marks.clear();
                changed = true;
                continue;
            }
            Look::Workspace(ws) => {
                // looks at its repos are now covered by this one
                let inside = format!("{ws}:");
                seen.marks.retain(|k, _| !k.split_once(':').is_some_and(|(_, rest)| rest.starts_with(&inside)));
                ws_key(ws)
            }
            Look::Repo(_, scope) => {
                let inside = format!("b:{scope}|");
                seen.marks.retain(|k, _| !k.starts_with(&inside));
                repo_key(scope)
            }
            Look::Branch(_, scope, branch) => branch_key(scope, branch),
        };
        if throttle && at - seen.mark(&key) < MARK_EVERY_S {
            continue;
        }
        seen.marks.insert(key, at);
        changed = true;
    }
    if changed {
        save(store, uid, &seen, at);
        forget(uid);
    }
    changed
}

// ----------------------------------------------------------------- counting

/// One repo's unread updates, and how they fall across its branches (an
/// update on no known branch is in the repo's count only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoCount {
    pub total: u64,
    pub branches: BTreeMap<String, u64>,
}

/// Count one repo's unread updates for `viewer`.
pub fn count_scope(store: &Store, scope: &str, viewer: &Viewer, seen: &Seen, at: f64) -> RepoCount {
    let workspace = crate::presence::split_scope(scope).0;
    let floor = seen.floor(workspace, scope, at);
    let rows = store.ledger_fields_since(scope, floor, &KINDS, &FIELDS, SCAN_CAP);
    let mut out = RepoCount::default();
    if rows.is_empty() {
        return out;
    }
    // each agent's branch: the newest a row of its own said, else its last action's
    let mut branch_of: HashMap<(String, String), String> = HashMap::new();
    for (_, _, f) in &rows {
        if !f[4].is_empty() {
            branch_of.entry((f[0].clone(), f[1].clone())).or_insert_with(|| f[4].clone());
        }
    }
    let mut edits: BTreeSet<String> = BTreeSet::new();
    for (ts, kind, f) in &rows {
        let (user, session, path, turn, row_branch, to, from) = (&f[0], &f[1], &f[2], &f[3], &f[4], &f[5], &f[6]);
        match kind.as_str() {
            "agent_message" => {
                if !viewer.is(to) || viewer.is(from) {
                    continue;
                }
            }
            // a commit's ops refused over drafts of the same code, or the
            // same content refused (before the engine acked it): no news
            "op_conflict" if cork_protocol::is_landed(&f[7]) || matches!(f[8].as_str(), "1" | "true") => continue,
            "edit_reported" => {
                let once = if turn.is_empty() {
                    format!("{user}|{session}|{path}|~{}", (ts / TURNLESS_S).floor() as i64)
                } else {
                    format!("{user}|{session}|{turn}|{path}")
                };
                if !edits.insert(once) {
                    continue;
                }
            }
            _ => {}
        }
        let agent = (user.clone(), session.clone());
        let branch = if !row_branch.is_empty() {
            row_branch.clone()
        } else if session.is_empty() {
            String::new()
        } else {
            branch_of
                .entry(agent)
                .or_insert_with(|| {
                    store
                        .eph_get(&crate::presence::last_action_key(scope, user, session))
                        .map(|v| text(&v, "branch"))
                        .unwrap_or_default()
                })
                .clone()
        };
        let mut covered = floor;
        if !branch.is_empty() {
            covered = covered.max(seen.mark(&branch_key(scope, &branch)));
        }
        if *ts <= covered {
            continue;
        }
        out.total += 1;
        if !branch.is_empty() {
            *out.branches.entry(branch).or_default() += 1;
        }
    }
    out
}

/// One person's counts across every workspace they are in: repos by the id
/// the dashboard lists them under (watch.rs), each repo's count rolled into
/// its workspace's.
#[derive(Clone, Debug, Default)]
pub struct Counts {
    /// workspace -> its repos' sum
    pub workspaces: BTreeMap<String, u64>,
    /// workspace -> repo -> count
    pub repos: BTreeMap<String, BTreeMap<String, RepoCount>>,
    /// workspace -> repo -> its scope
    pub scopes: BTreeMap<String, BTreeMap<String, String>>,
    /// workspace -> its name
    pub names: BTreeMap<String, String>,
}

pub fn compute(store: &Store, aliases: &crate::repo::Aliases, viewer: &Viewer, seen: &Seen, at: f64) -> Counts {
    let mut counts = Counts::default();
    for ws in crate::workspaces::list_for(store, &viewer.uid) {
        let id = text(&ws, "id");
        let Some(member) = crate::workspaces::member(store, &id, &viewer.uid) else { continue };
        let name = text(&ws, "name");
        counts.names.insert(id.clone(), if name.is_empty() { id.clone() } else { name });
        let listed = crate::watch::api_list_repos(store, aliases, &id);
        let mut done: BTreeSet<String> = BTreeSet::new();
        let mut total = 0u64;
        for repo in listed.get("repos").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !crate::access::has_repo_access(Some(&member), repo) {
                continue;
            }
            let scope = aliases.scope_for(&id, repo);
            if !done.insert(scope.clone()) {
                continue;
            }
            let count = count_scope(store, &scope, viewer, seen, at);
            total += count.total;
            counts.scopes.entry(id.clone()).or_default().insert(repo.to_string(), scope);
            counts.repos.entry(id.clone()).or_default().insert(repo.to_string(), count);
        }
        counts.workspaces.insert(id, total);
    }
    counts
}

// -------------------------------------------------------------------- cache

type Cached = HashMap<String, (f64, Arc<Counts>)>;

fn cache() -> &'static Mutex<Cached> {
    static CACHE: OnceLock<Mutex<Cached>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drop a person's cached counts: their looks changed.
pub fn forget(uid: &str) {
    cache().lock().unwrap_or_else(|e| e.into_inner()).remove(uid);
}

/// The person's counts, from the cache when it is under `CACHE_S` old.
pub fn counts_for(store: &Store, aliases: &crate::repo::Aliases, viewer: &Viewer, at: f64) -> Arc<Counts> {
    if let Some((stamp, counts)) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(&viewer.uid) {
        if at - stamp < CACHE_S && at >= *stamp {
            return Arc::clone(counts);
        }
    }
    let seen = load(store, &viewer.uid, at);
    let counts = Arc::new(compute(store, aliases, viewer, &seen, at));
    let mut held = cache().lock().unwrap_or_else(|e| e.into_inner());
    if held.len() > 20_000 {
        held.retain(|_, (stamp, _)| at - *stamp < CACHE_S);
    }
    held.insert(viewer.uid.clone(), (at, Arc::clone(&counts)));
    counts
}

// ------------------------------------------------------------------- answer

/// What the dashboard is showing, when it says: its repo (as it names it)
/// and the branch picked, if any. Shown is seen.
#[derive(Clone, Debug, Default)]
pub struct Viewing {
    pub workspace: String,
    pub repo: String,
    pub branch: String,
}

/// The endpoint's answer: every workspace's count, and for `workspace` (the
/// one the dashboard has open) its repos' and branches'. What is
/// on screen right now reads as seen, whatever the stored look says.
pub fn answer(counts: &Counts, workspace: &str, viewing: Option<(&Viewing, &str)>) -> Value {
    let mut workspaces: BTreeMap<String, u64> = counts.workspaces.clone();
    let mut repos: Map<String, Value> = Map::new();
    let mut branches: Map<String, Value> = Map::new();
    for (ws, listed) in &counts.repos {
        for (repo, count) in listed {
            let mut count = count.clone();
            if let Some((view, scope)) = viewing {
                let here = view.workspace == *ws && counts.scopes.get(ws).and_then(|s| s.get(repo)).is_some_and(|s| s == scope);
                if here {
                    let before = count.total;
                    if view.branch.is_empty() {
                        count = RepoCount::default();
                    } else {
                        // the branch on screen: its updates are seen
                        let off = count.branches.remove(&view.branch).unwrap_or(0);
                        count.total = count.total.saturating_sub(off);
                    }
                    if let Some(sum) = workspaces.get_mut(ws) {
                        *sum = sum.saturating_sub(before - count.total);
                    }
                }
            }
            if ws != workspace {
                continue;
            }
            repos.insert(repo.clone(), json!(count.total));
            let picked: Map<String, Value> = count.branches.iter().filter(|(_, n)| **n > 0).map(|(b, n)| (b.clone(), json!(n))).collect();
            if !picked.is_empty() {
                branches.insert(repo.clone(), Value::Object(picked));
            }
        }
    }
    json!({
        "workspaces": workspaces,
        "workspace": workspace,
        "repos": repos,
        "branches": branches,
        "total": workspaces.values().sum::<u64>(),
        "window_days": (WINDOW_S / 86_400.0) as u64,
    })
}

/// The bell's compact lines: "N updates in <repo> on <branch>", busiest
/// first, at most `max`. Updates on no known branch read without one.
pub fn update_lines(counts: &Counts, max: usize) -> Vec<Value> {
    let mut lines: Vec<(u64, Value)> = Vec::new();
    for (ws, listed) in &counts.repos {
        let ws_name = counts.names.get(ws).cloned().unwrap_or_else(|| ws.clone());
        for (repo, count) in listed {
            if count.total == 0 {
                continue;
            }
            let name = repo.trim_end_matches('/').rsplit('/').next().unwrap_or(repo).to_string();
            let mut rest = count.total;
            let mut line = |n: u64, branch: &str| {
                let mut link = format!("/dashboard?workspace={}&repo={}", enc(ws), enc(repo));
                if !branch.is_empty() {
                    link.push_str(&format!("&branch={}", enc(branch)));
                }
                let text = format!(
                    "{n} update{} in {name}{}",
                    if n == 1 { "" } else { "s" },
                    if branch.is_empty() { String::new() } else { format!(" on {branch}") }
                );
                lines.push((n, json!({
                    "workspace": ws, "workspace_name": ws_name, "repo": repo, "branch": branch, "n": n, "text": text, "link": link,
                })));
            };
            for (branch, n) in &count.branches {
                if *n > 0 {
                    line(*n, branch);
                    rest = rest.saturating_sub(*n);
                }
            }
            if rest > 0 {
                line(rest, "");
            }
        }
    }
    lines.sort_by(|a, b| b.0.cmp(&a.0));
    lines.into_iter().take(max).map(|(_, v)| v).collect()
}

fn enc(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

// ------------------------------------------------------------------- routes

#[derive(serde::Deserialize, Default)]
pub struct UnreadQuery {
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub branch: String,
}

type Answer = (StatusCode, Json<Value>);

fn viewer_of(app: &App, headers: &HeaderMap, at: f64) -> Option<Viewer> {
    let principal = app.dashboard_auth.principal_of(&app.store, headers, "", at)?;
    Some(Viewer { uid: principal.uid, email: principal.email })
}

/// The scope a repo the dashboard names lives in, if the person may see it.
fn visible_scope(app: &App, viewer: &Viewer, workspace: &str, repo: &str) -> Option<String> {
    let repo = repo.trim();
    if workspace.is_empty() || repo.is_empty() {
        return None;
    }
    let member = crate::workspaces::member(&app.store, workspace, &viewer.uid)?;
    if !crate::access::has_repo_access(Some(&member), repo) {
        return None;
    }
    Some(app.aliases.scope_for(workspace, repo))
}

/// The looks one body asks for (see `api_seen`).
fn looks_of(app: &App, viewer: &Viewer, body: &Value) -> Result<Vec<Look>, &'static str> {
    if body.get("all").and_then(Value::as_bool) == Some(true) {
        return Ok(vec![Look::All]);
    }
    let workspace = text(body, "workspace");
    if workspace.is_empty() {
        return Err("workspace is required");
    }
    if crate::workspaces::member(&app.store, &workspace, &viewer.uid).is_none() {
        return Err("not a member of that workspace");
    }
    let repo = text(body, "repo");
    if repo.is_empty() {
        return Ok(vec![Look::Workspace(workspace)]);
    }
    let Some(scope) = visible_scope(app, viewer, &workspace, &repo) else { return Err("no access to that repo") };
    let branch = text(body, "branch");
    if branch.is_empty() {
        return Ok(vec![Look::Repo(workspace, scope)]);
    }
    Ok(vec![Look::Branch(workspace, scope, branch)])
}

/// The workspace the answer details: the one asked about, else the one last open.
fn detail_workspace(store: &Store, uid: &str, asked: &str) -> String {
    if !asked.trim().is_empty() {
        return asked.trim().to_string();
    }
    store.kv_get(BUCKET, uid).map(|v| text(&v, "workspace")).unwrap_or_default()
}

/// `GET /api/unread?workspace=&repo=&branch=`: the counts. A repo named is
/// the one on screen: it is marked seen (with a branch picked, only that),
/// at most every 30 seconds, and reads as seen in this answer whatever was
/// stored.
async fn api_unread(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<UnreadQuery>) -> Answer {
    let at = now();
    let Some(viewer) = viewer_of(&app, &headers, at) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"})));
    };
    let workspace = detail_workspace(&app.store, &viewer.uid, &q.workspace);
    let viewing = Viewing { workspace: q.workspace.trim().to_string(), repo: q.repo.trim().to_string(), branch: q.branch.trim().to_string() };
    let scope = visible_scope(&app, &viewer, &viewing.workspace, &viewing.repo);
    if let Some(scope) = &scope {
        let ws = viewing.workspace.clone();
        let looks = if viewing.branch.is_empty() {
            vec![Look::Repo(ws, scope.clone())]
        } else {
            vec![Look::Branch(ws, scope.clone(), viewing.branch.clone())]
        };
        mark(&app.store, &viewer.uid, &looks, at, true);
    }
    let counts = counts_for(&app.store, &app.aliases, &viewer, at);
    (StatusCode::OK, Json(answer(&counts, &workspace, scope.as_deref().map(|s| (&viewing, s)))))
}

/// `POST /api/unread/seen {all: true}` / `{workspace}` / `{workspace, repo,
/// branch?}`: "Mark all read" in a menu, for that menu's reach.
/// Answers the fresh counts.
async fn api_seen(State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>) -> Answer {
    let at = now();
    let Some(viewer) = viewer_of(&app, &headers, at) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"})));
    };
    let looks = match looks_of(&app, &viewer, &body) {
        Ok(looks) => looks,
        Err(why) => return (StatusCode::BAD_REQUEST, Json(json!({"error": why}))),
    };
    mark(&app.store, &viewer.uid, &looks, at, false);
    forget(&viewer.uid);
    let workspace = detail_workspace(&app.store, &viewer.uid, &text(&body, "workspace"));
    let counts = counts_for(&app.store, &app.aliases, &viewer, at);
    (StatusCode::OK, Json(answer(&counts, &workspace, None)))
}

/// The bell where there are no notifications to list (the free version):
/// only the update lines. `GET /api/notifications`.
#[cfg_attr(feature = "cloud", allow(dead_code))]
pub async fn api_updates_only(State(app): State<Arc<App>>, headers: HeaderMap) -> Answer {
    let at = now();
    let Some(viewer) = viewer_of(&app, &headers, at) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"})));
    };
    let counts = counts_for(&app.store, &app.aliases, &viewer, at);
    (StatusCode::OK, Json(json!({"items": [], "unread": 0, "updates": update_lines(&counts, 8), "updates_only": true})))
}

/// ...and its "Mark all read": every update seen. `POST /api/notifications/read`.
#[cfg_attr(feature = "cloud", allow(dead_code))]
pub async fn api_updates_read(State(app): State<Arc<App>>, headers: HeaderMap) -> Answer {
    let at = now();
    let Some(viewer) = viewer_of(&app, &headers, at) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"})));
    };
    mark(&app.store, &viewer.uid, &[Look::All], at, false);
    (StatusCode::OK, Json(json!({"ok": true, "items": [], "unread": 0, "updates": [], "updates_only": true})))
}

pub fn router() -> Router<Arc<App>> {
    Router::new().route("/api/unread", get(api_unread)).route("/api/unread/seen", post(api_seen))
}

#[cfg(test)]
#[path = "unread_tests.rs"]
mod tests;
