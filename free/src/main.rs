//! Cork's server.
//!
//! The hooks' hot routes (/gate, /report, /presence, /brief, ...), MCP over
//! Streamable HTTP, OAuth, billing, the dashboard's routes and the
//! first-call auto-bind, over the three storage tiers in one SQLite file
//! (store.rs). The ledger's hash chain and the merkle tree are part of the
//! stored data (hashing.rs); `parity` re-computes and re-verifies them
//! against a database file.
//!
//! Not here yet: per-call workspace ROUTING on the hot path
//! (`authorize_full` takes the credential's workspace) and the live
//! `tools/list_changed` notification.

// Without the cloud, what only the cloud calls is unused; the cloud build
// still checks it.
#![cfg_attr(not(feature = "cloud"), allow(dead_code))]

// The cloud server rebuilds multi-GB maps of big repos across many threads;
// glibc kept each rebuild's freed memory in per-thread arenas until the
// process filled its 24 GB. mimalloc returns it.
#[cfg(feature = "cloud")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Every environment read in the server goes through here. The settings
/// are named `CORK_*` now; a `CORK_X` that is set wins, and when it is unset
/// the `COLLIDE_X` it replaced is read, so Railway's `COLLIDE_*` variables
/// and an old machine's shell keep working unchanged. Any other name is read
/// as it is. Callers name the old `COLLIDE_X` or the new `CORK_X`: both mean
/// the same setting.
pub mod env {
    /// The setting's tail, `X` of `CORK_X` / `COLLIDE_X`, or `None`.
    fn tail(key: &str) -> Option<&str> {
        key.strip_prefix("CORK_").or_else(|| key.strip_prefix("COLLIDE_"))
    }

    /// `std::env::var`, with `CORK_X` first and `COLLIDE_X` after it.
    pub fn var(key: &str) -> Result<String, std::env::VarError> {
        match tail(key) {
            Some(rest) => std::env::var(format!("CORK_{rest}")).or_else(|_| std::env::var(format!("COLLIDE_{rest}"))),
            None => std::env::var(key),
        }
    }

    /// Sets the setting so `var` reads `value` whichever name the shell
    /// used: the `CORK_X` (which `var` reads first) is set and the old
    /// `COLLIDE_X` is set too, for a child process that reads only that.
    pub fn set(key: &str, value: impl AsRef<str>) {
        let value = value.as_ref();
        match tail(key) {
            Some(rest) => {
                std::env::set_var(format!("CORK_{rest}"), value);
                std::env::set_var(format!("COLLIDE_{rest}"), value);
            }
            None => std::env::set_var(key, value),
        }
    }

    /// Clears the setting under both names.
    pub fn remove(key: &str) {
        match tail(key) {
            Some(rest) => {
                std::env::remove_var(format!("CORK_{rest}"));
                std::env::remove_var(format!("COLLIDE_{rest}"));
            }
            None => std::env::remove_var(key),
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn cork_name_wins_and_the_collide_name_still_works() {
            std::env::remove_var("CORK_ENV_HELPER_PROBE");
            std::env::set_var("COLLIDE_ENV_HELPER_PROBE", "old");
            assert_eq!(super::var("COLLIDE_ENV_HELPER_PROBE").as_deref(), Ok("old"));
            assert_eq!(super::var("CORK_ENV_HELPER_PROBE").as_deref(), Ok("old"));
            std::env::set_var("CORK_ENV_HELPER_PROBE", "new");
            assert_eq!(super::var("COLLIDE_ENV_HELPER_PROBE").as_deref(), Ok("new"));
            super::remove("COLLIDE_ENV_HELPER_PROBE");
            assert!(super::var("CORK_ENV_HELPER_PROBE").is_err());
            super::set("CORK_ENV_HELPER_PROBE", "both");
            assert_eq!(std::env::var("COLLIDE_ENV_HELPER_PROBE").as_deref(), Ok("both"));
            super::remove("CORK_ENV_HELPER_PROBE");
            assert_eq!(super::var("ENV_HELPER_PLAIN_UNSET").ok(), None);
        }
    }
}

mod access;
mod overlap;
mod examples;
#[cfg(feature = "cloud")]
mod selfseat;
#[cfg(not(feature = "cloud"))]
#[path = "free/selfseat.rs"]
mod selfseat;
mod freshness;
#[cfg(feature = "cloud")]
mod vectors;
#[cfg(not(feature = "cloud"))]
#[path = "free/vectors.rs"]
mod vectors;
#[cfg(feature = "cloud")]
mod engine;
#[cfg(not(feature = "cloud"))]
#[path = "free/engine.rs"]
mod engine;
#[cfg(feature = "cloud")]
mod s3;
#[cfg(not(feature = "cloud"))]
#[path = "free/s3.rs"]
mod s3;
mod routestats;
#[cfg(feature = "cloud")]
mod routes_account;
#[cfg(not(feature = "cloud"))]
#[path = "free/routes_account.rs"]
mod routes_account;
#[cfg(feature = "cloud")]
mod routes_observe;
#[cfg(not(feature = "cloud"))]
#[path = "free/routes_observe.rs"]
mod routes_observe;
#[cfg(feature = "cloud")]
mod routes_workspaces;
#[cfg(not(feature = "cloud"))]
#[path = "free/routes_workspaces.rs"]
mod routes_workspaces;
// the status page's checks, history and incidents (the free version has none)
#[cfg(feature = "cloud")]
mod status;
// signs the requests the cloud sends to other sites (Web Bot Auth)
#[cfg(feature = "cloud")]
mod webbotauth;
#[cfg(not(feature = "cloud"))]
#[path = "free/status.rs"]
mod status;
mod watch;
mod hidden_refs;
#[cfg(feature = "cloud")]
mod github_app;
#[cfg(not(feature = "cloud"))]
#[path = "free/github_app.rs"]
mod github_app;
#[cfg(feature = "cloud")]
mod prchecks;
mod agenttools;
mod headsup;
#[cfg(feature = "cloud")]
mod dashmsg;
#[cfg(feature = "cloud")]
mod hubs;
#[cfg(not(feature = "cloud"))]
#[path = "free/hubs.rs"]
mod hubs;
#[cfg(feature = "cloud")]
mod channels;
#[cfg(not(feature = "cloud"))]
#[path = "free/channels.rs"]
mod channels;
// uploads need object storage, which only the cloud has
#[cfg(feature = "cloud")]
mod repoicons;
#[cfg(feature = "cloud")]
mod uploads;
#[cfg(feature = "cloud")]
mod crm;
#[cfg(not(feature = "cloud"))]
#[path = "free/crm.rs"]
mod crm;
mod blocks;
#[cfg(feature = "cloud")]
mod oauth;
#[cfg(not(feature = "cloud"))]
#[path = "free/oauth.rs"]
mod oauth;
mod workspaces;
// unread counts for the dashboard's switchers and the bell's update lines
mod unread;
mod activity;
#[cfg(feature = "cloud")]
mod admin;
#[cfg(not(feature = "cloud"))]
#[path = "free/admin.rs"]
mod admin;
mod claims;
mod advisory;
#[cfg(feature = "cloud")]
mod artifacts;
#[cfg(not(feature = "cloud"))]
#[path = "free/artifacts.rs"]
mod artifacts;
mod traffic;
mod today;
mod tree;
mod explain;
mod replay;
#[cfg(feature = "cloud")]
mod foresight;
#[cfg(feature = "cloud")]
mod waste;
// coordination types, streaks, badges, Wrapped: the dashboard's alone
#[cfg(feature = "cloud")]
mod habits;
mod outcomes;
#[cfg(not(feature = "cloud"))]
#[path = "free/foresight.rs"]
mod foresight;
mod auth;
#[cfg(feature = "cloud")]
mod analytics;
#[cfg(not(feature = "cloud"))]
#[path = "free/analytics.rs"]
mod analytics;
#[cfg(feature = "cloud")]
mod email;
#[cfg(not(feature = "cloud"))]
#[path = "free/email.rs"]
mod email;
// notification email: what a person must act on (the free version sends none)
#[cfg(feature = "cloud")]
mod notify;
#[cfg(not(feature = "cloud"))]
#[path = "free/notify.rs"]
mod notify;
#[cfg(feature = "cloud")]
mod billing;
#[cfg(not(feature = "cloud"))]
#[path = "free/billing.rs"]
mod billing;
mod brief;
mod brieflearn;
mod briefstat;
mod handoff;
mod trail;
mod check;
mod flaky;
mod deltas;
#[cfg(feature = "cloud")]
mod embed;
#[cfg(not(feature = "cloud"))]
#[path = "free/embed.rs"]
mod embed;
mod inflight;
mod land;
#[cfg(feature = "cloud")]
mod ratelimit;
#[cfg(not(feature = "cloud"))]
#[path = "free/ratelimit.rs"]
mod ratelimit;
// the public REST API's contract layer (v1, rate-limit headers,
// idempotency, async jobs) and its jobs
#[cfg(feature = "cloud")]
mod jobs;
#[cfg(feature = "cloud")]
mod restapi;
#[cfg(not(feature = "cloud"))]
#[path = "free/restapi.rs"]
mod restapi;
#[cfg(feature = "cloud")]
mod sharing;
#[cfg(not(feature = "cloud"))]
#[path = "free/sharing.rs"]
mod sharing;
mod recipes;
mod briefing;
mod codegraph;
mod graphview;
mod compat;
mod collisions;
mod diff;
mod envelope;
mod events;
mod gate;
mod decisions;
mod hashing;
mod insights;
mod pricing;
mod intents;
mod knows;
mod lint;
mod local;
#[cfg(feature = "cloud")]
mod machines;
#[cfg(not(feature = "cloud"))]
#[path = "free/machines.rs"]
mod machines;
#[cfg(feature = "cloud")]
mod downloads;
#[cfg(not(feature = "cloud"))]
#[path = "free/downloads.rs"]
mod downloads;
mod mcp;
mod memory;
mod merge;
mod branches;
mod trunk;
mod operations;
mod presence;
mod clash;
mod privacy;
mod protocol_ws;
mod membudget;
mod push;
mod rename;
mod moves;
mod repo;
mod report;
mod semantics;
mod store;
mod supervise;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};

use axum::extract::{DefaultBodyLimit, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use repo::Aliases;
use store::Store;

pub(crate) struct App {
    /// Shared with the OAuth router, which the SDK builds as its own axum
    /// tree: one connection, one ephemeral-wipe-at-startup, one truth.
    pub(crate) store: Arc<Store>,
    pub(crate) aliases: Aliases,
    pub(crate) started: std::time::Instant,
    pub(crate) dashboard_url: String,
    /// The address this server is reachable at, and the one agents connect
    /// to for MCP — the setup artifacts embed both, so they must be what a
    /// client can actually dial rather than what the process bound.
    pub(crate) public_url: String,
    pub(crate) mcp_url: String,
    /// How dashboard callers are identified: test tokens under the suite,
    /// Firebase ID tokens everywhere else.
    pub(crate) dashboard_auth: auth::DashboardAuth,
    pub(crate) hot_ttl_s: f64,
    pub(crate) idle_after_s: f64,
    pub(crate) intent_ttl_s: f64,
    /// The real-time protocol's engine and connections (protocol_ws.rs).
    pub(crate) protocol: Arc<protocol_ws::Hub>,
}

pub(crate) fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn number(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Which address this call came in on — the classic api.* host or the mcp.*
/// root, with or without the /mcp suffix. Rides every record so the dashboard
/// can show how an agent is actually reaching the server.
fn via_tag(headers: &HeaderMap, path: &str) -> String {
    let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
    format!("{host} {}", if path.is_empty() { "/" } else { path })
}

/// The shared preamble of every hot endpoint: resolve the credential, confirm
/// membership, resolve the scope. Returns the scope and the caller's display
/// id, or the fail-open body to return instead.
pub(crate) struct Caller {
    pub(crate) scope: String,
    /// The repo id exactly as the request spelled it — what the auto-bind
    /// records (`watch::ensure_watched`), raw.
    pub(crate) repo_id: String,
    pub(crate) user_id: String,
    pub(crate) workspace: String,
    /// The credential's workspace allowlist (`Principal.workspaces`) —
    /// empty means every workspace of the account.
    pub(crate) workspaces: Vec<String>,
    pub(crate) uid: String,
    pub(crate) email: String,
    pub(crate) name: String,
    /// What this member may DO right now, as `billing::effective_access`
    /// answers it — the stored toggle read through the live plan (on Free
    /// every non-owner is a viewer). Computed once per request, here, and
    /// read everywhere a record would be made: a viewer may look but never
    /// record — nor mint, since the setup artifacts embed a credential.
    pub(crate) read_only: bool,
    /// a session that started after the month's agent hours ran out
    /// (`billing::session_gate`): served as a viewer's agent is, with this
    /// line in place of the viewer's
    pub(crate) blocked: Option<String>,
    /// an unclaimed anonymous agent (`authorize_sandbox`): its own sandbox
    /// workspace, `sandbox/` repos, no agent hours and a short tool list
    pub(crate) sandbox: bool,
}

/// The viewer refusal, in the shape `authorize_full` refuses any other
/// access denial: the endpoint's fail-open body plus `access_denied` and the
/// one plain line. A refusal to RECORD, never to read.
fn viewer_refusal(app: &App, caller: &Caller, open: &Value) -> Json<Value> {
    let mut body = open.clone();
    if let Some(map) = body.as_object_mut() {
        map.insert("access_denied".into(), json!(true));
        map.insert("reason".into(), json!(viewer_notice(app, caller)));
    }
    Json(body)
}

/// The viewer's one line, addressed to this caller: it names the workspace,
/// who is covered and who to ask, through the caller's own nicknames.
fn viewer_notice(app: &App, caller: &Caller) -> String {
    if let Some(blocked) = &caller.blocked {
        return blocked.clone();
    }
    access::viewer_notice(&app.store, &caller.workspace, &caller.user_id, &app.dashboard_url)
}

/// The address this request came in on, in scope for
/// every ledger row the handler writes (`store::VIA`). Applied to the hot
/// routes — the hooks' and agents' surface — and not to the dashboard's
/// `/api/*`.
async fn via_scope(request: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let path = request.uri().path().to_string();
    let host = request.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let via = format!("{host} {}", if path.is_empty() { "/" } else { path.as_str() });
    let started = std::time::Instant::now();
    let response = store::VIA.scope(via, next.run(request)).await;
    // the route's first segment: /gate, /report, /mcp — never a repo id
    let route = format!("/{}", path.trim_start_matches('/').split('/').next().unwrap_or(""));
    crate::routestats::record(&route, started.elapsed().as_secs_f64() * 1000.0);
    response
}

/// A request answers once the engine's log holds every write it made: the
/// writes themselves return at local commit, and this waits once, here.
async fn durable(State(app): State<Arc<App>>, request: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    store::DURABLE
        .scope(std::cell::Cell::new(0), async move {
            let response = next.run(request).await;
            let seq = store::DURABLE.with(|mark| mark.get());
            if seq > 0 {
                if let Some(engine) = app.store.engine() {
                    engine.wait_acked_async(seq).await;
                }
            }
            response
        })
        .await
}

/// A hook post carrying a session's bill (`usage` entries from its
/// transcript): never held to the plan's call rate.
fn carries_bill(body: &Value) -> bool {
    body.get("usage").and_then(Value::as_array).is_some_and(|entries| !entries.is_empty())
}

pub(crate) fn authorize_full(
    app: &App, headers: &HeaderMap, body: &Value, open: Value,
) -> Result<Caller, Json<Value>> {
    let header = headers.get("authorization").and_then(|v| v.to_str().ok());
    let Some(principal) = auth::principal_for_bearer(&app.store, header, store::now()) else {
        let mut body = open.clone();
        if let Some(map) = body.as_object_mut() {
            map.insert("degraded".into(), json!(true));
            map.insert("reason".into(), json!("unauthorized"));
        }
        return Err(Json(body));
    };
    if principal.workspace.is_empty() {
        let mut body = open.clone();
        if let Some(map) = body.as_object_mut() {
            map.insert("degraded".into(), json!(true));
            map.insert("reason".into(), json!("unauthorized"));
        }
        return Err(Json(body));
    }
    // Route the call to the workspace that watches this repo — an explicit
    // dashboard watch, else the one workspace that already knows it, else
    // the workspace named after its GitHub org — and only then the
    // credential's own (its default: the first workspace ticked at
    // approval). Without it, one connection granted
    // several workspaces read every repo through its default: an agent in
    // another org's repo got the default's (empty) view, and its first call bound the
    // repo into the default. Candidates are the caller's memberships within the
    // credential's allowlist, so routing never widens what it may touch.
    let repo_for_routing = text(body, "repo_id");
    let workspace = if repo_for_routing.trim().is_empty() {
        principal.workspace.clone()
    } else {
        let routing = auth::AuthUser {
            user_id: principal.label().to_string(),
            workspace: principal.workspace.clone(),
            uid: principal.uid.clone(),
            workspaces: principal.workspaces.clone(),
        };
        watch::workspace_for(&app.store, &app.aliases, &routing, repo_for_routing.trim())
    };
    let Some(member) = auth::member(&app.store, &workspace, &principal.uid) else {
        let mut body = open.clone();
        if let Some(map) = body.as_object_mut() {
            map.insert("degraded".into(), json!(true));
            map.insert("reason".into(), json!("not a member"));
        }
        return Err(Json(body));
    };
    // the EFFECTIVE access: the owner's toggle read through the live plan
    // (a lapsed trial already reads as Free, where every non-owner is a
    // viewer). Computed once here; every recording endpoint reads it.
    let access = billing::effective_access(&billing::plan_of(&app.store, &workspace), &member);
    let mut read_only = access == "read";
    // the month's agent hours: a session that starts after they ran out is
    // served as a viewer's agent is, with the line that says why. A bill
    // (what a running session already spent) is never turned away.
    let mut blocked = None;
    if !read_only && !carries_bill(body) {
        if let Some(line) = billing::session_gate(&app.store, &workspace, &principal.uid, &text(body, "session"), &app.dashboard_url) {
            read_only = true;
            blocked = Some(line);
        }
    }
    let repo_id = text(body, "repo_id");
    if let access::Access::Denied(reason) =
        access::enforce_repo_access(&app.store, &workspace, &principal.uid, &repo_id)
    {
        // an explicit denial is NOT fail-open: it is the one case where the
        // answer is a refusal rather than a shrug
        let mut body = open.clone();
        if let Some(map) = body.as_object_mut() {
            map.insert("access_denied".into(), json!(true));
            map.insert("reason".into(), json!(reason));
        }
        return Err(Json(body));
    }
    // over the plan's rate: the endpoint's fail-open answer, never a block.
    // A session's bill is exempt: it is a record of what was already spent,
    // not a question, and it is sent once at a turn's end. Study 9: 24
    // sessions starting at once on a Team workspace drained the bucket, the
    // bills that came in answered `rate_limited` and were dropped, and 4-12%
    // of the run's tokens were never recorded.
    if !carries_bill(body) && !crate::ratelimit::allow_call(&app.store, &workspace) {
        let mut body = open.clone();
        if let Some(map) = body.as_object_mut() {
            map.insert("degraded".into(), json!(true));
            map.insert("reason".into(), json!("rate_limited"));
        }
        return Err(Json(body));
    }
    let scope = app.aliases.scope_for(&workspace, &repo_id);
    // on one machine every agent is the same person, and everything that
    // tells agents about each other compares people: locally, each agent
    // session is its own identity, so your agents are each other's teammates
    let user_id = if local::active() {
        local::agent_identity(&principal.label(), &text(body, "session"))
    } else {
        principal.label()
    };
    Ok(Caller {
        scope, repo_id, user_id, workspace, read_only, blocked, workspaces: principal.workspaces.clone(),
        email: principal.email.clone(), name: principal.name.clone(), uid: principal.uid, sandbox: false,
    })
}

/// The tool calls one sandbox registration may make in its life, and how
/// fast (`ratelimit::allow_sandbox`): enough to try every tool, not to work.
pub(crate) const SANDBOX_CALLS: i64 = 1_000;
pub(crate) const SANDBOX_CALLS_BUCKET: &str = "sandbox_calls";

/// What a sandbox token may not name: any repo outside `sandbox/`.
pub(crate) const SANDBOX_REPO_LINE: &str = "This is a sandbox registration: it works in repos named sandbox/<name> only \
    (for example sandbox/demo), with no agent hours. To work in real repos, claim it: POST the claim_token and the \
    person's email to the claim endpoint (https://corkcli.com/auth.md).";

/// `authorize_full` for an unclaimed anonymous agent, which only the MCP
/// endpoint accepts (`auth::sandbox_principal`): its own sandbox workspace,
/// never routed elsewhere; `sandbox/` repos only; SANDBOX_CALLS calls in all,
/// two a second; never metered, since a sandbox holds no agent hours and no
/// session in it can be a real one.
pub(crate) fn authorize_sandbox(
    app: &App, principal: &auth::Principal, body: &Value, open: Value,
) -> Result<Caller, Json<Value>> {
    let refuse = |key: &str, reason: &str| {
        let mut body = open.clone();
        if let Some(map) = body.as_object_mut() {
            map.insert(key.into(), json!(true));
            map.insert("reason".into(), json!(reason));
        }
        Json(body)
    };
    let repo_id = text(body, "repo_id");
    if !repo_id.trim().is_empty() && !auth::is_sandbox_repo(&repo_id) {
        return Err(refuse("access_denied", SANDBOX_REPO_LINE));
    }
    let workspace = principal.workspace.clone();
    if auth::member(&app.store, &workspace, &principal.uid).is_none() {
        return Err(refuse("degraded", "unauthorized"));
    }
    // a trial's allowance: SANDBOX_CALLS in all, two a second
    let made = app.store.kv_get(SANDBOX_CALLS_BUCKET, &workspace).and_then(|row| row.get("n").and_then(Value::as_i64)).unwrap_or(0)
        + app.store.kv_pending(SANDBOX_CALLS_BUCKET, &workspace, "n");
    if made >= SANDBOX_CALLS {
        let claim_url = format!("{}/agent/identity/claim", app.public_url.trim_end_matches('/'));
        let mut refusal = refuse("access_denied", &format!(
            "This sandbox has made its {SANDBOX_CALLS} tool calls. For real use, claim it: POST your claim_token and \
             your person's email to {claim_url}; they sign in to Cork and type the code it returns, and you work as \
             their agent, on their plan (https://corkcli.com/auth.md)."
        ));
        refusal.0["sandbox_calls_used"] = json!(made);
        refusal.0["claim_url"] = json!(claim_url);
        return Err(refusal);
    }
    if !crate::ratelimit::allow_sandbox(&workspace) {
        return Err(refuse("degraded", "rate_limited: a sandbox makes at most 2 calls a second"));
    }
    app.store.kv_bump(SANDBOX_CALLS_BUCKET, &workspace, "n", 1);
    Ok(Caller {
        scope: app.aliases.scope_for(&workspace, &repo_id),
        repo_id,
        user_id: principal.label(),
        workspaces: vec![workspace.clone()],
        workspace,
        read_only: false,
        blocked: None,
        email: String::new(),
        name: principal.name.clone(),
        uid: principal.uid.clone(),
        sandbox: true,
    })
}

/// The first call IS the watch. Binds the
/// caller's repo to the workspace when nothing has yet (`auto: true`, so a
/// dashboard watch outranks it) and holds the plan's repo cap at that door —
/// `Err` is the refusal payload (`billing::repo_cap_error`'s shape). Called
/// after the identity and access checks, and only for a caller who may RECORD — a viewer's agents
/// record nothing, a binding included.
pub(crate) fn bind_scope(app: &App, caller: &Caller) -> Result<(), Value> {
    bind_scope_as(app, caller, false)
}

/// `bind_scope`, saying whether the call is the explicit way in (`setup`):
/// only that adds a repo to a workspace that already has one.
pub(crate) fn bind_scope_as(app: &App, caller: &Caller, explicit: bool) -> Result<(), Value> {
    if caller.read_only {
        return Ok(());
    }
    let bound = watch::ensure_watched(
        &app.store, &app.aliases, &caller.workspace, &caller.repo_id, &caller.scope, &caller.user_id, explicit,
    );
    if let Err(refused) = &bound {
        if refused.get("new_repo").and_then(Value::as_bool) == Some(true) {
            // whichever way the agent came in (a hook, an MCP tool, /report),
            // the repo waits on the person's dashboard for a workspace — or,
            // when it looks like one the workspace has, for a link. The MCP
            // tools and /report used to answer with the question alone, so a
            // repo found by an agent without hooks never showed up there.
            let same = text(refused, "same_name_as");
            crate::watch::note_pending(&app.store, &caller.uid, &caller.repo_id, &caller.workspace, &same);
        }
    }
    bound
}

/// `authorize_full` plus the auto-bind, for every hot route with no refusal
/// shape of its own: the (N+1)th repo
/// is refused as `open` plus `workspace_full` and the one plain line — the
/// same way this layer already refuses an access denial — and is not
/// recorded, so it is never observed. /gate, /presence and /report answer
/// in their own shapes (allow-and-carry, the payload, the payload) and so
/// call `authorize_full` + `bind_scope` themselves.
pub(crate) fn authorize_bound(
    app: &App, headers: &HeaderMap, body: &Value, open: Value,
) -> Result<Caller, Json<Value>> {
    let caller = authorize_full(app, headers, body, open.clone())?;
    if let Err(refused) = bind_scope(app, &caller) {
        let mut body = open;
        if let Some(map) = body.as_object_mut() {
            if refused.get("new_repo").and_then(Value::as_bool) == Some(true) {
                // (bind_scope left it waiting on the person's dashboard)
                let same = text(&refused, "same_name_as");
                // the briefing tells it (brief_endpoint); every other route
                // just declines, so none of them spends the telling
                map.insert("new_repo".into(), json!(true));
                if !same.is_empty() {
                    map.insert("same_name_as".into(), json!(same));
                }
                map.insert("stamp".into(), json!(format!(
                    "newrepo:{}:{}:{}", caller.workspace, repo::repo_key(&caller.repo_id), caller.user_id)));
            } else if refused.get("not_shared").and_then(Value::as_bool) == Some(true) {
                map.insert("not_shared".into(), json!(true));
            } else {
                map.insert("workspace_full".into(), json!(true));
            }
            map.insert("reason".into(), refused.get("error").cloned().unwrap_or(Value::Null));
        }
        return Err(Json(body));
    }
    Ok(caller)
}

/// The scopes of the caller's workspace this caller may read — the one
/// argument every workspace-wide read (roster, recipes) takes. Computed from
/// the membership record, never from the request.
pub(crate) fn visible_for(app: &App, caller: &Caller) -> std::collections::BTreeSet<String> {
    let member = auth::member(&app.store, &caller.workspace, &caller.uid);
    access::visible_scopes(&app.store, &caller.workspace, member.as_ref())
}

async fn health(State(app): State<Arc<App>>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let ok = app.store.ping();
    let mut body = json!({
        "local": local::active(),
        "local_id": if local::active() { local::instance_id() } else { String::new() },
        "status": if ok { "ok" } else { "degraded" },
        "uptime_s": app.started.elapsed().as_secs_f64(),
        "tiers": {"sqlite": {"driver": "rusqlite", "ok": ok}},
        "engine": "rust",
        "store": app.store.write_stats(),
        "routes": crate::routestats::view(),
    });
    let mut memory = crate::membudget::health();
    memory["store_caches_mb"] = json!(app.store.cache_bytes() / (1024 * 1024));
    body["memory"] = memory;
    body["protocol"] = app.protocol.health();
    if let Some(state) = crate::embed::health() {
        body["embeddings"] = state;
    }
    // which build answered: a deploy can be checked instead of guessed at
    let commit: String = crate::env::var("RAILWAY_GIT_COMMIT_SHA").unwrap_or_default().chars().take(7).collect();
    if !commit.is_empty() {
        body["commit"] = json!(commit);
    }
    // where people and agents read how the service is doing, and its JSON
    if let Some(page) = crate::status::page_url(&app) {
        body["status_page"] = json!(page);
        body["status_json"] = json!(format!("{page}.json"));
    }
    // a machine still restoring takes no traffic: Railway keeps the old
    // deployment serving until this says ready
    if let Some(engine) = app.store.engine() {
        body["engine"] = engine.view(&app.store);
        if !engine.ready() {
            body["status"] = json!("restoring");
            return (http::StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response();
        }
    }
    Json(body).into_response()
}

/// PreToolUse: may this write proceed? Fails open on everything except a
/// certain collision on the exact path.
async fn gate_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"allow": true});
    let caller = match authorize_full(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let path = text(&body, "path");
    if path.is_empty() || text(&body, "repo_id").is_empty() {
        return Json(json!({"allow": true, "degraded": true, "reason": "missing repo_id or path"}));
    }
    let session = text(&body, "session");
    if caller.read_only {
        // a viewer's agent: the gate ALLOWS (fail-open contract) and records
        // nothing; the one line rides the first gate of the session and no other
        let mut body = json!({"allow": true, "access_denied": true, "viewer": true});
        let stamp = format!("viewertold:{}:{}", caller.workspace, presence::agent_id(&caller.user_id, &session));
        if access::tell_once(&app.store, &stamp, access::TOLD_ONCE_TTL_S) {
            body["reason"] = json!(viewer_notice(&app, &caller));
        }
        return Json(body);
    }
    // the first call IS the watch — and the (N+1)th repo is not bound, so it
    // is not observed, and never blocked: the gate allows and carries the line
    if let Err(refused) = bind_scope(&app, &caller) {
        let new_repo = refused.get("new_repo").and_then(Value::as_bool) == Some(true);
        let same = text(&refused, "same_name_as");
        let mut answer = json!({"allow": true, "reason": refused.get("error").cloned().unwrap_or(Value::Null)});
        answer[if new_repo { "new_repo" } else { "workspace_full" }] = json!(true);
        if !same.is_empty() {
            answer["same_name_as"] = json!(same);
        }
        return Json(answer);
    }
    let via = via_tag(&headers, "/gate");
    watch::note_hook_repo(&app.store, &caller.uid, &caller.repo_id);
    // a new numbered file (a migration, an ADR): the number is claimed for
    // this agent right here, or the write is steered to the next free one
    if let Some(claim) = body.get("claim").filter(|c| c.is_object()) {
        let after = claim.get("after").and_then(Value::as_i64).unwrap_or(0);
        if let Some(reason) = crate::claims::gate_check(
            &app.store, &caller.scope, &caller.user_id, &session, &text(&body, "agent"), &path, after, &via,
        ) {
            let decision = json!({"allow": false, "path": path, "reason": name_repo_id(&reason, &caller.repo_id), "claim": true});
            gate::note_verdict(&app.store, &caller.scope, &caller.user_id, &session, &path, &decision);
            return Json(decision);
        }
    }
    // a subagent's write carries the parent's session; a hook that names
    // the worker gets it its own hold on a standing decision
    let worker = text(&body, "worker");
    // what the write does by name (hooks v75+), and whether this session's
    // pushes land through Cork: a teammate's push is then no pull-now
    let write = crate::freshness::WriteFacts::from_json(body.get("write"));
    let lands = body.get("lands").and_then(Value::as_bool);
    crate::freshness::note_lands(&app.store, &caller.scope, &caller.user_id, &session, lands);
    // the branch this write lands on (hooks v75+): another branch's work is
    // never a block (branches.rs)
    crate::branches::note(&app.store, &caller.scope, &caller.user_id, &crate::presence::agent_session(&session, &worker),
        &text(&body, "branch"), &text(&body, "target"));
    let mut decision = gate::gate_with(
        &app.store, &caller.scope, &caller.user_id, &session, &worker, &path, &app.dashboard_url, &via,
        write.as_ref(), lands == Some(true),
    ).body;
    // the verdict waits for this write's report, which puts it on the step
    gate::note_verdict(&app.store, &caller.scope, &caller.user_id, &session, &path, &decision);
    // what it tells the agent, on the heads-up log
    let said: Vec<(String, f64)> = ["reason", "notice"]
        .iter()
        .map(|field| text(&decision, field))
        .filter(|line| !line.is_empty())
        .map(|line| (line, crate::store::now()))
        .collect();
    crate::headsup::received(&app.store, &caller.scope, &caller.user_id, &session, &worker, "gate", &said, "");
    // whose agent it was is the dashboard's, not the hook's
    gate::strip_links(&mut decision);
    // a player's agent hears once per session who Cork cannot see —
    // riding the first gate of the session, as the viewer's line does
    if let Some(nudge) = access::viewers_nudge(&app.store, &caller.workspace, &caller.user_id, &session, &app.dashboard_url) {
        decision["viewers_notice"] = json!(nudge);
    }
    // the sessions on other branches: the hook's protocol check holds no
    // write over their work either
    let mine = crate::branches::of(&app.store, &caller.scope, &caller.user_id, &session);
    let apart = crate::branches::others_apart(&app.store, &caller.scope, &mine, (&caller.user_id, &session));
    if !apart.is_empty() {
        decision["apart"] = Value::Object(apart);
    }
    // code a teammate is creating here, unpushed: the hook turns a lease on
    // it into "build against its interface", never a hold (hooks v78)
    if let Some(creating) = crate::inflight::creating(&app.store, &caller.scope, &caller.user_id, &session, &path) {
        decision["creating"] = creating;
    }
    Json(decision)
}

/// The model message a hook call came from (`step.msg`), "" when it does
/// not say: what tells an agent's next message from this one (headsup.rs).
fn step_msg(body: &Value) -> String {
    body.pointer("/step/msg").and_then(Value::as_str).unwrap_or("").to_string()
}

/// The context a hook post was made in: what a v72+ hook sends as
/// `context`, else input + cache read + cache creation of an older hook's
/// counters.
fn context_of(body: &Value) -> i64 {
    let given = number(body, "context");
    if given > 0 {
        given
    } else {
        number(body, "input_tokens") + number(body, "cache_read_tokens") + number(body, "cache_creation_tokens")
    }
}

/// A hook post's agent: the subagent it came from, with its name, and what
/// the harness calls the session.
fn note_worker(store: &store::Store, scope: &str, user: &str, body: &Value) {
    let session = text(body, "session");
    presence::note_worker(store, scope, user, &session, &text(body, "worker"), &text(body, "worker_type"), &text(body, "worker_name"));
    crate::freshness::note_title(store, scope, user, &session, &text(body, "title"));
    // and what Claude Code calls it, the tool it runs in and its machine
    crate::freshness::note_session(
        store, scope, user, &session, &text(body, "cc_name"), &text(body, "harness"), &text(body, "machine"));
    crate::freshness::note_task(store, scope, user, &session, &text(body, "task"));
    // what was posted to it through Claude Code and its transcript shows
    local::peer_seen(store, scope, user, body);
}

/// A session's bill, as `presence_endpoint` takes it: the usage entries
/// become ledger rows (`report::bill_usage`) and join the agent's token
/// counter, with the context of the newest. A subagent's end retires its
/// worker marker.
fn bill_session(app: &App, caller: &Caller, body: &Value, entries: &[Value]) {
    let (store, scope, user) = (&app.store, caller.scope.as_str(), caller.user_id.as_str());
    let session = text(body, "session");
    let worker = text(body, "worker");
    let billed = report::bill_usage(store, scope, user, &session, &text(body, "agent"), &worker, entries, store::now());
    if let Some(newest) = entries.last().filter(|_| billed.tokens > 0) {
        let turn_id = newest.get("turn_id").and_then(Value::as_str).unwrap_or("");
        let model = newest.get("model").and_then(Value::as_str).unwrap_or("");
        // a subagent's context is its own, not the session's
        let context = if worker.is_empty() { number(newest, "context") } else { 0 };
        presence::bump_tokens(store, scope, user, &session, billed.tokens, turn_id, context, model, &presence::Effort::of(newest));
    }
    // the agent hours the month is billed in: the session's own time and
    // each subagent's (the parent waits on them inside a tool call, which
    // the hooks do not count, so their model time is counted here)
    if billed.active_s > 0.0 {
        billing::meter_active(store, &caller.workspace, &caller.uid, billed.active_s, &app.dashboard_url);
    }
    if body.get("worker_ended").and_then(Value::as_bool) == Some(true) {
        presence::forget_worker(store, scope, user, &session, &worker);
        local::handoffs_finished(store, scope, user, &session, &worker, "", true);
        // and so does what that helper said was still under way
        crate::intents::end_status_notes(store, scope, &session, &worker);
        // what it read and changed joins its session's trail
        crate::trail::record(store, scope, user, &session, false);
    }
}

/// What a session said when its last turn ended, as its last action keeps it.
fn store_last_words(store: &store::Store, scope: &str, user: &str, session: &str) -> String {
    store
        .eph_get(&presence::last_action_key(scope, user, session))
        .filter(|last| text(last, "kind") == "done")
        .map(|last| text(&last, "note"))
        .unwrap_or_default()
}

/// PostToolUse read side: where an agent is looking. Never content.
async fn presence_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(mut body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_full(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    // the workspace's switches, held to before anything is stored (privacy.rs)
    crate::privacy::enforce(&app.store, &caller.workspace, "/presence", &mut body);
    // a refused call is still a call: counted before the viewer and
    // validation checks
    billing::count_call(&app.store, &caller.workspace);
    // presence is a RECORD: a viewer (effective access "read") is refused
    // in the same shape as any other access denial
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    crate::sharing::note(&app.store, &caller.workspace, &caller.user_id, &text(&body, "machine"), crate::store::now());
    watch::note_hook_repo(&app.store, &caller.uid, &caller.repo_id);
    // the working copy is the agent's: a helper may work in a worktree of its own
    let agent = crate::presence::agent_session(&text(&body, "session"), &text(&body, "worker"));
    crate::freshness::note_checkout(&app.store, &caller.scope, &caller.user_id, &agent, &text(&body, "checkout"));
    crate::freshness::note_ide(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "ide"), &text(&body, "ide_workspace"));
    crate::freshness::note_title(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "title"));
    crate::freshness::note_session(
        &app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "cc_name"), &text(&body, "harness"),
        &text(&body, "machine"));
    // a turn's end too says what its transcript shows arrived through Claude
    // Code: before anything below hands messages over
    local::peer_seen(&app.store, &caller.scope, &caller.user_id, &body);
    let path = text(&body, "path");
    let action = text(&body, "action");
    // the session's bill from its transcript (hooks v72+) rides a turn's end,
    // a session's end or a limit, or comes alone ("usage", a subagent's end)
    let bill = body.get("usage").and_then(Value::as_array).filter(|entries| !entries.is_empty());
    if (bill.is_some() || action == "usage") && !text(&body, "repo_id").is_empty() {
        if let Err(refused) = bind_scope(&app, &caller) {
            return Json(refused);
        }
        bill_session(&app, &caller, &body, bill.map(Vec::as_slice).unwrap_or(&[]));
        if action == "usage" {
            return Json(json!({"ok": true}));
        }
    }
    // a shell command has started: the agent runs it now (focus only; its
    // end writes the rest)
    if body.get("started").and_then(Value::as_bool) == Some(true) && !path.is_empty() && !text(&body, "repo_id").is_empty() {
        if let Err(refused) = bind_scope(&app, &caller) {
            return Json(refused);
        }
        let (session, worker) = (text(&body, "session"), text(&body, "worker"));
        if worker.is_empty() {
            presence::note_started(
                &app.store, &caller.scope, &caller.user_id, &session, &path, &text(&body, "branch"), &text(&body, "model"),
                &presence::Effort::of(&body),
            );
        } else {
            // a subagent's command (a hook that names the worker on its
            // started post): its own marker runs, the session's focus stands
            presence::note_worker_started(
                &app.store, &caller.scope, &caller.user_id, &session, &worker,
                &text(&body, "worker_type"), &text(&body, "worker_name"), &path,
            );
        }
        return Json(json!({"ok": true}));
    }
    // a turn's end: its closing message becomes the note on what it changed
    if action == "settled" && !text(&body, "repo_id").is_empty() {
        if let Err(refused) = bind_scope(&app, &caller) {
            return Json(refused);
        }
        presence::report_done(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "text"));
        // a turn that ends its task: the sessions it talked with hear so
        if text(&body, "worker").is_empty() && local::reads_finished(&text(&body, "text")) {
            local::tell_partners_finished(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "text"), false);
        }
        // what it says is left, for the session's handoff; a harness that
        // knows this turn was the session's last says so (`done`)
        crate::handoff::note_closing(&app.store, &caller.scope, &text(&body, "session"), &text(&body, "text"));
        // and what it read and changed, for the next agent here
        crate::trail::record(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"), false);
        if body.get("done").and_then(Value::as_bool) == Some(true) {
            crate::handoff::save(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"));
        }
        // the handoffs it took: their senders hear it finished, with how
        local::handoffs_finished(
            &app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "worker"), &text(&body, "text"), false);
        return Json(crate::intents::settle_turn_for(
            &app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "worker"), &text(&body, "text"),
            &text(&body, "agent")));
    }
    // a session end names no path: the agent is simply gone
    if text(&body, "repo_id").is_empty() || (path.is_empty() && action != "ended") {
        return Json(json!({"ok": false, "reason": "missing repo_id or path"}));
    }
    // the first call IS the watch — and the (N+1)th repo is refused here,
    // with the payload itself
    if let Err(refused) = bind_scope(&app, &caller) {
        return Json(refused);
    }
    let (visible, workspace) = (visible_for(&app, &caller), caller.workspace.clone());
    let (scope, user_id) = (caller.scope, caller.user_id);
    note_worker(&app.store, &scope, &user_id, &body);
    // the hook saw the repo's tests run in the agent's shell, and their verdict
    if let Some(ok) = body.get("tests_ok").and_then(Value::as_bool) {
        crate::examples::note_tests(&app.store, &scope, &user_id, &text(&body, "session"), ok, &path);
    }
    if action == "ended" {
        // the sessions it talked with hear it is gone, with its last words
        let last = store_last_words(&app.store, &scope, &user_id, &text(&body, "session"));
        local::tell_partners_finished(&app.store, &scope, &user_id, &text(&body, "session"), &last, true);
        // the handoff first: it reads the session's test run and task, and
        // what git says of the files it changed, as its hook found them
        crate::handoff::save_with(&app.store, &scope, &user_id, &text(&body, "session"), body.get("git_files"));
        local::handoffs_finished(&app.store, &scope, &user_id, &text(&body, "session"), "", "", true);
        crate::overlap::task_ended(&app.store, &scope, &text(&body, "session"));
        crate::examples::session_ended(&app.store, &scope, &text(&body, "session"));
        crate::freshness::note_ended(&app.store, &scope, &text(&body, "session"));
        crate::outcomes::session_ended(&app.store, &scope, &user_id, &text(&body, "session"));
        // what its turns said was still under way ends with it
        crate::intents::end_status_notes(&app.store, &scope, &text(&body, "session"), "");
    }
    let via = via_tag(&headers, "/presence");
    let mut answer = presence::report_presence(
        &app.store,
        &presence::PresenceInput {
            scope: &scope,
            user_id: &user_id,
            // the hook has always sent this; it names the agent
            session: &text(&body, "session"),
            path: &path,
            action: if action.is_empty() { "reading" } else { &action },
            agent: &text(&body, "agent"),
            model: &text(&body, "model"),
            effort: presence::Effort::of(&body),
            branch: &text(&body, "branch"),
            tokens: number(&body, "tokens"),
            turn_id: &text(&body, "turn_id"),
            context: context_of(&body),
            via: &via,
            worker: &text(&body, "worker"),
        },
    );
    // the session's trail, with its last step logged, for the next agent here
    if action == "ended" {
        crate::trail::record(&app.store, &scope, &user_id, &text(&body, "session"), true);
    }
    // what this step's message cost, for the dashboard's line (never billed)
    if matches!(action.as_str(), "" | "reading" | "searching" | "running") {
        if let Some(cost) = presence::step_usage(&body) {
            presence::note_exchange(&app.store, &scope, &user_id, &text(&body, "session"), presence::Step::Current, &cost);
            // and, before its first write, the session's discovery (trail.rs)
            crate::trail::note_cost(&app.store, &scope, &text(&body, "session"), &text(&body, "worker"), &cost, false);
        }
    }
    // the hooks post a test verdict, a usage limit and a session's end
    // without reading the answer: a message handed back on those would be
    // marked delivered and never seen, so it waits for a call that prints it
    if !discards_answer(&action, &body) {
        attach_messages(&app, &scope, &workspace, &user_id, &caller.uid, &text(&body, "session"), &text(&body, "worker"), &visible, &step_msg(&body), &mut answer);
    }
    Json(answer)
}

/// A prompt that names no code: at most this many modules matched by meaning,
/// and only this close.
const MEANING_ONLY_MODULES: usize = 2;
const MEANING_ONLY_FLOOR: f64 = 0.55;

/// The characters of teammates' messages one hook answer carries.
const MESSAGE_BUDGET_CHARS: usize = 7000;

/// Teammates' messages this session has not been shown, as one block, each
/// told once a session (the briefing and every hook answer share the mark).
/// An agent deep in one long task sends no prompt, so a message that waited
/// for the next briefing reached it only after the work it was about.
fn teammate_messages(
    app: &App, scope: &str, workspace: &str, user_id: &str, session: &str, worker: &str,
    visible: &std::collections::BTreeSet<String>, handed: &mut Vec<String>,
) -> Option<String> {
    if local::active() || session.is_empty() {
        return None;
    }
    let inbox = crate::agenttools::inbox_ack(&app.store, scope, user_id, visible, &[]);
    let inbox = access::own_messages_only(&app.store, workspace, user_id, inbox, &app.dashboard_url);
    // whole messages, not their first 600 characters: an instruction past
    // that cut was never seen. The block stays inside what a harness shows
    // of one hook message (Codex: about 2,500 tokens); past that budget a
    // message is cut and says where to read it whole
    let messages = inbox.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    // told once per agent: a helper is not its lead (`session/worker`), so
    // a note one of them sent never comes back to it and still reaches the other
    let agent = if worker.is_empty() { session.to_string() } else { format!("{session}/{worker}") };
    let (told, reached) = crate::push::hook_lines(&app.store, scope, user_id, &agent, &messages, MESSAGE_BUDGET_CHARS);
    // in this session's context now: delivered, for the dashboard's log
    crate::agenttools::mark_delivered(&app.store, &reached, session);
    handed.extend(reached.iter().map(|m| text(m, "id")).filter(|id| !id.is_empty()));
    // and on the step it rode in on, by count and sender (the dashboard's dropdown)
    let senders: Vec<String> = reached.iter().map(|m| text(m, "from")).filter(|f| !f.is_empty()).collect();
    presence::note_messages(&app.store, scope, user_id, session, reached.len(), &senders);
    // a call to action only over a message that asks something; and no
    // ack to run: the hooks show each message once per session already
    // notes from this person's own other sessions say so, not "teammates'"
    let own = !reached.is_empty() && reached.iter().all(|m| crate::agenttools::is_own_session_note(m, user_id));
    (!told.is_empty()).then(|| format!("{}\n{}", crate::agenttools::messages_heading(&reached, own), told.join("\n")))
}

/// A hook answer's waiting messages: this person's other sessions, then
/// teammates', on the `inbox_note` every hook prints. `step` is the model
/// message the call came from: what this agent was handed during an
/// earlier one is seen now (headsup.rs).
#[allow(clippy::too_many_arguments)]
fn attach_messages(
    app: &App, scope: &str, workspace: &str, user_id: &str, uid: &str, session: &str, worker: &str,
    visible: &std::collections::BTreeSet<String>, step: &str, answer: &mut Value,
) {
    crate::headsup::settle(&app.store, scope, user_id, session, worker, step);
    // the agent the hook ran for: the session's lead, or one of its helpers
    let mut handed = local::attach_inbox_for(&app.store, scope, user_id, session, worker, answer);
    if !text(answer, "inbox_note").is_empty() {
        presence::note_messages(&app.store, scope, user_id, session, 1, &["your other session".to_string()]);
    }
    let mut told: Vec<String> = Vec::new();
    told.extend(teammate_messages(app, scope, workspace, user_id, session, worker, visible, &mut handed));
    crate::headsup::received_messages(&app.store, scope, user_id, session, worker, &handed, step);
    told.extend(hub_messages(app, scope, user_id, uid, session));
    if !told.is_empty() {
        let told = told.join("\n\n");
        let own = text(answer, "inbox_note");
        if let Some(map) = answer.as_object_mut() {
            map.insert("inbox_note".into(), json!(if own.is_empty() { told } else { format!("{own}\n\n{told}") }));
        }
    }
}

/// Seat this agent session under its person (its hub number, on first
/// sight) and hand it the hub messages waiting for it (`hubs.rs`): from
/// people and agents in other accounts and repos, once per session.
fn hub_messages(app: &App, scope: &str, user_id: &str, uid: &str, session: &str) -> Option<String> {
    if local::active() || session.is_empty() || uid.is_empty() {
        return None;
    }
    hubs::note_session(&app.store, uid, session, scope, user_id);
    let note = hubs::hook_note(&app.store, uid, session);
    if note.is_some() {
        presence::note_messages(&app.store, scope, user_id, session, 1, &["hub".to_string()]);
    }
    note
}

/// A notice that tells the agent to call a Cork tool names the repo it
/// is about: `claim(repo_id, "db/migrations/")` becomes
/// `claim(repo_id="github.com/acme/app", "db/migrations/")`.
pub(crate) fn name_repo_id(line: &str, repo_id: &str) -> String {
    let repo_id = repo_id.trim();
    if repo_id.is_empty() {
        return line.to_string();
    }
    line.replace("(repo_id, ", &format!("(repo_id=\"{repo_id}\", "))
        .replace("(repo_id)", &format!("(repo_id=\"{repo_id}\")"))
}

/// A presence post whose answer no hook prints.
fn discards_answer(action: &str, body: &Value) -> bool {
    matches!(action, "limit" | "ended") || body.get("tests_ok").is_some()
}

/// Hook traffic proves this user's machine auto-reports: remember it
/// durably, once, so the install nudge never fires for them again. Only the
/// hook paths (/report, /observe) call this — a report made by the model
/// through the MCP tool is exactly the case the nudge exists for.
fn mark_hooks_seen(store: &Store, scope: &str, user_id: &str) {
    let hook_key = format!("{scope}:{user_id}");
    if !compat::truthy(store.kv_get("hookseen", &hook_key).as_ref()) {
        let _ = store.kv_put("hookseen", &hook_key, &json!({"ts": store::now()}), store::now());
    }
}

/// Whether this person's hooks have ever reported in this repo: their
/// sessions are then billed from their transcripts, and a tool call over
/// MCP is not metered on its own.
pub(crate) fn hooks_seen(store: &Store, scope: &str, user_id: &str) -> bool {
    compat::truthy(store.kv_get("hookseen", &format!("{scope}:{user_id}")).as_ref())
}

/// An account's first connected agent: the month's bonus hours land now
/// (not at sign-up, so a throwaway account with no agent earns nothing).
fn first_agent_bonus(app: &App, caller: &Caller) {
    billing::grant_bonus(&app.store, &caller.workspace, &caller.uid, "signup", billing::SIGNUP_BONUS_HOURS);
}

/// Files the agent LOOKED at, and the tracked tree the session-start index
/// sweeps: folded into the map, nothing else. One file (`path`, `content`)
/// or a batch (`files: [{path, content}]`) so an index of a few hundred
/// modules is a handful of calls. A single read also registers as presence,
/// which is what the read side of the hook posted before it carried content.
fn string_list(body: &Value, key: &str) -> Vec<String> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// The hook ran the repo's check after a batch of writes: stamp the
/// caller's write records with the result.
async fn verified_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(mut body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    // the workspace's switches, held to before anything is stored (privacy.rs)
    crate::privacy::enforce(&app.store, &caller.workspace, "/verified", &mut body);
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    let paths = string_list(&body, "paths");
    let command = text(&body, "command");
    if paths.is_empty() || command.is_empty() {
        return Json(json!({"ok": false, "reason": "missing paths or command"}));
    }
    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let agent = {
        let a = text(&body, "agent");
        if a.is_empty() { "cork-report-hook".to_string() } else { a }
    };
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    billing::count_call(&app.store, &caller.workspace);
    mark_hooks_seen(&app.store, &caller.scope, &caller.user_id);
    Json(crate::check::stamp_verified(
        &app.store, &caller.scope, &caller.user_id, &agent, &paths, &command, ok, crate::store::now(),
    ))
}

/// The hook ran the dependents' tests for an interface change it reported
/// (the /report response's `verify` plan) and says how they went; the server
/// grades the change on the graph. Fail open.
async fn verify_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(mut body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    // the workspace's switches, held to before anything is stored (privacy.rs)
    crate::privacy::enforce(&app.store, &caller.workspace, "/verify", &mut body);
    billing::count_call(&app.store, &caller.workspace);
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let path = text(&body, "path");
    if text(&body, "repo_id").is_empty() || path.is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id or path"}));
    }
    let via = via_tag(&headers, "/verify");
    Json(report::record_verification(
        &app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &path, &body, app.hot_ttl_s, &via,
    ))
}

/// A machine learned tests are flaky (they failed, then passed, on the same
/// tree): their names and counts join the repo's registry. With no tests it
/// is a pull. Either way the answer is the registry (flaky.rs).
async fn flaky_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false, "flaky": []});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    billing::count_call(&app.store, &caller.workspace);
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let tests: Vec<Value> = body.get("tests").and_then(Value::as_array).cloned().unwrap_or_default();
    let now = crate::store::now();
    let listed = if tests.is_empty() {
        crate::flaky::registry(&app.store, &caller.scope, now).unwrap_or_default()
    } else {
        crate::flaky::record(&app.store, &caller.scope, &caller.user_id, &tests, now)
    };
    Json(json!({"ok": true, "flaky": listed}))
}

/// A failure named files and symbols: who wrote the files, what the
/// symbols really are.
async fn attribute_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    let paths = string_list(&body, "paths");
    let symbols = string_list(&body, "symbols");
    let session = text(&body, "session");
    billing::count_call(&app.store, &caller.workspace);
    let answer = crate::check::attribute(
        &app.store, &caller.scope, &caller.user_id, &session, &paths, &symbols, crate::store::now(),
    );
    // the grep companion: the map answered the name the agent searched for
    // with its signature, docstring and callers, in the same step — the read
    // of that file, not sent. One saving per answered grep, not per symbol.
    // a name the map could not tell apart answered nothing the agent can skip a read for
    let answered = answer
        .get("facts")
        .and_then(Value::as_object)
        .map(|f| f.values().filter(|fact| fact.get("ambiguous").is_none()).count())
        .unwrap_or(0);
    if !symbols.is_empty() && answered > 0 && !caller.read_only {
        let agent = if text(&body, "agent").is_empty() { "cork-report-hook".to_string() } else { text(&body, "agent") };
        crate::briefstat::record_saving(
            &app.store, &caller.scope, "map_answered", &caller.user_id, &agent, &session, 1,
            &json!({"symbols": answered}), crate::store::now(),
        );
    }
    Json(answer)
}

/// What other agents changed in this session's working set since its last
/// step — a few lines the hook prepends to the next event.
async fn deltas_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    let session = text(&body, "session");
    crate::freshness::note_lands(&app.store, &caller.scope, &caller.user_id, &session, body.get("lands").and_then(Value::as_bool));
    let since = body.get("since_ts").and_then(Value::as_f64).unwrap_or(0.0);
    let budget = body.get("budget").and_then(Value::as_u64).unwrap_or(0) as usize;
    // what the session's hook already told it (pre-write gate, protocol push)
    let told: std::collections::BTreeSet<String> = body
        .get("told")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(500)
        .map(str::to_string)
        .collect();
    // the reader by agent: a helper hears its siblings and its lead, never itself
    let reader = crate::presence::agent_session(&session, &text(&body, "worker"));
    let mut answer = crate::deltas::deltas_told(&app.store, &caller.scope, &caller.user_id, &reader, since, budget, &told);
    // the heads-up log: each line handed to this agent, with when its change was reported
    let sent = answer.as_object_mut().and_then(|map| map.remove("sent")).unwrap_or_else(|| json!([]));
    if !caller.read_only {
        let lines: Vec<(String, f64)> = string_list(&answer, "lines")
            .into_iter()
            .zip(sent.as_array().cloned().unwrap_or_default())
            .filter_map(|(line, at)| at.as_f64().map(|at| (line, at)))
            .collect();
        crate::headsup::received(&app.store, &caller.scope, &caller.user_id, &session, &text(&body, "worker"), "delta", &lines, "");
    }
    // each change delivered is the re-read that would have caught it, not sent
    let delivered = answer.get("count").and_then(Value::as_u64).unwrap_or(0);
    // and it rode in on the step the agent is on: how many, and whose
    let from = answer.as_object_mut().and_then(|map| map.remove("from")).unwrap_or_else(|| json!([]));
    if delivered > 0 && !caller.read_only {
        presence::note_exchange(
            &app.store, &caller.scope, &caller.user_id, &session, presence::Step::Current,
            &json!({"deltas": delivered, "deltas_from": from}),
        );
    }
    if delivered > 0 && !caller.read_only {
        let agent = if text(&body, "agent").is_empty() { "cork-report-hook".to_string() } else { text(&body, "agent") };
        crate::briefstat::record_saving(
            &app.store, &caller.scope, "deltas_delivered", &caller.user_id, &agent, &session, delivered,
            &json!({"count": delivered}), crate::store::now(),
        );
    }
    Json(answer)
}

/// `cork land`: the push turn (`acquire` / `release`) and the renames
/// that landed (`renames`). See land.rs.
async fn land_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(mut body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    // the workspace's switches, held to before anything is stored (privacy.rs)
    crate::privacy::enforce(&app.store, &caller.workspace, "/land", &mut body);
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let session = text(&body, "session");
    let holder = format!("{}#{}", caller.user_id, session);
    let now = crate::store::now();
    let agent = if text(&body, "agent").is_empty() { "collide-hook land".to_string() } else { text(&body, "agent") };
    let via = via_tag(&headers, "/land");
    Json(match text(&body, "action").as_str() {
        "acquire" => crate::land::acquire(&app.store, &caller.scope, &caller.user_id, &holder, now),
        "release" => crate::land::release(&app.store, &caller.scope, &caller.user_id, &session, &agent, &holder, &body, now, &via),
        "renames" => json!({"ok": true, "renames": crate::land::renames(&app.store, &caller.scope, now)}),
        // git's own record, counts only: a conflict, a rejected push or a
        // revert the hook saw, and the repo's weekly history (land.rs)
        "signal" => crate::land::signal(&app.store, &caller.scope, &caller.user_id, &session, &body, now),
        "baseline" => crate::land::baseline(&app.store, &caller.scope, &caller.user_id, &body, now),
        // before the push: which of the symbols it carries other live
        // sessions on the same branch are changing too (merge.rs). Advice:
        // the hook prints it and lands anyway
        "preview" => {
            crate::branches::note(&app.store, &caller.scope, &caller.user_id, &session, &text(&body, "branch"), &text(&body, "target"));
            let changed = body.get("changed").and_then(Value::as_array).cloned().unwrap_or_default();
            crate::merge::preview_landing(&app.store, &caller.scope, &caller.user_id, &session, &changed)
        }
        _ => json!({"ok": false, "reason": "action is acquire, release, renames, signal, baseline or preview"}),
    })
}

/// Trunk mode (trunk.rs): the repo's setting (`get`, and `set` from
/// `cork trunk`, the owner's in a team), and the registry of the branches
/// agents make (`branch`, as the hooks see them; `want`, a Land or Delete
/// for the owner's hooks to do).
async fn trunk_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(mut body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    // a branch's purpose is a conversation's title by another name
    crate::privacy::enforce(&app.store, &caller.workspace, "/trunk", &mut body);
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    let now = crate::store::now();
    let action = text(&body, "action");
    if action == "get" {
        let mut answer = json!({"ok": true});
        crate::trunk::attach(&app.store, &caller.scope, &caller.user_id, &mut answer);
        answer["branches"] = json!(crate::trunk::branches(&app.store, &caller.scope));
        return Json(answer);
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let failed = |why: String| json!({"ok": false, "reason": why});
    Json(match action.as_str() {
        "set" => {
            let role = auth::member(&app.store, &caller.workspace, &caller.uid).map(|m| text(&m, "role")).unwrap_or_default();
            if !local::active() && role != "owner" {
                failed("only the workspace's owner switches trunk mode".into())
            } else {
                match crate::trunk::set(&app.store, &caller.scope, &caller.user_id, &body, now) {
                    Ok(trunk) => json!({"ok": true, "trunk": trunk}),
                    Err(why) => failed(why),
                }
            }
        }
        "branch" => match crate::trunk::record(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &body, now) {
            Ok(row) => json!({"ok": true, "branch": row}),
            Err(why) => failed(why),
        },
        "want" => match crate::trunk::want(&app.store, &caller.scope, &text(&body, "branch"), &text(&body, "want"), &caller.user_id, now) {
            Ok(row) => json!({"ok": true, "branch": row}),
            Err(why) => failed(why),
        },
        _ => failed("action is get, set, branch or want".into()),
    })
}

/// `cork apply` landed a batch: every file written and verified
/// without the agent reading or editing it, two messages a file less the one
/// command that did it. With a recipe id it was another agent's work replayed.
async fn apply_done_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let n = |key: &str| body.get(key).and_then(Value::as_u64).unwrap_or(0).min(10_000);
    let files = n("files");
    if files == 0 {
        return Json(json!({"ok": false, "reason": "files must be at least 1"}));
    }
    let recipe = text(&body, "recipe");
    let agent = if text(&body, "agent").is_empty() { "collide-hook/apply".to_string() } else { text(&body, "agent") };
    let answer = crate::briefstat::record_saving(
        &app.store, &caller.scope, "batch_applied", &caller.user_id, &agent, &text(&body, "session"), (2 * files).saturating_sub(1).max(1),
        &json!({"files": files, "ops": n("ops"), "recipe": recipe, "replayed": !recipe.is_empty()}), crate::store::now(),
    );
    Json(answer)
}

/// A verified typed-op batch, saved as a recipe the next agent can replay.
async fn recipe_save_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    billing::count_call(&app.store, &caller.workspace);
    Json(crate::recipes::save(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &body, crate::store::now()))
}

async fn recipe_get_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let id = text(&body, "id");
    if text(&body, "repo_id").is_empty() || id.is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id or id"}));
    }
    // no recipes for a viewer's agents, nor for the agent over the cap: the
    // notice instead of the content, once
    if caller.read_only {
        return Json(json!({"ok": false, "viewer": true, "reason": viewer_notice(&app, &caller)}));
    }
    let (over_cap, cap_notice) = access::agent_cap_state(
        &app.store, &caller.workspace, &caller.user_id, &text(&body, "session"), &app.dashboard_url);
    if over_cap {
        return Json(json!({"ok": false, "over_cap": true, "reason": cap_notice}));
    }
    let visible = visible_for(&app, &caller);
    let share = access::knowledge_shared(&app.store, &caller.workspace);
    Json(crate::recipes::get(&app.store, &caller.scope, &visible, &id, crate::store::now(), &caller.user_id, share))
}

/// Direct callers of a symbol, for expanding a recipe's `$caller` ops.
async fn callers_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let path = text(&body, "path");
    let symbol = text(&body, "symbol");
    if text(&body, "repo_id").is_empty() || path.is_empty() || symbol.is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id, path or symbol"}));
    }
    Json(json!({"ok": true, "callers": crate::recipes::callers(&app.store, &caller.scope, &path, &symbol)}))
}

/// A question that names no code and asks for no change is briefed only when
/// its meaning lands this close to real code (the ranked score, the wired
/// bonus included): "why do UK customers pay no VAT?" does; "which database
/// should we use?" does not.
const QUESTION_MEANING_FLOOR: f64 = 0.75;

/// Worked examples in the briefing: OFF unless CORK_EXAMPLES=1. The
/// three-round study (8 new people a round, similar features) showed them
/// firing on the right tasks (7/8 and 8/8 of rounds 2 and 3, none on bug
/// reports, every scored use good) but no clear saving: Cork already
/// brings a small feature to about ten steps, and rounds without them cost
/// the same within noise. Tasks are still recorded, so switching them on
/// starts from the team's history.
fn examples_on() -> bool {
    crate::env::var("CORK_EXAMPLES").map(|v| v == "1").unwrap_or(false)
}

/// The session-start briefing the hook prints into the agent's context:
/// recently changed modules as exact facts, under a byte budget.
/// `POST /brief/touch` `{repo_id, session, paths, write}`: the briefing for
/// a repo a session reaches after it started (brief::touch), asked by the
/// hook once per repo per session. Empty text when there is nothing to say,
/// and for anyone the full briefing would also give nothing (a viewer, an
/// agent over the plan's cap, a repo the workspace lacks).
async fn touch_brief_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let empty = json!({"ok": true, "text": ""});
    let Ok(caller) = authorize_bound(&app, &headers, &body, empty.clone()) else { return Json(empty) };
    let repo_id = text(&body, "repo_id");
    let session = text(&body, "session");
    if repo_id.is_empty() || caller.read_only {
        return Json(empty);
    }
    let (over_cap, _) = access::agent_cap_state(&app.store, &caller.workspace, &caller.user_id, &session, &app.dashboard_url);
    if over_cap {
        return Json(empty);
    }
    let paths: Vec<String> = string_list(&body, "paths").into_iter().take(10).collect();
    let write = body.get("write").and_then(Value::as_bool).unwrap_or(false);
    let visible = visible_for(&app, &caller);
    let plan = access::plan_for(&app.store, &caller.workspace);
    let input = crate::brief::BriefInput {
        repo_id: &repo_id, since: 0.0, budget: 0, identifiers: &[], exclude: &[], session: &session, resume: false,
        idle_after_s: app.idle_after_s,
        visible: &visible, viewer: &caller.user_id, share_knowledge: plan.flag("knowledge_sharing", true),
        verified: body.get("verifies").and_then(Value::as_bool).unwrap_or(false),
    };
    let said = crate::brief::touch(&app.store, &caller.scope, &input, &paths, write);
    Json(json!({"ok": true, "text": said}))
}

/// The briefing, with the repo's trunk mode (trunk.rs) on every answer:
/// the hooks learn it here, at session start and each prompt, and never
/// ask on a tool call.
async fn brief_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let mut asker = (String::new(), String::new());
    let Json(mut answer) = brief_answer(Arc::clone(&app), headers, body, &mut asker).await;
    let (scope, user) = asker;
    if !scope.is_empty() && answer.get("ok").and_then(Value::as_bool) == Some(true) {
        crate::trunk::attach(&app.store, &scope, &user, &mut answer);
    }
    Json(answer)
}

/// `asker` is set to the caller's (scope, user) once they are known.
async fn brief_answer(app: Arc<App>, headers: HeaderMap, body: Value, asker: &mut (String, String)) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(Json(mut response)) => {
            // the hooks print a briefing's text: the question about a repo
            // the workspace lacks reaches the agent at session start or the
            // next prompt, twice a day at most
            let stamp = text(&response, "stamp");
            if let Some(map) = response.as_object_mut() {
                map.remove("stamp");
                if map.get("new_repo").and_then(Value::as_bool) == Some(true)
                    && !stamp.is_empty()
                    && access::tell_once(&app.store, &stamp, 12.0 * 3600.0)
                {
                    let said = map.get("reason").and_then(Value::as_str).unwrap_or("").to_string();
                    map.insert("text".into(), json!(format!("Cork: {said}")));
                }
            }
            return Json(response);
        }
    };
    *asker = (caller.scope.clone(), caller.user_id.clone());
    let repo_id = text(&body, "repo_id");
    if repo_id.is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    let since = body.get("since").and_then(Value::as_f64).unwrap_or(0.0);
    let budget = body.get("budget").and_then(Value::as_u64).unwrap_or(0) as usize;
    let mut identifiers = string_list(&body, "identifiers");
    let exclude = string_list(&body, "exclude");
    let session = text(&body, "session");
    let resume = body.get("resume").and_then(Value::as_bool).unwrap_or(false);
    // the prompt's own words, when the hook sends them: used for this one
    // lookup by meaning (embed.rs) and dropped, never stored or logged
    let prompt = text(&body, "prompt");
    billing::count_call(&app.store, &caller.workspace);
    // a viewer's agents get no briefing: the notice IS the briefing text
    if caller.read_only {
        return Json(json!({"ok": true, "text": viewer_notice(&app, &caller),
                           "modules": 0, "total": 0, "paths": [], "viewer": true}));
    }
    // the fourth agent at once on Free: recorded and on Now, briefed nothing
    // — the notice once, then an empty briefing
    let (over_cap, cap_notice) = access::agent_cap_state(
        &app.store, &caller.workspace, &caller.user_id, &session, &app.dashboard_url);
    if over_cap {
        return Json(json!({"ok": true, "text": cap_notice, "modules": 0, "total": 0, "paths": [], "over_cap": true}));
    }
    // only the hook asks for this: proof the hooks are live
    mark_hooks_seen(&app.store, &caller.scope, &caller.user_id);
    first_agent_bonus(&app, &caller);
    // the repo this person's agents work in: the MCP tools' default repo_id
    watch::note_hook_repo(&app.store, &caller.uid, &caller.repo_id);
    // the repo's branches as this person's checkout has them,
    // for the dashboard's switchers (routes_observe::activity merges them)
    let named = |key: &str| -> Vec<String> {
        string_list(&body, key).into_iter().map(|v| v.trim().chars().take(120).collect::<String>())
            .filter(|v| !v.is_empty()).take(200).collect()
    };
    let branches = named("branches");
    // and the IDE workspace this session opened the checkout in: one per
    // session, so the person's last few are kept, newest first
    let ide_workspace: String = text(&body, "ide_workspace").trim().chars().take(120).collect();
    if !branches.is_empty() || !ide_workspace.is_empty() {
        let key = format!("repogit:{}:{}", caller.scope, caller.user_id);
        let mut ide_workspaces: Vec<String> = app.store.eph_get(&key)
            .and_then(|r| r.get("ide_workspaces").and_then(Value::as_array).cloned()).unwrap_or_default()
            .iter().filter_map(Value::as_str).map(str::to_string).collect();
        if !ide_workspace.is_empty() {
            ide_workspaces.retain(|w| *w != ide_workspace);
            ide_workspaces.insert(0, ide_workspace);
            ide_workspaces.truncate(10);
        }
        let _ = app.store.eph_set(&key,
            &json!({"branches": branches, "ide_workspaces": ide_workspaces}), Some(7.0 * 86_400.0));
    }
    // a prompt just arrived: the agent is running from this moment, before
    // its first tool call (a failed or backgrounded one never reports). The
    // prompt's text stays out of presence; it is the person's.
    // a prompt a person typed (the hook says so): they are here now, which
    // their next morning's "since you left" is counted from (today.rs)
    if body.get("here").and_then(Value::as_bool) == Some(true) && !text(&body, "prompt").trim().is_empty() && !caller.read_only {
        let viewer = crate::today::Viewer { user: &caller.user_id, local: local::active() };
        crate::today::note_here(&app.store, &caller.scope, viewer, crate::store::now());
    }
    if !text(&body, "prompt").trim().is_empty() && !session.is_empty() && !caller.read_only {
        let via = via_tag(&headers, "/brief");
        presence::report_presence(&app.store, &presence::PresenceInput {
            scope: &caller.scope, user_id: &caller.user_id, session: &session, path: "", action: "thinking",
            agent: &text(&body, "agent"), model: &text(&body, "model"), effort: presence::Effort::of(&body), branch: &text(&body, "branch"),
            tokens: 0, turn_id: "", context: 0, via: &via, worker: "",
        });
    }
    let visible = visible_for(&app, &caller);
    // the recent-writes window never reaches past the plan's history
    let plan = access::plan_for(&app.store, &caller.workspace);
    let since = match plan.history_window_s() {
        Some(window) => {
            let floor = crate::store::now() - window;
            if since > 0.0 { since.max(floor) } else { floor.max(crate::store::now() - 7.0 * 86_400.0) }
        }
        None => since,
    };
    // code the prompt names only to keep away from is not its target
    let negated = crate::overlap::negated_names(&prompt);
    if !negated.is_empty() {
        identifiers.retain(|name| !crate::overlap::is_negated(name, &negated));
    }
    let mut by_meaning: Vec<Value> = Vec::new();
    let mut named_for_overlap: Vec<String> = Vec::new();
    if !prompt.trim().is_empty() {
        let names_code = !identifiers.is_empty();
        let named = identifiers.clone();
        named_for_overlap = named.clone();
        // a repo indexed moments ago is still being embedded: the first
        // prompt of a session arrives in the same second as its index, and
        // matched names only. A short wait gets it the meaning match.
        // matching by meaning (the embedding model) is Team and up; Free
        // matches the names a prompt uses
        let (meant, shown) = if plan.flag("meaning_match", true) {
            let waited = std::time::Instant::now();
            while crate::embed::pending_for(&caller.scope) > 0 && waited.elapsed() < std::time::Duration::from_millis(2500) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            // a prompt that names no code is matched by meaning alone: fewer
            // modules, and only close ones (a vague ask was briefed six
            // loosely related modules every turn)
            let k = if names_code { 6 } else { MEANING_ONLY_MODULES };
            let (meant, shown) = crate::embed::prompt_identifiers(&app.store, &caller.scope, &prompt, k, Some((&caller.user_id, &session)));
            let best = shown.first().and_then(|h| h.get("score")).and_then(Value::as_f64).unwrap_or(0.0);
            if names_code || best >= MEANING_ONLY_FLOOR {
                (meant, shown)
            } else {
                (Vec::new(), Vec::new())
            }
        } else {
            (Vec::new(), Vec::new())
        };
        // a conversation, not a task: no briefing (the agent is still marked working above)
        let closest = shown.first().and_then(|h| h.get("score")).and_then(Value::as_f64).unwrap_or(0.0);
        if !crate::brief::about_the_code(&prompt, names_code, closest >= QUESTION_MEANING_FLOOR) {
            if !session.is_empty() {
                presence::note_exchange(
                    &app.store, &caller.scope, &caller.user_id, &session, presence::Step::Latest { kind: "thinking", path: "" },
                    &json!({"skipped": "not about the code", "modules": 0, "tokens": 0}),
                );
            }
            return Json(json!({"ok": true, "text": "", "modules": 0, "total": 0, "paths": [], "skipped": "not about the code"}));
        }
        for id in meant {
            if !identifiers.contains(&id) {
                identifiers.push(id);
            }
        }
        by_meaning = shown;
        // a prompt that named nothing and meant nothing gets no briefing, not
        // the session-start one again; only, when it is another live
        // session's task too, that warning
        if identifiers.is_empty() {
            let task_q = if plan.flag("meaning_match", true) {
                crate::embed::query_vector(&crate::examples::task_text(&prompt)).unwrap_or_default()
            } else {
                Vec::new()
            };
            let warn = crate::overlap::duplicate_task(&app.store, &caller.scope, &caller.user_id, &session, &task_q, &[]);
            // an earlier agent's similar task: where it worked is where a
            // prompt that names no code needs to look (trail.rs)
            let similar = crate::trail::similar_work(
                &app.store, &caller.scope, &caller.user_id, &session, &task_q, &BTreeSet::new(), plan.flag("knowledge_sharing", true));
            let mut said: Vec<String> = warn.into_iter().collect();
            said.extend(similar.as_ref().map(|s| s.lines.join("\n")));
            let said = crate::brief::with_extras("", &said, crate::brief::prompt_ceiling(1));
            let (named, checks) = similar.map(|s| (s.paths, s.checks)).unwrap_or_default();
            return Json(json!({"ok": true, "text": said, "modules": 0, "total": 0, "paths": [], "named": named, "checks": checks}));
        }
    }
    let input = crate::brief::BriefInput {
        repo_id: &repo_id, since, budget, identifiers: &identifiers, exclude: &exclude, session: &session, resume,
        idle_after_s: app.idle_after_s,
        visible: &visible, viewer: &caller.user_id, share_knowledge: plan.flag("knowledge_sharing", true),
        // the hook checks each symbol line against its copy (mapcheck.rs)
        verified: body.get("verifies").and_then(Value::as_bool).unwrap_or(false),
    };
    let mut answer = crate::brief::brief(&app.store, &caller.scope, &input);
    // a person's session start: on their first here after a while away,
    // what happened since they left (today.rs)
    if body.get("morning").and_then(Value::as_bool) == Some(true) && prompt.trim().is_empty() {
        let viewer = crate::today::Viewer { user: &caller.user_id, local: local::active() };
        let picked = body.get("picked").and_then(Value::as_u64).unwrap_or(0) as usize;
        let lines = crate::today::morning(&app.store, &caller.scope, viewer, &session, picked, crate::store::now());
        if !lines.is_empty() {
            answer["morning"] = json!(lines);
        }
    }
    if !by_meaning.is_empty() {
        answer["meant"] = json!(by_meaning);
    }
    // the task as a vector (the prompt without its process sentences), made
    // once: the worked examples, the same-task check and the handoffs use it
    let meaning = plan.flag("meaning_match", true);
    let task_q = if meaning && !prompt.trim().is_empty() {
        crate::embed::query_vector(&crate::examples::task_text(&prompt)).unwrap_or_default()
    } else {
        Vec::new()
    };
    // two tasks on the same code: say so to both now, before either writes;
    // and a task that ends in a push: how to finish it in one step
    if !prompt.trim().is_empty() {
        let overlap = crate::overlap::announce(&app.store, &caller.scope, &caller.user_id, &session, &named_for_overlap, &by_meaning);
        // the same TASK as another live session, before any code is touched
        let duplicate = crate::overlap::duplicate_task(&app.store, &caller.scope, &caller.user_id, &session, &task_q, &named_for_overlap);
        let overlap = match (overlap, duplicate) {
            (Some(a), Some(b)) => Some(format!("{b}\n{a}")),
            (a, b) => b.or(a),
        };
        // what an earlier session left unfinished on this code or task
        let mut on_paths = string_list(&answer, "paths");
        on_paths.extend(crate::deltas::working_set(&app.store, &caller.scope, &session));
        let handoffs = crate::handoff::brief_lines(&app.store, &crate::handoff::Receiver {
            scope: &caller.scope, viewer: &caller.user_id, session: &session, paths: &on_paths,
            identifiers: &named_for_overlap, q: &task_q, share_knowledge: plan.flag("knowledge_sharing", true),
        });
        // an earlier agent's similar task: what it read and changed, less
        // what this briefing already lists (trail.rs)
        let listed: BTreeSet<String> =
            string_list(&answer, "paths").into_iter().chain(string_list(&answer, "named")).chain(exclude.iter().cloned()).collect();
        let similar = crate::trail::similar_work(
            &app.store, &caller.scope, &caller.user_id, &session, &task_q, &listed, plan.flag("knowledge_sharing", true));
        let mut similar_lines: Vec<String> = Vec::new();
        if let Some(similar) = similar {
            similar_lines = similar.lines;
            // a write to one of its files is a search this session did not make
            let mut named = string_list(&answer, "named");
            named.extend(similar.paths.into_iter().filter(|p| !named.contains(p)).collect::<Vec<_>>());
            answer["named"] = json!(named);
            // its line ranges are checked against the reader's copy like the rest
            match answer.get_mut("checks").and_then(Value::as_array_mut) {
                Some(checks) => checks.extend(similar.checks),
                None => answer["checks"] = json!(similar.checks),
            }
        }
        // what rides after the module blocks, in the order it matters, each
        // kept whole: the overlap warning, an earlier agent's similar work,
        // the handoffs, then (below) a worked example, helpers, the finish line
        let mut units: Vec<String> = overlap.into_iter().collect();
        if !similar_lines.is_empty() {
            units.push(similar_lines.join("\n"));
        }
        units.extend(handoffs);
        let adds = crate::brief::adds_code(&prompt);
        let reuse = if adds && meaning { crate::embed::reuse_candidates(&app.store, &caller.scope, &prompt, 3) } else { Vec::new() };
        // a worked example: a finished, tested task like this one. A live
        // teammate in the same code comes first, and then there is none
        crate::examples::start_task(&app.store, &caller.scope, &caller.user_id, &session, &task_q, adds);
        let example = if adds && units.is_empty() && examples_on() {
            let mut points_at: BTreeSet<String> = BTreeSet::new();
            for list in [answer.get("paths"), answer.get("meant")] {
                for v in list.and_then(Value::as_array).into_iter().flatten() {
                    match v.as_str() {
                        Some(p) => { points_at.insert(p.to_string()); }
                        None => { points_at.insert(text(v, "path")); }
                    }
                }
            }
            for helper in &reuse {
                points_at.insert(text(helper, "path"));
                points_at.insert(format!("{}::{}", text(helper, "path"), text(helper, "symbol")));
            }
            let readable: BTreeSet<String> = crate::recipes::readable_scopes(&caller.scope, &visible).into_iter().collect();
            crate::examples::offer(&app.store, &crate::examples::Offer {
                scope: &caller.scope, readable: &readable, user: &caller.user_id, session: &session, q: &task_q,
                points_at: &points_at, history_s: plan.history_window_s(), share_knowledge: plan.flag("knowledge_sharing", true),
            })
        } else {
            None
        };
        let (example_line, example_meta) = match example {
            Some((line, meta)) => (Some(line), Some(meta)),
            None => (None, None),
        };
        if let Some(meta) = example_meta {
            answer["example"] = meta;
        }
        let reuse_line = crate::brief::reuse_line(&reuse);
        if !reuse.is_empty() {
            answer["reuse"] = json!(reuse);
        }
        units.extend(example_line.into_iter().chain(reuse_line).chain(crate::brief::finish_line(&prompt, &text(&body, "commit"))));
        // all of it under the briefing's ceiling, which scales with the task:
        // a warning or a handoff on a prompt that matched no code is the briefing
        let ceiling = answer.get("ceiling").and_then(Value::as_u64).map(|c| c as usize).unwrap_or_else(|| crate::brief::prompt_ceiling(1));
        answer["text"] = json!(crate::brief::with_extras(&text(&answer, "text"), &units, ceiling));
    }
    if !identifiers.is_empty() {
        let shown = string_list(&answer, "paths");
        crate::deltas::touch_briefed(&app.store, &caller.scope, &session, &shown, crate::store::now());
    }
    // what the hooks never carried before: messages for this person (the
    // MCP tools deliver them on every response, but an agent that only has
    // hooks never saw one), each once per session; and the numbers others
    // have claimed, whenever there is a briefing to put them in
    let mut extra: Vec<String> = Vec::new();
    // a repo or its files renamed this week: each person told once, at a
    // session's start (moves.rs)
    if prompt.trim().is_empty() {
        extra.extend(crate::moves::brief_lines(&app.store, &app.aliases, &caller.workspace, &caller.scope, &caller.user_id));
    }
    let mut own = json!({});
    let mut handed = local::attach_inbox(&app.store, &caller.scope, &caller.user_id, &session, &mut own);
    if let Some(note) = own.get("inbox_note").and_then(Value::as_str) {
        extra.push(note.to_string());
        presence::note_messages(&app.store, &caller.scope, &caller.user_id, &session, 1, &["your other session".to_string()]);
    }
    if let Some(told) = teammate_messages(&app, &caller.scope, &caller.workspace, &caller.user_id, &session, "", &visible, &mut handed) {
        extra.push(told);
    }
    // before any model message of the turn: its first call is past them
    crate::headsup::received_messages(&app.store, &caller.scope, &caller.user_id, &session, "", &handed, crate::headsup::PROMPT);
    if let Some(told) = hub_messages(&app, &caller.scope, &caller.user_id, &caller.uid, &session) {
        extra.push(told);
    }
    // the caller's hubs, in one line, when there are any and it changed
    if !local::active() && !session.is_empty() {
        extra.extend(hubs::brief_line(&app.store, &caller.uid, &session));
    }
    if !text(&answer, "text").is_empty() || identifiers.is_empty() {
        extra.extend(
            crate::claims::brief_lines(&app.store, &caller.scope, &caller.user_id, &session, crate::store::now())
                .into_iter()
                .map(|line| name_repo_id(&line, &caller.repo_id)),
        );
    }
    if !extra.is_empty() {
        let shown = text(&answer, "text");
        let joined = extra.join("\n");
        answer["text"] = json!(if shown.is_empty() { joined } else { format!("{shown}\n{joined}") });
    }
    // once a week, who Cork cannot see: the hook prints `text`, so the
    // nudge rides inside it — but only inside a briefing that is being
    // shown anyway. A prompt that matched nothing injects nothing, and an
    // upsell is never the only thing an agent is handed.
    if !text(&answer, "text").is_empty() {
        if let Some(nudge) = access::viewers_nudge(&app.store, &caller.workspace, &caller.user_id, &session, &app.dashboard_url) {
            let shown = text(&answer, "text");
            answer["text"] = json!(format!("{shown}\n\n{nudge}"));
            answer["viewers_notice"] = json!(nudge);
        }
    }
    // what this briefing handed a prompt, by counts and names, on the
    // prompt's step (the dashboard's dropdown); never the prompt itself
    let exchange = answer.as_object_mut().and_then(|map| map.remove("exchange"));
    if !prompt.trim().is_empty() && !session.is_empty() {
        presence::note_exchange(
            &app.store, &caller.scope, &caller.user_id, &session, presence::Step::Latest { kind: "thinking", path: "" },
            &prompt_exchange(&answer, exchange, by_meaning.len()),
        );
    }
    // the workspace's switches, for the hooks to keep before they send
    answer["privacy"] = crate::privacy::of(&app.store, &caller.workspace).to_json();
    Json(answer)
}

/// A prompt's briefing as its step keeps it: the modules and symbols
/// briefed, notes and recipes served, callers expanded, how many symbols
/// the prompt matched by meaning (`vectors`), and the briefing's size in
/// tokens (about four characters each).
fn prompt_exchange(answer: &Value, exchange: Option<Value>, by_meaning: usize) -> Value {
    let mut x = exchange.filter(Value::is_object).unwrap_or_else(|| json!({}));
    let modules = string_list(answer, "paths");
    x["modules"] = json!(answer.get("modules").and_then(Value::as_u64).unwrap_or(modules.len() as u64));
    x["module_names"] = json!(modules);
    x["vectors"] = json!(by_meaning);
    let said = text(answer, "text");
    x["tokens"] = json!(said.chars().count().div_ceil(4));
    let example = answer.get("example").is_some_and(|e| !e.is_null());
    if example {
        x["example"] = json!(true);
    }
    if let Some(skipped) = answer.get("skipped").and_then(Value::as_str) {
        x["skipped"] = json!(skipped);
    }
    x
}

/// `POST /renames {repo_id, renamed_from?, moves?, present?}`: what a
/// checkout saw renamed (moves.rs). A repo rename is authorized as a call on
/// the OLD id, so it is the old repo's workspace, and a member who may record
/// there, that the new name is carried into; the hook sends it before
/// reporting anything under the new id. Path moves land in the repo's scope,
/// and the moved files' old copies leave its map. `present` is what a
/// checkout on the trunk tracks (`{paths, head_ts}`): the map lets go of
/// the files it no longer has (codegraph::forget_absent).
async fn renames_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let mut answer = json!({"ok": true});
    let old = text(&body, "renamed_from").trim().to_string();
    if !old.is_empty() {
        let mut probe = body.clone();
        probe["repo_id"] = json!(old);
        match authorize_full(&app, &headers, &probe, open.clone()) {
            Ok(caller) if !caller.read_only => {
                answer["repo_rename"] = crate::moves::repo_rename_reported(
                    &app.store, &app.aliases, &caller.workspace, &old, &text(&body, "repo_id"), &caller.user_id,
                );
            }
            Ok(_) => answer["repo_rename"] = json!({"ok": false, "error": "a viewer's agents record nothing"}),
            Err(refused) => return refused,
        }
    }
    let moves = crate::moves::reported_moves(&body);
    let present = body.get("present").filter(|p| p.get("paths").is_some_and(Value::is_array));
    if moves.is_empty() && present.is_none() {
        return Json(answer);
    }
    let caller = match authorize_bound(&app, &headers, &body, open) {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    if caller.read_only {
        let refused = json!({"ok": false, "error": "a viewer's agents record nothing"});
        if !moves.is_empty() {
            answer["moves"] = refused.clone();
        }
        if present.is_some() {
            answer["present"] = refused;
        }
        return Json(answer);
    }
    if !moves.is_empty() {
        let mut recorded = crate::moves::record_moves(&app.store, &caller.scope, &caller.user_id, &moves);
        if recorded.get("added").and_then(Value::as_u64).unwrap_or(0) > 0 {
            recorded["map_dropped"] = json!(crate::codegraph::forget_moved(&app.store, &caller.scope).len());
        }
        answer["moves"] = recorded;
    }
    if let Some(present) = present {
        let paths: std::collections::BTreeSet<String> = string_list(present, "paths").into_iter().collect();
        let now = crate::store::now();
        let head_ts = present.get("head_ts").and_then(Value::as_f64).unwrap_or(now).min(now);
        answer["present"] = crate::codegraph::forget_absent(&app.store, &caller.scope, &paths, head_ts, now);
    }
    Json(answer)
}

/// A hook fired that proves the hooks are loaded on this machine, without a
/// file having been written. Claude Code's ConfigChange is the case that
/// matters: the agent writes the hooks into settings.json, the harness
/// reloads them, and this is the first thing that runs. Before it existed the
/// setup nag cleared only on the first reported WRITE, so a session that
/// installed the hooks and then only read kept being told to install them.
async fn hook_alive_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    mark_hooks_seen(&app.store, &caller.scope, &caller.user_id);
    app.store.ledger_append_later(
        &caller.scope,
        "hooks_alive",
        &json!({"user": caller.user_id, "session": text(&body, "session"),
                "event": text(&body, "event"), "source": text(&body, "source")}),
        crate::store::now(),
    );
    Json(json!({"ok": true, "seen": true}))
}

async fn brief_outcome_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let n = |key: &str| body.get(key).and_then(Value::as_u64).unwrap_or(0).min(10_000);
    let big = |key: &str| body.get(key).and_then(Value::as_u64).unwrap_or(0).min(10_000_000_000);
    let session = text(&body, "session");
    let mut context = body.get("context_tokens").and_then(Value::as_u64).unwrap_or(0).min(10_000_000);
    let mut model = text(&body, "model");
    let mut effort = presence::Effort::of(&body);
    if context == 0 || model.is_empty() {
        // a hook that could not read its transcript: the session's newest
        // reported usage stands in, so a hit is never worth nothing by accident
        let (known_context, known_model, known_effort) =
            crate::briefstat::last_usage(&app.store, &caller.scope, &session, &caller.user_id);
        if context == 0 {
            context = known_context;
        }
        if model.is_empty() {
            model = known_model;
            effort = known_effort;
        }
    }
    let agent = {
        let named = text(&body, "agent");
        if named.is_empty() { "cork-report-hook".to_string() } else { named }
    };
    let outcome = crate::briefstat::Outcome {
        hits: n("hits"), misses: n("misses"), needed: n("needed"), found: n("found"), callers: n("callers"),
        context_tokens: context, model: &model, effort,
        brief_injected: big("brief_injected"), brief_carried: big("brief_carried"),
        read_avoided_carried: big("read_avoided_carried"), map_avoided_carried: big("map_avoided_carried"),
    };
    let answer = crate::briefstat::record(
        &app.store, &caller.scope, &caller.user_id, &agent, &session, &outcome, crate::store::now(),
    );
    // which briefed path settled how: the repo's next briefings follow it
    if let Some(paths) = body.get("paths") {
        crate::brieflearn::record(&app.store, &caller.scope, &session, paths);
    }
    Json(answer)
}

async fn brief_stats_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    Json(crate::briefstat::stats(&app.store, &caller.scope))
}

async fn observe_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(mut body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    // the workspace's switches, held to before anything is stored (privacy.rs)
    crate::privacy::enforce(&app.store, &caller.workspace, "/observe", &mut body);
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    // observing fills the map from a viewer's machine: a record, refused
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    // each file arrives as its content, or as the hook's own parse of it
    let mut files: Vec<(String, String, Option<Value>)> = Vec::new();
    let one = |item: &Value| -> Option<(String, String, Option<Value>)> {
        let path = item.get("path").and_then(Value::as_str)?.to_string();
        if let Some(structure) = item.get("structure") {
            return Some((path, String::new(), Some(structure.clone())));
        }
        Some((path, item.get("content").and_then(Value::as_str)?.to_string(), None))
    };
    if let Some(items) = body.get("files").and_then(Value::as_array) {
        files.extend(items.iter().take(200).filter_map(one));
    } else if let Some(file) = one(&body) {
        files.push(file);
    }
    let batch = body.get("files").is_some();
    if files.is_empty() {
        return Json(json!({"ok": false, "reason": "missing path or content"}));
    }
    billing::count_call(&app.store, &caller.workspace);
    mark_hooks_seen(&app.store, &caller.scope, &caller.user_id);
    note_worker(&app.store, &caller.scope, &caller.user_id, &body);

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut results: Vec<Value> = Vec::with_capacity(files.len());
    // parse and index the whole batch against one read of the graph
    let mut slots: Vec<Option<Value>> = Vec::with_capacity(files.len());
    let mut wanted: Vec<(&str, &str, Option<&Value>)> = Vec::new();
    for (path, content, structure) in &files {
        if path.is_empty() {
            continue;
        }
        if content.len() > 2_000_000 {
            slots.push(Some(json!({"path": path, "parse": "too large", "map": "skipped"})));
            continue;
        }
        wanted.push((path.as_str(), content.as_str(), structure.as_ref()));
        slots.push(None);
    }
    let mut indexed = report::observe_batch(&app.store, &caller.scope, &wanted, app.hot_ttl_s).into_iter();
    for slot in slots {
        let result = match slot {
            Some(skipped) => skipped,
            None => indexed.next().unwrap_or(Value::Null),
        };
        let outcome = result.get("map").and_then(Value::as_str).unwrap_or("skipped");
        *counts.entry(match outcome {
            "indexed" => "indexed", "unchanged" => "unchanged", "deferred" => "deferred", _ => "skipped",
        }).or_insert(0) += 1;
        results.push(result);
    }
    // any read is this agent's copy: one that already holds a change the gate
    // was blocking on clears that block (parsed only where a block could be)
    // and one file read is the agent's own copy, remembered so its next write
    // is told apart from a teammate's work it has not pulled
    // by agent: a helper's copy is its own (presence::agent_session)
    let reader = crate::presence::agent_session(&text(&body, "session"), &text(&body, "worker"));
    if !reader.is_empty() {
        for (path, content, structure) in &files {
            let blocking = crate::gate::has_blocking_marker(&app.store, &caller.scope, path);
            if !blocking && files.len() != 1 {
                continue;
            }
            if let Some(symbols) = report::symbol_signatures(path, content, structure.as_ref()) {
                if blocking {
                    crate::gate::acknowledge_read(&app.store, &caller.scope, &caller.user_id, &reader, path, &symbols);
                }
                if files.len() == 1 {
                    report::remember_copy(&app.store, &caller.scope, &caller.user_id, &reader, path, &symbols);
                }
            }
        }
    }
    let mut response = json!({
        "ok": true, "files": results,
        "indexed": counts.get("indexed").copied().unwrap_or(0),
        "unchanged": counts.get("unchanged").copied().unwrap_or(0),
        "deferred": counts.get("deferred").copied().unwrap_or(0),
        "skipped": counts.get("skipped").copied().unwrap_or(0),
    });
    if !batch {
        // the agent is reading a file another agent (or another session of
        // the same user) wrote moments ago: say so with the read, so the
        // edit that follows expects the file to move under it
        let session = text(&body, "session");
        crate::freshness::note_checkout(&app.store, &caller.scope, &caller.user_id, &reader, &text(&body, "checkout"));
        crate::freshness::note_ide(&app.store, &caller.scope, &caller.user_id, &session, &text(&body, "ide"), &text(&body, "ide_workspace"));
        if let Some((path, content, structure)) = files.first() {
            crate::deltas::touch(&app.store, &caller.scope, &reader, &[path.clone()], crate::store::now());
            // what this copy holds now: the gate and the notes compare against it
            let hashes: Vec<String> = body.get("line_hashes").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|h| h.as_str().map(str::to_string)).collect()).unwrap_or_default();
            let fp = if !hashes.is_empty() {
                crate::freshness::fingerprint(&hashes)
            } else if !content.is_empty() {
                crate::freshness::fingerprint_content(content)
            } else {
                String::new()
            };
            crate::freshness::saw(&app.store, &caller.scope, &caller.user_id, &reader, path, &fp, false);
            let _ = structure;
            let who = crate::check::attribute(
                &app.store, &caller.scope, &caller.user_id, &reader, &[path.clone()], &[], crate::store::now(),
            );
            if let Some(entry) = who.get("files").and_then(|f| f.get(path.as_str())) {
                let mine = entry.get("mine").and_then(Value::as_bool).unwrap_or(true);
                let age = entry.get("age_s").and_then(Value::as_f64).unwrap_or(f64::MAX);
                // once per write: Study-style runs showed the same "wrote it just
                // now" on every read of the file, turn after turn
                // the write itself, exactly: its time and fingerprint when the
                // write was recorded, else its age to the tenth of a second
                let write_id = crate::freshness::latest_write(&app.store, &caller.scope, &path)
                    .map(|(ts, h)| format!("{ts:.6}:{h}"))
                    .unwrap_or_else(|| format!("{:.1}", crate::store::now() - age));
                let told_key = format!("toldwrite:{}:{}:{}", caller.scope, reader, crate::codegraph::path_key(&path));
                let told = app.store.eph_get(&told_key).and_then(|v| v.as_str().map(str::to_string)).as_deref() == Some(write_id.as_str());
                if !mine && age <= 600.0 && !told {
                    let _ = app.store.eph_set(&told_key, &json!(write_id), Some(600.0));
                    // named the way the reader knows them: nickname, handle, or email
                    let mut entry = entry.clone();
                    let user = entry.get("user").and_then(Value::as_str).unwrap_or("").to_string();
                    let users: std::collections::BTreeSet<String> = [user.clone()].into_iter().filter(|u| !u.is_empty()).collect();
                    let labels = crate::activity::identity_labels(&app.store, &caller.user_id, &users);
                    if let Some(label) = labels.get(&user).and_then(|l| l.get("label")).and_then(Value::as_str) {
                        entry["who"] = json!(label);
                    }                    let view = crate::freshness::read_view(&app.store, &caller.scope, &caller.user_id, &reader, &path);
                    if let (Some(map), Some(v)) = (entry.as_object_mut(), view.as_object()) {
                        for (k, val) in v {
                            map.insert(k.clone(), val.clone());
                        }
                    }
                    if let Some(map) = response.as_object_mut() {
                        map.insert("recent_write".into(), entry);
                    }
                }
            }
        }
        let via = via_tag(&headers, "/observe");
        let presence = presence::report_presence(
            &app.store,
            &presence::PresenceInput {
                scope: &caller.scope,
                user_id: &caller.user_id,
                session: &session,
                path: &files[0].0,
                action: "reading",
                agent: &text(&body, "agent"),
                model: &text(&body, "model"),
                effort: presence::Effort::of(&body),
                branch: &text(&body, "branch"),
                tokens: number(&body, "tokens"),
                turn_id: &text(&body, "turn_id"),
                context: context_of(&body),
                via: &via,
                worker: &text(&body, "worker"),
            },
        );
        if let Some(map) = response.as_object_mut() {
            map.insert("presence".into(), presence);
        }
        if let Some(cost) = presence::step_usage(&body) {
            presence::note_exchange(&app.store, &caller.scope, &caller.user_id, &session, presence::Step::Current, &cost);
            crate::trail::note_cost(&app.store, &caller.scope, &session, &text(&body, "worker"), &cost, false);
        }
    }
    let visible = visible_for(&app, &caller);
    attach_messages(&app, &caller.scope, &caller.workspace, &caller.user_id, &caller.uid, &text(&body, "session"), &text(&body, "worker"), &visible, &step_msg(&body), &mut response);
    Json(response)
}

/// PostToolUse write side: the file's new content.
///
/// The write path and the awareness envelope are both here. Still missing
/// before this can be routed: the lint findings and autofix applier, and the
/// setup, artifact and rename notices. Compare-and-swap is deliberately
/// absent — the HTTP endpoint never took `expected_hashes`; that belongs to
/// the MCP `report_edit` tool and moves with it.
async fn report_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(mut body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_full(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    // the workspace's switches, held to before anything is stored (privacy.rs)
    crate::privacy::enforce(&app.store, &caller.workspace, "/report", &mut body);
    // a refused call is still a call: counted before the viewer and
    // validation checks
    billing::count_call(&app.store, &caller.workspace);
    // reporting is a RECORD: a viewer (effective access "read") is refused
    // in the same shape as any other access denial, with one plain line
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    crate::sharing::note(&app.store, &caller.workspace, &caller.user_id, &text(&body, "machine"), crate::store::now());
    watch::note_hook_repo(&app.store, &caller.uid, &caller.repo_id);
    let (scope, user_id) = (caller.scope.clone(), caller.user_id.clone());
    let member = auth::member(&app.store, &caller.workspace, &caller.uid);
    let path = text(&body, "path");
    // a native hook that parsed on its machine sends the structure, not the file
    let local = body.get("structure").is_some().then_some(&body);
    let Some(content) = body.get("content").and_then(Value::as_str).or(local.map(|_| "")) else {
        return Json(json!({"ok": false, "reason": "missing repo_id, path, or content"}));
    };
    if path.is_empty() || text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id, path, or content"}));
    }
    if content.len() > 2_000_000 {
        return Json(json!({"ok": false, "reason": "file too large"}));
    }
    let agent = {
        let named = text(&body, "agent");
        if named.is_empty() { "cork-report-hook".to_string() } else { named }
    };
    // the git origin disagreeing with the configured id IS a GitHub rename:
    // adopt it server-side and hand the new id back for the client to take up
    let renamed_to = rename::adopt_origin(
        &app.store, &app.aliases, &caller.workspace, &text(&body, "repo_id"),
        &text(&body, "origin"), &user_id, member.as_ref(),
    );
    // the first call IS the watch — and the (N+1)th repo is not bound, not
    // recorded, the human told: the payload itself
    if let Err(refused) = bind_scope(&app, &caller) {
        return Json(refused);
    }
    // a report the hook sends again from its spool is history, not presence:
    // recorded, but no worker marker, branch or step is refreshed by it, or a
    // subagent that ended hours ago comes back to life on the roster
    let resent = body.get("resent").and_then(Value::as_bool).unwrap_or(false);
    if !resent {
        note_worker(&app.store, &scope, &user_id, &body);
        crate::branches::note(&app.store, &scope, &user_id, &crate::presence::agent_session(&text(&body, "session"), &text(&body, "worker")),
            &text(&body, "branch"), &text(&body, "target"));
    }
    crate::examples::note_write(&app.store, &scope, &text(&body, "session"), &path);
    let response = report::report_edit(
        &app.store,
        &report::ReportInput {
            scope: &scope,
            user_id: &user_id,
            path: &path,
            content,
            local,
            agent: &agent,
            model: &text(&body, "model"),
            effort: presence::Effort::of(&body),
            branch: &text(&body, "branch"),
            session: &text(&body, "session"),
            tokens: number(&body, "tokens"),
            input_tokens: number(&body, "input_tokens"),
            output_tokens: number(&body, "output_tokens"),
            cache_read_tokens: number(&body, "cache_read_tokens"),
            cache_creation_tokens: number(&body, "cache_creation_tokens"),
            turn_id: &text(&body, "turn_id"),
            context: number(&body, "context"),
            why: &text(&body, "why"),
            hot_ttl_s: app.hot_ttl_s,
            via: &via_tag(&headers, "/report"),
            workspace: &caller.workspace,
            draft: body.get("draft").and_then(Value::as_bool).unwrap_or(false),
            auto: true,
            verified: body.get("verified").filter(|v| v.is_object()).cloned(),
            worker: &text(&body, "worker"),
            resent,
        },
    );
    mark_hooks_seen(&app.store, &scope, &user_id);
    first_agent_bonus(&app, &caller);
    // what this write's message cost, for the dashboard's line (never billed)
    if let Some(cost) = presence::step_usage(&body).filter(|_| !resent) {
        let kind = if body.get("draft").and_then(Value::as_bool).unwrap_or(false) { "draft" } else { "edit" };
        presence::note_exchange(&app.store, &scope, &user_id, &text(&body, "session"), presence::Step::Latest { kind, path: &path }, &cost);
    }
    // the session's first write ends its discovery (trail.rs)
    if !resent {
        crate::trail::note_cost(&app.store, &scope, &text(&body, "session"), &text(&body, "worker"), &Value::Null, true);
    }
    // activation: the auto-report hook is live for this person
    analytics::capture_once(
        &app.store, &caller.uid, &caller.uid, "hooks_installed", &caller.workspace,
        json!({"repo_id": text(&body, "repo_id"), "agent": agent, "hook_version": body.get("hook_version")}),
    );
    analytics::capture_daily(&app.store, &caller.uid, "agent_active", &caller.workspace, json!({"via": "report"}));
    // a write joins the writer's working set: later changes to it by others are its deltas
    crate::deltas::touch_as(&app.store, &scope, &crate::presence::agent_session(&text(&body, "session"), &text(&body, "worker")),
        &[path.clone()], true, crate::store::now());
    let mut answer = envelope::decorate(
        &app.store,
        &app.aliases,
        &envelope::EnvelopeInput {
            renamed_to: &renamed_to,
            scope: &scope,
            workspace: &caller.workspace,
            repo_id: &text(&body, "repo_id"),
            user_id: &user_id,
            hook_version: envelope::as_int(body.get("hook_version")),
            agents_version: envelope::as_int(body.get("agents_version")),
            settings_digest: body.get("settings_digest").and_then(Value::as_str).unwrap_or(""),
        },
        response,
    );
    // a resent report's answer is never read: the session's messages wait
    // for its next live post rather than being spent on this one
    if !resent {
        let visible = visible_for(&app, &caller);
        attach_messages(&app, &scope, &caller.workspace, &user_id, &caller.uid, &text(&body, "session"), &text(&body, "worker"), &visible, &step_msg(&body), &mut answer);
    }
    // the workspace's switches, for the hook to keep before its next send
    answer["privacy"] = crate::privacy::of(&app.store, &caller.workspace).to_json();
    // the repo's flaky tests, for the next check (flaky.rs); only once any were seen
    if let Some(listed) = crate::flaky::registry(&app.store, &scope, crate::store::now()) {
        answer["flaky"] = json!(listed);
    }
    Json(answer)
}

/// What another workspace changed under the code you are about to touch.
///
/// `check_collisions`' engine as an HTTP route; the MCP tool runs the same
/// engine.
///
/// Fails open: any trouble returns an empty result rather than an exception
/// into an agent's turn. A check that can break its caller is one agents learn
/// to skip, and the call is the whole product.
async fn check_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"collisions": [], "intents": []});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let strings = |key: &str| -> Vec<String> {
        body.get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let paths = strings("paths");
    let symbols = strings("symbols_referenced");
    let agent = text(&body, "agent");
    let session = text(&body, "session");
    let via = via_tag(&headers, "/check");

    // presence from any protocol call, so an agent appears the moment it does
    // anything rather than only after its first reported edit
    presence::touch_presence(&app.store, &caller.scope, &caller.user_id, &session, &agent, &via);
    let _ = app.store.ledger_append(
        &caller.scope,
        "check_performed",
        &json!({"user": caller.user_id, "session": session, "agent": agent,
                "paths": paths, "symbols": symbols}),
        store::now(),
    );

    let response = collisions::check_collisions(&app.store, &collisions::CheckInput {
        scope: &caller.scope,
        user_id: &caller.user_id,
        paths,
        symbols,
        idle_after_s: app.idle_after_s,
        visible: visible_for(&app, &caller),
    });
    Json(events::with_deltas(&app.store, &caller.scope, &caller.user_id, response))
}

/// Declare an intent: the paths and symbols an agent is about to change.
///
/// Like `/check`, the HTTP route for an engine the `declare_intent` MCP tool
/// also runs.
async fn declare_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let strings = |key: &str| -> Vec<String> {
        body.get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let via = via_tag(&headers, "/intent/declare");
    // presence from any protocol call, before the lease is even validated:
    // declaring seats the agent first
    presence::touch_presence(
        &app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "agent"), &via);
    Json(intents::declare(&app.store, &intents::DeclareInput {
        scope: &caller.scope,
        user_id: &caller.user_id,
        repo_id: &text(&body, "repo_id"),
        paths: strings("paths"),
        symbols: strings("symbols"),
        change_type: &text(&body, "change_type"),
        before: &text(&body, "before"),
        after: &text(&body, "after"),
        summary: &text(&body, "summary"),
        agent: &text(&body, "agent"),
        session: &text(&body, "session"),
        reference: &text(&body, "ref"),
        operations: body.get("operations").cloned(),
        idempotency_key: &text(&body, "idempotency_key"),
        ttl_s: app.intent_ttl_s,
        idle_after_s: app.idle_after_s,
        via: &via,
    }))
}

/// Renew an intent lease. The cheapest call in the protocol and the most
/// frequent, which is why it does nothing but read one key and write it back.
async fn heartbeat_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if caller.read_only {
        return viewer_refusal(&app, &caller, &json!({"ok": false}));
    }
    billing::count_call(&app.store, &caller.workspace);
    Json(intents::heartbeat(
        &app.store, &caller.scope, &caller.user_id,
        &text(&body, "intent_id"), app.intent_ttl_s,
    ))
}

/// Release an intent lease, optionally leaving the rationale behind as memory.
async fn complete_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if caller.read_only {
        return viewer_refusal(&app, &caller, &json!({"ok": false}));
    }
    billing::count_call(&app.store, &caller.workspace);
    Json(intents::complete(&app.store, &intents::CompleteInput {
        scope: &caller.scope,
        user_id: &caller.user_id,
        intent_id: &text(&body, "intent_id"),
        session: &text(&body, "session"),
        rationale: &text(&body, "rationale"),
        via: &via_tag(&headers, "/intent/complete"),
    }))
}

/// Leave a tripwire on work that is not finished.
async fn defer_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let strings = |key: &str| -> Vec<String> {
        body.get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    if caller.read_only {
        return viewer_refusal(&app, &caller, &json!({"ok": false}));
    }
    Json(intents::defer(
        &app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "agent"),
        &strings("paths"), &strings("symbols"), &text(&body, "note"),
        body.get("expires_days").and_then(Value::as_f64).unwrap_or(14.0),
    ))
}

/// Write one durable team fact, optionally anchored to the code it is about.
async fn remember_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let tags: Vec<String> = body
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
    if caller.read_only {
        return viewer_refusal(&app, &caller, &json!({"ok": false}));
    }
    if crate::decisions::wants_decision(&body) {
        let (decided, _notes) = crate::decisions::remember(
            &app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "worker"),
            &text(&body, "agent"), &body);
        return Json(decided);
    }
    Json(memory::save(&app.store, &memory::SaveInput {
        scope: &caller.scope,
        user_id: &caller.user_id,
        fact: &text(&body, "fact"),
        tags: &tag_refs,
        agent: &text(&body, "agent"),
        anchor: &text(&body, "anchor"),
        auto: &text(&body, "auto"),
        supersedes: &text(&body, "supersedes"),
    }))
}

/// Search the workspace's saved facts, annotated with honest staleness.
async fn recall_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"memories": [], "total_matched": 0})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let tags: Vec<String> = body
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let plan = access::plan_for(&app.store, &caller.workspace);
    Json(memory::recall(
        &app.store, &caller.workspace, &caller.uid, &caller.user_id, &caller.scope,
        &text(&body, "query"), &tags,
        body.get("limit").and_then(Value::as_i64).unwrap_or(25),
        access::oldest_ts(&plan), plan.flag("knowledge_sharing", true),
    ))
}

/// The whole briefing: who did what, renames to adapt to, hotspots, mined
/// scars, protocol health, and the repo map an agent reads instead of
/// grepping.
async fn briefing_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"summary": []})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let since_s = body
        .get("since_days")
        .and_then(Value::as_f64)
        .map(|days| days * 86_400.0)
        .unwrap_or(7.0 * 86_400.0);
    let plan = access::plan_for(&app.store, &caller.workspace);
    Json(briefing::activity(
        &app.store, &caller.scope, &text(&body, "repo_id"),
        access::clamp_since(since_s, plan.history_window_s()), app.idle_after_s,
        &caller.user_id, plan.flag("knowledge_sharing", true)))
}

/// Counterfactual: if these workspaces landed together right now, which
/// symbols would disagree — and which notes and settled decisions would that
/// disturb?
async fn simulate_merge_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"conflicts": []})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let users: Vec<String> = body
        .get("users")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    Json(merge::simulate_merge(&app.store, &caller.scope, &text(&body, "repo_id"), &users))
}

/// Where this map is unreliable, answered about itself.
async fn blind_spots_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"never_clean": []})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    Json(merge::blind_spots(
        &app.store, &caller.scope, &text(&body, "repo_id"),
        app.idle_after_s, app.hot_ttl_s,
    ))
}

/// Does my diff preserve the assumptions the other workspace's code relies
/// on? Interface oracle only, and it says so.
async fn differential_check_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    Json(merge::differential_check(
        &app.store, &caller.scope, &caller.user_id, &text(&body, "other_user"),
    ))
}

/// `CORK_CORS_ORIGINS`: a comma-separated list, or `*` (the default).
pub(crate) fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{AllowOrigin, Any, CorsLayer};
    let configured = crate::env::var("CORK_CORS_ORIGINS").unwrap_or_else(|_| "*".into());
    let origins: Vec<String> = configured
        .split(',')
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .map(str::to_string)
        .collect();
    let allow = if origins.is_empty() || origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(origins.iter().filter_map(|o| o.parse().ok()))
    };
    CorsLayer::new().allow_origin(allow).allow_methods(Any).allow_headers(Any)
}

/// Discovery, dynamic registration, authorize, token: the OAuth flow an
/// MCP client walks before it ever calls a tool. The free version has none:
/// its local server is reached with the token kept beside it.
#[cfg(feature = "cloud")]
fn oauth_router(app: &Arc<App>) -> Router {
    oauth::build_oauth_router(
        Arc::clone(&app.store), app.public_url.clone(),
        app.dashboard_url.clone(), app.mcp_url.clone(), app.dashboard_auth.clone(),
        oauth::agent_identity::issuers_from_env())
}

#[cfg(not(feature = "cloud"))]
fn oauth_router(_app: &Arc<App>) -> Router {
    Router::new()
}

/// MCP at `/mcp` on any host AND at the root — production's canonical
/// address is https://mcp.corkcli.com with no path, and that is what every
/// client config points at. One service, mounted twice, so a session opened
/// at either address is found at either. Both mounts sit behind the gate.
///
/// The first Rust deploy mounted only `/mcp`, and every client configured
/// with the root address got "no MCP endpoint found" — a full outage for
/// exactly the clients that had been working: this gate is not in a route
/// table, which is how it was missed.
fn mcp_mounts(app: Arc<App>) -> Router<Arc<App>> {
    let service = mcp_service(Arc::clone(&app));
    Router::new()
        .nest_service("/mcp", service.clone())
        .route_service("/", service)
        .route_layer(axum::middleware::from_fn_with_state(app, mcp_gate))
}

/// The address the caller reached us at, for the metadata pointer in a 401:
/// the configured public URL, else scheme and host off the request.
fn public_url_of(app: &App, headers: &HeaderMap) -> String {
    if !app.public_url.is_empty() {
        return app.public_url.clone();
    }
    let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("").trim();
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

/// The MCP auth gate.
///
/// Three jobs, in this order. A human or a probe opening the root — anything
/// that is not a JSON-RPC POST, an event-stream GET, or a session DELETE —
/// is sent to the website, or handed a JSON card when no dashboard is
/// configured. An MCP request with no valid workspace-bound credential gets
/// the RFC 9728 401, whose WWW-Authenticate header is how an OAuth-capable
/// client discovers where to log in; without it a new user's client simply
/// fails, with nothing to follow. Everything else carries the address it was
/// reached on into the tool layer as a header, because the mount hides it.
async fn mcp_gate(
    State(app): State<Arc<App>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = request.uri().path().to_string();
    let at_root = path.trim_matches('/').is_empty();
    let headers = request.headers().clone();
    let accept = headers.get("accept").and_then(|v| v.to_str().ok()).unwrap_or("");
    let method = request.method().clone();

    if at_root
        && method != axum::http::Method::POST
        && method != axum::http::Method::DELETE
        && !accept.contains("text/event-stream")
    {
        if !app.dashboard_url.is_empty() {
            return axum::response::Redirect::temporary(&app.dashboard_url).into_response();
        }
        let card = json!({
            "service": "cork",
            "mcp_endpoint": if app.mcp_url.is_empty() { "/mcp".to_string() } else { app.mcp_url.clone() },
            "health": "/health",
        });
        return Json(card).into_response();
    }

    let authorization = headers.get("authorization").and_then(|v| v.to_str().ok());
    // an unclaimed anonymous agent's sandbox token is good here, on MCP,
    // and nowhere else (`authorize_sandbox` holds it to its sandbox)
    let principal = auth::principal_for_bearer(&app.store, authorization, store::now())
        .or_else(|| auth::sandbox_principal(&app.store, authorization, store::now()));
    if principal.as_ref().map(|p| p.workspace.is_empty()).unwrap_or(true) {
        let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
        let root_host = at_root && oauth::mcp_at_root(&app.mcp_url, host);
        // the metadata of the host the client asked: a challenge on api.
        // that named mcp.'s document described a different resource, and a
        // strict client (Codex) refused it and never signed in
        let configured = public_url_of(&app, &headers);
        let scheme = configured.split("://").next().unwrap_or("https");
        let asked = if host.trim().is_empty() { configured.clone() } else { format!("{scheme}://{}", host.trim()) };
        let (status, parts, body) = oauth::unauthorized_mcp_response(&asked, root_host);
        let mut response = axum::response::Response::builder().status(status);
        for (name, value) in parts {
            response = response.header(name, value);
        }
        return response
            .body(axum::body::Body::from(body))
            .unwrap_or_else(|_| axum::http::StatusCode::UNAUTHORIZED.into_response());
    }

    let mut request = request;
    let via = format!(
        "{} {}",
        headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or(""),
        if path.is_empty() { "/" } else { path.as_str() }
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&via) {
        request.headers_mut().insert("x-collide-via", value);
    }
    next.run(request).await
}

/// The MCP endpoint, wired to the same `App` every HTTP route uses so a tool
/// call and its equivalent POST run identical code against identical state.
///
/// Sessions are kept in memory. That is correct for a single process and
/// wrong for several behind a load balancer.
fn mcp_service(
    app: Arc<App>,
) -> StreamableHttpService<mcp::CorkMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(mcp::CorkMcp { app: Arc::clone(&app) }),
        Arc::new(LocalSessionManager::default()),
        // The transport's DNS-rebinding guard admits only localhost by
        // default. That guard protects servers running on a developer's own
        // machine; this one sits behind a proxy under its public hostname,
        // where the default answered every real client with 403.
        StreamableHttpServerConfig::default().disable_allowed_hosts(),
    )
}

/// The dashboard's graph: structure plus every overlay, and the repo map.
async fn graph_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"nodes": [], "edges": []})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let limit = body.get("limit").and_then(Value::as_u64).unwrap_or(600).clamp(50, 2000) as usize;
    Json(graphview::graph_view(
        &app.store, &caller.scope, &text(&body, "repo_id"), app.idle_after_s, limit,
    ))
}

/// The wake client's directed-message poll: pending messages for the
/// credential's user, acking the ids it has already handled. Same hook-token
/// auth as /report; fails open.
/// An agent tells the others something, from its shell (`collide-hook
/// message`): the free version has no MCP tools, and every agent can run a
/// command. See [`local::send`].
async fn message_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if caller.read_only {
        return Json(json!({"ok": false, "error": "a viewer's agents send nothing"}));
    }
    Json(local::send(&app.store, &caller.scope, &caller.repo_id, &caller.workspace, &caller.user_id, &body, app.idle_after_s))
}

/// A session's start takes the handoffs waiting for its person in this repo
/// (sent while no other session of theirs was open; see [`local::handoff`]).
async fn handoffs_take_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false, "handoffs": []});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if caller.read_only {
        return Json(open);
    }
    let taken = local::take_handoffs(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"));
    Json(json!({"ok": true, "handoffs": taken}))
}

/// `cork today`: the person's day across the workspace's repos since the
/// moment they name (their local midnight), as numbers and plain lines
/// (see [`today::day`]).
async fn today_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_full(&app, &headers, &json!({}), json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let at = crate::store::now();
    let since = body.get("since").and_then(Value::as_f64).unwrap_or(at - 86_400.0).clamp(at - 2.0 * 86_400.0, at);
    let scopes = app.store.list_scopes(&format!("{}:", caller.workspace));
    let viewer = crate::today::Viewer { user: &caller.user_id, local: local::active() };
    let mut answer = crate::today::day(&app.store, &scopes, viewer, since, at);
    answer["ok"] = json!(true);
    Json(answer)
}

/// What Cork did on this machine since `since` (see [`local::summary`]).
/// A local server's own credential only: the cloud has a dashboard instead.
async fn local_summary_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    if !local::active() {
        return Json(json!({"ok": false, "reason": "not a local server"}));
    }
    let open = json!({"ok": false});
    let caller = match authorize_full(&app, &headers, &json!({}), open) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let since = body.get("since").and_then(Value::as_f64).unwrap_or(crate::store::now() - 86_400.0);
    let mut answer = local::summary(&app.store, &caller.workspace, since);
    answer["ok"] = json!(true);
    Json(answer)
}

/// One batch of this machine's record, for `cork sync` to send on
/// (see [`local::export`]).
async fn local_export_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    if !local::active() {
        return Json(json!({"ok": false, "reason": "not a local server"}));
    }
    let caller = match authorize_full(&app, &headers, &json!({}), json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let cursor = body.get("cursor").cloned().unwrap_or_else(|| json!({}));
    Json(local::export(&app.store, &caller.workspace, &cursor, 2_000))
}

/// The person who may connect or sync a machine: the credential's owner,
/// signed in, a player in the workspace (a viewer's agents record nothing).
fn machine_caller(app: &App, headers: &HeaderMap) -> Result<Caller, Json<Value>> {
    let caller = authorize_full(app, headers, &json!({}), json!({"ok": false}))?;
    if caller.read_only {
        return Err(Json(json!({"ok": false, "error": "a viewer's machine stays local: on Free only the owner's agents join the workspace; with units every member's do, so ask the owner to add units"})));
    }
    Ok(caller)
}

/// `cork login` on a machine running the free version: the
/// machine is recorded under the workspace the sign-in chose, a hook
/// credential is minted for it, and the machine moves to Cork's servers and
/// syncs, on any plan (Free is metered in agent hours). See [`machines`].
async fn machine_connect_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match machine_caller(&app, &headers) {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    if text(&body, "machine").is_empty() {
        return Json(json!({"ok": false, "error": "missing machine"}));
    }
    machines::record(&app.store, &caller.workspace, &caller.uid, &caller.email, &body);
    let identity = crate::blocks::HookIdentity {
        auth_uid: &caller.uid, email: &caller.email, name: &caller.name, workspace: &caller.workspace,
        workspaces: &caller.workspaces,
    };
    let credentials = crate::blocks::mint_hook_credential(&app.store, &identity, "collide-machine", &app.public_url);
    let workspace_name = crate::workspaces::get(&app.store, &caller.workspace)
        .and_then(|w| w.get("name").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    let mut answer = machines::plan_answer(&app.store, &caller.workspace);
    answer["ok"] = json!(true);
    answer["credentials"] = credentials;
    answer["user"] = json!(caller.email);
    answer["workspace"] = json!(caller.workspace);
    answer["workspace_name"] = json!(workspace_name);
    answer["server_url"] = json!(app.public_url.trim_end_matches('/'));
    answer["dashboard_url"] = json!(app.dashboard_url);
    Json(answer)
}

/// A signed-in machine asks, now and then, about its workspace's plan; its
/// local stats ride along for the dashboard.
async fn machine_status_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match machine_caller(&app, &headers) {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    if !text(&body, "machine").is_empty() {
        machines::record(&app.store, &caller.workspace, &caller.uid, &caller.email, &body);
    }
    let mut answer = machines::plan_answer(&app.store, &caller.workspace);
    // the subscription's hours, for `cork status`
    answer["hours"] = machines::hours_answer(&app.store, &caller.workspace, &caller.uid);
    answer["ok"] = json!(true);
    Json(answer)
}

/// `cork add`: the person said yes, this repo joins the workspace. The
/// explicit way in from a terminal, as `setup` is from the MCP tools.
async fn repo_add_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_full(&app, &headers, &body, open.clone()) {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    if caller.repo_id.trim().is_empty() {
        return Json(json!({"ok": false, "error": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    if let Err(refused) = bind_scope_as(&app, &caller, true) {
        return Json(refused);
    }
    let workspace_name = crate::workspaces::get(&app.store, &caller.workspace)
        .and_then(|w| w.get("name").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    Json(json!({"ok": true, "repo_id": caller.repo_id, "workspace": caller.workspace, "workspace_name": workspace_name}))
}

/// `cork install --claim CODE`: see [`machines::claim`].
async fn machine_claim_endpoint(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Json<Value> {
    Json(machines::claim(&app.store, &app.public_url, &body))
}

/// The free version's install line for the website: no account, no code.
async fn free_install_endpoint(State(app): State<Arc<App>>) -> Json<Value> {
    let server = if app.public_url.is_empty() { "https://mcp.corkcli.com".to_string() } else { app.public_url.clone() };
    match machines::free_install(server.trim_end_matches('/')) {
        Some((version, command)) => Json(json!({"ok": true, "version": version, "command": command})),
        None => Json(json!({"ok": false, "error": "no builds of the free version on this server yet"})),
    }
}

/// The script behind `curl -fsSL https://corkcli.com/install.sh | sh`.
async fn install_script_endpoint(State(app): State<Arc<App>>, headers: HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;
    // the status page's probe (status.rs, and the website's) is not an install
    if !headers.contains_key(crate::status::PROBE_HEADER) {
        downloads::record(&app.store, "script", "", &downloads::client_ip(&headers));
    }
    let server = if app.public_url.is_empty() { "https://mcp.corkcli.com".to_string() } else { app.public_url.clone() };
    match machines::free_script_now(server.trim_end_matches('/')).await {
        Some(script) => ([(axum::http::header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")], script).into_response(),
        None => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "#!/bin/sh\necho \"Cork: the builds are being published. Try again in a few minutes.\"\nexit 1\n",
        )
            .into_response(),
    }
}

/// One batch of a machine's local record, into its paid workspace.
async fn machine_import_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match machine_caller(&app, &headers) {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    Json(machines::import(&app.store, &app.aliases, &caller.workspace, &caller.uid, &caller.workspaces, &caller.email, &body))
}

async fn inbox_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_full(&app, &headers, &body, open) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    // a refused call is still a call: counted before any refusal
    billing::count_call(&app.store, &caller.workspace);
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    // the first call IS the watch; the (N+1)th repo is not bound and its
    // box is not read — the flag and the line, as `authorize_bound` shapes it
    if let Err(refused) = bind_scope(&app, &caller) {
        return Json(json!({"ok": false, "workspace_full": true,
                           "reason": refused.get("error").cloned().unwrap_or(Value::Null)}));
    }
    let ack: Vec<String> = body
        .get("ack")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str()).filter(|s| !s.is_empty())
            .map(str::to_string).collect())
        .unwrap_or_default();
    let inbox = agenttools::inbox_ack(
        &app.store, &caller.scope, &caller.user_id, &visible_for(&app, &caller), &ack);
    let inbox = access::own_messages_only(&app.store, &caller.workspace, &caller.user_id, inbox, &app.dashboard_url);
    agenttools::mark_delivered(
        &app.store, inbox.get("messages").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]), &text(&body, "session"));
    Json(inbox)
}

/// `POST /inbox/wait {repo_id?, session?, role, wait_s?, repos?}`: held until
/// a message is there for the caller, or `wait_s` (at most 25) passes; then
/// the messages this caller takes (`push::take`). `role: "session"` is an
/// open session's listener, in `repo_id`; `role: "wake"` is the machine's
/// wake watcher, with `repos` its checkouts (the ids as the machine spells
/// them). Nothing is acked here: an agent acks what it handled, as before.
async fn inbox_wait_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false, "messages": []});
    let caller = match authorize_full(&app, &headers, &body, open) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if local::active() {
        // one machine: its agents hear each other on every hook call
        return Json(json!({"ok": false, "reason": "local", "messages": []}));
    }
    let taker = push::Taker::parse(&text(&body, "role"));
    let session = text(&body, "session");
    let scope = if taker == push::Taker::Session && !caller.repo_id.trim().is_empty() {
        caller.scope.clone()
    } else {
        String::new()
    };
    let checkouts: Vec<(String, String)> = body
        .get("repos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(|id| (app.aliases.scope_for(&caller.workspace, id), id.to_string()))
        .collect();
    // an open session waiting here takes a handoff at once (local::handoff)
    if taker == push::Taker::Session {
        local::note_listening(&app.store, &scope, &caller.user_id, &session);
    }
    let wait = body.get("wait_s").and_then(Value::as_f64).unwrap_or(push::MAX_WAIT_S).clamp(0.0, push::MAX_WAIT_S);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(wait);
    loop {
        // armed before the look, so a message filed during it still wakes us
        let arrived = push::arrivals().notified();
        let visible = visible_for(&app, &caller);
        let ask = push::Ask {
            taker, workspace: &caller.workspace, user_id: &caller.user_id, scope: &scope,
            session: &session, visible: &visible, checkouts: &checkouts,
        };
        let mut taken = if taker == push::Taker::Session {
            push::take_for_session(&app.store, &scope, &caller.user_id, &session)
        } else {
            Vec::new()
        };
        // hub messages: an open session's listener takes its own at once
        if taker == push::Taker::Session {
            taken.extend(hubs::take_for_session(&app.store, &caller.uid, &session));
        }
        taken.extend(push::take(&app.store, &ask, |candidates| {
            let inbox = access::own_messages_only(
                &app.store, &caller.workspace, &caller.user_id, json!({"messages": candidates}), &app.dashboard_url);
            inbox.get("messages").and_then(Value::as_array).cloned().unwrap_or_default()
        }));
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if !taken.is_empty() || left.is_zero() {
            let senders: Vec<String> = taken.iter().map(|m| text(m, "from")).collect();
            let names = access::display_names(&app.store, &caller.user_id, &senders);
            for (message, name) in taken.iter_mut().zip(names) {
                message["from_name"] = json!(name);
            }
            // pushed into the listening session (or to the wake watcher,
            // which starts an agent on it): delivered
            agenttools::mark_delivered(&app.store, &taken, &session);
            return Json(json!({"ok": true, "messages": taken}));
        }
        // a grace running out is a reason to look again too
        let _ = tokio::time::timeout(left.min(std::time::Duration::from_secs(5)), arrived).await;
    }
}

/// The dashboard's live feed: one scope's events as they happen. Refused
/// with close codes — 4401 for no credential,
/// 4403 for the wrong workspace or no access — before the socket is accepted.
async fn ws_feed(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(app): State<Arc<App>>,
    axum::extract::Path(repo_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let repo_id = repo_id.trim_start_matches('/').to_string();
    let token = text(&query, "token");
    let mut workspace_id = text(&query, "workspace");
    let principal = app.dashboard_auth.principal_for(&app.store, &token, store::now());
    let mut refusal: Option<u16> = None;
    let principal = match principal {
        None => { refusal = Some(4401); None }
        Some(principal) => {
            if !principal.workspace.is_empty() {
                if workspace_id.is_empty() {
                    workspace_id = principal.workspace.clone();
                }
                if workspace_id != principal.workspace {
                    refusal = Some(4403);
                }
            }
            Some(principal)
        }
    };
    if refusal.is_none() {
        let member = principal.as_ref().and_then(|p| {
            (!workspace_id.is_empty()).then(|| auth::member(&app.store, &workspace_id, &p.uid)).flatten()
        });
        if !access::has_repo_access(member.as_ref(), &repo_id) {
            refusal = Some(4403);
        }
    }
    let scope = app.aliases.scope_for(&workspace_id, &repo_id);
    // a viewer's live feed carries who and where, never the code (as the
    // dashboard routes do, routes_observe::redact_for_viewer)
    let viewer = refusal.is_none()
        && principal.as_ref().and_then(|p| auth::member(&app.store, &workspace_id, &p.uid)).is_some_and(|member| {
            billing::effective_access(&billing::plan_of(&app.store, &workspace_id), &member) == "read"
        });
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::{CloseFrame, Message};
        if let Some(code) = refusal {
            let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: "".into() }))).await;
            return;
        }
        let mut feed = app.store.subscribe(&scope);
        loop {
            match feed.recv().await {
                Ok(event) => {
                    let mut event = serde_json::to_value(&event).unwrap_or(Value::Null);
                    if viewer {
                        crate::access::redact_for_viewer(&mut event);
                    }
                    let Ok(text) = serde_json::to_string(&event) else { continue };
                    if socket.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    })
    .into_response()
}

/// Pure functions, exposed (`collide-server compute`, JSON on stdin) so tests
/// can feed them input directly. These are the pieces where "reasonable but
/// different" is a bug: hunks land in the ledger and on change pages, and the
/// parse state machine decides what counts as a removal.
fn compute(input: &str) -> i32 {
    let Ok(request) = serde_json::from_str::<Value>(input) else {
        eprintln!("compute: stdin was not JSON");
        return 1;
    };
    let strings = |key: &str| -> Vec<String> {
        request
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect()
            })
            .unwrap_or_default()
    };
    let output = match request.get("op").and_then(Value::as_str).unwrap_or("") {
        "line_hashes" => {
            let lines = strings("lines");
            let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
            json!(diff::line_hashes(&refs))
        }
        "hunks" => {
            let hunks = diff::hunks(&strings("old"), &strings("new"));
            let (added, removed) = diff::hunk_totals(&hunks);
            json!({"hunks": hunks, "lines_added": added, "lines_removed": removed})
        }
        "apply_parse" => {
            let record = request.get("record").filter(|v| !v.is_null());
            let symbols = request
                .get("symbols")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let status = request.get("status").and_then(Value::as_str).unwrap_or("clean");
            let now = request.get("now").and_then(Value::as_f64).unwrap_or(0.0);
            let (record, events) = semantics::apply_parse(record, status, symbols, now);
            json!({"record": record, "events": events})
        }
        "serve" => {
            let record = request.get("record").filter(|v| !v.is_null());
            let now = request.get("now").and_then(Value::as_f64).unwrap_or(0.0);
            match semantics::serve(record, now) {
                Some(served) => json!({
                    "symbols": served.symbols,
                    "freshness": served.freshness,
                    "age_s": served.age_s,
                }),
                None => Value::Null,
            }
        }
        other => {
            eprintln!("compute: unknown op {other:?}");
            return 1;
        }
    };
    println!("{}", serde_json::to_string(&output).unwrap_or_default());
    0
}

/// Recompute the stored hashes over whatever the database already holds,
/// and re-verify the ledger chain link by link. Prints JSON; tests run it
/// against a database they wrote.
fn parity(path: &PathBuf) -> i32 {
    let Ok(store) = Store::open(path) else {
        eprintln!("parity: cannot open {}", path.display());
        return 1;
    };
    let mut report = json!({
        "engine": "rust",
        "empty_root": hashing::empty_root(),
        "chain_hash_of_known_input": hashing::chain_hash(
            store::GENESIS,
            "ws:github.com/acme/api",
            1789646694.5665562,
            "edit_reported",
            &json!({"user": "alice", "path": "a.py", "lines_added": 3}),
        ),
    });

    // a merkle tree over a fixed file set, and the canonical JSON forms
    let mut files = BTreeMap::new();
    files.insert("pkg/a.py".to_string(), "aaa".to_string());
    files.insert("pkg/b.py".to_string(), "bbb".to_string());
    files.insert("top.py".to_string(), "ccc".to_string());
    let tree = hashing::Tree::build(files);

    let mut symbols = BTreeMap::new();
    symbols.insert("compute_tax".to_string(), "h1".to_string());
    symbols.insert("Ledger".to_string(), "h2".to_string());

    let sample = json!({"b": 2, "a": [1, {"d": 4, "c": 3}], "e": "x"});
    if let Some(map) = report.as_object_mut() {
        map.insert("merkle_root".into(), json!(tree.root));
        map.insert("merkle_dirs".into(), json!(tree.dirs));
        map.insert("file_hash".into(), json!(hashing::file_hash(&symbols)));
        map.insert("canonical_json".into(), json!(hashing::canonical_json(&sample)));
        // the encoding a ledger row is STORED in: `", "` / `": "` separators
        // with keys sorted (the formatter alone does not sort)
        map.insert("python_dumps".into(), json!(store::canonical_sorted(&sample)));
        // Non-ASCII is the case that actually broke a chain: the stored
        // encoding escapes it as \uXXXX and serde emits raw UTF-8. An em
        // dash in an agent's summary was enough.
        let unicode_sample = json!({
            "summary": "renamed get_symbol \u{2014} see notes",
            "path": "src/caf\u{e9}.py",
            "emoji": "\u{1f680}",
            "control": "a\tb\nc",
        });
        map.insert("canonical_json_unicode".into(),
                   json!(hashing::canonical_json(&unicode_sample)));
        map.insert("python_dumps_unicode".into(),
                   json!(store::canonical_sorted(&unicode_sample)));
        // and re-verify the chain, link by link
        let (chain_ok, bad_seq, rows) = store.ledger_verify();
        map.insert("ledger_rows_in_db".into(), json!(rows));
        map.insert("chain_ok".into(), json!(chain_ok));
        map.insert("chain_bad_seq".into(), json!(bad_seq));
    }
    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    0
}

/// Handlers call the store directly, and a store call can wait on the
/// writer: with one worker per core, a handful of waiting handlers stalled
/// every other request and kept write batches tiny. Eight per core went too
/// far the other way on staging (192 threads on 24 cores queued for CPU).
/// Two per core, 16 to 64, measured best; CORK_WORKERS overrides.
/// Every route this server answers, behind the layers every answer passes
/// through, and the REST contract around them (restapi.rs). One builder for
/// the server and for the tests that drive it end to end.
pub(crate) fn build_router(app: &Arc<App>) -> Router {
    let router = Router::new()
        .route("/health", get(health))
        .route("/gate", post(gate_endpoint))
        .route("/presence", post(presence_endpoint))
        .route("/report", post(report_endpoint))
        .route("/observe", post(observe_endpoint))
        .route("/brief", post(brief_endpoint))
        .route("/brief/touch", post(touch_brief_endpoint))
        .route("/hook-alive", post(hook_alive_endpoint))
        .route("/brief/outcome", post(brief_outcome_endpoint))
        .route("/brief/stats", post(brief_stats_endpoint))
        .route("/renames", post(renames_endpoint))
        .route("/verified", post(verified_endpoint))
        .route("/verify", post(verify_endpoint))
        .route("/attribute", post(attribute_endpoint))
        .route("/flaky", post(flaky_endpoint))
        .route("/deltas", post(deltas_endpoint))
        .route("/land", post(land_endpoint))
        .route("/trunk", post(trunk_endpoint))
        .route("/apply/done", post(apply_done_endpoint))
        .route("/recipe", post(recipe_save_endpoint))
        .route("/recipe/get", post(recipe_get_endpoint))
        .route("/callers", post(callers_endpoint))
        .route("/check", post(check_endpoint))
        .route("/intent/declare", post(declare_endpoint))
        .route("/intent/heartbeat", post(heartbeat_endpoint))
        .route("/intent/complete", post(complete_endpoint))
        .route("/intent/defer", post(defer_endpoint))
        .route("/memory/save", post(remember_endpoint))
        .route("/memory/recall", post(recall_endpoint))
        .route("/briefing", post(briefing_endpoint))
        .route("/simulate-merge", post(simulate_merge_endpoint))
        .route("/blind-spots", post(blind_spots_endpoint))
        .route("/differential-check", post(differential_check_endpoint))
        .route("/graph", post(graph_endpoint))
        .route("/inbox", post(inbox_endpoint))
        .route("/inbox/wait", post(inbox_wait_endpoint))
        .route("/message", post(message_endpoint))
        .route("/handoffs/take", post(handoffs_take_endpoint))
        .route("/local/summary", post(local_summary_endpoint))
        .route("/today", post(today_endpoint))
        .route("/local/export", post(local_export_endpoint))
        .route("/machine/connect", post(machine_connect_endpoint))
        .route("/machine/status", post(machine_status_endpoint))
        .route("/machine/import", post(machine_import_endpoint))
        .route("/machine/claim", post(machine_claim_endpoint))
        .route("/repo/add", post(repo_add_endpoint))
        .route("/artifacts/free", get(free_install_endpoint))
        .route("/install.sh", get(install_script_endpoint))
        .route("/ws/*repo_id", get(ws_feed))
        // the request's address, for every row these handlers write
        .route_layer(axum::middleware::from_fn(via_scope))
        // MCP over Streamable HTTP, at /mcp.
        // This is the surface agents connect to; everything above it is the
        // engine those tools dispatch into, and the hooks' own fast paths.
        .merge(mcp_mounts(Arc::clone(app)))
        // the dashboard's read side: activity, ledger, graph, journal, why,
        // costs, compliance, briefing, search
        .merge(routes_observe::router())
        .merge(billing::router())
        .merge(admin::router())
        .merge(routes_workspaces::router())
        .merge(github_app::router())
        .merge(routes_account::router())
        .merge(hubs::router())
        .merge(channels::router())
        // email settings, unsubscribe and approve links, the dry run's outbox
        .merge(notify::router())
        // unread counts for the dashboard's switchers
        .merge(unread::router())
        // the status page's JSON (/api/status) and its incidents
        .merge(status::router())
        // the real-time protocol: its WebSocket and the dashboard's read
        .merge(protocol_ws::router())
        // the MCP manifest and server card, served from the tool table
        .merge(restapi::router())
        // axum caps request bodies at 2MB by default, which would reject an
        // oversized report with a bare 413 before the handler could answer
        // with its JSON. The real limit is the
        // handler's own check.
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(Arc::clone(app))
        // discovery, dynamic registration, authorize, token: the OAuth flow
        // an MCP client walks before it ever calls a tool. Merged after the
        // state is applied because it carries its own.
        .merge(oauth_router(app))
        // A panic inside a handler used to drop the connection with no
        // response at all — which the dashboard experienced as loading
        // forever. Now it is a 500 with a log line, which is at least a
        // fact someone can act on.
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        // every route: an answer leaves once the log holds what it wrote
        .layer(axum::middleware::from_fn_with_state(Arc::clone(app), durable));
    // The REST contract around all of it: /api/v1, rate-limit headers and
    // 429s, Idempotency-Key, async jobs (restapi.rs). And CORS outermost:
    // the dashboard calls /api/* and /ws/* from the browser on another
    // origin, and without it every one of those calls is refused before it
    // reaches a handler. Origins from the env, any method, any header, no
    // credentials.
    restapi::wrap(Arc::clone(app), router)
}

fn main() {
    // the free version's `cork` program: the hooks, unless asked to be
    // the local server (or one of the server's own tools)
    #[cfg(feature = "cli")]
    {
        let first = std::env::args().nth(1);
        if !matches!(first.as_deref(), Some("local" | "compute" | "embedder" | "parity")) {
            cork_hooks::machine::set_serves_local();
            cork_hooks::run(std::env::args().skip(1).collect());
        }
    }
    // the free version, on the developer's machine: its environment is set
    // before the runtime starts and before anything reads it
    if std::env::args().nth(1).as_deref() == Some("local") {
        local::prepare_env();
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let workers = crate::env::var("CORK_WORKERS").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or((cores * 2).clamp(16, 64)).clamp(2, 512);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(serve_main());
}

async fn serve_main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let data_dir = crate::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    let db_path = PathBuf::from(&data_dir).join("collide.db");

    if args.first().map(String::as_str) == Some("compute") {
        use std::io::Read;

        let mut input = String::new();
        let _ = std::io::stdin().read_to_string(&mut input);
        std::process::exit(compute(&input));
    }

    if args.first().map(String::as_str) == Some("embedder") {
        crate::embed::serve_embedder().await;
        return;
    }

    if args.first().map(String::as_str) == Some("parity") {
        let target = args.get(1).map(PathBuf::from).unwrap_or(db_path);
        std::process::exit(parity(&target));
    }

    // local: a port on 127.0.0.1 chosen now, so the server's own address is known
    let local_port = if local::active() {
        let asked = args.iter().position(|a| a == "--port").and_then(|i| args.get(i + 1)).and_then(|p| p.parse().ok());
        match local::pick_port(asked) {
            Ok(port) => {
                crate::env::set("CORK_PUBLIC_URL", format!("http://127.0.0.1:{port}"));
                Some(port)
            }
            Err(said) => {
                // another local Cork is already serving: nothing to do
                eprintln!("cork local: {said}");
                std::process::exit(0);
            }
        }
    } else {
        None
    };
    // an explicit MCP address wins,
    // else a host that is itself `mcp.` serves MCP at its root, else `/mcp`
    let public_url = crate::env::var("CORK_PUBLIC_URL").unwrap_or_default()
        .trim().trim_end_matches('/').to_string();
    let mcp_url = {
        let explicit = crate::env::var("CORK_MCP_URL").unwrap_or_default()
            .trim().trim_end_matches('/').to_string();
        if !explicit.is_empty() || public_url.is_empty() {
            explicit
        } else {
            let host = public_url.split("://").nth(1).unwrap_or("").split('/').next()
                .unwrap_or("").to_lowercase();
            if host.starts_with("mcp.") { public_url.clone() } else { format!("{public_url}/mcp") }
        }
    };
    restore_if_staged(&db_path);
    // with an engine the database lives on local disk and is made current
    // from the snapshot and the log before anything opens it
    let db_path = match crate::env::var("CORK_DB_PATH").ok().filter(|v| !v.trim().is_empty()) {
        Some(local) if crate::engine::configured().is_some() => std::path::PathBuf::from(local.trim()),
        _ => db_path,
    };
    if crate::engine::configured().is_some() {
        let seed = std::path::PathBuf::from(&data_dir).join("collide.db");
        let seed = (seed != db_path).then_some(seed);
        // the Postgres client blocks: off the async runtime
        let (target, seed_path) = (db_path.clone(), seed.clone());
        let prepared = std::thread::spawn(move || crate::engine::prepare(&target, seed_path.as_deref()))
            .join()
            .unwrap_or_else(|_| Err("the restore thread panicked".into()));
        match prepared {
            Ok(said) => tracing::info!("engine: {said}"),
            Err(error) => {
                eprintln!("engine: cannot make the database current: {error}");
                std::process::exit(1);
            }
        }
    }
    let store = match Store::open(&db_path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("cannot open {}: {error}", db_path.display());
            std::process::exit(1);
        }
    };
    if local::active() {
        let workspace = local::bootstrap(&store);
        tracing::info!("cork local: workspace {workspace}, data in {}", local::dir().display());
    }
    // before the first request: existing accounts are not new signups
    analytics::backfill(&store);
    // seed the alias registry from scopes that already carry data, so a
    // friendly short name resolves onto the existing scope rather than
    // minting an empty one on the first call
    let aliases = Aliases::new();
    aliases.seed(&store.list_scopes(""));
    for (key, record) in store.kv_list("repo_alias", "") {
        let Some((workspace_id, from)) = key.split_once(':') else { continue };
        if let Some(canonical) = record.get("canonical").and_then(Value::as_str) {
            aliases.add_rename(workspace_id, from, canonical);
        }
    }

    // warm the Firebase key cache before the first dashboard request lands
    // (a local server has no dashboard and calls nothing out)
    if !local::active() {
        auth::spawn_firebase_key_refresh();
    }
    let store = Arc::new(store);
    let dashboard_url = crate::env::var("CORK_DASHBOARD_URL").unwrap_or_default();
    // the protocol's op log, replayed before the first connection
    let protocol = protocol_ws::Hub::open(Arc::clone(&store), &dashboard_url);
    // the waste report's newer counters start now on this database
    crate::outcomes::mark_instrumented(&store);
    let app = Arc::new(App {
        store,
        protocol,
        aliases,
        started: std::time::Instant::now(),
        dashboard_url,
        public_url: public_url.clone(),
        mcp_url,
        dashboard_auth: auth::DashboardAuth::from_env(),
        hot_ttl_s: crate::env::var("CORK_HOT_TTL_S")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(900.0),
        idle_after_s: crate::env::var("CORK_IDLE_AFTER_S")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(600.0),
        intent_ttl_s: crate::env::var("CORK_INTENT_TTL_S")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(900.0),
    });
    let router = build_router(&app);

    // the compiled hooks this server hands out: pulled from the GitHub
    // release for this hook version at boot and hourly, so a fresh container
    // serves them within seconds and a version bump refreshes them by itself.
    // Off (and silent) unless a token or CORK_HOOK_SYNC=1 says otherwise.
    //
    // A push deploys this server and starts the CI job that publishes the
    // release, and the deploy usually wins: the boot fetch finds no release
    // yet. So a miss retries every minute for the first half hour, then
    // hourly — the builds appear within a minute of CI publishing them.
    // meaning-based prompt matching: model and indexing thread, off the
    // request path, and only when CORK_EMBEDDINGS=1
    crate::embed::start(app.store.clone());
    // the ephemeral tier and batched counters reach disk from here
    Store::start_flusher(app.store.clone());
    // collisions a commit's ingest made before the engine took landed history
    // as the truth leave the Inbox's feed, once per database (protocol_ws.rs)
    crate::protocol_ws::start_landed_sweep(app.store.clone());
    // the durable log, the lease and snapshots (CORK_PG_URL)
    if crate::engine::Engine::start(app.store.clone()).is_some() {
        tracing::info!("engine: started; /health reports ready once caught up");
    }
    // the old copies of files git moved leave every map, once per database
    // (codegraph.rs); after the engine is attached, so it waits to catch up
    crate::codegraph::start_moved_sweep(app.store.clone());
    // past the soft memory limit every cache is shed and the founders told
    crate::membudget::start_watchdog(app.store.clone());
    // images attached to messages: a free workspace's go after a day
    #[cfg(feature = "cloud")]
    crate::uploads::start_sweeper(app.store.clone());
    // workspaces their owners deleted go for good after their 30 days
    #[cfg(feature = "cloud")]
    crate::workspaces::start_purger(app.store.clone());
    // notification email: a sweep every five minutes (notify.rs; silent
    // without RESEND_API_KEY or CORK_EMAIL_DRY_RUN, off with CORK_EMAILS=0)
    crate::notify::start(Arc::clone(&app));
    // the status page: a self-check every minute, recorded (status.rs;
    // not on a local server)
    crate::status::start(Arc::clone(&app));
    // repos idle for half an hour leave memory: their graph records, index,
    // snapshot and vectors are rebuilt on next use. CORK_CACHE_IDLE_S tunes it.
    std::thread::Builder::new()
        .name("collide-evict".into())
        .spawn(|| {
            let idle = crate::env::var("CORK_CACHE_IDLE_S").ok().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(1_800.0);
            let every = (idle / 30.0).clamp(1.0, 60.0);
            loop {
                std::thread::sleep(std::time::Duration::from_secs_f64(every));
                let dropped = crate::codegraph::evict_idle(idle) + crate::graphview::evict_idle(idle) + crate::embed::evict_idle(idle);
                if dropped > 0 {
                    tracing::info!("memory: dropped {dropped} idle repo cache(s)");
                }
            }
        })
        .ok();
    if artifacts::sync_enabled() {
        tokio::spawn(async {
            let mut quick_retries = 30u32;
            loop {
                // the status page watches that this loop goes round
                crate::status::note_loop("sync");
                let wait = match artifacts::sync_from_github().await {
                    Ok(n) => {
                        if n == 0 {
                            tracing::info!("hook binaries: {} current", artifacts::release_tag());
                        } else {
                            tracing::info!("hook binaries: {} file(s) of {} fetched", n, artifacts::release_tag());
                        }
                        quick_retries = 0;
                        3600
                    }
                    Err(error) => {
                        tracing::warn!("hook binaries: {error}");
                        // until this release's builds exist, the version before
                        // is what the install script and setup hand out: have it
                        let previous = crate::envelope::HOOK_ARTIFACT_VERSION - 1;
                        if let Err(why) = artifacts::ensure_version(previous).await {
                            tracing::warn!("hook binaries: v{previous} too: {why}");
                        }
                        if quick_retries > 0 {
                            quick_retries -= 1;
                            60
                        } else {
                            3600
                        }
                    }
                };
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            }
        });
    }

    let port: u16 = crate::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8000);
    // [::] first: Railway's private network is IPv6, and on Linux a [::]
    // socket takes IPv4 too, so the public edge keeps working. A host with
    // IPv6 off falls back to plain IPv4, as the embedder does. A local
    // server listens on this machine's loopback and nowhere else.
    let (addr, bound) = if let Some(port) = local_port {
        let v4 = SocketAddr::from(([127, 0, 0, 1], port));
        (v4, tokio::net::TcpListener::bind(v4).await)
    } else {
        let addr = SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => (addr, Ok(listener)),
            Err(_) => {
                let v4 = SocketAddr::from(([0, 0, 0, 0], port));
                (v4, tokio::net::TcpListener::bind(v4).await)
            }
        }
    };
    if let (Some(port), Ok(_)) = (local_port, &bound) {
        local::write_port(port);
        local::watch_for_update(app.store.clone());
        // the free version's long-lived process: brings Cork into agent tools installed since
        #[cfg(feature = "cli")]
        cork_hooks::harnesses::watch(cork_hooks::config::env_map());
    }
    tracing::info!("collide-server (rust) listening on {addr}");
    let listener = match bound {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot bind {addr}: {error}");
            std::process::exit(1);
        }
    };
    // a deploy sends SIGTERM: stop taking requests, then write what the
    // in-memory tiers still hold, so presence and counters survive it
    let flush_store = app.store.clone();
    let deadline_store = app.store.clone();
    let stopping = async move {
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
            tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        // A deploy's standby is already serving, and writes reaching it
        // wait for this process's lease: hand it over fast. In-flight
        // requests get a short grace, then memory is written, shipped and
        // the lease released. Open websockets and MCP streams never finish
        // on their own, so this does not wait for the server to drain.
        std::thread::spawn(move || {
            let grace = crate::env::var("CORK_HANDOVER_GRACE_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(300);
            std::thread::sleep(std::time::Duration::from_millis(grace));
            crate::embed::persist_now();
            match deadline_store.engine() {
                Some(engine) => engine.release(&deadline_store),
                None => {
                    deadline_store.flush();
                    deadline_store.flush_trees(true);
                }
            }
            std::process::exit(0);
        });
    };
    if let Err(error) = axum::serve(listener, router).with_graceful_shutdown(stopping).await {
        eprintln!("server stopped: {error}");
        flush_store.flush();
        flush_store.flush_trees(true);
        std::process::exit(1);
    }
    flush_store.flush();
    flush_store.flush_trees(true);
    crate::embed::persist_now();
    if let Some(engine) = flush_store.engine() {
        let store = flush_store.clone();
        let _ = std::thread::spawn(move || engine.release(&store)).join();
    }
    // the runtime would otherwise wait on its blocking threads (the embedder,
    // the evictor) for ever: a deploy's SIGTERM has to end the process
    tracing::info!("collide-server stopped: in-memory state written");
    std::process::exit(0);
}

/// A database copied in whole (staging taking a copy of production, or a
/// restore from backup) is staged at `<data>/restore/collide.db` (with its
/// `-wal` when there is one) and swapped in here, before anything opens the
/// file: the current database is kept beside it as `collide.db.before-<ts>`,
/// never deleted.
fn restore_if_staged(db_path: &std::path::Path) {
    let Some(dir) = db_path.parent() else { return };
    let staged = dir.join("restore").join("collide.db");
    if !staged.exists() {
        return;
    }
    let stamp = crate::store::now() as u64;
    for suffix in ["", "-wal", "-shm"] {
        let current = std::path::PathBuf::from(format!("{}{suffix}", db_path.display()));
        if current.exists() {
            let kept = dir.join(format!("collide.db.before-{stamp}{suffix}"));
            if let Err(error) = std::fs::rename(&current, &kept) {
                eprintln!("restore: cannot set {} aside: {error}; keeping the current database", current.display());
                return;
            }
        }
    }
    for suffix in ["", "-wal"] {
        let from = dir.join("restore").join(format!("collide.db{suffix}"));
        if from.exists() {
            let to = std::path::PathBuf::from(format!("{}{suffix}", db_path.display()));
            if let Err(error) = std::fs::rename(&from, &to) {
                eprintln!("restore: cannot move {} in: {error}", from.display());
            }
        }
    }
    tracing::info!("restore: swapped in the staged database; the previous one is kept as collide.db.before-{stamp}");
}
