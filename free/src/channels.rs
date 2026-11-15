//! Channels: a workspace's own rooms, the way a Discord server has them,
//! where its people and their agents talk and share files.
//!
//! - **A workspace is the server.** Its channels sit in categories, in an
//!   order its managers drag. A brand-new workspace gets `#general` the
//!   first time anyone opens Channels. (The cross-workspace group hubs of
//!   hubs.rs keep working beside them; the dashboard lists them as Shared.)
//! - **Roles and permissions** are Discord's, checked on every route here:
//!   the owner holds everything; everyone holds the `@everyone` role's
//!   permissions plus those of each role they were given. A channel's
//!   overrides then apply in Discord's order (`@everyone`'s, the member's
//!   roles' denies then allows, the member's own), so denying `@everyone`
//!   View channel makes a channel private. Without View channel a member
//!   has nothing in it; without Send messages, no attaching and no
//!   `@everyone` either. Roles have positions: a member who is not the
//!   owner manages only roles below their highest one, only members whose
//!   highest role is below theirs, and grants or denies only what they hold.
//! - **Agents are members through their person.** An agent sees and posts
//!   in the channels its person can see and post in, posting as
//!   "<person>'s <tool>". It hears a channel message (on its socket, else
//!   its hooks' inbox, through hubs.rs' delivery) when the message
//!   @mentions it (`@kim/2`), its person (`@kim`) or `@everyone`, or when
//!   the channel's "agents hear every message" is on. Like a hub, nothing
//!   unaddressed comes back to the poster's own agents, and the posting
//!   session never hears itself.
//! - **Files** are uploads (uploads.rs) made for one channel by one person,
//!   carried by the message that names them; deleting the message deletes
//!   them. Messages are read within the workspace's history window, as the
//!   ledger is, and their files last as long.
//! - **Unread** is a channel whose last message is newer than the member
//!   last read it; **mentions** count the messages that pinged them (or
//!   their agents) since.
//!
//! Storage (all kv):
//! - `chan` `{ws}/{id}` -> `{id, workspace, name, topic, category, position,
//!   overrides: [{type: role|member, id, allow, deny}], agents_hear_all,
//!   created_by, created, last}`; `chanid` `{id}` -> `{workspace}`
//! - `chancat` `{ws}/{id}` -> `{id, name, position}`
//! - `chanrole` `{ws}/{id}` -> `{id, name, color, position, perms}` (the
//!   `@everyone` role is `everyone`, position 0, made on first change)
//! - `chanmroles` `{ws}/{uid}` -> `{roles: [id]}`
//! - `chanmsg` `{chan}:{ts:017.6}:{id}` -> a message; a deleted one stays as
//!   `{id, ts, deleted}` so a reader polling for changes sees it go.
//!   `chanmsgid` `{chan}:{id}` -> `{key}`
//! - `chanread` `{chan}:{uid}` -> `{read_ts}`; `chanmention` `{chan}:{uid}`
//!   -> `{count}`; `chanboot` `{ws}` once `#general` was made
//! - eph `chanlive:{ws}:{uid}`: the person had Channels open lately.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use regex::Regex;
use serde_json::{json, Value};

use crate::store::{now, Store};
use crate::App;

pub const CHANNELS: &str = "chan";
pub const CHANNEL_IDS: &str = "chanid";
pub const CATEGORIES: &str = "chancat";
pub const ROLES: &str = "chanrole";
pub const MEMBER_ROLES: &str = "chanmroles";
pub const MESSAGES: &str = "chanmsg";
pub const MESSAGE_IDS: &str = "chanmsgid";
pub const READS: &str = "chanread";
pub const MENTIONS: &str = "chanmention";
pub const BOOTED: &str = "chanboot";

// ------------------------------------------------------------ permissions

pub const VIEW: u32 = 1;
pub const SEND: u32 = 1 << 1;
pub const ATTACH: u32 = 1 << 2;
pub const MENTION_EVERYONE: u32 = 1 << 3;
pub const MANAGE_MESSAGES: u32 = 1 << 4;
pub const MANAGE_CHANNELS: u32 = 1 << 5;
pub const MANAGE_ROLES: u32 = 1 << 6;
pub const ALL: u32 = (1 << 7) - 1;

/// Each permission's name, as the API and the stored records spell it.
pub const PERMISSIONS: [(u32, &str); 7] = [
    (VIEW, "view_channel"),
    (SEND, "send_messages"),
    (ATTACH, "attach_files"),
    (MENTION_EVERYONE, "mention_everyone"),
    (MANAGE_MESSAGES, "manage_messages"),
    (MANAGE_CHANNELS, "manage_channels"),
    (MANAGE_ROLES, "manage_roles"),
];

pub const EVERYONE: &str = "everyone";
/// What `@everyone` may do until someone changes it: read, write, attach
/// and ping the room; managing is a role's to give.
pub const EVERYONE_DEFAULT: u32 = VIEW | SEND | ATTACH | MENTION_EVERYONE;

pub const MAX_TEXT: usize = 4000;
pub const MAX_NAME: usize = 50;
pub const MAX_TOPIC: usize = 300;
pub const MAX_ROLE_NAME: usize = 40;
pub const MAX_CHANNELS: usize = 200;
pub const MAX_CATEGORIES: usize = 50;
pub const MAX_ROLES: usize = 50;
pub const MAX_ATTACHMENTS: usize = 10;
/// Messages one page holds, unless asked for fewer (or up to `MAX_PAGE`).
pub const PAGE: usize = 50;
pub const MAX_PAGE: usize = 100;
/// A person with Channels open is online this long after their last read.
const ONLINE_TTL_S: f64 = 90.0;
/// A reader polling for changes asks from a little before its last answer,
/// so a write committed while the last read ran is not missed.
const CHANGES_SLACK_S: f64 = 2.0;
/// How far back `changes` looks, at most.
const CHANGES_MAX_S: f64 = 86_400.0;

pub const NOT_MEMBER: &str = "not a member of this workspace";
const NO_CHANNEL: &str = "no such channel";
const NO_MESSAGE: &str = "no such message";

/// A refusal: the HTTP status and the line the dashboard shows.
pub type Refusal = (u16, String);

fn refuse<T>(status: u16, error: impl Into<String>) -> Result<T, Refusal> {
    Err((status, error.into()))
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn num(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

/// One process-wide lock around the read-modify-write records (positions,
/// a channel's last line): two managers dragging at once never interleave.
fn guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

pub fn names_of(bits: u32) -> Vec<&'static str> {
    PERMISSIONS.iter().filter(|(bit, _)| bits & bit != 0).map(|(_, name)| *name).collect()
}

/// A list of permission names as bits; a name that is not one is refused.
pub fn bits_of(list: Option<&Value>) -> Result<u32, String> {
    let mut bits = 0;
    for item in list.and_then(Value::as_array).into_iter().flatten() {
        let name = item.as_str().unwrap_or("");
        match PERMISSIONS.iter().find(|(_, n)| *n == name) {
            Some((bit, _)) => bits |= bit,
            None => return Err(format!("there is no permission called {name:?}")),
        }
    }
    Ok(bits)
}

/// A stored record's permission list as bits (a name no longer known is
/// skipped).
fn stored_bits(record: &Value, key: &str) -> u32 {
    strings(record.get(key)).iter().filter_map(|name| PERMISSIONS.iter().find(|(_, n)| n == name)).fold(0, |bits, (bit, _)| bits | bit)
}

// ------------------------------------------------------------------ roles

fn everyone_default() -> Value {
    json!({"id": EVERYONE, "name": "@everyone", "color": "", "position": 0, "perms": names_of(EVERYONE_DEFAULT)})
}

/// The workspace's roles, the highest first, `@everyone` last.
pub fn role_list(store: &Store, ws: &str) -> Vec<Value> {
    let mut roles: Vec<Value> = store.kv_list(ROLES, &format!("{ws}/")).into_iter().map(|(_, role)| role).collect();
    if !roles.iter().any(|r| text(r, "id") == EVERYONE) {
        roles.push(everyone_default());
    }
    roles.sort_by(|a, b| num(b, "position").partial_cmp(&num(a, "position")).unwrap_or(std::cmp::Ordering::Equal));
    roles
}

pub fn member_roles(store: &Store, ws: &str, uid: &str) -> Vec<String> {
    store.kv_get(MEMBER_ROLES, &format!("{ws}/{uid}")).map(|r| strings(r.get("roles"))).unwrap_or_default()
}

/// What one member of a workspace may do: read once, asked per channel.
pub struct Who {
    pub uid: String,
    pub owner: bool,
    pub roles: BTreeSet<String>,
    /// the workspace-wide permissions: `@everyone` and their roles together
    pub base: u32,
    /// their highest role's position (the owner's is above every role)
    pub top: i64,
}

impl Who {
    /// None when `uid` is not a member.
    pub fn of(store: &Store, ws: &str, uid: &str) -> Option<Who> {
        Who::with_roles(store, ws, uid, &role_list(store, ws))
    }

    fn with_roles(store: &Store, ws: &str, uid: &str, roles: &[Value]) -> Option<Who> {
        let member = crate::auth::member(store, ws, uid)?;
        let owner = text(&member, "role") == "owner";
        let held: BTreeSet<String> =
            member_roles(store, ws, uid).into_iter().filter(|id| roles.iter().any(|r| text(r, "id") == *id)).collect();
        let (mut base, mut top) = (0, 0);
        for role in roles {
            let id = text(role, "id");
            if id == EVERYONE || held.contains(&id) {
                base |= stored_bits(role, "perms");
                if id != EVERYONE {
                    top = top.max(num(role, "position") as i64);
                }
            }
        }
        if owner {
            (base, top) = (ALL, i64::MAX);
        }
        Some(Who { uid: uid.to_string(), owner, roles: held, base, top })
    }

    pub fn has(&self, bit: u32) -> bool {
        self.base & bit == bit
    }

    /// What they may do in one channel, its overrides applied in Discord's
    /// order: `@everyone`'s, their roles' (every deny, then every allow),
    /// their own. No View channel is nothing at all; no Send messages takes
    /// attaching and `@everyone` with it.
    pub fn channel(&self, chan: &Value) -> u32 {
        if self.owner {
            return ALL;
        }
        let overrides: Vec<Value> = chan.get("overrides").and_then(Value::as_array).cloned().unwrap_or_default();
        let one = |kind: &str, id: &str| overrides.iter().find(|o| text(o, "type") == kind && text(o, "id") == id);
        let mut bits = self.base;
        if let Some(o) = one("role", EVERYONE) {
            bits = (bits & !stored_bits(o, "deny")) | stored_bits(o, "allow");
        }
        let (mut deny, mut allow) = (0, 0);
        for o in overrides.iter().filter(|o| text(o, "type") == "role" && self.roles.contains(&text(o, "id"))) {
            deny |= stored_bits(o, "deny");
            allow |= stored_bits(o, "allow");
        }
        bits = (bits & !deny) | allow;
        if let Some(o) = one("member", &self.uid) {
            bits = (bits & !stored_bits(o, "deny")) | stored_bits(o, "allow");
        }
        if bits & VIEW == 0 {
            return 0;
        }
        if bits & SEND == 0 {
            bits &= !(ATTACH | MENTION_EVERYONE);
        }
        bits
    }

    /// May they manage this role: the owner any, anyone else with Manage
    /// roles one below their highest (`@everyone` is below everyone's).
    fn outranks(&self, role: &Value) -> bool {
        self.owner || (self.has(MANAGE_ROLES) && (text(role, "id") == EVERYONE || (num(role, "position") as i64) < self.top))
    }
}

fn who_or_refuse(store: &Store, ws: &str, uid: &str) -> Result<Who, Refusal> {
    Who::of(store, ws, uid).ok_or((403, NOT_MEMBER.to_string()))
}

/// `#rrggbb`, or "" for none.
fn clean_color(color: &str) -> Result<String, Refusal> {
    let color = color.trim().to_ascii_lowercase();
    if color.is_empty() || (color.len() == 7 && color.starts_with('#') && color[1..].bytes().all(|b| b.is_ascii_hexdigit())) {
        Ok(color)
    } else {
        refuse(400, "a colour is #rrggbb")
    }
}

fn clean_role_name(name: &str) -> Result<String, Refusal> {
    let name: String = name.trim().trim_start_matches('@').chars().filter(|c| !c.is_control()).take(MAX_ROLE_NAME).collect();
    if name.is_empty() {
        return refuse(400, "name the role");
    }
    if name.eq_ignore_ascii_case(EVERYONE) || name.eq_ignore_ascii_case("here") {
        return refuse(400, "that name is taken by @everyone");
    }
    Ok(name)
}

fn role_get(store: &Store, ws: &str, id: &str) -> Option<Value> {
    if id == EVERYONE {
        return Some(store.kv_get(ROLES, &format!("{ws}/{EVERYONE}")).unwrap_or_else(everyone_default));
    }
    store.kv_get(ROLES, &format!("{ws}/{id}"))
}

fn put_role(store: &Store, ws: &str, role: &Value) {
    let _ = store.kv_put(ROLES, &format!("{ws}/{}", text(role, "id")), role, now());
}

/// A role a manager may grant only what they hold themselves.
fn within_reach(who: &Who, changed: u32) -> Result<(), Refusal> {
    if who.owner || changed & !who.base == 0 {
        Ok(())
    } else {
        refuse(403, "you can only give or take a permission you hold yourself")
    }
}

/// A new role, just above `@everyone` (Discord's place for one), with the
/// permissions named (no more than the maker holds).
pub fn create_role(store: &Store, ws: &str, actor: &str, body: &Value) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    if !who.has(MANAGE_ROLES) {
        return refuse(403, "you can't make roles here: it takes Manage roles");
    }
    let name = clean_role_name(&text(body, "name"))?;
    let color = clean_color(&text(body, "color"))?;
    let perms = bits_of(body.get("perms")).map_err(|e| (400, e))?;
    within_reach(&who, perms)?;
    let _held = guard();
    let roles = role_list(store, ws);
    if roles.len() > MAX_ROLES {
        return refuse(400, format!("a workspace holds at most {MAX_ROLES} roles"));
    }
    // the others move up one to make room at the bottom
    for mut role in roles.into_iter().filter(|r| text(r, "id") != EVERYONE) {
        role["position"] = json!(num(&role, "position") as i64 + 1);
        put_role(store, ws, &role);
    }
    let role = json!({
        "id": format!("r{}", crate::compat::new_id()), "name": name, "color": color, "position": 1,
        "perms": names_of(perms), "created": now(),
    });
    put_role(store, ws, &role);
    Ok(role)
}

/// Rename, recolour or re-permission a role (`@everyone` keeps its name).
pub fn update_role(store: &Store, ws: &str, actor: &str, id: &str, body: &Value) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    let _held = guard();
    let Some(mut role) = role_get(store, ws, id) else { return refuse(404, "no such role") };
    if !who.outranks(&role) {
        return refuse(403, if who.has(MANAGE_ROLES) { "that role is not below yours" } else { "you can't change roles: it takes Manage roles" });
    }
    if let Some(name) = body.get("name").and_then(Value::as_str) {
        if id == EVERYONE {
            return refuse(400, "@everyone keeps its name");
        }
        role["name"] = json!(clean_role_name(name)?);
    }
    if let Some(color) = body.get("color").and_then(Value::as_str) {
        role["color"] = json!(clean_color(color)?);
    }
    if body.get("perms").is_some() {
        let next = bits_of(body.get("perms")).map_err(|e| (400, e))?;
        within_reach(&who, next ^ stored_bits(&role, "perms"))?;
        role["perms"] = json!(names_of(next));
    }
    put_role(store, ws, &role);
    Ok(role)
}

/// Delete a role: it leaves every member who had it and every channel
/// override that named it.
pub fn delete_role(store: &Store, ws: &str, actor: &str, id: &str) -> Result<(), Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    if id == EVERYONE {
        return refuse(400, "@everyone can't be deleted");
    }
    let _held = guard();
    let Some(role) = role_get(store, ws, id) else { return refuse(404, "no such role") };
    if !who.outranks(&role) {
        return refuse(403, if who.has(MANAGE_ROLES) { "that role is not below yours" } else { "you can't delete roles: it takes Manage roles" });
    }
    let _ = store.kv_delete(ROLES, &format!("{ws}/{id}"));
    for (key, mut held) in store.kv_list(MEMBER_ROLES, &format!("{ws}/")) {
        let roles = strings(held.get("roles"));
        if roles.iter().any(|r| r == id) {
            held["roles"] = json!(roles.into_iter().filter(|r| r != id).collect::<Vec<_>>());
            let _ = store.kv_put(MEMBER_ROLES, &key, &held, now());
        }
    }
    for mut chan in channels_of(store, ws) {
        let overrides: Vec<Value> = chan.get("overrides").and_then(Value::as_array).cloned().unwrap_or_default();
        if overrides.iter().any(|o| text(o, "type") == "role" && text(o, "id") == id) {
            chan["overrides"] = json!(overrides.into_iter().filter(|o| !(text(o, "type") == "role" && text(o, "id") == id)).collect::<Vec<_>>());
            put_channel(store, ws, &chan);
        }
    }
    // close the gap it left
    let rest: Vec<Value> = role_list(store, ws).into_iter().filter(|r| text(r, "id") != EVERYONE).collect();
    let count = rest.len() as i64;
    for (i, mut role) in rest.into_iter().enumerate() {
        role["position"] = json!(count - i as i64);
        put_role(store, ws, &role);
    }
    Ok(())
}

/// Put the roles in a new order, `ids` the highest first. Someone who is
/// not the owner moves only roles below their own, and only to places
/// below it.
pub fn reorder_roles(store: &Store, ws: &str, actor: &str, ids: &[String]) -> Result<(), Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    if !who.has(MANAGE_ROLES) {
        return refuse(403, "you can't reorder roles: it takes Manage roles");
    }
    let _held = guard();
    let roles: Vec<Value> = role_list(store, ws).into_iter().filter(|r| text(r, "id") != EVERYONE).collect();
    let known: BTreeSet<String> = roles.iter().map(|r| text(r, "id")).collect();
    let asked: BTreeSet<String> = ids.iter().cloned().collect();
    if known != asked || asked.len() != ids.len() {
        return refuse(400, "list every role once, the highest first");
    }
    let count = ids.len() as i64;
    let mut moved: Vec<Value> = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let Some(mut role) = roles.iter().find(|r| text(r, "id") == *id).cloned() else { continue };
        let (was, next) = (num(&role, "position") as i64, count - i as i64);
        if was == next {
            continue;
        }
        if !who.owner && (was >= who.top || next >= who.top) {
            return refuse(403, "you can only move roles below your own");
        }
        role["position"] = json!(next);
        moved.push(role);
    }
    for role in moved {
        put_role(store, ws, &role);
    }
    Ok(())
}

/// Give a member exactly these roles. Someone who is not the owner changes
/// only members below them, and only roles below their own.
pub fn set_member_roles(store: &Store, ws: &str, actor: &str, target: &str, ids: &[String]) -> Result<Vec<String>, Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    if !who.has(MANAGE_ROLES) {
        return refuse(403, "you can't give roles: it takes Manage roles");
    }
    let roles = role_list(store, ws);
    let Some(them) = Who::with_roles(store, ws, target, &roles) else { return refuse(404, "they are not a member of this workspace") };
    if !who.owner && target != actor && them.top >= who.top {
        return refuse(403, "their highest role is not below yours");
    }
    let next: BTreeSet<String> = ids.iter().cloned().collect();
    for id in &next {
        if id == EVERYONE || !roles.iter().any(|r| text(r, "id") == *id) {
            return refuse(404, "no such role");
        }
    }
    for id in next.symmetric_difference(&them.roles) {
        let role = roles.iter().find(|r| text(r, "id") == *id).cloned().unwrap_or_else(|| json!({"id": id, "position": i64::MAX}));
        if !who.outranks(&role) {
            return refuse(403, "you can only give or take roles below your own");
        }
    }
    let list: Vec<String> = next.into_iter().collect();
    let _ = store.kv_put(MEMBER_ROLES, &format!("{ws}/{target}"), &json!({"roles": list}), now());
    Ok(list)
}

/// The Roles screen: every role (highest first) with how many hold it and
/// whether the viewer may change it, every member with their roles, and
/// what the viewer may do.
pub fn roles_view(store: &Store, ws: &str, viewer: &str) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, viewer)?;
    let roles = role_list(store, ws);
    let members: Vec<Value> = crate::workspaces::members(store, ws)
        .iter()
        .map(|m| {
            let uid = text(m, "uid");
            let mut row = person_of(store, ws, &uid);
            row["owner"] = json!(text(m, "role") == "owner");
            row["roles"] = json!(member_roles(store, ws, &uid).into_iter().filter(|id| roles.iter().any(|r| text(r, "id") == *id)).collect::<Vec<_>>());
            if let Some(them) = Who::with_roles(store, ws, &uid, &roles) {
                row["manageable"] = json!(who.has(MANAGE_ROLES) && (who.owner || uid == viewer || them.top < who.top));
            }
            row
        })
        .collect();
    let rows: Vec<Value> = roles
        .iter()
        .map(|role| {
            let id = text(role, "id");
            let holders = members.iter().filter(|m| strings(m.get("roles")).contains(&id)).count();
            let mut row = role.clone();
            row["members"] = json!(if id == EVERYONE { members.len() } else { holders });
            row["editable"] = json!(who.outranks(role));
            row
        })
        .collect();
    Ok(json!({
        "roles": rows, "members": members, "permissions": PERMISSIONS.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
        "me": {"uid": viewer, "owner": who.owner, "perms": names_of(who.base)},
    }))
}

// ---------------------------------------------------------------- people

/// `{uid, username, name, avatar}`: hubs' person, named by the workspace's
/// membership when the profile has no name or username.
fn person_of(store: &Store, ws: &str, uid: &str) -> Value {
    let mut row = crate::hubs::person(store, uid);
    if text(&row, "name").is_empty() {
        let member = crate::auth::member(store, ws, uid).unwrap_or(json!({}));
        let email = text(&member, "email");
        let name = match text(&member, "name") {
            empty if empty.is_empty() => email.split('@').next().unwrap_or("").to_string(),
            named => named,
        };
        row["name"] = json!(if name.is_empty() { "someone".to_string() } else { name });
    }
    row
}

/// Which agent tool a session runs in: (key, how it is named), from what
/// its hooks said (the harness) or the MCP client's name.
pub fn tool_of(store: &Store, uid: &str, session: &str, agent: &str) -> (&'static str, &'static str) {
    let mut probe = agent.to_ascii_lowercase();
    if !session.is_empty() {
        let record = store.kv_get(crate::hubs::AGENT_NO, uid).unwrap_or(json!({}));
        if let Some(pair) = record.get("where").and_then(|w| w.get(session)).and_then(Value::as_array) {
            let (scope, label) = (pair.first().and_then(Value::as_str).unwrap_or(""), pair.get(1).and_then(Value::as_str).unwrap_or(""));
            probe = format!("{} {probe}", crate::freshness::session_facts(store, scope, label, session).harness);
        }
    }
    const TOOLS: [(&str, &str, &str); 9] = [
        ("claude", "claude", "Claude Code"),
        ("codex", "codex", "Codex"),
        ("openai", "codex", "Codex"),
        ("cursor", "cursor", "Cursor"),
        ("gemini", "gemini", "Gemini CLI"),
        ("hermes", "hermes", "Hermes"),
        ("opencode", "opencode", "opencode"),
        ("copilot", "copilot", "Copilot"),
        ("windsurf", "windsurf", "Windsurf"),
    ];
    TOOLS.iter().find(|(needle, _, _)| probe.contains(needle)).map(|(_, key, label)| (*key, *label)).unwrap_or(("agent", "agent"))
}

/// The session behind an agent number of one person.
fn session_of_agent(store: &Store, uid: &str, n: i64) -> Option<String> {
    let record = store.kv_get(crate::hubs::AGENT_NO, uid)?;
    record.get("sessions")?.as_object()?.iter().find(|(_, v)| v.as_i64() == Some(n)).map(|(session, _)| session.clone())
}

/// The workspace an agent session works in, from where Cork last saw it.
fn session_workspace(store: &Store, uid: &str, session: &str) -> String {
    let record = store.kv_get(crate::hubs::AGENT_NO, uid).unwrap_or(json!({}));
    record
        .get("where")
        .and_then(|w| w.get(session))
        .and_then(Value::as_array)
        .and_then(|pair| pair.first())
        .and_then(Value::as_str)
        .map(|scope| crate::presence::split_scope(scope).0.to_string())
        .unwrap_or_default()
}

fn mark_online(store: &Store, ws: &str, uid: &str) {
    let _ = store.eph_set(&format!("chanlive:{ws}:{uid}"), &json!({"ts": now()}), Some(ONLINE_TTL_S));
}

// --------------------------------------------------------------- channels

pub fn channel_get(store: &Store, ws: &str, id: &str) -> Option<Value> {
    if id.is_empty() {
        return None;
    }
    store.kv_get(CHANNELS, &format!("{ws}/{id}"))
}

fn put_channel(store: &Store, ws: &str, chan: &Value) {
    let _ = store.kv_put(CHANNELS, &format!("{ws}/{}", text(chan, "id")), chan, now());
}

pub fn channels_of(store: &Store, ws: &str) -> Vec<Value> {
    store.kv_list(CHANNELS, &format!("{ws}/")).into_iter().map(|(_, c)| c).collect()
}

fn categories_of(store: &Store, ws: &str) -> Vec<Value> {
    let mut cats: Vec<Value> = store.kv_list(CATEGORIES, &format!("{ws}/")).into_iter().map(|(_, c)| c).collect();
    cats.sort_by(|a, b| num(a, "position").partial_cmp(&num(b, "position")).unwrap_or(std::cmp::Ordering::Equal));
    cats
}

fn category_get(store: &Store, ws: &str, id: &str) -> Option<Value> {
    store.kv_get(CATEGORIES, &format!("{ws}/{id}"))
}

/// `c` and 12 hex characters: a channel's id (a hub's starts with `h`).
pub fn is_channel_id(id: &str) -> bool {
    id.len() == 13 && id.starts_with('c') && id[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn workspace_of_channel(store: &Store, id: &str) -> Option<String> {
    if !is_channel_id(id) {
        return None;
    }
    store.kv_get(CHANNEL_IDS, id).map(|r| text(&r, "workspace")).filter(|ws| !ws.is_empty())
}

/// Does `send_agent_message(hub=…)` name a channel: `#name`, or a
/// channel's id.
pub fn is_channel_ref(store: &Store, named: &str) -> bool {
    let named = named.trim();
    named.starts_with('#') || workspace_of_channel(store, named).is_some()
}

/// A channel's name the way Discord keeps one: lower case, a dash for each
/// run of spaces, letters, digits, dashes and underscores only.
pub fn channel_name(raw: &str) -> Result<String, Refusal> {
    let lowered = raw.trim().trim_start_matches('#').to_lowercase();
    let mut out = String::new();
    for c in lowered.chars() {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
        } else if (c == '-' || c.is_whitespace()) && !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_end_matches('-').chars().take(MAX_NAME).collect();
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        return refuse(400, "name the channel: letters, numbers and dashes");
    }
    Ok(out)
}

fn clean_topic(topic: &str) -> String {
    topic.trim().chars().filter(|c| !c.is_control() || *c == '\n').take(MAX_TOPIC).collect()
}

fn new_channel(store: &Store, ws: &str, name: &str, topic: &str, category: &str, position: i64, overrides: Vec<Value>, by: &str) -> Value {
    let stamp = now();
    let id = format!("c{}", crate::compat::new_id());
    let chan = json!({
        "id": id, "workspace": ws, "name": name, "topic": topic, "category": category, "position": position,
        "overrides": overrides, "agents_hear_all": false, "created_by": by, "created": stamp, "last": null,
    });
    put_channel(store, ws, &chan);
    let _ = store.kv_put(CHANNEL_IDS, &id, &json!({"workspace": ws}), stamp);
    chan
}

fn new_category(store: &Store, ws: &str, name: &str, position: i64) -> Value {
    let cat = json!({"id": format!("k{}", crate::compat::new_id()), "name": name, "position": position});
    let _ = store.kv_put(CATEGORIES, &format!("{ws}/{}", text(&cat, "id")), &cat, now());
    cat
}

/// The first time a workspace's channels are read: a category and
/// `#general` in it, so nobody starts in an empty room. Once only: a
/// workspace whose managers delete every channel stays empty.
pub fn ensure_default(store: &Store, ws: &str) {
    if store.kv_get(BOOTED, ws).is_some() {
        return;
    }
    let _held = guard();
    if store.kv_get(BOOTED, ws).is_some() {
        return;
    }
    if channels_of(store, ws).is_empty() {
        let cat = new_category(store, ws, "Text channels", 0);
        new_channel(store, ws, "general", "Your team and your agents, in one room.", &text(&cat, "id"), 0, Vec::new(), "");
    }
    let stamp = now();
    let _ = store.kv_put(BOOTED, ws, &json!({"ts": stamp}), stamp);
}

/// `@everyone` may not see it.
fn is_private(chan: &Value) -> bool {
    chan.get("overrides").and_then(Value::as_array).into_iter().flatten().any(|o| {
        text(o, "type") == "role" && text(o, "id") == EVERYONE && stored_bits(o, "deny") & VIEW != 0
    })
}

fn read_ts(store: &Store, chan: &str, uid: &str) -> f64 {
    store.kv_get(READS, &format!("{chan}:{uid}")).map(|r| num(&r, "read_ts")).unwrap_or(0.0)
}

/// They have read the channel up to now: unread and mentions clear.
pub fn mark_read(store: &Store, chan: &str, uid: &str) {
    let stamp = now();
    let _ = store.kv_put(READS, &format!("{chan}:{uid}"), &json!({"read_ts": stamp}), stamp);
    let _ = store.kv_delete(MENTIONS, &format!("{chan}:{uid}"));
}

fn bump_mention(store: &Store, chan: &str, uid: &str) {
    let key = format!("{chan}:{uid}");
    let count = store.kv_get(MENTIONS, &key).map(|m| num(&m, "count") as i64).unwrap_or(0);
    let _ = store.kv_put(MENTIONS, &key, &json!({"count": count + 1}), now());
}

/// One channel as a member sees it in the list.
fn channel_view(store: &Store, chan: &Value, who: &Who, bits: u32) -> Value {
    let id = text(chan, "id");
    let read = read_ts(store, &id, &who.uid);
    let last = chan.get("last").filter(|l| l.is_object());
    // what they posted themselves on the dashboard is read; their agents' is news
    let unread = last.is_some_and(|l| num(l, "ts") > read && !(text(l, "uid") == who.uid && text(l, "session").is_empty()));
    let mentions = store.kv_get(MENTIONS, &format!("{id}:{}", who.uid)).map(|m| num(&m, "count") as i64).unwrap_or(0);
    let mut row = json!({
        "id": id, "name": text(chan, "name"), "topic": text(chan, "topic"), "category": text(chan, "category"),
        "position": num(chan, "position") as i64, "private": is_private(chan),
        "agents_hear_all": chan.get("agents_hear_all").and_then(Value::as_bool).unwrap_or(false),
        "perms": names_of(bits), "unread": unread, "mentions": mentions, "read_ts": read,
        "last_ts": last.map(|l| num(l, "ts")).unwrap_or(0.0),
    });
    if bits & (MANAGE_ROLES | MANAGE_CHANNELS) != 0 {
        row["overrides"] = chan.get("overrides").cloned().unwrap_or(json!([]));
    }
    row
}

fn sorted_channels(store: &Store, ws: &str) -> Vec<Value> {
    let order: BTreeMap<String, i64> =
        categories_of(store, ws).iter().map(|c| (text(c, "id"), num(c, "position") as i64)).collect();
    let mut channels = channels_of(store, ws);
    // uncategorized first, as Discord shows them, then each category in turn
    channels.sort_by_key(|c| {
        let cat = text(c, "category");
        let place = if cat.is_empty() { -1 } else { order.get(&cat).copied().unwrap_or(i64::MAX / 2) };
        (place, num(c, "position") as i64, text(c, "name"))
    });
    channels
}

/// The channel list: the channels the viewer can see, in order, the
/// categories, and what the viewer may do here.
pub fn list(store: &Store, ws: &str, viewer: &str) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, viewer)?;
    ensure_default(store, ws);
    mark_online(store, ws, viewer);
    let channels: Vec<Value> = sorted_channels(store, ws)
        .iter()
        .filter_map(|chan| {
            let bits = who.channel(chan);
            (bits & VIEW != 0).then(|| channel_view(store, chan, &who, bits))
        })
        .collect();
    let name = crate::workspaces::get(store, ws).map(|w| text(&w, "name")).unwrap_or_default();
    Ok(json!({
        "workspace": {"id": ws, "name": name},
        "channels": channels,
        "categories": categories_of(store, ws),
        "me": {"uid": viewer, "owner": who.owner, "perms": names_of(who.base), "roles": who.roles},
    }))
}

/// A new channel at the end of its category. A private one hides from
/// `@everyone`; its maker keeps it.
pub fn create_channel(store: &Store, ws: &str, actor: &str, body: &Value) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    if !who.has(MANAGE_CHANNELS) {
        return refuse(403, "you can't make channels here: it takes Manage channels");
    }
    let name = channel_name(&text(body, "name"))?;
    let category = text(body, "category");
    let private = body.get("private").and_then(Value::as_bool).unwrap_or(false);
    let _held = guard();
    let existing = channels_of(store, ws);
    if existing.len() >= MAX_CHANNELS {
        return refuse(400, format!("a workspace holds at most {MAX_CHANNELS} channels"));
    }
    if existing.iter().any(|c| text(c, "name") == name) {
        return refuse(409, format!("there is already a #{name}"));
    }
    if !category.is_empty() && category_get(store, ws, &category).is_none() {
        return refuse(404, "no such category");
    }
    let position = existing.iter().filter(|c| text(c, "category") == category).map(|c| num(c, "position") as i64 + 1).max().unwrap_or(0);
    let mut overrides = Vec::new();
    if private {
        overrides.push(json!({"type": "role", "id": EVERYONE, "allow": [], "deny": ["view_channel"]}));
        if !who.owner {
            overrides.push(json!({"type": "member", "id": actor, "allow": ["view_channel"], "deny": []}));
        }
    }
    let chan = new_channel(store, ws, &name, &clean_topic(&text(body, "topic")), &category, position, overrides, actor);
    Ok(channel_view(store, &chan, &who, who.channel(&chan)))
}

/// Rename, re-topic, move to another category, or let agents hear every
/// message: Manage channels in that channel.
pub fn update_channel(store: &Store, ws: &str, actor: &str, id: &str, body: &Value) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    let _held = guard();
    let Some(mut chan) = channel_get(store, ws, id).filter(|c| who.channel(c) & VIEW != 0) else { return refuse(404, NO_CHANNEL) };
    if who.channel(&chan) & MANAGE_CHANNELS == 0 {
        return refuse(403, "you can't change this channel: it takes Manage channels");
    }
    if let Some(name) = body.get("name").and_then(Value::as_str) {
        let name = channel_name(name)?;
        if channels_of(store, ws).iter().any(|c| text(c, "name") == name && text(c, "id") != id) {
            return refuse(409, format!("there is already a #{name}"));
        }
        chan["name"] = json!(name);
    }
    if let Some(topic) = body.get("topic").and_then(Value::as_str) {
        chan["topic"] = json!(clean_topic(topic));
    }
    if let Some(category) = body.get("category").and_then(Value::as_str) {
        if !category.is_empty() && category_get(store, ws, category).is_none() {
            return refuse(404, "no such category");
        }
        if category != text(&chan, "category") {
            let end = channels_of(store, ws).iter().filter(|c| text(c, "category") == category).map(|c| num(c, "position") as i64 + 1).max().unwrap_or(0);
            chan["category"] = json!(category);
            chan["position"] = json!(end);
        }
    }
    if let Some(flag) = body.get("agents_hear_all").and_then(Value::as_bool) {
        chan["agents_hear_all"] = json!(flag);
    }
    put_channel(store, ws, &chan);
    Ok(channel_view(store, &chan, &who, who.channel(&chan)))
}

/// Replace a channel's permission overrides. It takes Manage roles in the
/// channel; someone who is not the owner touches only roles below theirs
/// and allows or denies only what they hold in it.
pub fn set_overrides(store: &Store, ws: &str, actor: &str, id: &str, overrides: &Value) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    let _held = guard();
    let Some(mut chan) = channel_get(store, ws, id).filter(|c| who.channel(c) & VIEW != 0) else { return refuse(404, NO_CHANNEL) };
    let mine = who.channel(&chan);
    if mine & MANAGE_ROLES == 0 {
        return refuse(403, "you can't change who may do what here: it takes Manage roles");
    }
    let Some(list) = overrides.as_array() else { return refuse(400, "overrides is a list") };
    if list.len() > 100 {
        return refuse(400, "a channel holds at most 100 overrides");
    }
    let roles = role_list(store, ws);
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut clean: Vec<Value> = Vec::new();
    for o in list {
        let (kind, target) = (text(o, "type"), text(o, "id"));
        match kind.as_str() {
            "role" => {
                let Some(role) = roles.iter().find(|r| text(r, "id") == target) else { return refuse(404, "no such role") };
                if !who.owner && target != EVERYONE && num(role, "position") as i64 >= who.top {
                    return refuse(403, "that role is not below yours");
                }
            }
            "member" => {
                if crate::auth::member(store, ws, &target).is_none() {
                    return refuse(404, "they are not a member of this workspace");
                }
            }
            _ => return refuse(400, "an override is for a role or a member"),
        }
        if !seen.insert((kind.clone(), target.clone())) {
            return refuse(400, "one override per role or member");
        }
        let allow = bits_of(o.get("allow")).map_err(|e| (400, e))?;
        let deny = bits_of(o.get("deny")).map_err(|e| (400, e))? & !allow;
        if allow | deny != 0 {
            clean.push(json!({"type": kind, "id": target, "allow": names_of(allow), "deny": names_of(deny)}));
        }
    }
    if !who.owner {
        let old: Vec<Value> = chan.get("overrides").and_then(Value::as_array).cloned().unwrap_or_default();
        let bits = |rows: &[Value], kind: &str, target: &str, key: &str| {
            rows.iter().find(|o| text(o, "type") == kind && text(o, "id") == target).map(|o| stored_bits(o, key)).unwrap_or(0)
        };
        let mut changed = 0;
        for o in old.iter().chain(clean.iter()) {
            let (kind, target) = (text(o, "type"), text(o, "id"));
            for key in ["allow", "deny"] {
                changed |= bits(&old, &kind, &target, key) ^ bits(&clean, &kind, &target, key);
            }
        }
        if changed & !mine != 0 {
            return refuse(403, "you can only allow or deny what you can do in this channel yourself");
        }
    }
    chan["overrides"] = json!(clean);
    put_channel(store, ws, &chan);
    Ok(channel_view(store, &chan, &who, who.channel(&chan)))
}

/// Delete a channel and everything said in it. The answer is the uploads
/// its messages carried, for the caller to delete from the bucket.
pub fn delete_channel(store: &Store, ws: &str, actor: &str, id: &str) -> Result<Vec<String>, Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    let Some(chan) = channel_get(store, ws, id).filter(|c| who.channel(c) & VIEW != 0) else { return refuse(404, NO_CHANNEL) };
    if who.channel(&chan) & MANAGE_CHANNELS == 0 {
        return refuse(403, "you can't delete this channel: it takes Manage channels");
    }
    let mut files = Vec::new();
    for (key, message) in store.kv_list(MESSAGES, &format!("{id}:")) {
        files.extend(attachment_ids(&message));
        let _ = store.kv_delete(MESSAGES, &key);
    }
    for bucket in [MESSAGE_IDS, READS, MENTIONS] {
        for (key, _) in store.kv_list(bucket, &format!("{id}:")) {
            let _ = store.kv_delete(bucket, &key);
        }
    }
    let _ = store.kv_delete(CHANNELS, &format!("{ws}/{id}"));
    let _ = store.kv_delete(CHANNEL_IDS, id);
    Ok(files)
}

fn attachment_ids(message: &Value) -> Vec<String> {
    message.get("attachments").and_then(Value::as_array).into_iter().flatten().map(|a| text(a, "id")).filter(|id| !id.is_empty()).collect()
}

fn require_manager(who: &Who) -> Result<(), Refusal> {
    if who.has(MANAGE_CHANNELS) { Ok(()) } else { refuse(403, "it takes Manage channels") }
}

fn clean_category_name(name: &str) -> Result<String, Refusal> {
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    if name.is_empty() { refuse(400, "name the category") } else { Ok(name) }
}

pub fn create_category(store: &Store, ws: &str, actor: &str, name: &str) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    require_manager(&who)?;
    let name = clean_category_name(name)?;
    let _held = guard();
    let cats = categories_of(store, ws);
    if cats.len() >= MAX_CATEGORIES {
        return refuse(400, format!("a workspace holds at most {MAX_CATEGORIES} categories"));
    }
    let end = cats.iter().map(|c| num(c, "position") as i64 + 1).max().unwrap_or(0);
    Ok(new_category(store, ws, &name, end))
}

pub fn rename_category(store: &Store, ws: &str, actor: &str, id: &str, name: &str) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    require_manager(&who)?;
    let Some(mut cat) = category_get(store, ws, id) else { return refuse(404, "no such category") };
    cat["name"] = json!(clean_category_name(name)?);
    let _ = store.kv_put(CATEGORIES, &format!("{ws}/{id}"), &cat, now());
    Ok(cat)
}

/// Delete a category; its channels stay, out of any category.
pub fn delete_category(store: &Store, ws: &str, actor: &str, id: &str) -> Result<(), Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    require_manager(&who)?;
    let _held = guard();
    if category_get(store, ws, id).is_none() {
        return refuse(404, "no such category");
    }
    let loose = channels_of(store, ws).iter().filter(|c| text(c, "category").is_empty()).count() as i64;
    let mut moving: Vec<Value> = channels_of(store, ws).into_iter().filter(|c| text(c, "category") == id).collect();
    moving.sort_by_key(|c| num(c, "position") as i64);
    for (i, mut chan) in moving.into_iter().enumerate() {
        chan["category"] = json!("");
        chan["position"] = json!(loose + i as i64);
        put_channel(store, ws, &chan);
    }
    let _ = store.kv_delete(CATEGORIES, &format!("{ws}/{id}"));
    Ok(())
}

/// The list as a manager dragged it: `{categories: [{id, channels: [id]}],
/// channels: [id]}` top to bottom, `channels` those in no category. A
/// channel the manager cannot see keeps its category, after the others.
pub fn reorder(store: &Store, ws: &str, actor: &str, layout: &Value) -> Result<(), Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    require_manager(&who)?;
    let _held = guard();
    let cats = categories_of(store, ws);
    let groups: Vec<Value> = layout.get("categories").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut order: Vec<String> = groups.iter().map(|g| text(g, "id")).filter(|id| cats.iter().any(|c| text(c, "id") == *id)).collect();
    order.dedup();
    for cat in &cats {
        if !order.contains(&text(cat, "id")) {
            order.push(text(cat, "id"));
        }
    }
    for (i, id) in order.iter().enumerate() {
        if let Some(mut cat) = cats.iter().find(|c| text(c, "id") == *id).cloned() {
            if num(&cat, "position") as i64 != i as i64 {
                cat["position"] = json!(i);
                let _ = store.kv_put(CATEGORIES, &format!("{ws}/{id}"), &cat, now());
            }
        }
    }
    let mut placed: BTreeMap<String, (String, i64)> = BTreeMap::new();
    for group in &groups {
        let cat = text(group, "id");
        if !cats.iter().any(|c| text(c, "id") == cat) {
            continue;
        }
        for (j, id) in strings(group.get("channels")).into_iter().enumerate() {
            placed.entry(id).or_insert((cat.clone(), j as i64));
        }
    }
    for (j, id) in strings(layout.get("channels")).into_iter().enumerate() {
        placed.entry(id).or_insert((String::new(), j as i64));
    }
    let mut channels = channels_of(store, ws);
    for chan in channels.iter_mut() {
        let place = placed.get(&text(chan, "id")).filter(|_| who.channel(chan) & VIEW != 0).cloned();
        let (cat, position) = place.unwrap_or_else(|| (text(chan, "category"), 10_000 + num(chan, "position") as i64));
        chan["category"] = json!(cat);
        chan["position"] = json!(position);
    }
    // each category numbered 0, 1, 2… again
    channels.sort_by_key(|c| (text(c, "category"), num(c, "position") as i64));
    let mut at: BTreeMap<String, i64> = BTreeMap::new();
    for chan in channels.iter_mut() {
        let n = at.entry(text(chan, "category")).or_insert(0);
        chan["position"] = json!(*n);
        *n += 1;
    }
    let before: BTreeMap<String, Value> = channels_of(store, ws).into_iter().map(|c| (text(&c, "id"), c)).collect();
    for chan in channels {
        let was = before.get(&text(&chan, "id"));
        if was.map(|w| (text(w, "category"), num(w, "position") as i64)) != Some((text(&chan, "category"), num(&chan, "position") as i64)) {
            put_channel(store, ws, &chan);
        }
    }
    Ok(())
}

// --------------------------------------------------------------- messages

/// Who posts: a person on the dashboard (`session` empty) or one of their
/// agent sessions.
pub struct Poster<'a> {
    pub uid: &'a str,
    pub session: &'a str,
    pub agent: &'a str,
    pub via: &'a str,
}

/// Who a message named: `@everyone` (only when the poster may), people,
/// and single agents (`@kim/2`, by the session behind the number).
#[derive(Default, Debug)]
struct Mentions {
    everyone: bool,
    people: Vec<String>,
    agents: Vec<(String, i64, String)>,
}

impl Mentions {
    fn to_json(&self) -> Value {
        json!({
            "everyone": self.everyone,
            "people": self.people,
            "agents": self.agents.iter().map(|(uid, n, _)| json!({"uid": uid, "agent_no": n})).collect::<Vec<_>>(),
        })
    }
}

/// The mentions in a message's words, outside code. A name that is not a
/// member here mentions no one.
fn parse_mentions(store: &Store, ws: &str, said: &str, may_ping_everyone: bool) -> Mentions {
    static CODE: OnceLock<Regex> = OnceLock::new();
    static AT: OnceLock<Regex> = OnceLock::new();
    let code = CODE.get_or_init(|| Regex::new(r"```[\s\S]*?(```|$)|`[^`\n]*`").expect("valid pattern"));
    let at = AT.get_or_init(|| Regex::new(r"(?:^|[^A-Za-z0-9_@/.-])@([A-Za-z0-9][A-Za-z0-9_-]{1,23})(?:/(\d{1,4}))?").expect("valid pattern"));
    let bare = code.replace_all(said, " ");
    let mut found = Mentions::default();
    for caps in at.captures_iter(&bare) {
        let name = caps[1].to_lowercase();
        if name == EVERYONE || name == "here" {
            found.everyone |= may_ping_everyone;
            continue;
        }
        let Some(uid) = crate::hubs::uid_for_username(store, &name) else { continue };
        if crate::auth::member(store, ws, &uid).is_none() {
            continue;
        }
        match caps.get(2).and_then(|n| n.as_str().parse::<i64>().ok()) {
            Some(n) => {
                if let Some(session) = session_of_agent(store, &uid, n) {
                    if !found.agents.iter().any(|(u, m, _)| *u == uid && *m == n) {
                        found.agents.push((uid, n, session));
                    }
                }
            }
            None => {
                if !found.people.contains(&uid) {
                    found.people.push(uid);
                }
            }
        }
    }
    found
}

fn message_key(chan: &str, stamp: f64, id: &str) -> String {
    format!("{chan}:{stamp:017.6}:{id}")
}

/// A message and the key it is kept under.
fn message_get(store: &Store, chan: &str, id: &str) -> Option<(String, Value)> {
    let key = store.kv_get(MESSAGE_IDS, &format!("{chan}:{id}")).map(|r| text(&r, "key"))?;
    store.kv_get(MESSAGES, &key).map(|m| (key, m))
}

/// A message's `from`: who, and for an agent which one and in what tool.
fn from_of(store: &Store, ws: &str, poster: &Poster) -> Value {
    let person = person_of(store, ws, poster.uid);
    let (agent_no, tool, tool_label, title) = if poster.session.is_empty() {
        (Value::Null, "", "", String::new())
    } else {
        let (tool, label) = tool_of(store, poster.uid, poster.session, poster.agent);
        let record = store.kv_get(crate::hubs::AGENT_NO, poster.uid).unwrap_or(json!({}));
        let title = match record.get("where").and_then(|w| w.get(poster.session)).and_then(Value::as_array) {
            Some(pair) => crate::freshness::title_of(
                store, pair.first().and_then(Value::as_str).unwrap_or(""), pair.get(1).and_then(Value::as_str).unwrap_or(""), poster.session),
            None => String::new(),
        };
        (json!(crate::hubs::agent_no(store, poster.uid, poster.session)), tool, label, title)
    };
    json!({
        "uid": poster.uid, "username": text(&person, "username"), "name": text(&person, "name"), "avatar": text(&person, "avatar"),
        "session": poster.session, "agent_no": agent_no, "agent": poster.agent, "tool": tool, "tool_label": tool_label,
        "title": title, "via": poster.via,
    })
}

/// How a message's sender is written for an agent: `@kim/2`, `@kim`, or by
/// name when they have no username.
fn sender_tag(from: &Value) -> String {
    let username = text(from, "username");
    let n = from.get("agent_no").and_then(Value::as_i64);
    match (username.is_empty(), n) {
        (false, Some(n)) => format!("@{username}/{n}"),
        (false, None) => format!("@{username}"),
        (true, Some(_)) => format!("{}'s {}", text(from, "name"), text(from, "tool_label")),
        (true, None) => text(from, "name"),
    }
}

/// How a sender is shown to people: "Kim Lee", or "Kim's Claude Code".
fn display_name(from: &Value) -> String {
    let name = text(from, "name");
    if text(from, "session").is_empty() {
        return name;
    }
    let first = name.split_whitespace().next().unwrap_or("someone").to_string();
    let tool = text(from, "tool_label");
    format!("{first}'s {}", if tool.is_empty() { "agent" } else { tool.as_str() })
}

/// What an agent reads: where it was said, by whom, what it answers, the
/// files' links, and how to reply.
fn label(chan_name: &str, ws_name: &str, chan_id: &str, message: &Value) -> String {
    let from = message.get("from").cloned().unwrap_or(json!({}));
    let mut body = text(message, "text");
    if let Some(reply) = message.get("reply_to").filter(|r| r.is_object()) {
        let quoted: String = text(reply, "text").chars().take(80).collect();
        body = format!("(replying to {}: \"{quoted}\") {body}", text(reply, "from_name"));
    }
    let files: Vec<String> = message
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|a| format!("{} {}", text(a, "name"), text(a, "url")))
        .collect();
    if !files.is_empty() {
        body = format!("{} Files: {}", body.trim(), files.join(", "));
    }
    let place = if ws_name.is_empty() { format!("#{chan_name}") } else { format!("#{chan_name} in {ws_name}") };
    format!(
        "[{place} · {}] {} — reply with send_agent_message(hub=\"{chan_id}\", message=\"...\")",
        sender_tag(&from),
        body.trim()
    )
}

/// Post a message. `attachments` are uploads made for this channel by the
/// same person and not posted yet; `reply_to` a message in it. The answer
/// is the message as readers get it.
#[allow(clippy::too_many_arguments)]
pub fn post_message(
    store: &Store, ws: &str, chan_id: &str, poster: &Poster, said: &str, reply_to: &str, attachments: &[String],
    live: &dyn Fn(&str, &str, &str, &str) -> Vec<String>,
) -> Result<Value, Refusal> {
    let roles = role_list(store, ws);
    let Some(who) = Who::with_roles(store, ws, poster.uid, &roles) else { return refuse(403, NOT_MEMBER) };
    let Some(chan) = channel_get(store, ws, chan_id).filter(|c| who.channel(c) & VIEW != 0) else { return refuse(404, NO_CHANNEL) };
    let bits = who.channel(&chan);
    let chan_name = text(&chan, "name");
    if bits & SEND == 0 {
        return refuse(403, format!("you can't send messages in #{chan_name}"));
    }
    let said: String = said.trim().chars().take(MAX_TEXT).collect();
    if said.is_empty() && attachments.is_empty() {
        return refuse(400, "nothing to send: write a message first");
    }
    if attachments.len() > MAX_ATTACHMENTS {
        return refuse(400, format!("at most {MAX_ATTACHMENTS} files a message"));
    }
    if !attachments.is_empty() && bits & ATTACH == 0 {
        return refuse(403, format!("you can't attach files in #{chan_name}"));
    }
    let mut files: Vec<(String, Value)> = Vec::new();
    for id in attachments {
        let record = crate::uploads::is_id(id).then(|| store.kv_get(crate::uploads::KV_BUCKET, id)).flatten().filter(|r| {
            text(r, "workspace") == ws && text(r, "channel") == chan_id && text(r, "by_uid") == poster.uid && text(r, "attached").is_empty()
        });
        let Some(record) = record else { return refuse(400, "a file is not yours to post here: upload it to this channel first") };
        if !files.iter().any(|(f, _)| f == id) {
            files.push((id.clone(), record));
        }
    }
    let reply = if reply_to.trim().is_empty() {
        Value::Null
    } else {
        let Some((_, parent)) = message_get(store, chan_id, reply_to.trim()).filter(|(_, m)| !m.get("deleted").and_then(Value::as_bool).unwrap_or(false))
        else {
            return refuse(404, "the message you replied to is gone");
        };
        let from = parent.get("from").cloned().unwrap_or(json!({}));
        json!({
            "id": text(&parent, "id"), "text": text(&parent, "text").chars().take(200).collect::<String>(),
            "from_uid": text(&from, "uid"), "from_name": sender_tag(&from), "from_display": display_name(&from),
            "agent_no": from.get("agent_no").cloned().unwrap_or(Value::Null),
            "has_files": !attachment_ids(&parent).is_empty(),
        })
    };
    let mentions = parse_mentions(store, ws, &said, bits & MENTION_EVERYONE != 0);
    let stamp = now();
    let id = crate::compat::new_id();
    let key = message_key(chan_id, stamp, &id);
    for (file, record) in &files {
        let mut record = record.clone();
        record["attached"] = json!(id);
        let _ = store.kv_put(crate::uploads::KV_BUCKET, file, &record, stamp);
    }
    let mut message = json!({
        "id": id, "channel": chan_id, "ts": stamp, "text": said, "from": from_of(store, ws, poster), "reply_to": reply,
        "attachments": files.iter().map(|(file, record)| crate::uploads::attachment_of(file, record)).collect::<Vec<_>>(),
        "mentions": mentions.to_json(), "edited": null,
    });
    let _ = store.kv_put(MESSAGES, &key, &message, stamp);
    let _ = store.kv_put(MESSAGE_IDS, &format!("{chan_id}:{id}"), &json!({"key": key}), stamp);
    {
        let _held = guard();
        if let Some(mut current) = channel_get(store, ws, chan_id) {
            let preview: String = if said.is_empty() { format!("{} file(s)", files.len()) } else { said.chars().take(140).collect() };
            current["last"] = json!({"ts": stamp, "uid": poster.uid, "session": poster.session, "text": preview, "name": message["from"]["name"]});
            put_channel(store, ws, &current);
        }
    }
    if poster.session.is_empty() {
        mark_read(store, chan_id, poster.uid);
    }
    let reached = deliver(store, ws, &chan, &roles, &message, &mentions, poster, live);
    crate::push::arrived();
    message["cursor"] = json!(cursor_of(&key));
    message["delivered_to"] = json!(reached);
    Ok(message)
}

/// Hand a message to the agents that should hear it, and count the
/// mentions. How many people's agents it reached.
#[allow(clippy::too_many_arguments)]
fn deliver(
    store: &Store, ws: &str, chan: &Value, roles: &[Value], message: &Value, mentions: &Mentions, poster: &Poster,
    live: &dyn Fn(&str, &str, &str, &str) -> Vec<String>,
) -> usize {
    let chan_id = text(chan, "id");
    let chan_name = text(chan, "name");
    let ws_name = crate::workspaces::get(store, ws).map(|w| text(&w, "name")).unwrap_or_default();
    let labelled = label(&chan_name, &ws_name, &chan_id, message);
    let from = message.get("from").cloned().unwrap_or(json!({}));
    let (id, said, tag, hub_name) = (text(message, "id"), text(message, "text"), sender_tag(&from), format!("#{chan_name}"));
    let handed = crate::hubs::Handed {
        id: &id, hub_id: &chan_id, hub_name: &hub_name, from_uid: poster.uid, from_session: poster.session, from_tag: &tag,
        from_name: json!(text(&from, "name")), said: &said, labelled: &labelled, sent_ts: num(message, "ts"),
    };
    let hear_all = chan.get("agents_hear_all").and_then(Value::as_bool).unwrap_or(false);
    let mut reached = 0;
    for member in crate::workspaces::members(store, ws) {
        let uid = text(&member, "uid");
        let Some(who) = Who::with_roles(store, ws, &uid, roles) else { continue };
        if who.channel(chan) & VIEW == 0 {
            continue;
        }
        let own = uid == poster.uid;
        let person = mentions.people.contains(&uid);
        let agents: Vec<&String> = mentions.agents.iter().filter(|(u, _, _)| *u == uid).map(|(_, _, s)| s).collect();
        // the badge: anyone pinged, but not a person by their own hand
        if (person || !agents.is_empty() || mentions.everyone) && !(own && poster.session.is_empty()) {
            bump_mention(store, &chan_id, &uid);
        }
        if person {
            crate::hubs::hand_over(store, &handed, &uid, "", live);
        } else if !agents.is_empty() {
            for session in agents {
                crate::hubs::hand_over(store, &handed, &uid, session, live);
            }
        } else if !own && (mentions.everyone || hear_all) {
            crate::hubs::hand_over(store, &handed, &uid, "", live);
        } else {
            continue;
        }
        reached += 1;
    }
    reached
}

/// Change a message's words: only its author (the same person, from the
/// same place: the dashboard, or the same agent session).
pub fn edit(store: &Store, ws: &str, chan_id: &str, actor: &Poster, msg_id: &str, said: &str) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, actor.uid)?;
    let Some(chan) = channel_get(store, ws, chan_id).filter(|c| who.channel(c) & VIEW != 0) else { return refuse(404, NO_CHANNEL) };
    let Some((key, mut message)) = message_get(store, chan_id, msg_id).filter(|(_, m)| !m.get("deleted").and_then(Value::as_bool).unwrap_or(false))
    else {
        return refuse(404, NO_MESSAGE);
    };
    let from = message.get("from").cloned().unwrap_or(json!({}));
    if text(&from, "uid") != actor.uid || text(&from, "session") != actor.session {
        return refuse(403, "you can only edit your own messages");
    }
    let said: String = said.trim().chars().take(MAX_TEXT).collect();
    if said.is_empty() && attachment_ids(&message).is_empty() {
        return refuse(400, "a message needs words: delete it instead");
    }
    let mentions = parse_mentions(store, ws, &said, who.channel(&chan) & MENTION_EVERYONE != 0);
    let stamp = now();
    message["text"] = json!(said);
    message["edited"] = json!(stamp);
    message["mentions"] = mentions.to_json();
    let _ = store.kv_put(MESSAGES, &key, &message, stamp);
    message["cursor"] = json!(cursor_of(&key));
    Ok(message)
}

/// Delete a message: its author's (any of their sessions), or anyone's with
/// Manage messages. The answer is the uploads it carried, to delete.
pub fn remove(store: &Store, ws: &str, chan_id: &str, actor: &str, msg_id: &str) -> Result<Vec<String>, Refusal> {
    let who = who_or_refuse(store, ws, actor)?;
    let Some(chan) = channel_get(store, ws, chan_id).filter(|c| who.channel(c) & VIEW != 0) else { return refuse(404, NO_CHANNEL) };
    let Some((key, message)) = message_get(store, chan_id, msg_id).filter(|(_, m)| !m.get("deleted").and_then(Value::as_bool).unwrap_or(false))
    else {
        return refuse(404, NO_MESSAGE);
    };
    let author = message.get("from").map(|f| text(f, "uid")).unwrap_or_default();
    if author != actor && who.channel(&chan) & MANAGE_MESSAGES == 0 {
        return refuse(403, "you can only delete your own messages: anyone's takes Manage messages");
    }
    let files = attachment_ids(&message);
    let stamp = now();
    let tombstone = json!({"id": msg_id, "channel": chan_id, "ts": num(&message, "ts"), "deleted": true, "from": {"uid": author}});
    let _ = store.kv_put(MESSAGES, &key, &tombstone, stamp);
    Ok(files)
}

/// `{ts:017.6}:{id}`: where a message sits, for a page that starts before it.
fn cursor_of(key: &str) -> String {
    key.split_once(':').map(|(_, rest)| rest.to_string()).unwrap_or_default()
}

fn valid_cursor(cursor: &str) -> bool {
    !cursor.is_empty() && cursor.len() < 64 && cursor.bytes().all(|b| b.is_ascii_hexdigit() || b == b'.' || b == b':')
}

/// The oldest a message may be and still be read: the plan's history window.
fn window_start(store: &Store, ws: &str) -> f64 {
    crate::access::plan_for(store, ws).history_window_s().map(|window| now() - window).unwrap_or(0.0)
}

/// One page of a channel, oldest first: the newest messages, or those
/// before `before` (a message's cursor), at most `limit`, within the
/// history window. `server_ts` is what to ask `changes` from. (Reading the
/// newest page on the dashboard marks the channel read: the route's call.)
pub fn page(store: &Store, ws: &str, chan_id: &str, viewer: &str, before: &str, limit: usize) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, viewer)?;
    if channel_get(store, ws, chan_id).filter(|c| who.channel(c) & VIEW != 0).is_none() {
        return refuse(404, NO_CHANNEL);
    }
    let started = now();
    let limit = limit.clamp(1, MAX_PAGE);
    let oldest = window_start(store, ws);
    let prefix = format!("{chan_id}:");
    let mut from = if before.is_empty() {
        String::new()
    } else if valid_cursor(before) {
        format!("{prefix}{before}")
    } else {
        return refuse(400, "that is not a message's cursor");
    };
    let mut shown: Vec<Value> = Vec::new();
    let mut more = false;
    // tombstones are skipped, so a page may take a few reads to fill
    'reads: for _ in 0..8 {
        let rows = store.kv_page(MESSAGES, &prefix, &from, limit + 1);
        let exhausted = rows.len() <= limit;
        for (key, mut message) in rows {
            from = key.clone();
            if num(&message, "ts") < oldest {
                break 'reads;
            }
            if message.get("deleted").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            if shown.len() == limit {
                more = true;
                break 'reads;
            }
            message["cursor"] = json!(cursor_of(&key));
            shown.push(message);
        }
        if exhausted {
            break;
        }
    }
    shown.reverse();
    mark_online(store, ws, viewer);
    Ok(json!({"messages": shown, "has_more": more, "server_ts": started - CHANGES_SLACK_S}))
}

/// What changed in a channel since `since` (a `server_ts`): new messages,
/// edits, and deletions (`deleted: true`), oldest first, from no further
/// back than `CHANGES_MAX_S` (a reader away longer reads a page again).
pub fn changes(store: &Store, ws: &str, chan_id: &str, viewer: &str, since: f64) -> Result<Value, Refusal> {
    let who = who_or_refuse(store, ws, viewer)?;
    if channel_get(store, ws, chan_id).filter(|c| who.channel(c) & VIEW != 0).is_none() {
        return refuse(404, NO_CHANNEL);
    }
    let started = now();
    let since = since.max(started - CHANGES_MAX_S);
    let oldest = window_start(store, ws);
    let mut rows: Vec<(String, Value)> = store
        .kv_list_since(MESSAGES, &format!("{chan_id}:"), since)
        .into_iter()
        .filter(|(_, m, _)| num(m, "ts") >= oldest)
        .map(|(key, mut message, _)| {
            message["cursor"] = json!(cursor_of(&key));
            (key, message)
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    mark_online(store, ws, viewer);
    Ok(json!({"messages": rows.into_iter().map(|(_, m)| m).collect::<Vec<_>>(), "server_ts": started - CHANGES_SLACK_S}))
}

/// The member list beside a channel: who can see it, grouped by their
/// highest role while online and together while not, each with their
/// agents under them.
pub fn members_view(store: &Store, ws: &str, chan_id: &str, viewer: &str) -> Result<Value, Refusal> {
    let roles = role_list(store, ws);
    let Some(who) = Who::with_roles(store, ws, viewer, &roles) else { return refuse(403, NOT_MEMBER) };
    let Some(chan) = channel_get(store, ws, chan_id).filter(|c| who.channel(c) & VIEW != 0) else { return refuse(404, NO_CHANNEL) };
    let mut groups: Vec<(String, Value, Vec<Value>)> = roles
        .iter()
        .filter(|r| text(r, "id") != EVERYONE)
        .map(|r| (text(r, "id"), json!({"id": text(r, "id"), "name": text(r, "name"), "color": text(r, "color")}), Vec::new()))
        .collect();
    let mut plain: Vec<Value> = Vec::new();
    let mut offline: Vec<Value> = Vec::new();
    for member in crate::workspaces::members(store, ws) {
        let uid = text(&member, "uid");
        let Some(them) = Who::with_roles(store, ws, &uid, &roles) else { continue };
        if them.channel(&chan) & VIEW == 0 {
            continue;
        }
        let agents: Vec<Value> = crate::hubs::agents_of(store, &uid)
            .into_iter()
            .map(|mut a| {
                let (tool, label) = tool_of(store, &uid, &text(&a, "session"), "");
                a["tool"] = json!(tool);
                a["tool_label"] = json!(label);
                a
            })
            .collect();
        let online = store.eph_get(&format!("chanlive:{ws}:{uid}")).is_some() || agents.iter().any(|a| a["live"] == json!(true));
        let held: Vec<&Value> = roles.iter().filter(|r| them.roles.contains(&text(r, "id"))).collect();
        let color = held.iter().map(|r| text(r, "color")).find(|c| !c.is_empty()).unwrap_or_default();
        let mut row = person_of(store, ws, &uid);
        row["owner"] = json!(them.owner);
        row["online"] = json!(online);
        row["color"] = json!(color);
        row["roles"] = json!(held.iter().map(|r| text(r, "id")).collect::<Vec<_>>());
        row["agents"] = json!(agents);
        if !online {
            offline.push(row);
        } else if let Some(top) = held.first() {
            if let Some(group) = groups.iter_mut().find(|(id, _, _)| *id == text(top, "id")) {
                group.2.push(row);
            }
        } else {
            plain.push(row);
        }
    }
    let by_name = |rows: &mut Vec<Value>| rows.sort_by_key(|r| text(r, "name").to_lowercase());
    let mut out: Vec<Value> = Vec::new();
    for (_, mut head, mut rows) in groups.into_iter().filter(|(_, _, rows)| !rows.is_empty()) {
        by_name(&mut rows);
        head["members"] = json!(rows);
        out.push(head);
    }
    for (id, name, mut rows) in [("online", "Online", plain), ("offline", "Offline", offline)] {
        if !rows.is_empty() {
            by_name(&mut rows);
            out.push(json!({"id": id, "name": name, "color": "", "members": rows}));
        }
    }
    Ok(json!({"groups": out}))
}

// ----------------------------------------------------------------- agents

/// The channel an agent names (`#general`, or an id): in the workspace its
/// session works in first, else the one of its person's workspaces that
/// has it. (workspace, channel id).
fn resolve_for_agent(store: &Store, uid: &str, session: &str, named: &str) -> Result<(String, String), String> {
    let named = named.trim();
    if let Some(ws) = workspace_of_channel(store, named) {
        let visible = Who::of(store, &ws, uid).zip(channel_get(store, &ws, named)).is_some_and(|(who, c)| who.channel(&c) & VIEW != 0);
        return if visible { Ok((ws, named.to_string())) } else { Err("you are in no channel with that id".into()) };
    }
    let name = channel_name(named).map_err(|(_, e)| e)?;
    let here = session_workspace(store, uid, session);
    let mut found: Vec<(String, String)> = Vec::new();
    for (_, entry) in store.kv_list("uws", &format!("{uid}/")) {
        let ws = text(&entry, "workspace");
        let Some(who) = Who::of(store, &ws, uid) else { continue };
        if let Some(chan) = channels_of(store, &ws).into_iter().find(|c| text(c, "name") == name && who.channel(c) & VIEW != 0) {
            found.push((ws, text(&chan, "id")));
        }
    }
    if let Some(mine) = found.iter().find(|(ws, _)| *ws == here) {
        return Ok(mine.clone());
    }
    match found.len() {
        0 => Err(format!("you are in no channel called #{name}")),
        1 => Ok(found.remove(0)),
        _ => Err(format!(
            "#{name} is in several of your workspaces: name it by id ({})",
            found.iter().map(|(_, id)| format!("hub=\"{id}\"")).collect::<Vec<_>>().join(" or ")
        )),
    }
}

/// `send_agent_message(hub="#general", message=…)` from an agent session:
/// a post to that channel as "<person>'s <tool>". `to` names people or
/// agents to mention (`"@lee, @max/2"`), put before the words.
#[allow(clippy::too_many_arguments)]
pub fn agent_post(
    store: &Store, uid: &str, session: &str, agent: &str, named: &str, said: &str, to: &str,
    live: &dyn Fn(&str, &str, &str, &str) -> Vec<String>,
) -> Value {
    let fail = |error: String| json!({"ok": false, "error": error});
    let (ws, chan_id) = match resolve_for_agent(store, uid, session, named) {
        Ok(found) => found,
        Err(error) => return fail(error),
    };
    if !crate::ratelimit::allow_message("channels", uid) {
        return fail("a lot of messages at once: wait a few seconds".into());
    }
    let pings: Vec<String> = to
        .split([',', ' '])
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| if h.starts_with('@') { h.to_string() } else { format!("@{h}") })
        .filter(|h| !said.contains(h.as_str()))
        .collect();
    let said = if pings.is_empty() { said.to_string() } else { format!("{} {}", pings.join(" "), said.trim()) };
    let poster = Poster { uid, session, agent, via: "mcp" };
    match post_message(store, &ws, &chan_id, &poster, &said, "", &[], live) {
        Ok(message) => json!({
            "ok": true, "channel": chan_id, "channel_name": format!("#{}", channel_get(store, &ws, &chan_id).map(|c| text(&c, "name")).unwrap_or_default()),
            "id": message["id"], "delivered_to": message["delivered_to"],
        }),
        Err((_, error)) => fail(error),
    }
}

/// The channels of the workspace an agent session works in, for its
/// briefing: None when it has none it can see.
pub fn brief_part(store: &Store, uid: &str, session: &str) -> Option<String> {
    if uid.is_empty() || session.is_empty() {
        return None;
    }
    let ws = session_workspace(store, uid, session);
    if ws.is_empty() {
        return None;
    }
    let who = Who::of(store, &ws, uid)?;
    let names: Vec<String> =
        sorted_channels(store, &ws).iter().filter(|c| who.channel(c) & VIEW != 0).map(|c| format!("#{}", text(c, "name"))).collect();
    let first = names.first()?.clone();
    let more = if names.len() > 8 { format!(" and {} more", names.len() - 8) } else { String::new() };
    Some(format!(
        "Your workspace's channels: {}{more}. Post with send_agent_message(hub=\"{first}\", message=...); you hear a channel message when it @mentions you, your person or @everyone.",
        names.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
    ))
}

// ----------------------------------------------------------------- routes

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/api/workspaces/:workspace_id/channels", get(api_list).post(api_create))
        .route("/api/workspaces/:workspace_id/channel-order", put(api_reorder))
        .route("/api/workspaces/:workspace_id/channels/:channel_id", patch(api_update).delete(api_delete))
        .route("/api/workspaces/:workspace_id/channels/:channel_id/overrides", put(api_overrides))
        .route("/api/workspaces/:workspace_id/channels/:channel_id/members", get(api_members))
        .route("/api/workspaces/:workspace_id/channels/:channel_id/messages", get(api_messages).post(api_post))
        .route(
            "/api/workspaces/:workspace_id/channels/:channel_id/messages/:message_id",
            patch(api_edit).delete(api_remove),
        )
        .route("/api/workspaces/:workspace_id/channels/:channel_id/read", post(api_read))
        .route("/api/workspaces/:workspace_id/channels/:channel_id/files", post(api_upload))
        .route("/api/workspaces/:workspace_id/categories", post(api_create_category))
        .route("/api/workspaces/:workspace_id/categories/:category_id", patch(api_rename_category).delete(api_delete_category))
        .route("/api/workspaces/:workspace_id/roles", get(api_roles).post(api_create_role))
        .route("/api/workspaces/:workspace_id/role-order", put(api_reorder_roles))
        .route("/api/workspaces/:workspace_id/roles/:role_id", patch(api_update_role).delete(api_delete_role))
        .route("/api/workspaces/:workspace_id/members/:member_uid/roles", put(api_member_roles))
}

type Answer = (StatusCode, Json<Value>);

fn status(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST)
}

fn answer(result: Result<Value, Refusal>) -> Answer {
    match result {
        Ok(body) => (StatusCode::OK, Json(body)),
        Err((code, error)) => (status(code), Json(json!({"ok": false, "error": error}))),
    }
}

/// The caller, a member of the workspace in the path; `hooks` when they
/// came with a hooks credential (`cat_…`), which may post as one of
/// their agent sessions.
struct Caller {
    uid: String,
    label: String,
    hooks: bool,
}

fn caller(app: &App, headers: &HeaderMap, ws: &str) -> Result<Caller, Answer> {
    let principal = app.dashboard_auth.principal_of(&app.store, headers, "", now());
    let hooks = principal.as_ref().is_some_and(|p| !p.workspace.is_empty());
    match crate::auth::workspace_auth(&app.store, principal, ws) {
        Ok(auth) if auth.workspace == ws => Ok(Caller { uid: auth.uid, label: auth.user_id, hooks }),
        Ok(_) => Err((StatusCode::FORBIDDEN, Json(json!({"error": "credential is bound to another workspace"})))),
        Err((code, body)) => Err((status(code), Json(body))),
    }
}

fn live_for(app: &App) -> impl Fn(&str, &str, &str, &str) -> Vec<String> + '_ {
    move |uid: &str, only: &str, skip: &str, said: &str| app.protocol.deliver_to_person(uid, only, skip, "channel", said)
}

/// Delete uploads from the bucket, off the request's thread.
fn forget_files(app: &App, files: Vec<String>) {
    if files.is_empty() {
        return;
    }
    let store = app.store.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let Some(bucket) = crate::s3::Bucket::from_env() else {
            for id in files {
                let _ = store.kv_delete(crate::uploads::KV_BUCKET, &id);
            }
            return;
        };
        for id in files {
            let _ = crate::uploads::forget(&store, &bucket, &id);
        }
    });
}

macro_rules! member {
    ($app:expr, $headers:expr, $ws:expr) => {
        match caller(&$app, &$headers, &$ws) {
            Ok(caller) => caller,
            Err(refused) => return refused,
        }
    };
}

async fn api_list(State(app): State<Arc<App>>, Path(ws): Path<String>, headers: HeaderMap) -> Answer {
    let me = member!(app, headers, ws);
    answer(list(&app.store, &ws, &me.uid))
}

async fn api_create(State(app): State<Arc<App>>, Path(ws): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> Answer {
    let me = member!(app, headers, ws);
    answer(create_channel(&app.store, &ws, &me.uid, &body))
}

async fn api_reorder(State(app): State<Arc<App>>, Path(ws): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> Answer {
    let me = member!(app, headers, ws);
    answer(reorder(&app.store, &ws, &me.uid, &body).map(|_| json!({"ok": true})))
}

async fn api_update(
    State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap, Json(body): Json<Value>,
) -> Answer {
    let me = member!(app, headers, ws);
    answer(update_channel(&app.store, &ws, &me.uid, &id, &body))
}

async fn api_delete(State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap) -> Answer {
    let me = member!(app, headers, ws);
    match delete_channel(&app.store, &ws, &me.uid, &id) {
        Ok(files) => {
            forget_files(&app, files);
            answer(Ok(json!({"ok": true})))
        }
        Err(refused) => answer(Err(refused)),
    }
}

async fn api_overrides(
    State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap, Json(body): Json<Value>,
) -> Answer {
    let me = member!(app, headers, ws);
    answer(set_overrides(&app.store, &ws, &me.uid, &id, body.get("overrides").unwrap_or(&Value::Null)))
}

async fn api_members(State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap) -> Answer {
    let me = member!(app, headers, ws);
    answer(members_view(&app.store, &ws, &id, &me.uid))
}

/// `GET …/messages`: the newest page, `?before=<cursor>` an older one, or
/// `?since=<server_ts>` what changed since the last answer.
async fn api_messages(
    State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap, Query(query): Query<Value>,
) -> Answer {
    let me = member!(app, headers, ws);
    let since = text(&query, "since").trim().parse::<f64>().ok().filter(|s| s.is_finite());
    if let Some(since) = since {
        return answer(changes(&app.store, &ws, &id, &me.uid, since));
    }
    let limit = text(&query, "limit").trim().parse::<usize>().unwrap_or(PAGE);
    let before = text(&query, "before");
    let read = page(&app.store, &ws, &id, &me.uid, before.trim(), limit);
    // the person read it; their agent reading it through the hooks did not
    if read.is_ok() && before.trim().is_empty() && !me.hooks {
        mark_read(&app.store, &id, &me.uid);
    }
    answer(read)
}

/// `POST …/messages` `{text, reply_to?, attachments?: [upload id]}`; with
/// a hooks credential also `session` and `agent`, to post as that agent.
async fn api_post(
    State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap, Json(body): Json<Value>,
) -> Answer {
    let me = member!(app, headers, ws);
    if text(&body, "text").trim().chars().count() > MAX_TEXT {
        return answer(refuse(400, format!("a message is at most {MAX_TEXT} characters")));
    }
    if !crate::ratelimit::allow_message("channels", &me.uid) {
        return answer(refuse(429, "a lot of messages at once: wait a few seconds"));
    }
    let (session, agent) = if me.hooks { (text(&body, "session"), text(&body, "agent")) } else { (String::new(), String::new()) };
    if me.hooks && !session.is_empty() {
        crate::hubs::note_session(&app.store, &me.uid, &session, "", "");
    }
    let via = if me.hooks { "cli" } else { "dashboard" };
    let agent = if agent.is_empty() { via.to_string() } else { agent };
    let poster = Poster { uid: &me.uid, session: &session, agent: &agent, via };
    let live = live_for(&app);
    let files = strings(body.get("attachments"));
    answer(post_message(&app.store, &ws, &id, &poster, &text(&body, "text"), &text(&body, "reply_to"), &files, &live))
}

async fn api_edit(
    State(app): State<Arc<App>>, Path((ws, id, message_id)): Path<(String, String, String)>, headers: HeaderMap,
    Json(body): Json<Value>,
) -> Answer {
    let me = member!(app, headers, ws);
    if text(&body, "text").trim().chars().count() > MAX_TEXT {
        return answer(refuse(400, format!("a message is at most {MAX_TEXT} characters")));
    }
    let session = if me.hooks { text(&body, "session") } else { String::new() };
    let actor = Poster { uid: &me.uid, session: &session, agent: "", via: "" };
    answer(edit(&app.store, &ws, &id, &actor, &message_id, &text(&body, "text")))
}

async fn api_remove(
    State(app): State<Arc<App>>, Path((ws, id, message_id)): Path<(String, String, String)>, headers: HeaderMap,
) -> Answer {
    let me = member!(app, headers, ws);
    match remove(&app.store, &ws, &id, &me.uid, &message_id) {
        Ok(files) => {
            forget_files(&app, files);
            answer(Ok(json!({"ok": true})))
        }
        Err(refused) => answer(Err(refused)),
    }
}

async fn api_read(State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap) -> Answer {
    let me = member!(app, headers, ws);
    let Some(who) = Who::of(&app.store, &ws, &me.uid) else { return answer(refuse(403, NOT_MEMBER)) };
    if channel_get(&app.store, &ws, &id).filter(|c| who.channel(c) & VIEW != 0).is_none() {
        return answer(refuse(404, NO_CHANNEL));
    }
    mark_read(&app.store, &id, &me.uid);
    answer(Ok(json!({"ok": true})))
}

/// `POST …/files?name=<file name>`, the body the file's bytes: an upload
/// for a message in this channel (Attach files there), answered with its
/// id for the message to name. Up to `uploads::file_cap` by plan. A hooks
/// credential may upload, so an agent can share a file.
async fn api_upload(
    State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap, Query(query): Query<Value>,
    body: axum::body::Body,
) -> Answer {
    let me = member!(app, headers, ws);
    let Some(who) = Who::of(&app.store, &ws, &me.uid) else { return answer(refuse(403, NOT_MEMBER)) };
    let Some(chan) = channel_get(&app.store, &ws, &id).filter(|c| who.channel(c) & VIEW != 0) else { return answer(refuse(404, NO_CHANNEL)) };
    if who.channel(&chan) & ATTACH == 0 {
        return answer(refuse(403, format!("you can't attach files in #{}", text(&chan, "name"))));
    }
    let Some(bucket) = crate::s3::Bucket::from_env() else {
        return answer(refuse(503, "file sharing is not configured on this server"));
    };
    let plan = crate::billing::plan_of(&app.store, &ws).get("plan").and_then(Value::as_str).unwrap_or("free").to_string();
    let cap = crate::uploads::file_cap(&plan);
    let too_big = format!("the file is over {} MB, this plan's limit", cap / (1024 * 1024));
    let declared_len = headers.get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok());
    if declared_len.is_some_and(|len| len > cap) {
        return answer(refuse(413, too_big));
    }
    // one byte over the ceiling is enough to know it is over
    let Ok(bytes) = axum::body::to_bytes(body, cap + 1).await else { return answer(refuse(413, too_big)) };
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let name = text(&query, "name");
    let public_url = crate::routes_workspaces::public_url_of(&app, &headers);
    let store = app.store.clone();
    let (ws_c, id_c, label, uid) = (ws.clone(), id.clone(), me.label.clone(), me.uid.clone());
    let (code, result) = tokio::task::spawn_blocking(move || {
        let owner = crate::uploads::FileFor { workspace: &ws_c, channel: &id_c, by: &label, by_uid: &uid };
        crate::uploads::save_file(&store, &bucket, &owner, &name, &content_type, &bytes, cap, &public_url)
    })
    .await
    .unwrap_or_else(|_| (500, json!({"ok": false, "error": "the upload failed"})));
    (status(code), Json(result))
}

async fn api_create_category(State(app): State<Arc<App>>, Path(ws): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> Answer {
    let me = member!(app, headers, ws);
    answer(create_category(&app.store, &ws, &me.uid, &text(&body, "name")))
}

async fn api_rename_category(
    State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap, Json(body): Json<Value>,
) -> Answer {
    let me = member!(app, headers, ws);
    answer(rename_category(&app.store, &ws, &me.uid, &id, &text(&body, "name")))
}

async fn api_delete_category(State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap) -> Answer {
    let me = member!(app, headers, ws);
    answer(delete_category(&app.store, &ws, &me.uid, &id).map(|_| json!({"ok": true})))
}

async fn api_roles(State(app): State<Arc<App>>, Path(ws): Path<String>, headers: HeaderMap) -> Answer {
    let me = member!(app, headers, ws);
    answer(roles_view(&app.store, &ws, &me.uid))
}

async fn api_create_role(State(app): State<Arc<App>>, Path(ws): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> Answer {
    let me = member!(app, headers, ws);
    answer(create_role(&app.store, &ws, &me.uid, &body))
}

async fn api_reorder_roles(State(app): State<Arc<App>>, Path(ws): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> Answer {
    let me = member!(app, headers, ws);
    answer(reorder_roles(&app.store, &ws, &me.uid, &strings(body.get("roles"))).map(|_| json!({"ok": true})))
}

async fn api_update_role(
    State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap, Json(body): Json<Value>,
) -> Answer {
    let me = member!(app, headers, ws);
    answer(update_role(&app.store, &ws, &me.uid, &id, &body))
}

async fn api_delete_role(State(app): State<Arc<App>>, Path((ws, id)): Path<(String, String)>, headers: HeaderMap) -> Answer {
    let me = member!(app, headers, ws);
    answer(delete_role(&app.store, &ws, &me.uid, &id).map(|_| json!({"ok": true})))
}

async fn api_member_roles(
    State(app): State<Arc<App>>, Path((ws, uid)): Path<(String, String)>, headers: HeaderMap, Json(body): Json<Value>,
) -> Answer {
    let me = member!(app, headers, ws);
    answer(set_member_roles(&app.store, &ws, &me.uid, &uid, &strings(body.get("roles"))).map(|roles| json!({"ok": true, "roles": roles})))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "ws1";

    fn none(_: &str, _: &str, _: &str, _: &str) -> Vec<String> {
        Vec::new()
    }

    fn join(db: &Store, uid: &str, username: &str, role: &str) {
        db.kv_put("member", &format!("{WS}/{uid}"), &json!({"uid": uid, "email": format!("{username}@x.dev"), "role": role, "added": 1.0}), 1.0).unwrap();
        db.kv_put("uws", &format!("{uid}/{WS}"), &json!({"workspace": WS}), 1.0).unwrap();
        db.kv_put("profile", uid, &json!({"username": username, "name": username.to_uppercase()}), 1.0).unwrap();
        db.kv_put("usernames", username, &json!({"uid": uid}), 1.0).unwrap();
    }

    /// kim owns the workspace, lee and max are members; #general is made.
    /// The uids carry `tag`: the posting rate limit is process-wide.
    fn team(tag: &str) -> (Store, [String; 3]) {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let ids = [format!("u-{tag}-kim"), format!("u-{tag}-lee"), format!("u-{tag}-max")];
        join(&db, &ids[0], "kim", "owner");
        join(&db, &ids[1], "lee", "member");
        join(&db, &ids[2], "max", "member");
        db.kv_put("ws", WS, &json!({"id": WS, "name": "Acme"}), 1.0).unwrap();
        ensure_default(&db, WS);
        (db, ids)
    }

    fn channel_id(db: &Store, name: &str) -> String {
        channels_of(db, WS).into_iter().find(|c| text(c, "name") == name).map(|c| text(&c, "id")).expect(name)
    }

    fn person(uid: &str) -> Poster<'_> {
        Poster { uid, session: "", agent: "dashboard", via: "dashboard" }
    }

    fn say(db: &Store, uid: &str, chan: &str, words: &str) -> Result<Value, Refusal> {
        post_message(db, WS, chan, &person(uid), words, "", &[], &none)
    }

    fn visible(db: &Store, uid: &str) -> Vec<String> {
        list(db, WS, uid).map(|l| l["channels"].as_array().unwrap().iter().map(|c| text(c, "name")).collect()).unwrap_or_default()
    }

    fn set_everyone(db: &Store, owner: &str, perms: &[&str]) {
        update_role(db, WS, owner, EVERYONE, &json!({"perms": perms})).unwrap();
    }

    fn role(db: &Store, owner: &str, name: &str, perms: &[&str]) -> String {
        text(&create_role(db, WS, owner, &json!({"name": name, "perms": perms})).unwrap(), "id")
    }

    fn upload(db: &Store, uid: &str, chan: &str) -> String {
        let id = crate::uploads::new_id();
        let record = json!({"workspace": WS, "channel": chan, "by_uid": uid, "key": format!("uploads/{WS}/{id}.pdf"), "kind": "file",
            "name": "plan.pdf", "shown": "application/pdf", "ext": "pdf", "size": 10, "url": format!("https://x/u/{id}.pdf"),
            "created": now(), "retention": "history", "attached": ""});
        db.kv_put(crate::uploads::KV_BUCKET, &id, &record, now()).unwrap();
        id
    }

    #[test]
    fn a_workspace_starts_with_general_and_only_members_read_it() {
        let (db, [kim, lee, _]) = team("start");
        assert_eq!(visible(&db, &lee), vec!["general"]);
        let general = channel_id(&db, "general");
        assert_eq!(list(&db, WS, "u-stranger").unwrap_err().0, 403);
        assert_eq!(say(&db, "u-stranger", &general, "hi").unwrap_err().0, 403);
        assert_eq!(page(&db, WS, &general, "u-stranger", "", 50).unwrap_err().0, 403);
        // once only: a workspace emptied by its managers stays empty
        delete_channel(&db, WS, &kim, &general).unwrap();
        ensure_default(&db, WS);
        assert!(visible(&db, &lee).is_empty());
    }

    #[test]
    fn managing_channels_takes_manage_channels() {
        let (db, [kim, lee, _]) = team("manage");
        let general = channel_id(&db, "general");
        assert_eq!(create_channel(&db, WS, &lee, &json!({"name": "ops"})).unwrap_err().0, 403);
        assert_eq!(update_channel(&db, WS, &lee, &general, &json!({"topic": "x"})).unwrap_err().0, 403);
        assert_eq!(delete_channel(&db, WS, &lee, &general).unwrap_err().0, 403);
        assert_eq!(create_category(&db, WS, &lee, "Ops").unwrap_err().0, 403);
        assert_eq!(reorder(&db, WS, &lee, &json!({})).unwrap_err().0, 403);
        let mods = role(&db, &kim, "mods", &["manage_channels"]);
        set_member_roles(&db, WS, &kim, &lee, &[mods]).unwrap();
        let ops = create_channel(&db, WS, &lee, &json!({"name": "Release Ops!", "topic": "ship it"})).unwrap();
        assert_eq!(ops["name"], json!("release-ops"), "Discord's spelling");
        assert_eq!(create_channel(&db, WS, &lee, &json!({"name": "release ops"})).unwrap_err().0, 409, "one #release-ops");
        update_channel(&db, WS, &lee, &text(&ops, "id"), &json!({"topic": "ship it today", "agents_hear_all": true})).unwrap();
        assert!(delete_channel(&db, WS, &lee, &text(&ops, "id")).is_ok());
        // a channel-level deny takes it away there alone
        set_overrides(&db, WS, &kim, &general, &json!([{"type": "member", "id": lee, "allow": [], "deny": ["manage_channels"]}])).unwrap();
        assert_eq!(update_channel(&db, WS, &lee, &general, &json!({"topic": "x"})).unwrap_err().0, 403);
    }

    #[test]
    fn sending_takes_send_messages_and_attaching_takes_attach_files() {
        let (db, [kim, lee, max]) = team("send");
        let general = channel_id(&db, "general");
        assert!(say(&db, &max, &general, "hello").is_ok());
        let file = upload(&db, &max, &general);
        let sent = post_message(&db, WS, &general, &person(&max), "the plan", "", &[file.clone()], &none).unwrap();
        assert_eq!(sent["attachments"][0]["name"], json!("plan.pdf"));
        assert_eq!(post_message(&db, WS, &general, &person(&max), "again", "", &[file], &none).unwrap_err().0, 400, "a file is posted once");
        let others = upload(&db, &lee, &general);
        assert_eq!(post_message(&db, WS, &general, &person(&max), "x", "", &[others], &none).unwrap_err().0, 400, "not someone else's file");
        set_everyone(&db, &kim, &["view_channel", "send_messages"]);
        let file = upload(&db, &max, &general);
        let refused = post_message(&db, WS, &general, &person(&max), "x", "", &[file], &none).unwrap_err();
        assert_eq!(refused, (403, "you can't attach files in #general".into()));
        set_everyone(&db, &kim, &["view_channel", "attach_files"]);
        assert_eq!(say(&db, &max, &general, "hi").unwrap_err(), (403, "you can't send messages in #general".into()));
        let can = list(&db, WS, &max).unwrap()["channels"][0]["perms"].clone();
        assert_eq!(can, json!(["view_channel"]), "no sending takes attaching with it");
        assert!(page(&db, WS, &general, &max, "", 50).is_ok(), "still read");
        set_everyone(&db, &kim, &[]);
        assert!(visible(&db, &max).is_empty());
        assert_eq!(say(&db, &max, &general, "hi").unwrap_err().0, 404, "a channel you can't see is not there");
        assert!(say(&db, &kim, &general, "the owner holds everything").is_ok());
    }

    #[test]
    fn pinging_everyone_takes_mention_everyone() {
        let (db, [kim, lee, max]) = team("ping");
        let general = channel_id(&db, "general");
        crate::hubs::note_session(&db, &max, "m1", &format!("{WS}:r"), "max@x.dev");
        let sent = say(&db, &lee, &general, "@everyone standup").unwrap();
        assert_eq!(sent["mentions"]["everyone"], json!(true));
        assert_eq!(crate::hubs::take_for_session(&db, &max, "m1").len(), 1);
        set_everyone(&db, &kim, &["view_channel", "send_messages"]);
        let sent = say(&db, &lee, &general, "@everyone again").unwrap();
        assert_eq!(sent["mentions"]["everyone"], json!(false), "plain words without the permission");
        assert!(crate::hubs::take_for_session(&db, &max, "m1").is_empty());
    }

    #[test]
    fn edits_are_the_authors_and_deleting_others_takes_manage_messages() {
        let (db, [kim, lee, max]) = team("edit");
        let general = channel_id(&db, "general");
        let mine = say(&db, &lee, &general, "frist").unwrap();
        let id = text(&mine, "id");
        let edited = edit(&db, WS, &general, &person(&lee), &id, "first").unwrap();
        assert_eq!(edited["text"], json!("first"));
        assert!(edited["edited"].as_f64().is_some());
        assert_eq!(edit(&db, WS, &general, &person(&max), &id, "mine now").unwrap_err().0, 403);
        assert_eq!(edit(&db, WS, &general, &person(&kim), &id, "even the owner").unwrap_err().0, 403, "words are their author's");
        // an agent's message is its session's to edit, its person's to delete
        let agent = Poster { uid: &lee, session: "l1", agent: "claude-code", via: "mcp" };
        let said = post_message(&db, WS, &general, &agent, "from the agent", "", &[], &none).unwrap();
        let agents = text(&said, "id");
        assert_eq!(edit(&db, WS, &general, &person(&lee), &agents, "x").unwrap_err().0, 403);
        assert!(edit(&db, WS, &general, &agent, &agents, "from the agent, fixed").is_ok());
        assert!(remove(&db, WS, &general, &lee, &agents).is_ok());
        assert_eq!(remove(&db, WS, &general, &max, &id).unwrap_err().0, 403);
        let mods = role(&db, &kim, "mods", &["manage_messages"]);
        set_member_roles(&db, WS, &kim, &max, &[mods]).unwrap();
        let since = now() - 1.0;
        assert!(remove(&db, WS, &general, &max, &id).is_ok());
        assert_eq!(remove(&db, WS, &general, &max, &id).unwrap_err().0, 404, "gone");
        assert!(page(&db, WS, &general, &kim, "", 50).unwrap()["messages"].as_array().unwrap().is_empty());
        let changed = changes(&db, WS, &general, &kim, since).unwrap();
        assert!(changed["messages"].as_array().unwrap().iter().any(|m| text(m, "id") == id && m["deleted"] == json!(true)));
    }

    #[test]
    fn a_private_channel_shows_only_to_whom_it_allows_and_their_agents() {
        let (db, [kim, lee, max]) = team("private");
        for (uid, session) in [(&lee, "l1"), (&max, "m1")] {
            crate::hubs::note_session(&db, uid, session, &format!("{WS}:r"), "x");
        }
        let secret = create_channel(&db, WS, &kim, &json!({"name": "secret", "private": true})).unwrap();
        let id = text(&secret, "id");
        assert_eq!(secret["private"], json!(true));
        assert_eq!(visible(&db, &lee), vec!["general"]);
        assert_eq!(say(&db, &lee, &id, "hi").unwrap_err().0, 404);
        assert_eq!(page(&db, WS, &id, &lee, "", 50).unwrap_err().0, 404);
        let agent = agent_post(&db, &lee, "l1", "claude-code", "#secret", "let me in", "", &none);
        assert_eq!(agent["ok"], json!(false), "{agent}");
        let by_id = agent_post(&db, &lee, "l1", "claude-code", &id, "let me in", "", &none);
        assert_eq!(by_id["error"], json!("you are in no channel with that id"));
        say(&db, &kim, &id, "@everyone @lee secret plans").unwrap();
        assert!(crate::hubs::take_for_session(&db, &lee, "l1").is_empty(), "not even mentioned by name");
        assert!(crate::hubs::take_for_session(&db, &max, "m1").is_empty());
        let members = members_view(&db, WS, &id, &kim).unwrap().to_string();
        assert!(!members.contains(&lee) && !members.contains(&max));
        // a role let in, then one person
        let core = role(&db, &kim, "core", &[]);
        set_member_roles(&db, WS, &kim, &lee, &[core.clone()]).unwrap();
        let overrides = json!([
            {"type": "role", "id": EVERYONE, "allow": [], "deny": ["view_channel"]},
            {"type": "role", "id": core, "allow": ["view_channel"], "deny": []},
        ]);
        set_overrides(&db, WS, &kim, &id, &overrides).unwrap();
        // in no category, it sits above the categories, as Discord shows it
        assert_eq!(visible(&db, &lee), vec!["secret", "general"]);
        assert!(visible(&db, &max).len() == 1);
        let posted = agent_post(&db, &lee, "l1", "claude-code", "#secret", "in", "", &none);
        assert_eq!(posted["ok"], json!(true), "{posted}");
        let mut with_max = overrides.as_array().unwrap().clone();
        with_max.push(json!({"type": "member", "id": max, "allow": ["view_channel"], "deny": []}));
        set_overrides(&db, WS, &kim, &id, &json!(with_max)).unwrap();
        assert_eq!(visible(&db, &max), vec!["secret", "general"]);
    }

    #[test]
    fn overrides_apply_in_discords_order() {
        let (db, [kim, lee, max]) = team("order");
        let general = channel_id(&db, "general");
        let speakers = role(&db, &kim, "speakers", &[]);
        set_member_roles(&db, WS, &kim, &lee, &[speakers.clone()]).unwrap();
        set_member_roles(&db, WS, &kim, &max, &[speakers.clone()]).unwrap();
        set_overrides(&db, WS, &kim, &general, &json!([
            {"type": "role", "id": EVERYONE, "deny": ["send_messages"]},
            {"type": "role", "id": speakers, "allow": ["send_messages"]},
            {"type": "member", "id": max, "deny": ["send_messages"]},
        ])).unwrap();
        assert!(say(&db, &lee, &general, "a role's allow beats @everyone's deny").is_ok());
        assert_eq!(say(&db, &max, &general, "a member's own deny beats the role").unwrap_err().0, 403);
        // an override must name something real, once
        assert_eq!(set_overrides(&db, WS, &kim, &general, &json!([{"type": "role", "id": "nope", "allow": []}])).unwrap_err().0, 404);
        assert_eq!(set_overrides(&db, WS, &kim, &general, &json!([{"type": "member", "id": "u-nobody", "allow": ["send_messages"]}])).unwrap_err().0, 404);
        assert_eq!(set_overrides(&db, WS, &kim, &general, &json!([{"type": "role", "id": EVERYONE, "allow": ["fly"]}])).unwrap_err().0, 400);
        assert_eq!(set_overrides(&db, WS, &lee, &general, &json!([])).unwrap_err().0, 403, "it takes Manage roles");
    }

    #[test]
    fn roles_are_managed_from_below_your_own() {
        let (db, [kim, lee, max]) = team("roles");
        assert_eq!(create_role(&db, WS, &lee, &json!({"name": "x"})).unwrap_err().0, 403, "it takes Manage roles");
        let helpers = role(&db, &kim, "helpers", &["manage_messages"]);
        let admins = role(&db, &kim, "admins", &["manage_roles", "manage_messages"]);
        // new roles land at the bottom: admins went in under helpers; put it on top
        reorder_roles(&db, WS, &kim, &[admins.clone(), helpers.clone()]).unwrap();
        set_member_roles(&db, WS, &kim, &lee, &[admins.clone()]).unwrap();
        // lee manages helpers, below admins, and nothing at or above it
        assert!(update_role(&db, WS, &lee, &helpers, &json!({"color": "#ff8800"})).is_ok());
        assert_eq!(update_role(&db, WS, &lee, &admins, &json!({"name": "mine"})).unwrap_err().0, 403);
        assert!(set_member_roles(&db, WS, &lee, &max, &[helpers.clone()]).is_ok());
        assert_eq!(set_member_roles(&db, WS, &lee, &max, &[admins.clone()]).unwrap_err().0, 403);
        assert_eq!(set_member_roles(&db, WS, &lee, &kim, &[]).unwrap_err().0, 403, "not the owner's");
        // only what lee holds may be given
        assert_eq!(update_role(&db, WS, &lee, &helpers, &json!({"perms": ["manage_messages", "manage_channels"]})).unwrap_err().0, 403);
        assert_eq!(create_role(&db, WS, &lee, &json!({"name": "x", "perms": ["manage_channels"]})).unwrap_err().0, 403);
        assert!(create_role(&db, WS, &lee, &json!({"name": "x", "perms": ["manage_messages"]})).is_ok());
        assert_eq!(reorder_roles(&db, WS, &lee, &[helpers.clone(), admins.clone(), text(&role_list(&db, WS)[2], "id")]).unwrap_err().0, 403);
        assert_eq!(update_role(&db, WS, &kim, EVERYONE, &json!({"name": "all"})).unwrap_err().0, 400);
        assert_eq!(create_role(&db, WS, &kim, &json!({"name": "@everyone"})).unwrap_err().0, 400);
        // a deleted role leaves its members and overrides
        let general = channel_id(&db, "general");
        set_overrides(&db, WS, &kim, &general, &json!([{"type": "role", "id": helpers, "deny": ["send_messages"]}])).unwrap();
        delete_role(&db, WS, &kim, &helpers).unwrap();
        assert!(member_roles(&db, WS, &max).is_empty());
        assert_eq!(channel_get(&db, WS, &general).unwrap()["overrides"], json!([]));
        assert_eq!(delete_role(&db, WS, &kim, EVERYONE).unwrap_err().0, 400);
        let view = roles_view(&db, WS, &max).unwrap();
        assert_eq!(view["roles"].as_array().unwrap().last().unwrap()["id"], json!(EVERYONE));
        assert_eq!(view["roles"][0]["editable"], json!(false), "max manages nothing");
    }

    #[test]
    fn agents_hear_only_what_they_should() {
        let (db, [kim, lee, max]) = team("hear");
        let general = channel_id(&db, "general");
        let scope = format!("{WS}:r");
        crate::hubs::note_session(&db, &kim, "k1", &scope, "kim@x.dev");
        crate::hubs::note_session(&db, &lee, "l1", &scope, "lee@x.dev");
        crate::hubs::note_session(&db, &lee, "l2", &scope, "lee@x.dev");
        crate::hubs::note_session(&db, &max, "m1", &scope, "max@x.dev");
        let heard = |uid: &str, session: &str| crate::hubs::take_for_session(&db, uid, session).iter().map(|m| text(m, "text")).collect::<Vec<_>>();
        say(&db, &kim, &general, "plain words reach no agent").unwrap();
        assert!(heard(&lee, "l1").is_empty() && heard(&max, "m1").is_empty() && heard(&kim, "k1").is_empty());
        say(&db, &kim, &general, "@lee have a look").unwrap();
        assert_eq!(heard(&lee, "l1"), vec!["@lee have a look"]);
        assert_eq!(heard(&lee, "l2"), vec!["@lee have a look"], "every agent of lee's");
        assert!(heard(&max, "m1").is_empty());
        say(&db, &kim, &general, "@lee/2 take the migration").unwrap();
        assert!(heard(&lee, "l1").is_empty());
        assert_eq!(heard(&lee, "l2"), vec!["@lee/2 take the migration"]);
        say(&db, &kim, &general, "`@max` in code pings nobody").unwrap();
        assert!(heard(&max, "m1").is_empty());
        say(&db, &kim, &general, "@everyone standup").unwrap();
        assert_eq!(heard(&max, "m1"), vec!["@everyone standup"]);
        assert_eq!((heard(&lee, "l1").len(), heard(&lee, "l2").len()), (1, 1));
        assert!(heard(&kim, "k1").is_empty(), "not back to the poster's own agents");
        // an agent hands work to its siblings, never to itself
        let agent = Poster { uid: &lee, session: "l1", agent: "claude-code", via: "mcp" };
        post_message(&db, WS, &general, &agent, "@lee who takes the tests?", "", &[], &none).unwrap();
        assert!(heard(&lee, "l1").is_empty());
        assert_eq!(heard(&lee, "l2").len(), 1);
        // the channel's setting: every message, to everyone else's agents
        update_channel(&db, WS, &kim, &general, &json!({"agents_hear_all": true})).unwrap();
        say(&db, &kim, &general, "deploys are frozen").unwrap();
        assert_eq!(heard(&max, "m1"), vec!["deploys are frozen"]);
        assert_eq!(heard(&lee, "l2"), vec!["deploys are frozen"]);
        assert!(heard(&kim, "k1").is_empty());
        // what an agent reads, and the mention badges
        say(&db, &max, &general, "@kim ping").unwrap();
        let got = crate::hubs::take_for_session(&db, &kim, "k1");
        let line = text(&got[0], "message");
        assert!(line.starts_with("[#general in Acme · @max] @kim ping"), "{line}");
        assert!(line.contains(&format!("send_agent_message(hub=\"{general}\"")), "{line}");
        let note = crate::hubs::hook_note(&db, &lee, "l1");
        assert!(note.is_some_and(|n| n.starts_with("Messages in your workspace's Cork channels")));
        let row = |uid: &str| list(&db, WS, uid).unwrap()["channels"][0].clone();
        assert_eq!(row(&kim)["mentions"], json!(1));
        assert_eq!(row(&kim)["unread"], json!(true));
        mark_read(&db, &general, &kim);
        assert_eq!(row(&kim)["mentions"], json!(0), "reading clears them");
        assert_eq!(row(&kim)["unread"], json!(false));
    }

    #[test]
    fn an_agent_posts_by_channel_name_as_its_persons_tool() {
        let (db, [_, lee, _]) = team("agentpost");
        let general = channel_id(&db, "general");
        let scope = format!("{WS}:r");
        crate::hubs::note_session(&db, &lee, "l1", &scope, "lee@x.dev");
        crate::freshness::note_session(&db, &scope, "lee@x.dev", "l1", "", "claude", "");
        let sent = crate::hubs::agent_send(&db, &lee, "l1", "claude-code", &json!({"hub": "#general", "message": "tests pass", "to": "@kim"}), &none);
        assert_eq!(sent["ok"], json!(true), "{sent}");
        assert_eq!(sent["channel"], json!(general));
        let rows = page(&db, WS, &general, &lee, "", 50).unwrap();
        let said = rows["messages"][0].clone();
        assert_eq!(said["text"], json!("@kim tests pass"));
        assert_eq!(said["from"]["tool_label"], json!("Claude Code"));
        assert_eq!(said["from"]["agent_no"], json!(1));
        assert_eq!(said["from"]["via"], json!("mcp"));
        assert_eq!(display_name(&said["from"]), "LEE's Claude Code");
        let by_id = crate::hubs::agent_send(&db, &lee, "l1", "claude-code", &json!({"hub": general, "message": "by id"}), &none);
        assert_eq!(by_id["ok"], json!(true), "{by_id}");
        let missing = crate::hubs::agent_send(&db, &lee, "l1", "claude-code", &json!({"hub": "#nowhere", "message": "x"}), &none);
        assert_eq!(missing["error"], json!("you are in no channel called #nowhere"));
        let line = brief_part(&db, &lee, "l1").unwrap();
        assert!(line.starts_with("Your workspace's channels: #general."), "{line}");
    }

    #[test]
    fn a_channel_reads_a_page_at_a_time_and_polls_for_changes() {
        let (db, [kim, lee, _]) = team("paging");
        let general = channel_id(&db, "general");
        for n in 1..=7 {
            say(&db, &kim, &general, &format!("m{n}")).unwrap();
        }
        let newest = page(&db, WS, &general, &lee, "", 3).unwrap();
        let texts = |rows: &Value| rows["messages"].as_array().unwrap().iter().map(|m| text(m, "text")).collect::<Vec<_>>();
        assert_eq!(texts(&newest), vec!["m5", "m6", "m7"]);
        assert_eq!(newest["has_more"], json!(true));
        let before = text(&newest["messages"][0], "cursor");
        let older = page(&db, WS, &general, &lee, &before, 3).unwrap();
        assert_eq!(texts(&older), vec!["m2", "m3", "m4"]);
        let oldest = page(&db, WS, &general, &lee, &text(&older["messages"][0], "cursor"), 3).unwrap();
        assert_eq!((texts(&oldest), oldest["has_more"].clone()), (vec!["m1".to_string()], json!(false)));
        assert_eq!(page(&db, WS, &general, &lee, "'; drop", 3).unwrap_err().0, 400);
        let since = newest["server_ts"].as_f64().unwrap();
        let reply = post_message(&db, WS, &general, &person(&lee), "agreed", &text(&newest["messages"][2], "id"), &[], &none).unwrap();
        assert_eq!(reply["reply_to"]["text"], json!("m7"));
        assert_eq!(reply["reply_to"]["from_name"], json!("@kim"), "how an agent addresses them");
        assert_eq!(reply["reply_to"]["from_display"], json!("KIM"), "how a person reads them");
        let changed = changes(&db, WS, &general, &kim, since).unwrap();
        assert!(texts(&changed).contains(&"agreed".to_string()));
        assert_eq!(post_message(&db, WS, &general, &person(&lee), "x", "nope", &[], &none).unwrap_err().0, 404);
    }

    #[test]
    fn categories_and_channels_are_reordered_by_a_drag() {
        let (db, [kim, _, _]) = team("drag");
        let general = channel_id(&db, "general");
        let text_cat = text(&categories_of(&db, WS)[0], "id");
        let ops = text(&create_category(&db, WS, &kim, "Ops").unwrap(), "id");
        let deploys = text(&create_channel(&db, WS, &kim, &json!({"name": "deploys", "category": ops})).unwrap(), "id");
        let random = text(&create_channel(&db, WS, &kim, &json!({"name": "random", "category": text_cat})).unwrap(), "id");
        reorder(&db, WS, &kim, &json!({"categories": [
            {"id": ops, "channels": [general, deploys]},
            {"id": text_cat, "channels": [random]},
        ], "channels": []})).unwrap();
        let names: Vec<String> = sorted_channels(&db, WS).iter().map(|c| text(c, "name")).collect();
        assert_eq!(names, vec!["general", "deploys", "random"]);
        assert_eq!(text(&channel_get(&db, WS, &general).unwrap(), "category"), ops);
        delete_category(&db, WS, &kim, &ops).unwrap();
        let loose: Vec<String> = sorted_channels(&db, WS).iter().filter(|c| text(c, "category").is_empty()).map(|c| text(c, "name")).collect();
        assert_eq!(loose, vec!["general", "deploys"], "its channels stay, in no category");
    }
}
