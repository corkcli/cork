//! Cork on one machine: the free version.
//!
//! `cork local` is this same server, run by the hooks on the
//! developer's own machine for every repo on it. Everything that happens
//! between the agents of one machine happens here, for free and with no
//! account: the briefings, the graph, the gate, the deltas, messages,
//! intents, recipes, the savings. What stays on Cork's servers is what
//! needs them: agents on other machines (teammates), and the embedding
//! model. So in local mode:
//!
//! - it listens on 127.0.0.1 only, and answers one credential: a key minted
//!   here at first start and kept in `~/.cork/local/token` (0600; the data
//!   folder is `~/.collide` on a machine installed before the rename);
//! - the database is `~/.cork/local/collide.db`, one for every repo on
//!   the machine, each repo a scope of one local workspace;
//! - the cloud parts are off: the Postgres log, R2 snapshots, the embedding
//!   model, billing, analytics, Firebase, the hook release sync. Nothing
//!   here calls out.
//!
//! The hooks find it through `~/.cork/local/port` and start it when it
//! is not running (see cork-hooks `machine.rs`).

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::store::{now, Store};

/// The first port tried; the next few are tried when it is taken.
pub const DEFAULT_PORT: u16 = 47_600;
const PORTS_TRIED: u16 = 20;
/// The local user's uid: one person, this machine's.
pub const LOCAL_UID: &str = "local";

/// Set by `main` before anything reads the environment.
pub fn active() -> bool {
    crate::env::var("CORK_LOCAL").map(|v| v == "1").unwrap_or(false)
}

fn home() -> PathBuf {
    for key in ["CORK_HOME", "HOME", "USERPROFILE"] {
        if let Ok(value) = crate::env::var(key) {
            if !value.trim().is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    PathBuf::from(".")
}

/// The machine's data folder: `~/.cork`, except that a machine set up
/// before the rename keeps its `~/.collide` for as long as no `~/.cork`
/// exists beside it (the hooks move it on macOS and Linux, leaving
/// `~/.collide` as a link). The hooks pick the same folder.
fn data_home(home: &std::path::Path) -> PathBuf {
    let cork = home.join(".cork");
    let collide = home.join(".collide");
    if !cork.exists() && collide.exists() { collide } else { cork }
}

/// `~/.cork/local` (`~/.collide/local` on a machine that still has only
/// the old folder), or `CORK_LOCAL_DIR`.
pub fn dir() -> PathBuf {
    match crate::env::var("CORK_LOCAL_DIR") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => data_home(&home()).join("local"),
    }
}

/// The environment of a local server, set before the runtime starts: the
/// data directory, and every cloud part switched off whatever the shell
/// happened to export.
pub fn prepare_env() {
    let dir = dir();
    let _ = std::fs::create_dir_all(&dir);
    // under both names (env::set and env::remove), so a `CORK_*` the shell
    // exported cannot switch a cloud part back on
    let set = |key: &str, value: &str| crate::env::set(key, value);
    set("CORK_LOCAL", "1");
    set("DATA_DIR", &dir.to_string_lossy());
    for key in [
        "CORK_PG_URL", "CORK_DB_PATH", "CORK_ENGINE_ID", "R2_ENDPOINT", "R2_BUCKET", "R2_ACCESS_KEY_ID",
        "R2_SECRET_ACCESS_KEY", "R2_PREFIX", "CORK_EMBED_URL", "CORK_EMBED_TOKEN", "CORK_EMBEDDINGS",
        "POSTHOG_API_KEY", "POSTHOG_KEY", "STRIPE_SECRET_KEY", "STRIPE_WEBHOOK_SECRET", "CORK_GITHUB_TOKEN",
        "CORK_HOOK_SYNC", "CORK_TEST_AUTH", "CORK_DASHBOARD_URL", "RESEND_API_KEY",
    ] {
        crate::env::remove(key);
    }
    if crate::env::var("CORK_WORKERS").is_err() {
        set("CORK_WORKERS", "4");
    }
}

/// A free port on 127.0.0.1: the one asked for, else the default, else the
/// next free one after it. `None` when a local Cork already answers on
/// the first one taken (the hooks raced to start two).
pub fn pick_port(asked: Option<u16>) -> Result<u16, String> {
    let first = asked.unwrap_or(DEFAULT_PORT);
    for port in first..first.saturating_add(PORTS_TRIED) {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
        if already_serving(port) {
            return Err(format!("a local Cork already answers on port {port}"));
        }
    }
    Err(format!("no free port from {first} to {}", first + PORTS_TRIED - 1))
}

fn already_serving(port: u16) -> bool {
    use std::io::{Read, Write};
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(300),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(600)));
    let _ = stream.write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n");
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    answer.contains(&format!("\"local_id\":\"{}\"", instance_id()))
}

/// The identity one agent session reports as: `agent-3f2a91c0`, from
/// its session id. Without a session (the person themselves, a command run
/// by hand) it is the person.
pub fn agent_identity(person: &str, session: &str) -> String {
    let short: String = session.chars().filter(char::is_ascii_alphanumeric).take(8).collect();
    if short.is_empty() { person.to_string() } else { format!("agent-{}", short.to_lowercase()) }
}

/// Which machine account's data this server holds: a hash of its data
/// directory. `/health` carries it and the hooks compare it with their own,
/// so on a computer shared by two people neither uses the other's server.
pub fn instance_id() -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(dir().to_string_lossy().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Written once the port is bound: how the hooks find this server.
pub fn write_port(port: u16) {
    let _ = std::fs::write(dir().join("port"), format!("{port}\n"));
}

fn git_email() -> String {
    std::process::Command::new("git")
        .args(["config", "--global", "user.email"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|email| email.contains('@'))
        .unwrap_or_default()
}

fn write_private(path: &std::path::Path, body: &str) {
    let _ = std::fs::write(path, body);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// The records a local server needs, made on first start and kept: the
/// local person, their one workspace (every repo on the machine is a repo
/// of it, added on first sight), the credential the hooks present, and a
/// plan record with no limits. Returns the workspace id.
pub fn bootstrap(store: &Store) -> String {
    let dir = dir();
    let meta_path = dir.join("local.json");
    let mut meta = std::fs::read_to_string(&meta_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let mut workspace = meta.get("workspace").and_then(Value::as_str).unwrap_or("").to_string();
    let email = {
        let known = meta.get("email").and_then(Value::as_str).unwrap_or("").to_string();
        if !known.is_empty() { known } else { git_email() }
    };
    let label = if email.is_empty() { "you".to_string() } else { email.clone() };
    let principal = crate::workspaces::Principal {
        uid: LOCAL_UID.to_string(),
        email: email.clone(),
        name: label.split('@').next().unwrap_or("you").to_string(),
    };
    if workspace.is_empty() || crate::workspaces::get(store, &workspace).is_none() {
        match crate::workspaces::create(store, &principal, "This machine", "custom", "") {
            Ok(ws) => workspace = ws.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            Err(error) => {
                tracing::warn!("local: could not create the workspace: {error}");
                return workspace;
            }
        }
    }
    let stamp = now();
    let _ = store.kv_put("profile", LOCAL_UID, &json!({"username": principal.name, "email": email}), stamp);
    // no limits: everything that happens on one machine is free
    let _ = store.kv_put(
        crate::billing::BILLING_BUCKET, &workspace,
        &json!({"plan": "business", "status": "active", "seats": null, "interval": null, "local": true}), stamp,
    );
    let _ = store.kv_put(crate::watch::NEW_REPOS_BUCKET, &workspace, &json!({"mode": "add"}), stamp);

    // the credential: kept while its file exists, minted again when it does not
    let token_path = dir.join("token");
    let mut token = std::fs::read_to_string(&token_path).unwrap_or_default().trim().to_string();
    if token.is_empty() {
        token = crate::blocks::random_bytes(32).iter().map(|b| format!("{b:02x}")).collect();
        write_private(&token_path, &format!("{token}\n"));
    }
    let _ = store.kv_put(
        crate::auth::TOKEN_BUCKET,
        &crate::auth::token_key(&token),
        &json!({
            "kind": "access", "uid": LOCAL_UID, "email": email, "name": principal.name,
            "workspace": workspace, "workspaces": [workspace], "client_id": "collide-local",
            "created": stamp, "expires": stamp + 3650.0 * 86_400.0,
        }),
        stamp,
    );
    if let Some(map) = meta.as_object_mut() {
        map.insert("workspace".into(), json!(workspace));
        map.insert("email".into(), json!(email));
        map.insert("version".into(), json!(env!("CARGO_PKG_VERSION")));
    }
    write_private(&meta_path, &format!("{}\n", serde_json::to_string_pretty(&meta).unwrap_or_default()));
    workspace
}

// ------------------------------------------------------------ agent messages

/// Messages waiting for this agent, as one note for the hook to print, and
/// taken off its queue: an agent on this machine is told once. `None` when
/// nothing is waiting.
pub fn inbox_note(store: &Store, scope: &str, user_id: &str) -> Option<String> {
    inbox_note_as(store, scope, user_id, true, &mut Vec::new())
}

/// The inbox key for one session of a person. On a team a message goes to a
/// person, whichever session reads first; a person's own sessions have no
/// other way to tell each other apart.
pub fn session_inbox(user: &str, session: &str) -> String {
    format!("{user}#{session}")
}

/// The lead of a session, on an inbox of its own (`agent_inbox`).
pub const LEAD: &str = "lead";

/// One agent inside a session, on an inbox of its own: the session's lead
/// (`user#session/lead`) or one of its helpers, a subagent, by the id its
/// harness gives it (`user#session/<worker>`). A helper shares its lead's
/// session, so the session's inbox cannot tell them apart: what one of them
/// sends another inside the session waits here, for that agent alone. On
/// this machine's own server each session is its own identity already:
/// `agent-3f2a91c0/lead`, `agent-3f2a91c0/<worker>`.
pub fn agent_inbox(user: &str, session: &str, worker: &str) -> String {
    let base = if active() || session.is_empty() { user.to_string() } else { session_inbox(user, session) };
    format!("{base}/{}", if worker.is_empty() { LEAD } else { worker })
}

/// The helpers at work inside one session, each with the path it is on
/// now (its presence marker, alive while it works; "" while it thinks
/// between steps): `(worker, path)`. A helper counts from its first hook
/// call until it ends (`presence::session_helpers`), not only while its
/// marker lives.
pub fn session_workers(store: &Store, scope: &str, user: &str, session: &str) -> Vec<(String, String)> {
    if user.is_empty() || session.is_empty() {
        return Vec::new();
    }
    let repo_id = crate::presence::split_scope(scope).1.to_string();
    let field = |marker: &Value, key: &str| marker.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let mut workers: Vec<(String, String)> = store
        .eph_scan(&crate::presence::worker_key(scope, user, session, ""))
        .into_iter()
        .filter(|(_, marker)| {
            let repo = field(marker, "repo_id");
            repo.is_empty() || repo == repo_id
        })
        .map(|(_, marker)| (field(&marker, "worker"), field(&marker, "path")))
        .filter(|(worker, _)| !worker.is_empty())
        .collect();
    for helper in crate::presence::session_helpers(store, scope, user, session) {
        let worker = field(&helper, "worker");
        if !workers.iter().any(|(w, _)| *w == worker) {
            workers.push((worker, String::new()));
        }
    }
    workers.sort();
    workers.dedup();
    workers
}

/// What a helper is called (its name, else its type), as long as its
/// session lists it or its marker lives; "" after that.
pub fn helper_name(store: &Store, scope: &str, user: &str, session: &str, worker: &str) -> String {
    let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    [crate::presence::helper_key(scope, user, session, worker), crate::presence::worker_key(scope, user, session, worker)]
        .iter()
        .filter_map(|key| store.eph_get(key))
        .map(|record| if field(&record, "name").is_empty() { field(&record, "type") } else { field(&record, "name") })
        .find(|name| !name.is_empty())
        .unwrap_or_default()
}

/// Whether a message names this path: the path itself, or a file name
/// with an extension (`app.css`) on its own.
fn names_path(message: &str, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    message.contains(path) || (name.contains('.') && message.split(|c: char| !(c.is_alphanumeric() || "._-/".contains(c))).any(|w| w == name || w.ends_with(&format!("/{name}"))))
}

/// One agent here a message can be addressed to: its inbox, the names it
/// answers to, and how the agents are listed when a name matches none.
struct Addressee {
    inbox: String,
    session: String,
    title: String,
    /// what Claude Code calls the session (`cork-linear-test-e3`)
    cc_name: String,
    /// a helper's name and type ("" for a session)
    helper: String,
    helper_type: String,
    label: String,
}

/// The agents at work in this repo, as a message can reach them: each
/// session (its title, the name the harness gives the conversation, and
/// its Claude Code name) and each helper running in one, never the agent
/// asking.
fn addressees(store: &Store, scope: &str, from: &str, own_session: &str, own_worker: &str, idle_after_s: f64) -> Vec<Addressee> {
    let visible = std::collections::BTreeSet::new();
    let repo_id = crate::presence::split_scope(scope).1.to_string();
    let view = crate::activity::list_activity(store, scope, &repo_id, idle_after_s, &visible);
    let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut out: Vec<Addressee> = Vec::new();
    for row in view.get("workspaces").and_then(Value::as_array).into_iter().flatten() {
        let (user, session) = (field(row, "user"), field(row, "session"));
        if session.is_empty() || row.get("ended").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if row.get("repo_id").and_then(Value::as_str).is_some_and(|r| r != repo_id) {
            continue;
        }
        let mine = user == from && session == own_session;
        let (title, cc_name) = (field(row, "title"), field(row, "cc_name"));
        let short: String = session.chars().take(8).collect();
        // listed by its Claude Code name and what it is doing now, else by
        // both its names: `Cork version number (cork-linear-test-e3)`
        let task = crate::freshness::session_facts(store, scope, &user, &session).task;
        let both = crate::freshness::work_label(&title, &cc_name, &task);
        let named = if both.is_empty() { short.clone() } else { both };
        let whose = if user == from || active() { String::new() } else { format!(" ({user})") };
        // the session: another one by its inbox; the asker's own session,
        // to one of its helpers, is its lead
        if !mine || !own_worker.is_empty() {
            let (inbox, label) = if mine {
                (agent_inbox(&user, &session, ""), format!("{named}, your lead"))
            } else {
                (if active() { user.clone() } else { session_inbox(&user, &session) }, format!("{named}{whose}"))
            };
            out.push(Addressee {
                inbox, session: session.clone(), title: title.clone(), cc_name: cc_name.clone(), helper: String::new(),
                helper_type: String::new(), label,
            });
        }
        // its helpers, from their first hook call until they end: a live
        // marker alone (one written before the helper list) counts too
        let mut helpers = crate::presence::session_helpers(store, scope, &user, &session);
        for worker in row.get("worker_list").and_then(Value::as_array).into_iter().flatten() {
            let id = field(worker, "worker");
            if worker.get("live").and_then(Value::as_bool) != Some(false) && !helpers.iter().any(|h| field(h, "worker") == id) {
                helpers.push(worker.clone());
            }
        }
        for worker in &helpers {
            let id = field(worker, "worker");
            if id.is_empty() || (mine && id == own_worker) {
                continue;
            }
            let (name, kind) = (field(worker, "name"), field(worker, "type"));
            let called = if !name.is_empty() { name.clone() } else if !kind.is_empty() { kind.clone() } else { id.clone() };
            out.push(Addressee {
                inbox: agent_inbox(&user, &session, &id), session: session.clone(), title: title.clone(),
                cc_name: cc_name.clone(), helper: name, helper_type: kind, label: format!("{called} (a helper in {named}{whose})"),
            });
        }
    }
    out
}

/// Who `--to NAME` means, among the agents at work in this repo, in this
/// order: an exact inbox key (what a receipt or a message's sender shows);
/// an email or @username, a teammate; a session id or its 8-character
/// prefix; the name Claude Code gives a session (`cork-linear-test-e3`,
/// what its own ListAgents and SendMessage use: an agent tried it, and
/// until v103 it was refused); a session's title, as the harness names the
/// conversation; a helper's name (or its type); "lead", a helper's own
/// lead. Case does not matter. A name that fits more than one agent is
/// refused with the candidates, and one that fits none is refused with the
/// agents who are here: a message is never "sent" to an address nobody
/// reads (until v98, `--to cork-linear-test-37` was filed under that string
/// and never arrived).
pub fn resolve_to(
    store: &Store, scope: &str, from: &str, own_session: &str, own_worker: &str, name: &str, idle_after_s: f64,
) -> Result<String, String> {
    let wanted = name.trim();
    if wanted.is_empty() {
        return Err("nothing was sent: name the agent to send it to".to_string());
    }
    let lower = wanted.to_lowercase();
    if lower == LEAD {
        return if own_worker.is_empty() {
            Err("nothing was sent: \"lead\" is a helper's own lead, and this is the session's lead".to_string())
        } else {
            Ok(agent_inbox(from, own_session, ""))
        };
    }
    let here = addressees(store, scope, from, own_session, own_worker, idle_after_s);
    // the first rule that names anyone decides; more than one agent is refused
    let pick = |found: Vec<&Addressee>| -> Option<Result<String, String>> {
        let mut inboxes: Vec<&Addressee> = Vec::new();
        for a in found {
            if !inboxes.iter().any(|b| b.inbox == a.inbox) {
                inboxes.push(a);
            }
        }
        match inboxes.len() {
            0 => None,
            1 => Some(Ok(inboxes[0].inbox.clone())),
            _ => Some(Err(format!(
                "nothing was sent: \"{wanted}\" could be {}; send it again --to one of those exactly",
                inboxes.iter().map(|a| format!("{} (--to {})", a.label, a.inbox)).collect::<Vec<_>>().join(", ")
            ))),
        }
    };
    if let Some(found) = pick(here.iter().filter(|a| a.inbox.to_lowercase() == lower).collect()) {
        return found;
    }
    // a teammate by account: an email, or a Cork username
    if wanted.contains('@') {
        return Ok(wanted.to_string());
    }
    let sessions = || here.iter().filter(|a| a.helper.is_empty() && a.helper_type.is_empty());
    let by_id: Vec<&Addressee> = sessions()
        .filter(|a| { let s = a.session.to_lowercase(); s == lower || (lower.len() >= 8 && s.starts_with(&lower)) })
        .collect();
    if let Some(found) = pick(by_id) {
        return found;
    }
    if let Some(found) = pick(sessions().filter(|a| !a.cc_name.is_empty() && a.cc_name.to_lowercase() == lower).collect()) {
        return found;
    }
    if let Some(found) = pick(sessions().filter(|a| !a.title.is_empty() && a.title.to_lowercase() == lower).collect()) {
        return found;
    }
    let helpers = || here.iter().filter(|a| !a.helper.is_empty() || !a.helper_type.is_empty());
    if let Some(found) = pick(helpers().filter(|a| a.helper.to_lowercase() == lower).collect()) {
        return found;
    }
    if let Some(found) = pick(helpers().filter(|a| a.helper_type.to_lowercase() == lower).collect()) {
        return found;
    }
    let listed: Vec<String> = here.iter().map(|a| a.label.clone()).collect();
    let mut agents = if listed.is_empty() { "none other than you".to_string() } else { listed.join(", ") };
    if !own_worker.is_empty() {
        agents.push_str(", and your lead (--to lead)");
    }
    Err(format!("nothing was sent: no agent called \"{wanted}\" is at work in this repo; the agents here are: {agents}"))
}

/// Who signs a message an agent sends: on a team, the session that sent it
/// (`user#session`), a helper by its own inbox, so a reply `--to` the
/// sender reaches that helper; on this machine's own server, the session's
/// identity (and the helper's inbox).
fn signer(from: &str, own_session: &str, own_worker: &str) -> String {
    if !own_worker.is_empty() {
        agent_inbox(from, own_session, own_worker)
    } else if !active() && !own_session.is_empty() {
        session_inbox(from, own_session)
    } else {
        from.to_string()
    }
}

/// The messages waiting in `user_id`'s inbox here, as one note, taken off the
/// queue. The heading calls for action only when a message asks something
/// (`agenttools::messages_heading`); a reply goes with `--to <sender>`.
fn inbox_note_as(store: &Store, scope: &str, user_id: &str, own_sessions: bool, handed: &mut Vec<String>) -> Option<String> {
    let none = std::collections::BTreeSet::new();
    let inbox = crate::agenttools::inbox_ack(store, scope, user_id, &none, &[]);
    let messages: Vec<Value> = inbox.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    if messages.is_empty() {
        return None;
    }
    let lines: Vec<String> = vouched(store, scope, user_id, &messages).iter().map(crate::agenttools::message_line).collect();
    handed.extend(hand_over(store, scope, user_id, &messages));
    let mut heading = crate::agenttools::messages_heading(&messages, own_sessions);
    if messages.iter().any(|m| crate::agenttools::asks(m.get("message").and_then(Value::as_str).unwrap_or(""))) {
        heading.push_str(" To answer, run the cork message command with --to <sender>.");
    }
    Some(format!("{heading}\n{}", lines.join("\n")))
}

/// These messages as `inbox`'s agent is handed them: each with who Cork says
/// sent it (`agenttools::vouch`). The reader's session is the inbox's
/// (`user#session`), when it names one.
fn vouched(store: &Store, scope: &str, inbox: &str, messages: &[Value]) -> Vec<Value> {
    let (user, session) = inbox.split_once('#').unwrap_or((inbox, ""));
    let session = session.split('/').next().unwrap_or("");
    messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            crate::agenttools::vouch(store, scope, &mut m, user, session);
            m
        })
        .collect()
}

/// Take these messages off `inbox`, handed to its agent: delivered now, and
/// seen once the agent is past them (`headsup.rs`). Until v100 they were
/// acked here, and the log called a note "read" the moment a hook took it.
fn hand_over(store: &Store, scope: &str, inbox: &str, messages: &[Value]) -> Vec<String> {
    let ids: Vec<String> = messages.iter().filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string)).collect();
    for id in &ids {
        let _ = store.kv_delete("inbox", &format!("{scope}:{inbox}:{id}"));
    }
    let session = inbox.split_once('#').map(|(_, s)| s).unwrap_or("");
    crate::agenttools::mark_delivered(store, messages, session);
    ids
}

/// Put this agent's waiting messages on a hook response, so an agent deep
/// in a long task hears them on its next edit or read, not only its next
/// prompt.
pub fn attach_inbox(store: &Store, scope: &str, user_id: &str, session: &str, response: &mut Value) -> Vec<String> {
    attach_inbox_for(store, scope, user_id, session, "", response)
}

/// `attach_inbox` for one agent of the session: its lead (`worker` empty)
/// or a helper. The session's inbox, news from other sessions, goes to
/// whichever of them works next, as it always has; what the session's own
/// lead or helpers sent this one agent waits on its own inbox.
///
/// Returns the inbox ids it handed over, for the heads-up log.
pub fn attach_inbox_for(store: &Store, scope: &str, user_id: &str, session: &str, worker: &str, response: &mut Value) -> Vec<String> {
    let mut handed = Vec::new();
    // a call of the session's own: one more chance its hooks had to find
    // what was posted to it through Claude Code (`agenttools::peer_held`)
    if worker.is_empty() && (active() || !session.is_empty()) {
        let inbox = if active() { user_id.to_string() } else { session_inbox(user_id, session) };
        crate::agenttools::peer_call(store, scope, &inbox);
    }
    let shared = if active() {
        inbox_note_as(store, scope, user_id, true, &mut handed)
    } else if !session.is_empty() {
        // on a team: what this person's other sessions sent this one
        inbox_note_as(store, scope, &session_inbox(user_id, session), true, &mut handed)
    } else {
        None
    };
    let within = if active() || !session.is_empty() {
        inbox_note_within(store, scope, &agent_inbox(user_id, session, worker), &mut handed)
    } else {
        None
    };
    // who took this agent's handoffs, and who finished them
    let receipts = handoff_receipts(store, scope, user_id, session, worker);
    let receipts = (!receipts.is_empty()).then(|| receipts.join("\n"));
    let notes: Vec<String> = shared.into_iter().chain(within).chain(receipts).collect();
    if let (false, Some(map)) = (notes.is_empty(), response.as_object_mut()) {
        map.insert("inbox_note".into(), json!(notes.join("\n\n")));
    }
    handed
}

/// The messages the other agents of this agent's own session (its lead or
/// helpers) left on its inbox, as one note, taken off the queue.
fn inbox_note_within(store: &Store, scope: &str, inbox: &str, handed: &mut Vec<String>) -> Option<String> {
    let none = std::collections::BTreeSet::new();
    let messages: Vec<Value> = crate::agenttools::inbox_ack(store, scope, inbox, &none, &[])
        .get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    if messages.is_empty() {
        return None;
    }
    handed.extend(hand_over(store, scope, inbox, &messages));
    let lines: Vec<String> = vouched(store, scope, inbox, &messages).iter().map(crate::agenttools::message_line).collect();
    let heading = if messages.iter().any(crate::agenttools::is_handoff) {
        crate::agenttools::messages_heading(&messages, true)
    } else {
        "From the other agents of your own session (its lead or its helpers; not your user). Act on what asks \
something of you; to answer, run the cork message command with --to <sender>.".to_string()
    };
    Some(format!("{heading}\n{}", lines.join("\n")))
}

/// `POST /message {repo_id, session, text, to?, about?}`: an agent tells
/// the others something. `to` names one agent (as its messages and deltas
/// name it); `about` (`path.py::symbol`, a path or `dir/`) goes to whoever
/// is working on that code; neither goes to every agent live in the repo.
/// The receipt names each recipient as a person reads it (`sent_to_names`:
/// `Cork version number (cork-linear-test-e3)`), not by its inbox key.
///
/// A message to one agent (`to`, or a handoff) whose recipient is a Claude
/// Code session on the sender's machine (`peers`: the sessions the
/// sender's own Claude Code registry lists) is also held for the sender to
/// hand over through Claude Code's peer messaging (`peer` in the answer),
/// and `peer_done` says whether the post went through
/// (`agenttools::settle_peer`). The recipient's hooks then look for it in
/// its own transcript (`peer_seen`).
pub fn send(store: &Store, scope: &str, repo_id: &str, workspace: &str, from: &str, body: &Value, idle_after_s: f64) -> Value {
    let done = body.get("peer_done").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if !done.is_empty() {
        let to = body.get("to").and_then(Value::as_str).unwrap_or("").trim();
        return crate::agenttools::settle_peer(store, scope, from, to, &done, body.get("landed").and_then(Value::as_bool) == Some(true));
    }
    let mut out = send_unnamed(store, scope, repo_id, workspace, from, body, idle_after_s);
    let personal = body.get("handoff").and_then(Value::as_bool) == Some(true)
        || !body.get("to").and_then(Value::as_str).unwrap_or("").trim().is_empty();
    if personal && out.get("ok").and_then(Value::as_bool) == Some(true) {
        let handed = peer_handoffs(store, scope, from, body, &out);
        if !handed.is_empty() {
            out["peer"] = json!(handed);
        }
    }
    let sent: Vec<String> = out
        .get("sent_to")
        .or_else(|| out.get("delivered_to"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if !sent.is_empty() {
        out["sent_to_names"] = json!(sent.iter().map(|to| crate::headsup::recipient(store, scope, to)).collect::<Vec<_>>());
    }
    out
}

/// What a session's hooks found in its own transcript (`peer_seen` on any
/// hook post) of the messages posted to it through Claude Code: delivered,
/// and never printed by a hook.
pub fn peer_seen(store: &Store, scope: &str, user: &str, body: &Value) {
    let ids: Vec<String> =
        body.get("peer_seen").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).take(50).map(str::to_string).collect();
    let session = body.get("session").and_then(Value::as_str).unwrap_or("");
    if ids.is_empty() || session.is_empty() {
        return;
    }
    let inbox = if active() { user.to_string() } else { session_inbox(user, session) };
    crate::agenttools::peer_found(store, scope, &inbox, &ids);
}

/// The recipients of a message just sent that are Claude Code sessions on
/// its sender's machine (`peers`): each held back from the hooks and the
/// listener while the sender hands it over through Claude Code's own peer
/// messaging, and given back as that session will read it.
fn peer_handoffs(store: &Store, scope: &str, from: &str, body: &Value, out: &Value) -> Vec<Value> {
    let peers: Vec<&str> =
        body.get("peers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).take(64).collect();
    let id = text_of(out, "message_id");
    if peers.is_empty() || id.is_empty() {
        return Vec::new();
    }
    let account = from.split('#').next().unwrap_or(from);
    let sent: Vec<String> = out
        .get("sent_to")
        .or_else(|| out.get("delivered_to"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let mut handed = Vec::new();
    for to in sent {
        let inbox_of = |session: &str| if active() { agent_identity(account, session) } else { session_inbox(account, session) };
        let Some(session) = peers.iter().find(|p| !p.is_empty() && inbox_of(p) == to) else { continue };
        let Some(mut record) = store.kv_get("inbox", &format!("{scope}:{to}:{id}")) else { continue };
        crate::agenttools::claim_for_peer(store, scope, &id, &to, session, from);
        record["id"] = json!(id);
        record["age_s"] = json!(0);
        crate::agenttools::vouch(store, scope, &mut record, to.split('#').next().unwrap_or(&to), session);
        handed.push(json!({"to": to, "session": session, "message": record}));
    }
    handed
}

fn send_unnamed(store: &Store, scope: &str, repo_id: &str, workspace: &str, from: &str, body: &Value, idle_after_s: f64) -> Value {
    let text = |key: &str| body.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string();
    // `cork message --retract <id|last>`: take one back
    let retract = text("retract");
    if !retract.is_empty() {
        return crate::agenttools::retract_message(store, scope, from, &text("session"), &retract, "collide-hook");
    }
    // `cork message --show <id>`: one handed over cut, read whole
    let show = text("show");
    if !show.is_empty() {
        return crate::agenttools::show_message(store, scope, from, &text("session"), &show);
    }
    // `cork message --status`: what this agent sent, and how far it got
    if body.get("status").and_then(Value::as_bool) == Some(true) {
        let lines = crate::headsup::sent_status(store, scope, from, &text("session"), &text("worker"), 10);
        return json!({"ok": true, "lines": lines});
    }
    let message = text("text");
    if message.is_empty() {
        return json!({"ok": false, "error": "nothing to say: pass the message text"});
    }
    let (to, about) = (text("to"), text("about"));
    let own_session = text("session");
    // a helper (a subagent) inside the session, when the hooks named it
    let own_worker = text("worker");
    // `cork handoff`: work for ONE agent, not news for everyone
    if body.get("handoff").and_then(Value::as_bool) == Some(true) {
        return handoff_from(store, scope, workspace, from, &to, &own_session, &own_worker, &message, "collide-hook", idle_after_s);
    }
    // `--to NAME`: the agent that name means here, or nothing is sent
    let to = if to.is_empty() {
        to
    } else {
        match resolve_to(store, scope, from, &own_session, &own_worker, &to, idle_after_s) {
            Ok(inbox) => inbox,
            Err(why) => return json!({"ok": false, "error": why}),
        }
    };
    if !about.is_empty() {
        return crate::agenttools::send_anchored_message_from(
            store, scope, workspace, from, &own_session, &own_worker, &to, &message, &about, "collide-hook", "");
    }
    if !to.is_empty() {
        if !active() {
            if let Some(refusal) = crate::access::messaging_refusal(store, workspace, from, &to, "") {
                return json!({"ok": false, "error": refusal});
            }
        }
        // a helper signs as itself, so the answer reaches it
        let (by, session) = if own_worker.is_empty() {
            (from.to_string(), own_session.clone())
        } else {
            (agent_inbox(from, &own_session, &own_worker), format!("{own_session}/{own_worker}"))
        };
        return crate::agenttools::send_agent_message_in(store, scope, &by, &session, &to, &message, "collide-hook", false);
    }
    let _ = repo_id;
    broadcast_from(store, scope, workspace, from, &own_session, &own_worker, &message, "collide-hook", idle_after_s)
}

/// Where a handoff waits when no other session of its sender is open in the
/// repo: `{scope}:{owner}:{id}`, the owner being the sender's account (on
/// this machine's own server, every session is the one person's: "").
/// Its own bucket: `handoff.rs` keeps sessions' handoffs under "handoff"
/// with the same `{scope}:` prefix, and its briefing sweep deleted a waiting
/// one as an expired session handoff (v97).
pub const HANDOFF_BUCKET: &str = "handoffwait";

/// How long a session's listener counts as listening after its last wait
/// (each wait is `push::MAX_WAIT_S`, then it asks again at once).
pub const LISTENING_TTL_S: f64 = 90.0;

/// A session's listener is waiting on the server (`/inbox/wait`): a message
/// sent to that session alone starts it, however long it has been idle.
pub fn note_listening(store: &Store, scope: &str, user: &str, session: &str) {
    if scope.is_empty() || session.is_empty() {
        return;
    }
    let _ = store.eph_set(&format!("listening:{scope}:{}", session_inbox(user, session)), &json!({"ts": now()}), Some(LISTENING_TTL_S));
}

fn handoff_owner(from: &str) -> &str {
    if active() { "" } else { from.split('#').next().unwrap_or(from) }
}

/// The one agent a handoff with no recipient goes to: the sender's other
/// session in this repo, never the sending one and never a teammate's (a
/// handoff to them names them). First one whose listener is waiting and
/// that is not mid-turn (it starts on it at once), then any listening, then
/// the most recently active one at work (it hears at its next step).
/// None when no other session of the sender is open here.
fn handoff_target(store: &Store, scope: &str, from: &str, own_session: &str, idle_after_s: f64) -> Option<String> {
    let visible = std::collections::BTreeSet::new();
    let repo_id = crate::presence::split_scope(scope).1.to_string();
    let view = crate::activity::list_activity(store, scope, &repo_id, idle_after_s, &visible);
    let prefix = format!("listening:{scope}:");
    let listening: std::collections::BTreeSet<String> = store
        .eph_scan(&prefix)
        .into_iter()
        .filter_map(|(key, _)| key.strip_prefix(&prefix).map(str::to_string))
        .collect();
    let mut best: Option<((u8, u8, u64), String)> = None;
    let mut consider = |to: String, listens: bool, running: bool, idle_s: f64| {
        let rank = (u8::from(!listens), u8::from(running), (idle_s.max(0.0) * 1000.0) as u64);
        if best.as_ref().is_none_or(|(have, _)| rank < *have) {
            best = Some((rank, to));
        }
    };
    let mine = |user: &str| active() || user == from.split('#').next().unwrap_or(from);
    let mut seen = std::collections::BTreeSet::new();
    for row in view.get("workspaces").and_then(Value::as_array).into_iter().flatten() {
        let user = row.get("user").and_then(Value::as_str).unwrap_or("");
        let session = row.get("session").and_then(Value::as_str).unwrap_or("");
        let ended = row.get("ended").and_then(Value::as_bool).unwrap_or(false);
        if session.is_empty() || session == own_session || ended || !mine(user) {
            continue;
        }
        if row.get("repo_id").and_then(Value::as_str).is_some_and(|r| r != repo_id) {
            continue;
        }
        let to = if active() { user.to_string() } else { session_inbox(user, session) };
        let listens = listening.contains(&session_inbox(user, session));
        let online = row.get("online").and_then(Value::as_bool).unwrap_or(false);
        if !listens && !online {
            continue;
        }
        let running = row.get("running").and_then(Value::as_bool).unwrap_or(false);
        let idle_s = row.get("last_activity_s").and_then(Value::as_f64).unwrap_or(f64::MAX);
        seen.insert(session_inbox(user, session));
        consider(to, listens, running, idle_s);
    }
    // a listener whose session the activity view no longer lists (idle past
    // its window): still open, and it starts on a message by itself
    for inbox in listening.iter().filter(|inbox| !seen.contains(*inbox)) {
        let Some((user, session)) = inbox.split_once('#') else { continue };
        if session == own_session || !mine(user) {
            continue;
        }
        consider(if active() { user.to_string() } else { inbox.clone() }, true, false, f64::MAX / 2.0);
    }
    best.map(|(_, to)| to)
}

/// `cork handoff` (and `send_agent_message` with `handoff`): work the person
/// wants another agent to do, as a self-contained spec. Unlike a note to
/// everyone at work, it goes to exactly ONE agent, so no two start on it:
/// the one `to` names, else the person's other session in this repo
/// (`handoff_target`). With none open it waits (`HANDOFF_BUCKET`), and the
/// next session the person starts in this repo takes it (`take_handoffs`).
#[allow(clippy::too_many_arguments)]
pub fn handoff(
    store: &Store, scope: &str, workspace: &str, from: &str, to: &str, own_session: &str, message: &str, agent: &str,
    idle_after_s: f64,
) -> Value {
    handoff_from(store, scope, workspace, from, to, own_session, "", message, agent, idle_after_s)
}

/// `handoff` from one agent of the session: a helper signs as itself.
#[allow(clippy::too_many_arguments)]
pub fn handoff_from(
    store: &Store, scope: &str, workspace: &str, from: &str, to: &str, own_session: &str, own_worker: &str, message: &str,
    agent: &str, idle_after_s: f64,
) -> Value {
    let message = message.trim();
    if message.is_empty() {
        return json!({"ok": false, "error": "nothing to hand off: pass the spec"});
    }
    let signed = signer(from, own_session, own_worker);
    let to = match to.trim() {
        "" => handoff_target(store, scope, from, own_session, idle_after_s),
        named => match resolve_to(store, scope, from, own_session, own_worker, named, idle_after_s) {
            Ok(inbox) => Some(inbox),
            Err(why) => return json!({"ok": false, "error": why}),
        },
    };
    if let Some(to) = to {
        if !active() {
            if let Some(refusal) = crate::access::messaging_refusal(store, workspace, from, &to, "") {
                return json!({"ok": false, "error": refusal});
            }
        }
        let mut sent = crate::agenttools::send_handoff_in(store, scope, &signed, own_session, &to, message, agent);
        if sent.get("ok").and_then(Value::as_bool) == Some(true) {
            follow_handoff(store, scope, &text_of(&sent, "message_id"), &signed, message);
            sent["sent_to"] = json!([to]);
            sent["handoff"] = json!(true);
        }
        return sent;
    }
    let id = crate::compat::new_id();
    let ts = now();
    let record = json!({
        "id": id, "to": "", "from": signed, "message": message.chars().take(4000).collect::<String>(), "agent": agent,
        "ts": ts, "handoff": true, "from_session": own_session, "sender_session": own_session,
    });
    let _ = store.kv_put(HANDOFF_BUCKET, &format!("{scope}:{}:{id}", handoff_owner(from)), &record, ts);
    follow_handoff(store, scope, &id, &signed, message);
    let mut row = record.clone();
    row["to"] = json!("the next session");
    crate::agenttools::file_thread(store, scope, &crate::agenttools::with_sender(store, scope, &row, own_session));
    let _ = store.ledger_append(scope, "agent_message", &json!({
        "to": "", "from": signed, "id": id, "chars": message.chars().count(), "handoff": true,
    }), ts);
    json!({
        "ok": true, "queued": true, "handoff": true, "message_id": id, "sent_to": [],
        "note": "no other session of yours is open in this repo, so the handoff waits: the next session you start here picks it up",
    })
}

/// The handoffs waiting for this person in this repo, taken by `session`
/// (its start): each is handed to one session only, and one older than a
/// message's life (`agenttools::MESSAGE_TTL_S`) is dropped, not acted on.
pub fn take_handoffs(store: &Store, scope: &str, user: &str, session: &str) -> Vec<Value> {
    if scope.is_empty() {
        return Vec::new();
    }
    let at = now();
    let mut taken = Vec::new();
    for (key, mut record) in store.kv_list(HANDOFF_BUCKET, &format!("{scope}:{}:", handoff_owner(user))) {
        let sent = record.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        if crate::agenttools::is_expired(sent, at) {
            let _ = store.kv_delete(HANDOFF_BUCKET, &key);
            continue;
        }
        // the session that sent it, back after a restart, is not who it was for
        if !session.is_empty() && record.get("from_session").and_then(Value::as_str) == Some(session) {
            continue;
        }
        let _ = store.kv_delete(HANDOFF_BUCKET, &key);
        let taker = if active() { user.to_string() } else { session_inbox(user, session) };
        handoff_taken(store, record.get("id").and_then(Value::as_str).unwrap_or(""), &taker);
        record["age_s"] = json!((at - sent).max(0.0));
        // who sent it, as Cork authenticated it
        crate::agenttools::vouch(store, scope, &mut record, user, session);
        taken.push(record);
    }
    taken.sort_by(|a, b| {
        let ts = |m: &Value| m.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        ts(a).partial_cmp(&ts(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    taken
}

// ------------------------------------------------------------ finishing

/// A turn's closing words that say the work is done: not where unfinished
/// work stands, and nothing left to do in them.
pub fn reads_finished(said: &str) -> bool {
    !said.trim().is_empty() && !crate::intents::in_progress(said) && crate::handoff::left_items(said).is_empty()
}

/// An inbox as `(user, session)`: `kevin@x.com#s2` or `kevin@x.com#s2/lead`
/// on a team; on this machine's own server an agent's identity is its inbox
/// (`agent-3f2a91c0`), and its session is the one its hooks reported.
fn session_of_inbox(store: &Store, scope: &str, inbox: &str) -> Option<(String, String)> {
    let inbox = inbox.trim();
    if inbox.is_empty() || inbox == "*" {
        return None;
    }
    if let Some((user, rest)) = inbox.split_once('#') {
        let session = rest.split('/').next().unwrap_or("");
        return (!session.is_empty()).then(|| (user.to_string(), session.to_string()));
    }
    let agent = inbox.split('/').next().unwrap_or(inbox);
    if !active() || !agent.starts_with("agent-") {
        return None;
    }
    match crate::freshness::sessions_of(store, scope, agent).as_slice() {
        [session] => Some((agent.to_string(), session.clone())),
        _ => None,
    }
}

/// The sessions this one exchanged a message or a handoff with in this repo
/// (to it, or from it; not a note to everyone at work, nor one routed by
/// code), each with when they last did: `(user, session) -> ts`.
fn partners(store: &Store, scope: &str, user: &str, session: &str) -> std::collections::BTreeMap<(String, String), f64> {
    let mut out: std::collections::BTreeMap<(String, String), f64> = std::collections::BTreeMap::new();
    let me = (user.to_string(), session.to_string());
    let since = now() - crate::agenttools::ROUTE_WINDOW_S;
    for (_, row) in store.kv_list(crate::agenttools::THREAD_BUCKET, &format!("{scope}:")) {
        let ts = row.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        if ts < since || row.get("broadcast").and_then(Value::as_bool) == Some(true) || row.get("routed").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let mut to: Vec<String> = vec![text_of(&row, "to")];
        to.extend(row.get("to_list").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string));
        to.extend(row.get("ids").and_then(Value::as_object).into_iter().flatten().map(|(who, _)| who.clone()));
        let recipients: Vec<(String, String)> = to.iter().filter_map(|inbox| session_of_inbox(store, scope, inbox)).collect();
        let from_session = text_of(&row, "from_session");
        let from_user = text_of(&row, "from").split('#').next().unwrap_or("").split('/').next().unwrap_or("").to_string();
        let sender = (!from_session.is_empty()).then(|| (from_user, from_session.split('/').next().unwrap_or("").to_string()));
        let mut met = |who: (String, String)| {
            if who != me && !who.1.is_empty() {
                let at = out.entry(who).or_insert(0.0);
                *at = at.max(ts);
            }
        };
        if sender.as_ref() == Some(&me) {
            for who in recipients {
                met(who);
            }
        } else if recipients.contains(&me) {
            if let Some(who) = sender {
                met(who);
            }
        }
    }
    out
}

/// This session finished: its task (a turn whose closing words say it is
/// done) or the session itself (`ended`). Each session it exchanged messages
/// or handoffs with is told so once, with its closing words, on its own
/// inbox: one session waited on another by reading its log (v104's test).
/// Once per round of messages: a later exchange earns a later notice.
pub fn tell_partners_finished(store: &Store, scope: &str, user: &str, session: &str, said: &str, ended: bool) {
    if session.is_empty() {
        return;
    }
    let said = crate::presence::first_sentences(said, 2, 300);
    let named = crate::freshness::session_label(store, scope, user, session);
    let who = if named.is_empty() { "Your other session".to_string() } else { named };
    let what = match (ended, said.is_empty()) {
        (true, true) => format!("Cork: {who} finished: its session ended."),
        (true, false) => format!("Cork: {who} finished: its session ended. Its last words: \"{said}\""),
        (false, _) => format!("Cork: {who} finished its task: \"{said}\""),
    };
    let stamp = now();
    for ((partner_user, partner), last) in partners(store, scope, user, session) {
        if store.eph_get(&format!("sessended:{scope}:{partner}")).is_some() {
            continue;
        }
        let told = format!("finishtold:{scope}:{session}:{partner}:{last:.3}");
        if !crate::access::tell_once(store, &told, crate::agenttools::MESSAGE_TTL_S) {
            continue;
        }
        let inbox = if active() { partner_user.clone() } else { session_inbox(&partner_user, &partner) };
        let id = crate::compat::new_id();
        let record = json!({
            "id": id, "to": inbox, "from": signer(user, session, ""), "message": what, "agent": "collide-hook", "ts": stamp,
            "sender_session": session, "finished": true,
        });
        let _ = store.kv_put("inbox", &format!("{scope}:{inbox}:{id}"), &record, stamp);
        crate::push::arrived();
    }
}

// ------------------------------------------------------------ handoff receipts

/// A handoff followed for its sender, by its id: `{scope, from, what,
/// taker, taken_at}`. The sender hears, at its next hook output, who took
/// it, and then when the taker finished its turn or its session ended.
fn follow_key(id: &str) -> String {
    format!("handofffollow:{id}")
}

/// The handoffs an agent (by its inbox) has taken and not finished.
fn taken_key(scope: &str, taker: &str) -> String {
    format!("handofftaken:{scope}:{taker}")
}

/// What waits to be said to a sender (by its inbox): `[{event, taker, what, at, said}]`.
fn receipts_key(scope: &str, sender: &str) -> String {
    format!("handoffreceipt:{scope}:{sender}")
}

fn text_of(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn follow_handoff(store: &Store, scope: &str, id: &str, from: &str, message: &str) {
    if id.is_empty() {
        return;
    }
    let words: String = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let what = if words.chars().count() > 60 { format!("{}…", words.chars().take(60).collect::<String>()) } else { words };
    let _ = store.eph_set(&follow_key(id), &json!({"scope": scope, "from": from, "what": what}), Some(crate::agenttools::MESSAGE_TTL_S));
}

fn tell_sender(store: &Store, scope: &str, follow: &Value, event: &str, taker: &str, said: &str) {
    let key = receipts_key(scope, &text_of(follow, "from"));
    let mut waiting: Vec<Value> = store.eph_get(&key).and_then(|v| v.as_array().cloned()).unwrap_or_default();
    waiting.push(json!({"event": event, "taker": taker, "what": text_of(follow, "what"), "at": now(), "said": said}));
    let _ = store.eph_set(&key, &json!(waiting), Some(crate::agenttools::MESSAGE_TTL_S));
}

/// A handoff was handed to `taker` (its inbox): its sender hears who took
/// it, once.
pub fn handoff_taken(store: &Store, id: &str, taker: &str) {
    if id.is_empty() || taker.is_empty() {
        return;
    }
    let key = follow_key(id);
    let Some(mut follow) = store.eph_get(&key) else { return };
    if !text_of(&follow, "taker").is_empty() {
        return;
    }
    let scope = text_of(&follow, "scope");
    follow["taker"] = json!(taker);
    follow["taken_at"] = json!(now());
    let _ = store.eph_set(&key, &follow, Some(crate::agenttools::MESSAGE_TTL_S));
    let open = taken_key(&scope, taker);
    let mut ids: Vec<Value> = store.eph_get(&open).and_then(|v| v.as_array().cloned()).unwrap_or_default();
    ids.push(json!(id));
    let _ = store.eph_set(&open, &json!(ids), Some(crate::agenttools::MESSAGE_TTL_S));
    tell_sender(store, &scope, &follow, "taken", taker, "");
}

/// An agent's turn ended (`said`, its closing message) or its session or
/// helper did (`ended`): the senders of the handoffs it took hear so, and
/// they are no longer followed.
pub fn handoffs_finished(store: &Store, scope: &str, user: &str, session: &str, worker: &str, said: &str, ended: bool) {
    if session.is_empty() {
        return;
    }
    let takers = if !worker.is_empty() {
        vec![agent_inbox(user, session, worker)]
    } else if active() {
        vec![user.to_string(), agent_inbox(user, session, "")]
    } else {
        vec![session_inbox(user, session), agent_inbox(user, session, "")]
    };
    let said = crate::presence::first_sentences(said, 1, 200);
    for taker in takers {
        let open = taken_key(scope, &taker);
        let Some(ids) = store.eph_get(&open).and_then(|v| v.as_array().cloned()) else { continue };
        store.eph_delete(&open);
        for id in ids.iter().filter_map(Value::as_str) {
            let Some(follow) = store.eph_get(&follow_key(id)) else { continue };
            store.eph_delete(&follow_key(id));
            tell_sender(store, scope, &follow, if ended { "ended" } else { "done" }, &taker, &said);
        }
    }
}

/// The receipts waiting for this agent as the sender of handoffs, as lines,
/// taken off the queue.
fn handoff_receipts(store: &Store, scope: &str, user: &str, session: &str, worker: &str) -> Vec<String> {
    let senders = if !worker.is_empty() {
        vec![agent_inbox(user, session, worker)]
    } else if active() {
        vec![user.to_string()]
    } else {
        vec![session_inbox(user, session)]
    };
    let stamp = now();
    let mut lines = Vec::new();
    for sender in senders {
        let key = receipts_key(scope, &sender);
        let Some(waiting) = store.eph_get(&key).and_then(|v| v.as_array().cloned()) else { continue };
        store.eph_delete(&key);
        for r in waiting {
            let who = crate::headsup::recipient(store, scope, &text_of(&r, "taker"));
            let when = crate::headsup::ago(r.get("at").and_then(Value::as_f64).unwrap_or(stamp), stamp);
            let what = text_of(&r, "what");
            let said = text_of(&r, "said");
            lines.push(match text_of(&r, "event").as_str() {
                "taken" => format!("Cork: {who} took your handoff (\"{what}\") {when}; you hear here when it finishes."),
                "done" if said.is_empty() => format!("Cork: {who} finished a turn on your handoff (\"{what}\") {when}."),
                "done" => format!("Cork: {who} finished a turn on your handoff (\"{what}\") {when}: {said}"),
                _ => format!("Cork: {who} ended {when}, after taking your handoff (\"{what}\"). Check that it was done."),
            });
        }
    }
    lines
}

/// One message to every agent at work in this repo right now, each on its
/// own inbox (the sender's own other sessions too), never the sender's own
/// session and never an agent in another repo: news for whoever is working,
/// carried by their next hook. On a plan that keeps messages to the
/// sender's own agents, teammates are left out (as a message to them by
/// name is refused). `sent_to` is empty, with a note, when nobody is.
#[allow(clippy::too_many_arguments)]
pub fn broadcast(
    store: &Store, scope: &str, workspace: &str, from: &str, own_session: &str, message: &str, agent: &str, idle_after_s: f64,
) -> Value {
    broadcast_from(store, scope, workspace, from, own_session, "", message, agent, idle_after_s)
}

/// `broadcast` from one agent of the session. The session's own lead and
/// helpers are agents at work too: a helper's note reaches its lead and the
/// other helpers, the lead's reaches its helpers (only the ones on the code
/// it names, when it names code), each on its own inbox (`agent_inbox`),
/// never the agent that sent it. A helper shares its lead's session, and
/// counting the session as "the sender" left a lead and its helper unable
/// to reach each other ("no other agent is at work").
#[allow(clippy::too_many_arguments)]
pub fn broadcast_from(
    store: &Store, scope: &str, workspace: &str, from: &str, own_session: &str, own_worker: &str, message: &str, agent: &str,
    idle_after_s: f64,
) -> Value {
    let visible = std::collections::BTreeSet::new();
    let repo_id = crate::presence::split_scope(scope).1.to_string();
    let view = crate::activity::list_activity(store, scope, &repo_id, idle_after_s, &visible);
    let mut sent_to: Vec<String> = Vec::new();
    let mut ids = serde_json::Map::new();
    let mut withheld = 0usize;
    // on a team the sender's own other sessions count too, each addressed on
    // its own (here every session already is its own agent)
    let signed = signer(from, own_session, own_worker);
    // a message that names code is for the agents on that code (their
    // reads and writes, as deltas are targeted); one that names none, for
    // everyone at work
    let on_it = crate::deltas::sessions_named_by(store, scope, message);
    for row in view.get("workspaces").and_then(Value::as_array).into_iter().flatten() {
        let user = row.get("user").and_then(Value::as_str).unwrap_or("").to_string();
        let session = row.get("session").and_then(Value::as_str).unwrap_or("").to_string();
        let online = row.get("online").and_then(Value::as_bool).unwrap_or(false);
        if on_it.as_ref().is_some_and(|sessions| !sessions.contains(&session)) {
            continue;
        }
        // only this repo's agents: the view is this scope's, and a row that
        // names another repo is not here
        if row.get("repo_id").and_then(Value::as_str).is_some_and(|r| r != repo_id) {
            continue;
        }
        let to = if user == from {
            if active() || session.is_empty() || session == own_session {
                continue;
            }
            session_inbox(&user, &session)
        } else {
            user
        };
        if !online || to.is_empty() || sent_to.contains(&to) {
            continue;
        }
        if !active() && crate::access::messaging_refusal(store, workspace, from, &to, "").is_some() {
            withheld += 1;
            continue;
        }
        let sent = crate::agenttools::send_agent_message_in(store, scope, &signed, own_session, &to, message, agent, true);
        if let Some(id) = sent.get("message_id").and_then(Value::as_str) {
            ids.insert(to.clone(), json!(id));
        }
        sent_to.push(to);
    }
    // inside the sender's own session: a helper's lead, and the helpers at work
    let helpers = if active() || !own_session.is_empty() { session_workers(store, scope, from, own_session) } else { Vec::new() };
    let mut within: Vec<String> = Vec::new();
    if !own_worker.is_empty() {
        within.push(agent_inbox(from, own_session, ""));
    }
    for (worker, path) in &helpers {
        if *worker == own_worker || (on_it.is_some() && !names_path(message, path)) {
            continue;
        }
        within.push(agent_inbox(from, own_session, worker));
    }
    for to in within {
        let sent = crate::agenttools::send_agent_message_in(store, scope, &signed, own_session, &to, message, agent, true);
        if let Some(id) = sent.get("message_id").and_then(Value::as_str) {
            ids.insert(to.clone(), json!(id));
        }
        sent_to.push(to);
    }
    if sent_to.is_empty() {
        // who is actually here, so the agent stops retrying
        let others = helpers.iter().filter(|(worker, _)| *worker != own_worker).count();
        let session_part = match others {
            0 => "and this session has no helper running".to_string(),
            1 => "and this session's one helper is on other code".to_string(),
            n => format!("and this session's {n} helpers are on other code"),
        };
        let note = if on_it.is_some() {
            format!("no other agent at work has read or written the code this message names, {session_part}; nothing was sent")
        } else {
            format!("no other session is at work in this repo right now, {session_part}; nothing was sent")
        };
        let mut out = json!({"ok": true, "sent_to": [], "delivered_to": [], "note": note});
        if withheld > 0 {
            out["withheld"] = json!(withheld);
        }
        return out;
    }
    let row = json!({
        "id": crate::compat::new_id(), "to": "*", "to_list": sent_to, "from": signed, "message": message,
        "agent": agent, "ts": now(), "broadcast": true, "ids": ids,
    });
    crate::agenttools::file_thread(store, scope, &crate::agenttools::with_sender(store, scope, &row, own_session));
    let mut out = json!({"ok": true, "message_id": row.get("id").cloned().unwrap_or(Value::Null), "sent_to": sent_to, "delivered_to": sent_to, "broadcast": true});
    if withheld > 0 {
        out["withheld"] = json!(withheld);
    }
    out
}

// ------------------------------------------------------------ the summary

/// What Cork did on this machine since `since`, across every repo: the
/// savings and the spend, how many agents worked and in how many repos, the
/// changes they told each other about and the messages they sent. The
/// hooks print it once a day; `cork status` prints it on demand.
pub fn summary(store: &Store, workspace: &str, since: f64) -> Value {
    let scopes: Vec<String> = store.list_scopes(&format!("{workspace}:"));
    let days = ((now() - since) / 86_400.0).max(0.02);
    let saved = crate::briefstat::savings_across(store, &scopes, days);
    let spent = crate::insights::costs_across(store, &scopes, days);
    let (mut changes, mut messages) = (0u64, 0u64);
    let mut agents: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut repos: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for scope in &scopes {
        for row in store.ledger_since(scope, since) {
            let who = row.payload.get("user").and_then(Value::as_str).unwrap_or("");
            match row.kind.as_str() {
                "edit_reported" => {
                    changes += 1;
                    if who.starts_with("agent-") {
                        agents.insert(who.to_string());
                    }
                    repos.insert(scope.split_once(':').map(|(_, r)| r).unwrap_or(scope).to_string());
                }
                "agent_message" => messages += 1,
                _ => {}
            }
        }
    }
    let totals = &saved["totals"];
    json!({
        "since": since,
        "tokens_saved": totals["tokens_saved"], "est_usd_saved": totals["est_usd"], "messages_not_sent": totals["messages"],
        "tokens_spent": spent["total_tokens"], "est_usd_spent": spent["est_usd"],
        "agents": agents.len(), "repos": repos.len(), "changes": changes, "agent_messages": messages,
        "by_repo": saved["by_repo"],
    })
}

// ------------------------------------------------------------ the sync

/// One batch of this machine's record for the cloud: per repo, the ledger
/// rows after the cursor (`{scope: seq}`) of the kinds a sync carries, and
/// every note on the first batch. `more` says another batch is waiting.
pub fn export(store: &Store, workspace: &str, cursor: &Value, limit: usize) -> Value {
    let prefix = format!("{workspace}:");
    let mut repos: Vec<Value> = Vec::new();
    let mut next = cursor.as_object().cloned().unwrap_or_default();
    let mut budget = limit;
    let mut more = false;
    for scope in store.list_scopes(&prefix) {
        let repo_id = scope[prefix.len()..].to_string();
        let after = cursor.get(&scope).and_then(Value::as_i64).unwrap_or(0);
        let first = after == 0;
        let mut rows: Vec<Value> = Vec::new();
        let mut last = after;
        for row in store.ledger_since(&scope, 0.0) {
            if row.seq <= after || !crate::machines::SYNCED_KINDS.contains(&row.kind.as_str()) {
                continue;
            }
            if budget == 0 {
                more = true;
                break;
            }
            rows.push(json!({"seq": row.seq, "kind": row.kind, "ts": row.ts, "payload": row.payload}));
            last = last.max(row.seq);
            budget -= 1;
        }
        let notes: Vec<Value> = if first {
            store.kv_list("memory", &format!("{scope}:")).into_iter().map(|(_, v)| v).collect()
        } else {
            Vec::new()
        };
        next.insert(scope.clone(), json!(last));
        if !rows.is_empty() || !notes.is_empty() {
            repos.push(json!({"repo_id": repo_id, "rows": rows, "notes": notes}));
        }
        if more {
            break;
        }
    }
    let all: Vec<String> = store.list_scopes(&prefix).into_iter().map(|s| s[prefix.len()..].to_string()).collect();
    json!({"ok": true, "repos": repos, "cursor": next, "more": more, "all_repos": all})
}

/// A local server whose binary was replaced (an update) writes what it
/// holds and exits; the hooks start the new one on the next agent event.
pub fn watch_for_update(store: std::sync::Arc<Store>) {
    let Ok(exe) = std::env::current_exe() else { return };
    let stamp = |path: &std::path::Path| std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()));
    let first = stamp(&exe);
    let _ = std::thread::Builder::new().name("collide-local-update".into()).spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        let now = stamp(&exe);
        if now.is_some() && now != first {
            tracing::info!("cork local: the binary was updated; writing state and exiting for the new one");
            store.flush();
            store.flush_trees(true);
            std::process::exit(0);
        }
    });
}
