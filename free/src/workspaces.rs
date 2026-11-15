//! Workspace tenancy: invite-only sections of the shared database.
//!
//! A workspace is the partition every scoped row lives under
//! (`scope = "{workspace_id}:{repo_id}"`), not a separate table or database.
//! Metadata sits in the shared tier's bucketed KV:
//!
//!   ws       `<ws_id>`          -> {id, name, owner, created, kind, github_org}
//!   member   `<ws_id>/<uid>`    -> {uid, email, name, role, added, added_by, access, repos}
//!   uws      `<uid>/<ws_id>`    -> {workspace}          (membership index)
//!   invite   `<code>`           -> {workspace, created_by, created, expires, max_uses, uses,
//!                                   repos, email}
//!
//! A workspace holds many repos. `kind` is "personal" (the one every account
//! gets, named "<name>'s workspace"), "org" (named after a GitHub org — repos
//! under `github.com/<org>/...` route here automatically) or "custom". A
//! member's `repos` is "*" (everything, the owner always) or the list of repo
//! ids they may work in; an invite carries the same list so redeeming it
//! grants exactly that. Repo ids in those lists are stored as keys (lowercase,
//! no `#branch`) so any spelling of the same repo matches — see
//! [`crate::repo::repo_key`], reused rather than duplicated here.
//!
//! Joining requires an invite code from an existing member; a user without
//! one can only create a fresh workspace of their own.
//!
//! Nothing in the crate calls into this module yet (it is not wired to an
//! HTTP handler), hence the blanket allow below — every item here is dead
//! code until that wiring lands, the same position `store.rs` was in before
//! its first caller.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use serde_json::{json, Value};

use crate::access::has_repo_access;
use crate::compat::{new_id, python_repr, truthy};
use crate::repo::repo_key;
use crate::store::{now, Store};

/// Unambiguous, human-typable invite alphabet — no 0/O/1/I/L.
const INVITE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const INVITE_TTL_S: f64 = 7.0 * 86400.0;
const INVITE_MAX_USES: i64 = 20;

/// An 8-character invite code drawn from OS randomness.
///
/// Rejection sampling, not a plain modulo: 256 is not a multiple of the
/// alphabet's 31 characters, so `byte % 31` would hand out the first eight
/// letters slightly more often than the rest. This code gates who can join a
/// private workspace, so that bias is worth the extra branch.
fn invite_code() -> String {
    let ceiling = 256 - (256 % INVITE_ALPHABET.len());
    let mut source = std::fs::File::open("/dev/urandom").ok();
    let mut code = String::with_capacity(8);
    let mut byte = [0u8; 1];
    while code.len() < 8 {
        let drawn = source.as_mut().is_some_and(|f| f.read_exact(&mut byte).is_ok());
        if !drawn {
            // OS randomness unavailable (should not happen in practice): fall
            // back to compat::new_id's process-unique, unpredictably-seeded
            // churn rather than block invite creation entirely.
            byte[0] = new_id().as_bytes()[code.len() % 12];
        }
        if (byte[0] as usize) < ceiling {
            code.push(INVITE_ALPHABET[byte[0] as usize % INVITE_ALPHABET.len()] as char);
        }
    }
    code
}

/// The caller identity a workspace write needs: who owns the write, and what
/// name lands in a freshly created member record.
///
/// Deliberately not `auth::Principal` — that type does not carry a display
/// name on this side yet (the OAuth exchange that fills it in is a separate,
/// in-flight port), and every member-creating call here needs one. A local,
/// complete shape beats a partial borrow of a type that is still catching up;
/// once `auth::Principal` gains `name`, a caller can build one of these from
/// it in one line.
#[derive(Debug, Clone)]
pub struct Principal {
    pub uid: String,
    pub email: String,
    pub name: String,
}

impl Principal {
    /// The identity written into records and shown on dashboards.
    pub fn label(&self) -> &str {
        if self.email.is_empty() { &self.uid } else { &self.email }
    }
}

/// Which repos a member (or invite) may work in — `"*"` or a list, made
/// concrete so a caller cannot pass a stray string other than
/// `"*"`.
#[derive(Debug, Clone)]
pub enum RepoScope {
    All,
    Some(Vec<String>),
}

/// Optional arguments to [`create_invite`].
pub struct InviteOptions {
    pub ttl_s: f64,
    pub max_uses: i64,
    pub repos: RepoScope,
    pub email: String,
    pub github_login: String,
}

/// A seat-limit lookup: workspace id -> the cap, or `None` for unlimited.
/// [`redeem_invite`] takes this as an optional callback rather than calling
/// into `billing` directly.
///
/// Carries its own lifetime rather than defaulting to `'static`: a bare
/// `dyn Fn(&str) -> Option<i64>` alias with no lifetime parameter gets
/// `'static` as its default trait-object bound wherever it's substituted,
/// which silently forces every caller's closure to be `'static` too — one
/// couldn't borrow `&app.store` without first cloning it into an `Arc`.
/// With `<'a>` here and `SeatLimiter<'_>` at the call site, the bound
/// follows the reference's own lifetime instead, so a closure that borrows
/// something short-lived (a `&Store` included) just works.
pub type SeatLimiter<'a> = dyn Fn(&str) -> Option<i64> + 'a;

impl Default for InviteOptions {
    fn default() -> Self {
        Self {
            ttl_s: INVITE_TTL_S,
            max_uses: INVITE_MAX_USES,
            repos: RepoScope::All,
            email: String::new(),
            github_login: String::new(),
        }
    }
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn num(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn int_or(value: &Value, key: &str, default: i64) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(default)
}

/// The first `max` Unicode code points, not bytes; `&str[..n]` would
/// panic mid-codepoint on anything outside ASCII.
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Parse the `"*" | [repo, ...]` union stored for a `repos` field.
/// Anything that isn't a JSON array — `"*"`, missing, or null — reads as
/// every repo, matching every default this module ever wrote (`.get(k, "*")`).
fn repo_scope_of(value: Option<&Value>) -> RepoScope {
    match value.and_then(Value::as_array) {
        Some(items) => {
            RepoScope::Some(items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        }
        None => RepoScope::All,
    }
}

fn repo_scope_to_value(scope: &RepoScope) -> Value {
    match scope {
        RepoScope::All => json!("*"),
        RepoScope::Some(list) => json!(list),
    }
}

/// Clean a caller-supplied repo list into the canonical stored form: keys
/// normalized (so any spelling of the same repo collapses to one entry),
/// blanks dropped, deduped and sorted — matching
/// `sorted({repo_key(str(r)) for r in repos if str(r).strip()})`.
fn normalize_repos(list: &[String]) -> Vec<String> {
    list.iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .map(repo_key)
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect()
}

/// The name a fresh personal workspace gets when the caller didn't choose
/// one — every account's default workspace.
pub fn personal_name(principal: &Principal) -> String {
    format!("{}'s workspace", principal.label())
}

// ---------------------------------------------------------------- workspaces

/// Workspace names are unique per account: with automatic per-repo routing
/// and name-based switching, two same-named workspaces on one account would
/// be indistinguishable.
fn name_taken(store: &Store, uid: &str, name: &str, ignore_id: &str) -> bool {
    let wanted = name.trim().to_lowercase();
    list_for(store, uid)
        .iter()
        .any(|w| text(w, "id") != ignore_id && text(w, "name").trim().to_lowercase() == wanted)
}

/// `kind`: "personal" (no name given, or the personal name), "org"
/// (`github_org` set, or the name looks like a GitHub org slug the caller
/// passes explicitly) or "custom". One personal workspace per account.
pub fn create(
    store: &Store, principal: &Principal, name: &str, kind: &str, github_org: &str,
) -> Result<Value, String> {
    let now_ts = now();
    let mut cleaned = truncate_chars(name.trim(), 120);
    let mut kind = kind.to_string();
    if cleaned.is_empty() {
        cleaned = personal_name(principal);
        kind = "personal".to_string();
    }
    if kind == "personal" && personal_for(store, &principal.uid).is_some() {
        return Err("you already have a personal workspace".to_string());
    }
    if !matches!(kind.as_str(), "personal" | "org" | "custom") {
        kind = if !github_org.is_empty() { "org".to_string() } else { "custom".to_string() };
    }
    if name_taken(store, &principal.uid, &cleaned, "") {
        return Err(format!("you already have a workspace named {}", python_repr(&cleaned)));
    }
    let id = new_id();
    let ws = json!({
        "id": id,
        "name": cleaned,
        "owner": principal.uid,
        "created": now_ts,
        "kind": kind,
        "github_org": github_org.trim().to_lowercase(),
    });
    let _ = store.kv_put("ws", &id, &ws, now_ts);
    add_member(store, &id, principal, "owner", &principal.uid, now_ts);
    crate::analytics::signed_up(store, &principal.uid, &principal.email, &principal.name, "site");
    crate::analytics::capture(&principal.uid, "workspace_created", &id, json!({"kind": kind}));
    Ok(ws)
}

/// `personal_ws` `<uid>` -> `{workspace, at}`: the account was given its
/// personal workspace (or `{closed: true}`: the account was deleted). Either
/// way it is never given another, so a workspace removed later, or a stale
/// token's call after the account is gone, never brings one back.
pub const PERSONAL_MADE_BUCKET: &str = "personal_ws";

/// The name an account's own workspace gets: its first name ("Alice's
/// workspace"), else its username, else the email's local part.
pub fn personal_name_for(principal: &Principal, profile: &Value) -> String {
    let first_word = |s: &str| s.split_whitespace().next().unwrap_or("").to_string();
    let local_part = |s: &str| s.split('@').next().unwrap_or("").to_string();
    let who = [
        first_word(&text(profile, "name")),
        first_word(&principal.name),
        text(profile, "username"),
        text(profile, "github_login"),
        local_part(&principal.email),
    ]
    .into_iter()
    .map(|s| truncate_chars(s.trim(), 60))
    // a test or provider principal can carry its uid as its "name": not a name
    .find(|s| !s.is_empty() && s != &principal.uid);
    match who {
        Some(who) => format!("{who}'s workspace"),
        None => "My workspace".to_string(),
    }
}

/// Every account gets a workspace of its own the first time it signs in, so
/// a new person lands somewhere their agents can report to before they
/// have named anything. Made once, and only for an account in no workspace
/// at all: one invited first already has a place, and one that ever had a
/// personal workspace (or was deleted) never gets a second. It is the Free
/// plan's one workspace, so it can never put an account over that cap.
/// Returns the workspace when this call made it.
pub fn ensure_personal(store: &Store, principal: &Principal, profile: &Value) -> Option<Value> {
    let settled = || {
        principal.uid.is_empty()
            || store.kv_get(PERSONAL_MADE_BUCKET, &principal.uid).is_some()
            || !list_for(store, &principal.uid).is_empty()
    };
    // every workspace list asks: the answer is almost always "settled",
    // and that needs no lock
    if settled() {
        return None;
    }
    // two first loads at once (the navbar and the page) must not make two
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _held = ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if settled() {
        return None;
    }
    let name = personal_name_for(principal, profile);
    // the name is the account's first, so it cannot be taken; the
    // personal-kind check in create still refuses a second outright
    let ws = create(store, principal, &name, "personal", "").ok()?;
    let _ = store.kv_put(PERSONAL_MADE_BUCKET, &principal.uid, &json!({"workspace": text(&ws, "id"), "at": now()}), now());
    // (the first month's bonus hours land when the account's first agent
    // reports: main::first_agent_bonus)
    crate::analytics::capture(&principal.uid, "personal_workspace_created", &text(&ws, "id"), json!({}));
    Some(ws)
}

pub fn owned_by(store: &Store, uid: &str) -> Vec<Value> {
    list_for(store, uid).into_iter().filter(|w| text(w, "owner") == uid).collect()
}

/// The account's personal workspace: kind == personal, else (for accounts
/// predating kinds) the oldest one it owns whose name is the personal name.
pub fn personal_for(store: &Store, uid: &str) -> Option<Value> {
    let owned = owned_by(store, uid);
    if let Some(hit) = owned.iter().find(|w| text(w, "kind") == "personal") {
        return Some(hit.clone());
    }
    owned.into_iter().find(|w| text(w, "name").ends_with("'s workspace"))
}

/// The workspace that represents GitHub org `org` for this account:
/// `github_org` set to it, else a workspace NAMED like it. First match by
/// creation order; membership is already implied by [`list_for`].
pub fn org_workspace_for(
    store: &Store, uid: &str, org: &str, allowed: Option<&BTreeSet<String>>,
) -> Option<Value> {
    let wanted = org.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    let filter_active = allowed.is_some_and(|a| !a.is_empty());
    let mine: Vec<Value> = list_for(store, uid)
        .into_iter()
        .filter(|w| !filter_active || allowed.unwrap().contains(&text(w, "id")))
        .collect();
    if let Some(hit) = mine.iter().find(|w| text(w, "github_org").to_lowercase() == wanted) {
        return Some(hit.clone());
    }
    mine.into_iter().find(|w| text(w, "name").trim().to_lowercase() == wanted)
}

pub fn set_kind(store: &Store, workspace_id: &str, kind: &str, github_org: Option<&str>) -> Option<Value> {
    let mut ws = get(store, workspace_id)?;
    if matches!(kind, "personal" | "org" | "custom") {
        ws["kind"] = json!(kind);
    }
    if let Some(org) = github_org {
        ws["github_org"] = json!(org.trim().to_lowercase());
    }
    let _ = store.kv_put("ws", workspace_id, &ws, now());
    Some(ws)
}

pub fn get(store: &Store, workspace_id: &str) -> Option<Value> {
    store.kv_get("ws", workspace_id)
}

pub fn rename(store: &Store, workspace_id: &str, name: &str) -> Result<Option<Value>, String> {
    let Some(mut ws) = get(store, workspace_id) else { return Ok(None) };
    let cleaned = truncate_chars(name.trim(), 120);
    if !cleaned.is_empty() {
        // same uniqueness rule as create — two same-named workspaces on one
        // account would make every by-name reference ambiguous
        let owner = text(&ws, "owner");
        if name_taken(store, &owner, &cleaned, workspace_id) {
            return Err(format!("you already have a workspace named {}", python_repr(&cleaned)));
        }
        ws["name"] = json!(cleaned);
        let _ = store.kv_put("ws", workspace_id, &ws, now());
    }
    Ok(Some(ws))
}

pub fn list_for(store: &Store, uid: &str) -> Vec<Value> {
    let mut out: Vec<Value> = store
        .kv_list("uws", &format!("{uid}/"))
        .into_iter()
        .filter_map(|(_, entry)| get(store, &text(&entry, "workspace")))
        .map(|mut workspace| {
            // its uploaded icon's version: the dashboard fetches the picture
            // (`/api/workspaces/:id/icon`) only when there is one, and again
            // when it changes. Icons live in object storage: the cloud's alone
            #[cfg(feature = "cloud")]
            if let Some(version) = crate::repoicons::workspace_version(store, &text(&workspace, "id")) {
                workspace["icon_v"] = Value::String(version);
            }
            workspace
        })
        .collect();
    out.sort_by(|a, b| num(a, "created").partial_cmp(&num(b, "created")).unwrap());
    out
}

// ---------------------------------------------------------------- membership

/// The membership record. Same bucket and key shape as [`crate::auth::member`]
/// — reused there rather than duplicated here, since the hot-path auth
/// resolution needs the identical lookup.
pub use crate::auth::member;

pub fn members(store: &Store, workspace_id: &str) -> Vec<Value> {
    let mut out: Vec<Value> =
        store
            .kv_list("member", &format!("{workspace_id}/"))
            .into_iter()
            .map(|(_, v)| crate::auth::with_repo_renames(store, workspace_id, v))
            .collect();
    out.sort_by(|a, b| num(a, "added").partial_cmp(&num(b, "added")).unwrap());
    out
}

pub(crate) fn add_member(
    store: &Store, workspace_id: &str, principal: &Principal, role: &str, added_by: &str, now_ts: f64,
) -> Value {
    let record = json!({
        "uid": principal.uid,
        "email": principal.email,
        "name": principal.name,
        "role": role,
        "added": now_ts,
        "added_by": added_by,
    });
    let _ = store.kv_put("member", &format!("{workspace_id}/{}", principal.uid), &record, now_ts);
    let _ = store.kv_put(
        "uws", &format!("{}/{workspace_id}", principal.uid), &json!({"workspace": workspace_id}), now_ts,
    );
    record
}

/// Which repos this member may work in: every repo, or an explicit set. The
/// owner is always every repo.
pub fn set_member_repos(store: &Store, workspace_id: &str, uid: &str, repos: RepoScope) -> Option<Value> {
    let mut member = member(store, workspace_id, uid)?;
    let owner = text(&member, "role") == "owner";
    let normalized = if owner {
        RepoScope::All
    } else {
        match repos {
            RepoScope::All => RepoScope::All,
            RepoScope::Some(list) => RepoScope::Some(normalize_repos(&list)),
        }
    };
    member["repos"] = repo_scope_to_value(&normalized);
    let _ = store.kv_put("member", &format!("{workspace_id}/{uid}"), &member, now());
    Some(member)
}

pub fn members_with_access(store: &Store, workspace_id: &str, repo_id: &str) -> Vec<Value> {
    members(store, workspace_id).into_iter().filter(|m| has_repo_access(Some(m), repo_id)).collect()
}

pub fn repo_seats_used(store: &Store, workspace_id: &str, repo_id: &str) -> i64 {
    members_with_access(store, workspace_id, repo_id).len() as i64
}

/// `access`: "write" (default for every member) or "read" — a read-only
/// member observes the workspace but MCP write tools (declare_intent,
/// report_edit, ...) reject their calls.
pub fn set_access(store: &Store, workspace_id: &str, uid: &str, access: &str) -> Option<Value> {
    let mut member = member(store, workspace_id, uid)?;
    member["access"] = json!(access);
    let _ = store.kv_put("member", &format!("{workspace_id}/{uid}"), &member, now());
    Some(member)
}

// ------------------------------------------------------------------ invites

/// `opts.repos`: every repo, or the repo ids the joiner may work in — the
/// per-repo permission checked off on the invite form. `opts.email` /
/// `opts.github_login`: who it was sent to (the greyed-out contributor row).
pub fn create_invite(store: &Store, workspace_id: &str, principal: &Principal, opts: InviteOptions) -> Value {
    let now_ts = now();
    let code = invite_code();
    let repos = match opts.repos {
        RepoScope::All => RepoScope::All,
        RepoScope::Some(list) => RepoScope::Some(normalize_repos(&list)),
    };
    let invite = json!({
        "code": code,
        "workspace": workspace_id,
        "created_by": principal.uid,
        "created": now_ts,
        "expires": now_ts + opts.ttl_s.max(60.0),
        "max_uses": opts.max_uses.max(1),
        "uses": 0,
        "repos": repo_scope_to_value(&repos),
        "email": opts.email.trim().to_lowercase(),
        "github_login": opts.github_login.trim().to_lowercase(),
    });
    let _ = store.kv_put("invite", &code, &invite, now_ts);
    crate::analytics::capture(
        &principal.uid, "invite_sent", workspace_id,
        json!({"by_email": !text(&invite, "email").is_empty(), "by_github": !text(&invite, "github_login").is_empty()}),
    );
    invite
}

/// Returns `{"ok": true, "workspace": {...}, "repos": ..., "access":
/// "write"|"read"[, "viewer": true][, "share_play": {...}]}` or
/// `{"ok": false, "error": ...}`.
///
/// Who gets in, and as what (Figma/Xbox: viewers are free, players pay) —
/// decided from the workspace's plan record, in this order:
///   1. Free, trial never used, and the invite is the OWNER's: SHARE PLAY —
///      `billing::start_trial` puts the workspace on a 14-day Team trial
///      first, then the invitee joins as a PLAYER (access "write"). Both
///      get Team.
///   2. Paid or trialing (after step 1): the seat gate — join as a player
///      while the purchased seats allow, else the `seats_full` refusal (a
///      paid workspace with no seat left is still a refusal; the owner buys
///      a seat). A trial sells no seats, so it never refuses.
///   3. Free with the trial used or expired: join as a VIEWER (access
///      "read") — never `seats_full`, unlimited. Their agents get nothing;
///      they watch the radar and read the briefings.
/// The response's `access` is `billing::effective_access` — computed, not
/// stored: once a trial or plan lapses every non-owner reads as a viewer
/// whatever their stored access says.
///
/// `seat_limit_for` caps PLAYERS at the seats the plan bought;
/// `repo_seat_limit_for` caps PEOPLE PER REPO when a plan sets it (none does
/// today). The invite's repos are granted one by one — a repo already at its
/// cap is skipped, and when none can be granted the join bounces with
/// `seats_full`. Existing members always get through; re-redeeming widens
/// their repo list.
pub fn redeem_invite(
    store: &Store,
    code: &str,
    principal: &Principal,
    seat_limit_for: Option<&SeatLimiter<'_>>,
    repo_seat_limit_for: Option<&SeatLimiter<'_>>,
) -> Value {
    let code = code.trim().to_uppercase();
    let now_ts = now();
    let Some(mut invite) = (if code.is_empty() { None } else { store.kv_get("invite", &code) }) else {
        return json!({"ok": false, "error": "invalid invite code"});
    };
    if num(&invite, "expires") <= now_ts {
        return json!({"ok": false, "error": "invite expired"});
    }
    if int_or(&invite, "uses", 0) >= int_or(&invite, "max_uses", 1) {
        return json!({"ok": false, "error": "invite already used up"});
    }
    let Some(ws) = get(store, &text(&invite, "workspace")) else {
        return json!({"ok": false, "error": "workspace no longer exists"});
    };
    let ws_id = text(&ws, "id");
    let wanted = repo_scope_of(invite.get("repos"));
    let existing = member(store, &ws_id, &principal.uid);

    let plan = crate::billing::plan_of(store, &ws_id);
    let mut viewer = false;
    if existing.is_none() && !crate::billing::is_active_status(plan.get("status")) {
        // Free: viewers are free and unlimited. (Share Play, which put the
        // workspace on 14 unmetered days of Team at the owner's first
        // invite, is gone: the inviter gets bonus agent hours instead,
        // below, once the invitee is in.)
        viewer = true;
    }

    if existing.is_none() && !viewer {
        if let Some(seat_limit_for) = seat_limit_for {
            if let Some(limit) = seat_limit_for(&ws_id) {
                // a member's own seat (selfseat.rs) uses none of the plan's
                let taken = members(store, &ws_id).iter().filter(|m| !crate::selfseat::is_self_paid(m)).count();
                if taken as i64 >= limit {
                    // the joiner isn't the payer: leave a trace the OWNER sees
                    // (dashboard pressure pill, agent notices, digest)
                    record_blocked_join(store, &ws_id, principal, now_ts);
                    return json!({
                        "ok": false,
                        "seats_full": true,
                        "error": format!(
                            "this workspace's plan holds {limit} seat{} and all are taken — the \
owner can add seats on the billing page",
                            if limit != 1 { "s" } else { "" }
                        ),
                    });
                }
            }
        }
    }

    let per_repo = repo_seat_limit_for.and_then(|f| f(&ws_id));

    let (granted, skipped): (RepoScope, Vec<String>) = if let Some(limit) = per_repo {
        match wanted {
            RepoScope::Some(want_list) => {
                let mut keep = Vec::new();
                let mut skipped = Vec::new();
                for rid in &want_list {
                    if existing.as_ref().is_some_and(|m| has_repo_access(Some(m), rid)) {
                        keep.push(rid.clone());
                        continue;
                    }
                    if repo_seats_used(store, &ws_id, rid) >= limit {
                        skipped.push(rid.clone());
                    } else {
                        keep.push(rid.clone());
                    }
                }
                if keep.is_empty() && existing.is_none() {
                    record_blocked_join(store, &ws_id, principal, now_ts);
                    return json!({
                        "ok": false, "seats_full": true, "repos_full": skipped,
                        "error": format!(
                            "every repo on this invite already has {limit} people on it (the free \
plan's limit per repo) — the owner can add units on the billing page, or make room on a repo"
                        ),
                    });
                }
                (RepoScope::Some(keep), skipped)
            }
            RepoScope::All => {
                // "*" on a per-repo-capped plan: everything, as long as every
                // existing repo still has room for one more person (a
                // wildcard holder counts on every repo); otherwise just the
                // repos with room
                let mut room = Vec::new();
                let mut skipped = Vec::new();
                for rid in repos(store, &ws_id, false) {
                    let has_room = existing.as_ref().is_some_and(|m| has_repo_access(Some(m), &rid))
                        || repo_seats_used(store, &ws_id, &rid) < limit;
                    if has_room {
                        room.push(repo_key(&rid));
                    } else {
                        skipped.push(repo_key(&rid));
                    }
                }
                let granted = if skipped.is_empty() { RepoScope::All } else { RepoScope::Some(room) };
                if matches!(&granted, RepoScope::Some(v) if v.is_empty()) && existing.is_none() {
                    record_blocked_join(store, &ws_id, principal, now_ts);
                    return json!({
                        "ok": false, "seats_full": true, "repos_full": skipped,
                        "error": format!(
                            "every repo in this workspace already has {limit} people on it (the \
free plan's limit per repo) — the owner can add units on the billing page, or make room on a repo"
                        ),
                    });
                }
                (granted, skipped)
            }
        }
    } else {
        (wanted, Vec::new())
    };

    let newly_joined = existing.is_none();
    let member_record = if let Some(existing) = existing {
        // widen, never narrow: an invite can add repos to someone already in
        let current = repo_scope_of(existing.get("repos"));
        let merged = match (granted, current) {
            (RepoScope::All, _) | (_, RepoScope::All) => RepoScope::All,
            (RepoScope::Some(g), RepoScope::Some(c)) => {
                let set: BTreeSet<String> = c.into_iter().chain(g).collect();
                RepoScope::Some(set.into_iter().collect())
            }
        };
        set_member_repos(store, &ws_id, &principal.uid, merged).unwrap_or(existing)
    } else {
        let created = add_member(store, &ws_id, principal, "member", &text(&invite, "created_by"), now_ts);
        let with_repos = set_member_repos(store, &ws_id, &principal.uid, granted).unwrap_or(created);
        if viewer {
            set_access(store, &ws_id, &principal.uid, "read").unwrap_or(with_repos)
        } else {
            with_repos
        }
    };

    invite["uses"] = json!(int_or(&invite, "uses", 0) + 1);
    let _ = store.kv_put("invite", &code, &invite, now_ts);

    let access = crate::billing::effective_access(&plan, &member_record);
    let mut result = json!({
        "ok": true,
        "workspace": ws,
        "repos": member_record.get("repos").cloned().unwrap_or(json!("*")),
        "access": access,
    });
    if access == "read" {
        result["viewer"] = json!(true);
    }
    if !skipped.is_empty() {
        result["repos_full"] = json!(skipped);
    }
    if newly_joined {
        // the inviter's bonus hours, once per account: a teammate they
        // brought in joined
        let inviter = text(&invite, "created_by");
        if !inviter.is_empty() && inviter != principal.uid
            && crate::billing::grant_bonus(store, &ws_id, &inviter, "invite", crate::billing::INVITE_BONUS_HOURS)
        {
            result["inviter_bonus_hours"] = json!(crate::billing::INVITE_BONUS_HOURS);
        }
        crate::analytics::signed_up(store, &principal.uid, &principal.email, &principal.name, "invite");
        crate::analytics::capture(
            &principal.uid, "invite_joined", &ws_id,
            json!({"access": access, "invited_by": inviter}),
        );
    }
    result
}

// ---------------------------------------------------------- leaving & removal

/// Membership ends immediately: the member record and the reverse index both
/// go, so every workspace-bound credential this person holds fails its
/// membership check on the next call. Their edits, notes, and rationales stay
/// in the ledger — history is not rewritten.
pub fn remove_member(store: &Store, workspace_id: &str, uid: &str) {
    let _ = store.kv_delete("member", &format!("{workspace_id}/{uid}"));
    let _ = store.kv_delete("uws", &format!("{uid}/{workspace_id}"));
}

/// A member's role, kept in step with the workspace's `owner` by the caller.
pub fn set_role(store: &Store, workspace_id: &str, uid: &str, role: &str) -> Option<Value> {
    let mut member = member(store, workspace_id, uid)?;
    member["role"] = json!(role);
    let _ = store.kv_put("member", &format!("{workspace_id}/{uid}"), &member, now());
    Some(member)
}

pub fn transfer_ownership(store: &Store, workspace_id: &str, new_owner_uid: &str) -> Option<Value> {
    let mut ws = get(store, workspace_id)?;
    let mut member = member(store, workspace_id, new_owner_uid)?;
    let now_ts = now();
    ws["owner"] = json!(new_owner_uid);
    member["role"] = json!("owner");
    let _ = store.kv_put("ws", workspace_id, &ws, now_ts);
    let _ = store.kv_put("member", &format!("{workspace_id}/{new_owner_uid}"), &member, now_ts);
    Some(ws)
}

// ------------------------------------------------------------- seat pressure

/// A join that bounced off the seat limit, kept so the owner finds out (the
/// joiner isn't the one who can fix it). Capped, best-effort: a failed write
/// here must never be the reason a join itself fails.
pub fn record_blocked_join(store: &Store, workspace_id: &str, principal: &Principal, now_ts: f64) {
    let rec = store.kv_get("join_blocked", workspace_id);
    let mut attempts: Vec<Value> = rec
        .as_ref()
        .and_then(|r| r.get("attempts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    attempts.push(json!({"uid": principal.uid, "email": principal.email, "ts": now_ts}));
    let start = attempts.len().saturating_sub(20);
    let rec = json!({"attempts": attempts[start..].to_vec()});
    let _ = store.kv_put("join_blocked", workspace_id, &rec, now_ts);
    // an upgrade trigger: the owner's plan turned someone away
    let owner = get(store, workspace_id).map(|ws| text(&ws, "owner")).unwrap_or_default();
    // the count only: the person turned away is not the one being tracked
    crate::analytics::capture(&owner, "seats_full", workspace_id, json!({"attempts": attempts.len()}));
}

/// Recent joins that bounced off the seat cap — EXCLUDING anyone who has
/// since made it in (a resolved bounce is history, not pressure).
pub fn blocked_joins(store: &Store, workspace_id: &str, within_s: f64) -> Vec<Value> {
    let rec = store.kv_get("join_blocked", workspace_id).unwrap_or(json!({}));
    let cutoff = now() - within_s;
    let current: BTreeSet<String> = members(store, workspace_id).iter().map(|m| text(m, "uid")).collect();
    rec.get("attempts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| num(a, "ts") >= cutoff && !current.contains(&text(a, "uid")))
        .collect()
}

// ------------------------------------------------------------ repos in a workspace

/// Repo ids with any activity — persisted trees plus, when
/// `include_live_intents`, in-flight intents — plus explicitly watched repos,
/// so a freshly watched repo with no reports yet still shows as connected
/// after a reload. A repo moved to another workspace is hidden here even
/// though its pre-move history stays under the old scope.
///
/// `include_live_intents`: the routes pass `true`; `redeem_invite`'s seat
/// count passes `false` — a repo known only through an uncommitted intent
/// has no proof any seat is actually used on it yet.
pub fn repos(store: &Store, workspace_id: &str, include_live_intents: bool) -> Vec<String> {
    let prefix = format!("{workspace_id}:");
    let mut found: BTreeSet<String> = BTreeSet::new();
    for scope in store.list_scopes(&prefix) {
        found.insert(scope[prefix.len()..].to_string());
    }
    for (_, rec) in store.kv_list("ghrepo", &prefix) {
        let watched = text(&rec, "repo_id").trim().to_string();
        if !watched.is_empty() {
            found.insert(watched);
        }
    }
    if include_live_intents {
        let intent_prefix = format!("intent:{workspace_id}:");
        for (key, _) in store.eph_scan(&intent_prefix) {
            // key = "intent:<ws>:<repo>:<intent_id>"
            let middle = &key[intent_prefix.len()..];
            let repo = match middle.rfind(':') {
                Some(idx) => &middle[..idx],
                None => middle,
            };
            if !repo.is_empty() {
                found.insert(repo.to_string());
            }
        }
    }
    let moved: BTreeSet<String> = store
        .kv_list("repo_moved", &prefix)
        .into_iter()
        .map(|(_, rec)| repo_key(&text(&rec, "repo_id")))
        .collect();
    found.into_iter().filter(|r| !moved.contains(&repo_key(r))).collect()
}

/// Everything one workspace keeps, found by the id every row of it carries
/// (one sweep for an admin's deletion and an owner's):
/// its record, members and their index rows, invites, billing, settings and
/// integrations (keys that name the workspace); each repo's scope
/// (`<ws>:<repo>`: the map, notes, messages and inboxes, files and trees);
/// its ephemeral markers; and the credentials bound to it. A credential that
/// only lists the workspace among others keeps the others. A row that names
/// the workspace only in its value goes when it points at it (`workspace` or
/// `workspace_id` is the id) and is otherwise left, and counted.
///
/// The ledger stays: its rows carry no message text or code, and it is one
/// hash chain across every workspace, so deleting a row would break the
/// proof for all of them. Counts what is there; with `dry_run` false,
/// deletes it.
pub fn delete_workspace(store: &Store, workspace_id: &str, dry_run: bool) -> Value {
    let ws = workspace_id.trim();
    let Some(workspace) = get(store, ws) else {
        return json!({"ok": false, "error": "no such workspace"});
    };
    // the id is the needle: a short one would match other workspaces' rows
    if ws.len() < 8 {
        return json!({"ok": false, "error": "that workspace id is too short to delete by"});
    }
    let people: Vec<String> = members(store, ws)
        .iter()
        .map(|m| if text(m, "email").is_empty() { text(m, "uid") } else { text(m, "email") })
        .collect();
    let repo_ids = repos(store, ws, true);
    let prefix = format!("{ws}:");
    let mut scopes: BTreeSet<String> = store.list_scopes(&prefix).into_iter().collect();
    scopes.extend(store.ledger_scopes_since(0.0).into_iter().filter(|s| s.starts_with(&prefix)));
    scopes.extend(repo_ids.iter().map(|repo| format!("{prefix}{repo}")));

    let keyed = store.kv_keys_containing(ws);
    let mut deleted: BTreeMap<String, i64> = BTreeMap::new();
    for (bucket, _) in &keyed {
        *deleted.entry(bucket.clone()).or_default() += 1;
    }
    let mut pointers: Vec<(String, String)> = Vec::new();
    let mut narrowed: Vec<(String, Value)> = Vec::new();
    let mut left: BTreeMap<String, i64> = BTreeMap::new();
    for (bucket, key, value) in store.kv_values_containing(ws) {
        let listed = value.get("workspaces").and_then(Value::as_array).cloned().unwrap_or_default();
        if bucket == crate::auth::TOKEN_BUCKET && text(&value, "workspace") != ws && listed.iter().any(|w| w.as_str() == Some(ws)) {
            let mut kept = value.clone();
            kept["workspaces"] = json!(listed.into_iter().filter(|w| w.as_str() != Some(ws)).collect::<Vec<_>>());
            narrowed.push((key, kept));
        } else if text(&value, "workspace") == ws || text(&value, "workspace_id") == ws {
            *deleted.entry(bucket.clone()).or_default() += 1;
            pointers.push((bucket, key));
        } else {
            *left.entry(bucket).or_default() += 1;
        }
    }
    let ephemeral: Vec<String> = store.eph_scan("").into_iter().map(|(key, _)| key).filter(|key| key.contains(ws)).collect();
    let ledger_kept: i64 = scopes.iter().map(|scope| store.ledger_count(scope)).sum();

    if !dry_run {
        let stamp = now();
        for (bucket, key) in keyed.iter().chain(pointers.iter()) {
            let _ = store.kv_delete(bucket, key);
        }
        for (key, kept) in &narrowed {
            let _ = store.kv_put(crate::auth::TOKEN_BUCKET, key, kept, stamp);
        }
        for scope in &scopes {
            let _ = store.delete_scope(scope);
            crate::presence::forget_scope(store, scope);
        }
        for key in &ephemeral {
            store.eph_delete(key);
        }
        // a repo that routed here is routed afresh on its next call
        for repo in &repo_ids {
            let _ = store.eph_set(&format!("wsrouteepoch:{}", crate::repo::path_key(repo)), &json!({"t": stamp}), Some(3600.0));
        }
        for window in [7, 30, 90, 365] {
            store.eph_delete(&format!("admin:overview:{window}"));
        }
    }
    json!({
        "ok": true, "dry_run": dry_run,
        "workspace": {"id": ws, "name": text(&workspace, "name"), "owner": text(&workspace, "owner")},
        "members": people, "repos": repo_ids, "scopes": scopes,
        "deleted": deleted, "credentials_narrowed": narrowed.len(), "ephemeral": ephemeral.len(),
        "left": left, "ledger_rows_kept": ledger_kept,
    })
}

/// A workspace its owner deleted, waiting out its 30 days: the marker (when,
/// by whom, when it goes for good) under its id, and each of its rows under
/// `<ws>|<bucket>|<key>`. Every key holds the id, so the final sweep
/// (`delete_workspace`) takes these with the rest.
pub const CLOSED_BUCKET: &str = "closed_ws";
pub const CLOSED_ROWS: &str = "closed_ws_rows";
/// How long a deleted workspace can still be restored.
pub const CLOSED_KEEP_S: f64 = 30.0 * 86_400.0;

/// An owner deletes a workspace, undoably for 30 days. Every row that names
/// it (`delete_workspace`'s sweep: its record, members and their index
/// rows, invites, settings, each repo's map, notes, messages and inboxes,
/// the credentials bound to it) moves aside, so everyone loses access at
/// once, nothing finds it, and its repos route afresh on their next call;
/// its files, trees and ledger rows stay where they are until it goes.
/// `restore_closed` puts it back; `purge_closed` deletes it for good once
/// its 30 days are over.
pub fn close_workspace(store: &Store, workspace_id: &str, by: &str) -> Value {
    let ws = workspace_id.trim();
    let Some(workspace) = get(store, ws) else {
        return json!({"ok": false, "error": "no such workspace"});
    };
    if ws.len() < 8 {
        return json!({"ok": false, "error": "that workspace id is too short to delete by"});
    }
    let stamp = now();
    let repo_ids = repos(store, ws, true);
    let mut rows: Vec<(String, String, Value)> = store
        .kv_keys_containing(ws)
        .into_iter()
        .filter(|(bucket, _)| bucket != CLOSED_BUCKET && bucket != CLOSED_ROWS)
        .filter_map(|(bucket, key)| store.kv_get(&bucket, &key).map(|value| (bucket, key, value)))
        .collect();
    // rows that point at it from a key of their own (a credential bound to
    // it, an invite, a watch): one that only lists it among others stays
    rows.extend(
        store
            .kv_values_containing(ws)
            .into_iter()
            .filter(|(_, _, value)| text(value, "workspace") == ws || text(value, "workspace_id") == ws),
    );
    for (bucket, key, value) in &rows {
        let _ = store.kv_put(CLOSED_ROWS, &format!("{ws}|{bucket}|{key}"), &json!({"bucket": bucket, "key": key, "value": value}), stamp);
        let _ = store.kv_delete(bucket, key);
    }
    for key in store.eph_scan("").into_iter().map(|(key, _)| key).filter(|key| key.contains(ws)) {
        store.eph_delete(&key);
    }
    for repo in &repo_ids {
        let _ = store.eph_set(&format!("wsrouteepoch:{}", crate::repo::path_key(repo)), &json!({"t": stamp}), Some(3600.0));
    }
    let purge_at = stamp + CLOSED_KEEP_S;
    let _ = store.kv_put(CLOSED_BUCKET, ws, &json!({
        "id": ws, "name": text(&workspace, "name"), "owner": text(&workspace, "owner"),
        "by": by, "at": stamp, "purge_at": purge_at, "rows": rows.len(), "repos": repo_ids,
    }), stamp);
    json!({"ok": true, "name": text(&workspace, "name"), "purge_at": purge_at, "repos": repo_ids})
}

/// Undo an owner's deletion within its 30 days: every row goes back where
/// it was. Support's, from the admin page.
pub fn restore_closed(store: &Store, workspace_id: &str) -> Value {
    let ws = workspace_id.trim();
    let Some(marker) = store.kv_get(CLOSED_BUCKET, ws) else {
        return json!({"ok": false, "error": "not a deleted workspace"});
    };
    let stamp = now();
    let mut restored = 0;
    for (key, row) in store.kv_list(CLOSED_ROWS, &format!("{ws}|")) {
        if let Some(value) = row.get("value") {
            let _ = store.kv_put(&text(&row, "bucket"), &text(&row, "key"), value, stamp);
            restored += 1;
        }
        let _ = store.kv_delete(CLOSED_ROWS, &key);
    }
    let _ = store.kv_delete(CLOSED_BUCKET, ws);
    json!({"ok": true, "name": text(&marker, "name"), "restored": restored})
}

/// Delete for good every workspace whose 30 days are over: its rows go back
/// for the one sweep that takes everything (`delete_workspace`). How many.
pub fn purge_closed(store: &Store, at: f64) -> usize {
    let due: Vec<String> = store
        .kv_list(CLOSED_BUCKET, "")
        .into_iter()
        .filter(|(_, marker)| num(marker, "purge_at") > 0.0 && num(marker, "purge_at") <= at)
        .map(|(ws, _)| ws)
        .collect();
    for ws in &due {
        restore_closed(store, ws);
        delete_workspace(store, ws, false);
    }
    due.len()
}

/// The hourly purge of workspaces past their 30 days, on its own thread.
#[cfg(feature = "cloud")]
pub fn start_purger(store: std::sync::Arc<Store>) {
    std::thread::Builder::new()
        .name("collide-closed-ws".into())
        .spawn(move || loop {
            let gone = purge_closed(&store, now());
            if gone > 0 {
                tracing::info!("workspaces: {gone} deleted workspace(s) past their 30 days purged");
            }
            std::thread::sleep(std::time::Duration::from_secs(3600));
        })
        .ok();
}

#[cfg(test)]
mod closed_tests {
    use super::*;

    #[test]
    fn a_deleted_workspace_vanishes_at_once_comes_back_on_restore_and_goes_after_30_days() {
        let db = Store::open(std::path::Path::new(":memory:")).expect("in-memory store");
        let (gone, kept) = ("aaaa1111bbbb", "cccc2222dddd");
        let scope = format!("{gone}:github.com/acme/web");
        db.kv_put("ws", gone, &json!({"id": gone, "name": "Acme", "owner": "u1"}), 1.0).unwrap();
        db.kv_put("ws", kept, &json!({"id": kept, "name": "Collide", "owner": "u1"}), 1.0).unwrap();
        db.kv_put("member", &format!("{gone}/u1"), &json!({"uid": "u1", "email": "bob@example.com", "role": "owner"}), 1.0).unwrap();
        db.kv_put("member", &format!("{kept}/u1"), &json!({"uid": "u1", "email": "bob@example.com", "role": "owner"}), 1.0).unwrap();
        // an index row names its workspace, as `add_member` writes it
        db.kv_put("uws", &format!("u1/{gone}"), &json!({"workspace": gone, "role": "owner"}), 1.0).unwrap();
        db.kv_put("uws", &format!("u1/{kept}"), &json!({"workspace": kept, "role": "owner"}), 1.0).unwrap();
        db.kv_put("msgthread", &format!("{scope}:m1"), &json!({"message": "rename"}), 1.0).unwrap();
        db.kv_put(crate::auth::TOKEN_BUCKET, "t1", &json!({"kind": "access", "uid": "u1", "workspace": gone}), 1.0).unwrap();

        let closed = close_workspace(&db, gone, "bob@example.com");
        assert_eq!(closed["ok"], json!(true), "{closed}");
        // gone at once: no record, no member, not listed, its credential refused
        assert!(get(&db, gone).is_none() && member(&db, gone, "u1").is_none());
        assert_eq!(list_for(&db, "u1").iter().map(|w| text(w, "id")).collect::<Vec<_>>(), vec![kept.to_string()]);
        assert!(db.kv_get(crate::auth::TOKEN_BUCKET, "t1").is_none());
        assert!(db.kv_list("msgthread", &format!("{scope}:")).is_empty());
        // the other workspace is untouched
        assert!(get(&db, kept).is_some() && member(&db, kept, "u1").is_some());

        // support restores it within the 30 days: all of it is back
        assert_eq!(restore_closed(&db, gone)["ok"], json!(true));
        assert!(get(&db, gone).is_some() && member(&db, gone, "u1").is_some());
        assert!(db.kv_get(crate::auth::TOKEN_BUCKET, "t1").is_some());
        assert_eq!(db.kv_list("msgthread", &format!("{scope}:")).len(), 1);
        assert!(db.kv_list(CLOSED_ROWS, "").is_empty() && db.kv_get(CLOSED_BUCKET, gone).is_none());

        // deleted again and left: nothing goes before its day, all of it after
        close_workspace(&db, gone, "bob@example.com");
        assert_eq!(purge_closed(&db, now() + 86_400.0), 0);
        assert_eq!(purge_closed(&db, now() + CLOSED_KEEP_S + 1.0), 1);
        assert!(get(&db, gone).is_none() && db.kv_list(CLOSED_ROWS, "").is_empty() && db.kv_get(CLOSED_BUCKET, gone).is_none());
        assert!(db.kv_get(crate::auth::TOKEN_BUCKET, "t1").is_none() && db.kv_list("msgthread", &format!("{scope}:")).is_empty());
        assert!(get(&db, kept).is_some());
    }
}

#[cfg(test)]
mod personal_tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("in-memory store")
    }

    fn person(uid: &str, email: &str, name: &str) -> Principal {
        Principal { uid: uid.into(), email: email.into(), name: name.into() }
    }

    #[test]
    fn a_new_account_gets_one_personal_workspace_named_after_its_first_name() {
        let s = store();
        let alice = person("u-alice", "alice@example.test", "Alice Smith");
        let made = ensure_personal(&s, &alice, &json!({})).expect("made at first sign-in");
        assert_eq!(text(&made, "name"), "Alice's workspace");
        assert_eq!(text(&made, "kind"), "personal");
        assert_eq!(text(&made, "owner"), "u-alice");
        assert_eq!(list_for(&s, "u-alice").len(), 1);
        assert_eq!(text(&personal_for(&s, "u-alice").unwrap(), "id"), text(&made, "id"));
    }

    #[test]
    fn making_it_is_idempotent_never_two() {
        let s = store();
        let alice = person("u-alice", "alice@example.test", "Alice");
        assert!(ensure_personal(&s, &alice, &json!({})).is_some());
        assert!(ensure_personal(&s, &alice, &json!({})).is_none(), "a second sign-in makes nothing");
        assert!(ensure_personal(&s, &alice, &json!({})).is_none());
        assert_eq!(owned_by(&s, "u-alice").len(), 1);
    }

    #[test]
    fn concurrent_first_loads_make_one() {
        let s = std::sync::Arc::new(store());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = std::sync::Arc::clone(&s);
                std::thread::spawn(move || {
                    ensure_personal(&s, &person("u-race", "r@example.test", "Rae"), &json!({})).is_some()
                })
            })
            .collect();
        let made = handles.into_iter().map(|h| h.join().unwrap()).filter(|made| *made).count();
        assert_eq!(made, 1);
        assert_eq!(owned_by(&s, "u-race").len(), 1);
    }

    #[test]
    fn an_account_that_has_one_already_gets_none() {
        let s = store();
        let ann = person("u-ann", "ann@example.test", "Ann");
        let existing = create(&s, &ann, "", "personal", "").unwrap();
        assert!(ensure_personal(&s, &ann, &json!({})).is_none());
        let mine = list_for(&s, "u-ann");
        assert_eq!(mine.len(), 1);
        assert_eq!(text(&mine[0], "id"), text(&existing, "id"));
    }

    #[test]
    fn an_account_already_in_a_workspace_gets_none() {
        // owns a team workspace (the Free plan's one): a second would break the cap
        let s = store();
        let bo = person("u-bo", "bo@example.test", "Bo");
        create(&s, &bo, "acme-eng", "custom", "").unwrap();
        assert!(ensure_personal(&s, &bo, &json!({})).is_none());
        assert_eq!(owned_by(&s, "u-bo").len(), 1);
        // invited first: a member somewhere already has a place
        let cy = person("u-cy", "cy@example.test", "Cy");
        let team = list_for(&s, "u-bo")[0].clone();
        add_member(&s, &text(&team, "id"), &cy, "member", "u-bo", now());
        assert!(ensure_personal(&s, &cy, &json!({})).is_none());
        assert!(owned_by(&s, "u-cy").is_empty());
    }

    #[test]
    fn never_again_once_made_or_the_account_was_closed() {
        let s = store();
        let di = person("u-di", "di@example.test", "Di");
        let made = ensure_personal(&s, &di, &json!({})).unwrap();
        // its only membership goes away: no second workspace appears
        s.kv_delete("uws", &format!("u-di/{}", text(&made, "id"))).unwrap();
        assert!(list_for(&s, "u-di").is_empty());
        assert!(ensure_personal(&s, &di, &json!({})).is_none());
        // a deleted account's stale token makes nothing either
        let ed = person("u-ed", "ed@example.test", "Ed");
        s.kv_put(PERSONAL_MADE_BUCKET, "u-ed", &json!({"closed": true}), now()).unwrap();
        assert!(ensure_personal(&s, &ed, &json!({})).is_none());
        assert!(list_for(&s, "u-ed").is_empty());
    }

    #[test]
    fn the_name_comes_from_the_name_then_the_username_then_the_email() {
        let named = person("u1", "x@example.test", "Alice Smith");
        assert_eq!(personal_name_for(&named, &json!({})), "Alice's workspace");
        assert_eq!(personal_name_for(&named, &json!({"name": "Bob Li"})), "Bob's workspace");
        let handle = person("u2", "x@example.test", "");
        assert_eq!(personal_name_for(&handle, &json!({"username": "alice-s"})), "alice-s's workspace");
        assert_eq!(personal_name_for(&handle, &json!({"github_login": "octo"})), "octo's workspace");
        assert_eq!(personal_name_for(&handle, &json!({})), "x's workspace");
        // a test principal whose "name" is its uid, and no email: not a name
        let bare = person("u3", "", "u3");
        assert_eq!(personal_name_for(&bare, &json!({})), "My workspace");
    }
}

#[cfg(test)]
mod delete_tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("in-memory store")
    }

    #[test]
    fn deleting_a_workspace_takes_its_rows_and_leaves_every_other_workspace_alone() {
        let db = store();
        let (gone, kept) = ("aaaa1111bbbb", "cccc2222dddd");
        let (scope, other_scope) = (format!("{gone}:github.com/acme/web"), format!("{kept}:github.com/acme/api"));
        db.kv_put("ws", gone, &json!({"id": gone, "name": "Acme", "owner": "u1"}), 1.0).unwrap();
        db.kv_put("ws", kept, &json!({"id": kept, "name": "Collide", "owner": "u2"}), 1.0).unwrap();
        db.kv_put("member", &format!("{gone}/u1"), &json!({"uid": "u1", "email": "bob@example.com", "role": "owner", "added": 1.0}), 1.0).unwrap();
        db.kv_put("member", &format!("{gone}/u2"), &json!({"uid": "u2", "email": "k@x.io", "role": "member", "added": 2.0}), 1.0).unwrap();
        db.kv_put("member", &format!("{kept}/u2"), &json!({"uid": "u2", "email": "k@x.io", "role": "owner", "added": 1.0}), 1.0).unwrap();
        for key in [format!("u1/{gone}"), format!("u2/{gone}"), format!("u2/{kept}")] {
            db.kv_put("uws", &key, &json!({"role": "member"}), 1.0).unwrap();
        }
        db.kv_put("ghrepo", &format!("{gone}:github.com/acme/web"), &json!({"repo_id": "github.com/acme/web"}), 1.0).unwrap();
        db.kv_put("msgthread", &format!("{scope}:m1"), &json!({"from": "k@x.io", "message": "rename"}), 1.0).unwrap();
        db.kv_put("inbox", &format!("{scope}:bob@example.com:m1"), &json!({"from": "k@x.io", "message": "rename"}), 1.0).unwrap();
        db.kv_put("msgthread", &format!("{other_scope}:m2"), &json!({"from": "k@x.io", "message": "stays"}), 1.0).unwrap();
        // credentials: one bound here, one that lists it among others, one elsewhere
        db.kv_put(crate::auth::TOKEN_BUCKET, "t1", &json!({"kind": "access", "uid": "u1", "workspace": gone}), 1.0).unwrap();
        db.kv_put(crate::auth::TOKEN_BUCKET, "t2", &json!({"kind": "access", "uid": "u2", "workspace": kept, "workspaces": [kept, gone]}), 1.0).unwrap();
        db.kv_put(crate::auth::TOKEN_BUCKET, "t3", &json!({"kind": "access", "uid": "u2", "workspace": kept}), 1.0).unwrap();
        // a pointer at the workspace goes; a record that only mentions it stays
        db.kv_put("watch", "u2:github.com/acme/web", &json!({"workspace": gone}), 1.0).unwrap();
        db.kv_put("history", "note", &json!({"said": format!("moved from {gone}")}), 1.0).unwrap();
        db.eph_set(&format!("intent:{scope}:i1"), &json!({"owner": "k@x.io"}), Some(600.0)).unwrap();
        db.eph_set(&format!("intent:{other_scope}:i2"), &json!({"owner": "k@x.io"}), Some(600.0)).unwrap();
        db.ledger_append(&scope, "agent_message", &json!({"to": "bob@example.com", "from": "k@x.io", "chars": 6}), 1.0).unwrap();

        let dry = delete_workspace(&db, gone, true);
        assert_eq!(dry["ok"], json!(true));
        assert_eq!(dry["dry_run"], json!(true));
        assert_eq!(dry["workspace"]["name"], json!("Acme"));
        assert_eq!(dry["members"], json!(["bob@example.com", "k@x.io"]));
        assert_eq!(dry["deleted"], json!({"ghrepo": 1, "inbox": 1, "member": 2, "msgthread": 1, "oauth_token": 1, "uws": 2, "watch": 1, "ws": 1}));
        assert_eq!(dry["credentials_narrowed"], json!(1));
        assert_eq!(dry["left"], json!({"history": 1}));
        assert_eq!(dry["ledger_rows_kept"], json!(1));
        assert!(db.kv_get("ws", gone).is_some() && db.kv_get("inbox", &format!("{scope}:bob@example.com:m1")).is_some(), "a dry run changes nothing");

        let applied = delete_workspace(&db, gone, false);
        assert_eq!(applied["deleted"], dry["deleted"]);
        assert!(db.kv_get("ws", gone).is_none());
        assert!(members(&db, gone).is_empty());
        assert!(db.kv_get("uws", &format!("u1/{gone}")).is_none() && db.kv_get("uws", &format!("u2/{gone}")).is_none());
        assert!(db.kv_list("msgthread", &format!("{scope}:")).is_empty() && db.kv_list("inbox", &format!("{scope}:")).is_empty());
        assert!(db.kv_get(crate::auth::TOKEN_BUCKET, "t1").is_none());
        assert_eq!(db.kv_get(crate::auth::TOKEN_BUCKET, "t2").unwrap()["workspaces"], json!([kept]));
        assert!(db.kv_get("watch", "u2:github.com/acme/web").is_none());
        assert!(db.eph_get(&format!("intent:{scope}:i1")).is_none());
        // everything of the other workspace, and the ledger, is where it was
        assert!(db.kv_get("ws", kept).is_some() && db.kv_get("member", &format!("{kept}/u2")).is_some());
        assert!(db.kv_get("uws", &format!("u2/{kept}")).is_some() && db.kv_get("msgthread", &format!("{other_scope}:m2")).is_some());
        assert!(db.kv_get(crate::auth::TOKEN_BUCKET, "t3").is_some() && db.kv_get("history", "note").is_some());
        assert!(db.eph_get(&format!("intent:{other_scope}:i2")).is_some());
        assert_eq!(db.ledger_count(&scope), 1);

        assert_eq!(delete_workspace(&db, gone, false)["error"], json!("no such workspace"));
        db.kv_put("ws", "w1", &json!({"id": "w1", "name": "Short"}), 1.0).unwrap();
        assert_eq!(delete_workspace(&db, "w1", false)["ok"], json!(false), "a short id would match strangers' rows");
    }
}
