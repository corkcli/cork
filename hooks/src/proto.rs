//! Cork's real-time protocol, from the hooks' side (server/protocol/SPEC.md).
//!
//! Every write an agent makes becomes typed ops on versioned keys: one per
//! top-level symbol it changes (`code:<path>::<symbol>`), or one for the
//! whole file where no parser knows the language (`file:<path>`). The server
//! settles two agents on the same function in one round trip, from key
//! names, kinds, versions and hashes; it never sees the code (no payload is
//! sent).
//!
//! - The gate (PreToolUse) works out, in memory, what the write will change,
//!   remembers the file as it is now (the "before"), and `check`s those keys.
//!   It blocks only when the server names someone: a conflicting op since
//!   this copy, or a conflicting lease. Otherwise the session leases the keys.
//! - The report (PostToolUse) diffs the file on disk against the "before"
//!   and sends the ops; a reject becomes a notice in the agent's context.
//! - Teammates' changes arrive as pushed deltas, written by the session's
//!   daemon (protod.rs) to a spool that the next hook output carries as Δ
//!   lines.
//!
//! Bases are content hashes (`cork_core::local::symbol_hash`): a hook
//! knows what is on disk, not which version the server gave it, so it names
//! the content and the server finds the version (`base_of`).
//!
//! The session's daemon holds the connection; a hook asks it over a unix
//! socket. When there is no daemon, a hook connects itself, briefly, and
//! anything slow or failing allows the write: the protocol never stops work
//! because the network is slow. Off with `"protocol": false` in
//! .collide/config.json or `CORK_PROTOCOL=0`; not on Windows (no daemon).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cork_protocol::ws_client::BlockingClient;
use cork_protocol::{Base, ClientConfig, ServerFrame};
use cork_protocol_code::{file_key, symbol_key};
use serde_json::{json, Map, Value};

use crate::config::{self, Env};

/// A lease is released after this long without the session touching its key.
pub const LEASE_IDLE: Duration = Duration::from_secs(600);
/// How long a hook waits for its own short-lived connection.
pub const DIRECT_BUDGET: Duration = Duration::from_millis(1500);
/// How long the report waits for the daemon's answer to its ops; past it,
/// the daemon spools the answer for the next hook.
pub const OPS_WAIT: Duration = Duration::from_millis(300);
/// After a failed direct connection, hooks stop trying for this long.
const DOWN_FOR_S: f64 = 60.0;
/// A file larger than this is not keyed (the report skips it too).
const MAX_FILE_BYTES: u64 = 512 * 1024;
/// At most this many ops from one write, and keys subscribed from one read.
const MAX_OPS: usize = 200;
const MAX_READ_KEYS: usize = 100;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

// ------------------------------------------------------------ switch and identity

/// The protocol runs for this repo: on unless turned off, never on Windows.
/// On the free version's local server only when asked (`CORK_PROTOCOL=1`):
/// it speaks it, but one machine's agents are few and a daemon per session
/// is not yet worth it there by default.
pub fn enabled(root: &Path, cfg: &config::Config, env: &Env) -> bool {
    let switch = config::get(env, "CORK_PROTOCOL").trim().to_string();
    if !cfg!(unix) || !cfg.usable() || switch == "0" || (cfg.machine_local && switch != "1") {
        return false;
    }
    config::repo_config(&root).get("protocol").and_then(Value::as_bool) != Some(false)
}

/// `<server>/protocol/v1/ws`, wss for an https server.
pub fn ws_url(server: &str, env: &Env) -> String {
    let given = config::get(env, "CORK_PROTOCOL_URL");
    if !given.is_empty() {
        return given.to_string();
    }
    let base = server.trim_end_matches('/');
    let ws = match (base.strip_prefix("https://"), base.strip_prefix("http://")) {
        (Some(rest), _) => format!("wss://{rest}"),
        (_, Some(rest)) => format!("ws://{rest}"),
        _ => base.to_string(),
    };
    format!("{ws}/protocol/v1/ws")
}

/// The hello for `session`: the token the hook already sends to /report and
/// the repo id as the scope. The person is the git identity; a server with
/// an authorizer takes it from the token instead.
pub fn client_config(root: &Path, cfg: &config::Config, session: &str, agent: &str) -> ClientConfig {
    let person = crate::git::git(root, &["config", "user.email"], Duration::from_secs(2)).unwrap_or_default().trim().to_string();
    ClientConfig {
        session: session.to_string(),
        agent_id: if agent.is_empty() { "agent".to_string() } else { agent.to_string() },
        person,
        scope: cfg.repo_id.clone(),
        token: Some(cfg.token.clone()),
    }
}

// ------------------------------------------------------------ the base

/// A base named by content (SPEC.md §5.1): the server resolves the version
/// from the key's history, and answers `unknown_base` for content it never
/// saw. No content ("" for a key the write creates) is the key as never
/// written. The ONE place the representation lives.
pub fn base_of(content_hash: &str) -> Base {
    Base { version: 0, hash: String::new(), content: (!content_hash.is_empty()).then(|| content_hash.to_string()) }
}

/// What a session took in after the server named someone else's work on a
/// key (a block at the gate, a refused op): that work's version. The
/// agent was told to bring it in, so its next write builds on it, with the
/// chain hash, not on content the server cannot know (a local merge is new
/// content). Gone once an op on the key is accepted.
fn adopted_path(env: &Env, session: &str) -> PathBuf {
    session_dir(env, session).join("adopted.json")
}

const ADOPTED_FOR_S: f64 = 3600.0;

pub fn adopt(env: &Env, session: &str, key: &str, current: &Value) {
    let (Some(version), Some(hash)) = (current.get("version").and_then(Value::as_u64), current.get("hash").and_then(Value::as_str)) else {
        return;
    };
    if version == 0 || key.is_empty() {
        return;
    }
    let path = adopted_path(env, session);
    let mut all = config::load_json(&path);
    all[key] = json!({"version": version, "hash": hash, "ts": now_s()});
    save(&path, &all);
}

fn adopted(env: &Env, session: &str) -> Map<String, Value> {
    config::load_json(&adopted_path(env, session)).as_object().cloned().unwrap_or_default()
}

fn forget_adopted(env: &Env, session: &str, keys: &BTreeSet<String>) {
    let mut all = adopted(env, session);
    let before = all.len();
    all.retain(|k, _| !keys.contains(k));
    if all.len() != before {
        save(&adopted_path(env, session), &Value::Object(all));
    }
}

/// Planned ops on keys the session adopted build on that version.
fn with_adopted(env: &Env, session: &str, mut planned: Vec<Planned>) -> Vec<Planned> {
    let all = adopted(env, session);
    if all.is_empty() {
        return planned;
    }
    for p in &mut planned {
        let Some(row) = all.get(&p.key) else { continue };
        if row.get("ts").and_then(Value::as_f64).is_some_and(|t| now_s() - t > ADOPTED_FOR_S) {
            continue;
        }
        p.adopted = Some(Base { version: row.get("version").and_then(Value::as_u64).unwrap_or(0), hash: text(row, "hash"), content: None });
    }
    planned
}

/// The content an op leaves behind (`content_hash`, SPEC.md §4.1), so the
/// server can resolve the next base that names it ("" after a delete).
fn note_content(op: &mut Value, after_hash: &str) {
    op["content_hash"] = json!(after_hash);
}

// ------------------------------------------------------------ files as keys

/// One declaration as the protocol keys it.
#[derive(Debug, Clone, PartialEq)]
pub struct Sym {
    pub name: String,
    pub signature: String,
    pub hash: String,
    pub shape: String,
}

/// A file as the protocol sees it at one moment.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// Not on disk.
    Absent,
    /// No parser knows it: one key, the whole file's hash.
    File(String),
    /// Its top-level declarations, in order.
    Code(Vec<Sym>),
    /// A parser knows it but the parse failed: nothing can be keyed.
    Broken,
}

pub fn shape_of(rel: &str, content: Option<&str>) -> Shape {
    let Some(content) = content else { return Shape::Absent };
    match cork_core::local::symbols(rel, content) {
        None => Shape::File(cork_core::local::symbol_hash(content)),
        Some(parsed) if !parsed.clean => Shape::Broken,
        Some(parsed) => Shape::Code(
            parsed
                .symbols
                .into_iter()
                .map(|s| Sym { name: s.name, signature: s.signature, hash: s.hash, shape: s.shape })
                .collect(),
        ),
    }
}

impl Shape {
    pub fn to_json(&self) -> Value {
        match self {
            Shape::Absent => json!({"t": "absent"}),
            Shape::File(hash) => json!({"t": "file", "hash": hash}),
            Shape::Broken => json!({"t": "broken"}),
            Shape::Code(symbols) => json!({"t": "code", "symbols": symbols.iter()
                .map(|s| json!([s.name, s.signature, s.hash, s.shape])).collect::<Vec<_>>()}),
        }
    }

    pub fn from_json(value: &Value) -> Option<Shape> {
        Some(match value.get("t")?.as_str()? {
            "absent" => Shape::Absent,
            "file" => Shape::File(text(value, "hash")),
            "broken" => Shape::Broken,
            "code" => Shape::Code(
                value
                    .get("symbols")?
                    .as_array()?
                    .iter()
                    .filter_map(|s| {
                        let s = s.as_array()?;
                        let at = |i: usize| s.get(i).and_then(Value::as_str).map(str::to_string);
                        Some(Sym { name: at(0)?, signature: at(1)?, hash: at(2)?, shape: at(3)? })
                    })
                    .collect(),
            ),
            _ => return None,
        })
    }
}

/// A symbol's key. A name with spaces (`impl Foo`) cannot be a key as is.
pub fn sym_key(rel: &str, name: &str) -> String {
    symbol_key(rel, &name.split_whitespace().collect::<Vec<_>>().join("_"))
}

fn key_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join("_")
}

/// One op a write makes, before it has an id.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub key: String,
    pub kind: &'static str,
    pub public: Map<String, Value>,
    /// What the op builds on: the content hash before ("" for a new key).
    pub base_hash: String,
    /// The content hash after ("" for a delete).
    pub after_hash: String,
    /// A version the session adopted, which replaces the content base.
    pub adopted: Option<Base>,
}

impl Planned {
    fn new(key: String, kind: &'static str, base_hash: &str, after_hash: &str) -> Planned {
        Planned { key, kind, public: Map::new(), base_hash: base_hash.to_string(), after_hash: after_hash.to_string(), adopted: None }
    }

    /// The base on the wire. An adopted version carries the content too,
    /// which the server ignores beside a chain hash (§5.1).
    pub fn base(&self) -> Base {
        let content = base_of(&self.base_hash);
        match &self.adopted {
            Some(b) => Base { content: content.content, ..b.clone() },
            None => content,
        }
    }

    fn with(mut self, field: &str, value: &str) -> Planned {
        self.public.insert(field.into(), json!(value));
        self
    }

    /// The op on the wire (no id yet, no payload: the server never needs code).
    pub fn to_json(&self) -> Value {
        let mut op = json!({"key": self.key, "kind": self.kind, "public": self.public, "base": self.base()});
        note_content(&mut op, &self.after_hash);
        op
    }

    pub fn from_json(value: &Value) -> Option<Planned> {
        let named = text(value, "kind");
        let kind: &'static str = KINDS.iter().find(|k| **k == named)?;
        Some(Planned {
            key: text(value, "key"),
            kind,
            public: value.get("public").and_then(Value::as_object).cloned().unwrap_or_default(),
            base_hash: value.pointer("/base/content").and_then(Value::as_str).unwrap_or("").to_string(),
            after_hash: text(value, "content_hash"),
            adopted: value
                .get("base")
                .and_then(|b| serde_json::from_value::<Base>(b.clone()).ok())
                .filter(|b| b.version > 0)
                .map(|b| Base { content: None, ..b }),
        })
    }
}

const KINDS: [&str; 7] = ["modify_body", "signature_change", "rename", "insert", "delete", "move", "file_replace"];

/// The typed ops that turn `before` into `after` for the file at `rel`.
/// A body that changed under the same signature is `modify_body`; a new
/// signature is `signature_change`; a declaration that vanished while one of
/// the same shape appeared is a `rename`; the rest are `delete` and
/// `insert`. A file no parser knows is `file_replace` (or `delete`). A
/// broken parse on either side keys nothing: a half-typed edit is not a
/// change anyone can act on.
pub fn plan(rel: &str, before: &Shape, after: &Shape) -> Vec<Planned> {
    let mut out = Vec::new();
    match (before, after) {
        (Shape::Broken, _) | (_, Shape::Broken) => {}
        (Shape::Absent, Shape::Absent) => {}
        (Shape::File(a), Shape::File(b)) if a != b => out.push(Planned::new(file_key(rel), "file_replace", a, b)),
        (Shape::Absent, Shape::File(b)) => out.push(Planned::new(file_key(rel), "file_replace", "", b)),
        (Shape::File(a), Shape::Absent) => out.push(Planned::new(file_key(rel), "delete", a, "")),
        (Shape::File(_), Shape::File(_)) => {}
        (Shape::Code(_) | Shape::Absent, Shape::Code(_) | Shape::Absent) => {
            let empty = Vec::new();
            let old = if let Shape::Code(s) = before { s } else { &empty };
            let new = if let Shape::Code(s) = after { s } else { &empty };
            let new_names: BTreeSet<&str> = new.iter().map(|s| s.name.as_str()).collect();
            let old_names: BTreeSet<&str> = old.iter().map(|s| s.name.as_str()).collect();
            let mut appeared: Vec<&Sym> = new.iter().filter(|s| !old_names.contains(s.name.as_str())).collect();
            for o in old {
                let key = sym_key(rel, &o.name);
                if let Some(n) = new.iter().find(|n| n.name == o.name) {
                    if n.hash == o.hash {
                        continue;
                    }
                    // a breaking op says what callers were written against
                    out.push(if n.signature != o.signature {
                        Planned::new(key, "signature_change", &o.hash, &n.hash)
                            .with("new_signature", &n.signature)
                            .with("old_signature", &o.signature)
                    } else {
                        Planned::new(key, "modify_body", &o.hash, &n.hash)
                    });
                } else if let Some(at) = appeared.iter().position(|n| n.shape == o.shape) {
                    let n = appeared.remove(at);
                    out.push(
                        Planned::new(key, "rename", &o.hash, &n.hash).with("new_name", &key_name(&n.name)).with("old_signature", &o.signature),
                    );
                } else if !new_names.contains(o.name.as_str()) {
                    out.push(Planned::new(key, "delete", &o.hash, "").with("old_signature", &o.signature));
                }
            }
            for n in appeared {
                out.push(Planned::new(sym_key(rel, &n.name), "insert", "", &n.hash));
            }
        }
        _ => {}
    }
    out.truncate(MAX_OPS);
    out
}

// ------------------------------------------------------------ what a write will do

/// The file's content after this tool call, worked out in memory from the
/// call itself: `Some(None)` when it deletes the file, `None` when it cannot
/// be told (the report still sees what happened).
pub fn predict_after(current: Option<&str>, tool: &str, tool_input: &Value, target: &Path, cwd: &Path) -> Option<Option<String>> {
    match tool {
        "Write" => tool_input.get("content").and_then(Value::as_str).map(|c| Some(c.to_string())),
        "Edit" => {
            let mut content = current?.to_string();
            apply_edit(&mut content, tool_input)?;
            Some(Some(content))
        }
        "MultiEdit" => {
            let mut content = current?.to_string();
            for edit in tool_input.get("edits")?.as_array()? {
                apply_edit(&mut content, edit)?;
            }
            Some(Some(content))
        }
        _ => {
            let patch = text(tool_input, "command");
            if !patch.contains("*** ") {
                return None;
            }
            patch_after(&patch, target, cwd, current)
        }
    }
}

/// What a write does to one file, by name, for the server's gate: the
/// top-level declarations it adds, and the existing ones it changes,
/// renames or removes (`{"adds": [...], "changes": [...]}`). Names only,
/// never text. `None` when it cannot be told: the edit does not apply to the
/// file as it is, or no parser knows the file. With it the gate tells two
/// agents each adding their own tests to one file apart from two editing
/// the same test.
pub fn write_facts(tool: &str, tool_input: &Value, target: &Path, cwd: &Path, rel: &str) -> Option<Value> {
    let current = std::fs::read(target).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
    let after = predict_after(current.as_deref(), tool, tool_input, target, cwd)?;
    let (before, after) = (shape_of(rel, current.as_deref()), shape_of(rel, after.as_deref()));
    if !matches!(before, Shape::Code(_) | Shape::Absent) || !matches!(after, Shape::Code(_) | Shape::Absent) {
        return None;
    }
    let mut adds: BTreeSet<String> = BTreeSet::new();
    let mut changes: BTreeSet<String> = BTreeSet::new();
    for op in plan(rel, &before, &after) {
        let name = key_symbol(&op.key);
        match op.kind {
            "insert" => {
                adds.insert(name);
            }
            "rename" => {
                changes.insert(name);
                if let Some(new) = op.public.get("new_name").and_then(Value::as_str) {
                    changes.insert(new.to_string());
                }
            }
            _ => {
                changes.insert(name);
            }
        }
    }
    Some(json!({"adds": adds, "changes": changes}))
}

fn apply_edit(content: &mut String, edit: &Value) -> Option<()> {
    let old = edit.get("old_string")?.as_str()?;
    let new = edit.get("new_string").and_then(Value::as_str).unwrap_or("");
    if old.is_empty() || !content.contains(old) {
        return None;
    }
    *content = if edit.get("replace_all").and_then(Value::as_bool) == Some(true) {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
    Some(())
}

/// A Codex `apply_patch` applied to the one file at `target`.
fn patch_after(patch: &str, target: &Path, cwd: &Path, current: Option<&str>) -> Option<Option<String>> {
    // (path, [marker, body lines...]) per file the patch names
    let mut sections: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut open = false;
    for line in patch.lines() {
        let header = ["*** Add File:", "*** Update File:", "*** Delete File:"].iter().find(|m| line.trim_start().starts_with(**m));
        if let Some(marker) = header {
            sections.push((line.trim_start()[marker.len()..].trim(), vec![*marker]));
            open = true;
        } else if line.starts_with("*** End Patch") || line.starts_with("*** Begin Patch") {
            open = false;
        } else if open {
            if let Some((_, body)) = sections.last_mut() {
                body.push(line);
            }
        }
    }
    let wanted = config::absolute(target);
    let (_, body) = sections.into_iter().find(|(path, _)| config::absolute(&cwd.join(path)) == wanted)?;
    match *body.first()? {
        "*** Delete File:" => Some(None),
        "*** Add File:" => {
            let lines: Vec<&str> = body[1..].iter().filter_map(|l| l.strip_prefix('+')).collect();
            Some(Some(if lines.is_empty() { String::new() } else { format!("{}\n", lines.join("\n")) }))
        }
        _ => {
            let mut lines: Vec<String> = current?.split('\n').map(str::to_string).collect();
            let mut from = 0;
            let mut hunks: Vec<Vec<&str>> = vec![Vec::new()];
            for line in &body[1..] {
                if line.starts_with("@@") {
                    hunks.push(Vec::new());
                } else if line.starts_with("*** Move to:") || line.starts_with("*** End of File") {
                    continue;
                } else if let Some(h) = hunks.last_mut() {
                    h.push(line);
                }
            }
            for hunk in hunks.into_iter().filter(|h| h.iter().any(|l| l.starts_with('+') || l.starts_with('-'))) {
                let old: Vec<&str> = hunk.iter().filter(|l| !l.starts_with('+')).map(|l| l.get(1..).unwrap_or("")).collect();
                let new: Vec<String> = hunk.iter().filter(|l| !l.starts_with('-')).map(|l| l.get(1..).unwrap_or("").to_string()).collect();
                let at = if old.is_empty() {
                    from
                } else {
                    (from..=lines.len().saturating_sub(old.len())).find(|&i| lines[i..i + old.len()].iter().zip(&old).all(|(a, b)| a == b))?
                };
                lines.splice(at..at + old.len(), new.iter().cloned());
                from = at + new.len();
            }
            Some(Some(lines.join("\n")))
        }
    }
}

// ------------------------------------------------------------ the session's files

fn safe(session: &str) -> String {
    let s: String = session.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    if s.is_empty() {
        return "nosession".to_string();
    }
    if s.len() <= 64 {
        return s;
    }
    // a helper's agent (`session/worker`) runs past the limit: its tail as a
    // hash, so two helpers of one session never share a folder
    let digest = crate::report::sha256_hex(s.as_bytes());
    format!("{}-{}", &s[..47], &digest[..16])
}

pub fn proto_dir(env: &Env) -> PathBuf {
    crate::machine::data_dir(env).join("proto")
}

pub fn session_dir(env: &Env, session: &str) -> PathBuf {
    proto_dir(env).join(safe(session))
}

/// The daemon's socket. A unix socket path is limited to ~104 bytes, so a
/// long home falls back to the temp dir, named by a hash of the long path.
pub fn socket_path(env: &Env, session: &str) -> PathBuf {
    let path = session_dir(env, session).join("d.sock");
    if path.as_os_str().len() < 100 {
        return path;
    }
    use sha2::{Digest, Sha256};
    let tag: String = Sha256::digest(path.to_string_lossy().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    PathBuf::from("/tmp").join(format!("collide-{tag}.sock"))
}

fn snap_path(env: &Env, session: &str, rel: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let tag: String = Sha256::digest(rel.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    session_dir(env, session).join("before").join(format!("{tag}.json"))
}

fn save(path: &Path, value: &Value) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // whole-file replace: a reader never sees half a file
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, value.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

pub fn remember_before(env: &Env, session: &str, rel: &str, shape: &Shape) {
    save(&snap_path(env, session, rel), &json!({"path": rel, "ts": now_s(), "shape": shape.to_json()}));
}

pub fn recall_before(env: &Env, session: &str, rel: &str) -> Option<Shape> {
    let saved = config::load_json(&snap_path(env, session, rel));
    if text(&saved, "path") != rel {
        return None;
    }
    Shape::from_json(saved.get("shape")?)
}

/// This session's accepted ops not yet promoted: `{op_id, key, path}`.
fn drafts_path(env: &Env, session: &str) -> PathBuf {
    session_dir(env, session).join("drafts.json")
}

pub fn drafts(env: &Env, session: &str) -> Vec<Value> {
    config::load_json(&drafts_path(env, session)).get("ops").and_then(Value::as_array).cloned().unwrap_or_default()
}

pub fn add_drafts(env: &Env, session: &str, ops: &[Value]) {
    if ops.is_empty() {
        return;
    }
    let mut all = drafts(env, session);
    all.extend(ops.iter().cloned());
    let keep = all.len().saturating_sub(2000);
    save(&drafts_path(env, session), &json!({"ops": all[keep..]}));
}

pub fn drop_drafts(env: &Env, session: &str, ids: &BTreeSet<String>) {
    let all: Vec<Value> = drafts(env, session).into_iter().filter(|d| !ids.contains(&text(d, "op_id"))).collect();
    save(&drafts_path(env, session), &json!({"ops": all}));
}

/// The path a key names.
pub fn key_path(key: &str) -> String {
    if let Some(path) = key.strip_prefix("file:") {
        return path.to_string();
    }
    cork_protocol_code::split_symbol_key(key).map(|(p, _)| p.to_string()).unwrap_or_default()
}

pub fn key_symbol(key: &str) -> String {
    cork_protocol_code::split_symbol_key(key).map(|(_, s)| s.to_string()).unwrap_or_else(|| key_path(key))
}

// ------------------------------------------------------------ notices

fn inbox_path(env: &Env, session: &str) -> PathBuf {
    session_dir(env, session).join("inbox.jsonl")
}

/// Queue a line for the session's next hook output (the daemon's side).
pub fn spool_line(env: &Env, session: &str, kind: &str, line: &str) {
    use std::io::Write;
    let path = inbox_path(env, session);
    let _ = std::fs::create_dir_all(session_dir(env, session));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", json!({"kind": kind, "line": line, "ts": now_s()}));
    }
}

/// Everything queued for this session, taken (once), as context lines.
pub fn take_notices(env: &Env, session: &str) -> String {
    if session.is_empty() {
        return String::new();
    }
    let path = inbox_path(env, session);
    let taking = path.with_extension(format!("taking{}", std::process::id()));
    if std::fs::rename(&path, &taking).is_err() {
        return String::new();
    }
    let raw = std::fs::read_to_string(&taking).unwrap_or_default();
    let _ = std::fs::remove_file(&taking);
    let mut deltas: Vec<String> = Vec::new();
    let mut others: Vec<String> = Vec::new();
    for row in raw.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        let line = text(&row, "line");
        let bucket = if text(&row, "kind") == "delta" { &mut deltas } else { &mut others };
        if !line.is_empty() && !bucket.contains(&line) {
            bucket.push(line);
        }
    }
    let mut out: Vec<String> = others.into_iter().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    if !deltas.is_empty() {
        let shown: Vec<String> = deltas.iter().rev().take(6).rev().cloned().collect();
        out.push(format!(
            "Cork (live): teammates changed code you use or edit, in their own checkouts; landing merges both when you \
push. Write against the NEW names/signatures, and say so to the user:\n{}",
            shown.join("\n")
        ));
    }
    out.join("\n")
}

/// Who, as a person reads it: `alice@x.com (codex)`.
pub fn who(peer: &Value) -> String {
    let person = text(peer, "person");
    let mut agent = text(peer, "agent_id");
    // Cork's server names agents `<email>#<session>`: the person says it
    if agent.contains('#') {
        agent.clear();
    }
    match (person.is_empty(), agent.is_empty()) {
        (false, false) => format!("{person} ({agent})"),
        (false, true) => person,
        (true, false) => agent,
        (true, true) => "a teammate".to_string(),
    }
}

/// What an op did, in words: "renamed compute_tax -> tax_of".
pub fn change_words(op: &Value) -> String {
    let symbol = key_symbol(&text(op, "key"));
    let public = op.get("public").cloned().unwrap_or(Value::Null);
    match text(op, "kind").as_str() {
        "rename" => format!("renamed {symbol} -> {}", text(&public, "new_name")),
        "signature_change" => format!("changed the signature of {symbol} to `{}`", text(&public, "new_signature")),
        "delete" => format!("deleted {symbol}"),
        "move" => format!("moved {symbol} to {}", text(&public, "new_path")),
        "insert" => format!("added {symbol}"),
        "file_replace" => format!("rewrote {symbol}"),
        _ => format!("edited {symbol}"),
    }
}

fn intent_words(intent: Option<&Value>, fallback_id: &str) -> String {
    let said = intent.map(|i| text(i, "text")).filter(|t| !t.is_empty());
    let id = intent.map(|i| text(i, "id")).filter(|t| !t.is_empty()).unwrap_or_else(|| fallback_id.to_string());
    match (said, id.is_empty()) {
        (Some(t), _) => format!(" (their intent: {t})"),
        (None, false) => format!(" (their intent: {id})"),
        _ => String::new(),
    }
}

/// The agent session behind a protocol session: a hook's side sessions (the
/// commit ingest `<session>.ingest`, the fallback writer `<session>.d<pid>`)
/// belong to the session that opened them. Mirrors the server's
/// `protocol_ws::agent_session`.
pub fn agent_session(session: &str) -> &str {
    if let Some(base) = session.strip_suffix(".ingest") {
        return base;
    }
    if let Some((base, pid)) = session.rsplit_once(".d") {
        if !base.is_empty() && !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()) {
            return base;
        }
    }
    session
}

/// The agent a hook speaks for: the session (its lead), or `session/worker`
/// for one of its helpers (the hook input's `agent_id`). Mirrors the
/// server's `presence::agent_session`. A helper is its own agent: what it
/// was told, how far it has read the deltas, whose lines are whose. The
/// protocol's session stays the session's: its daemon, leases and drafts
/// are one per session.
pub fn agent_of(session: &str, worker: &str) -> String {
    if session.is_empty() || worker.is_empty() { session.to_string() } else { format!("{session}/{worker}") }
}

/// A delta of this session's own making: written through one of its side
/// sessions, or by a helper agent it started (whose hooks carry its id). The
/// session already knows; saying it back read as a teammate's change.
pub fn own_delta(delta: &Value, session: &str) -> bool {
    let author = delta.pointer("/op/author/session").and_then(Value::as_str).unwrap_or("");
    !session.is_empty() && !author.is_empty() && agent_session(author) == agent_session(session)
}

/// A teammate's delta as one Δ line, or None when it changes nothing this
/// session must adapt to. `editing` holds the keys this session leases or
/// has drafts on.
pub fn delta_line(delta: &Value, editing: &BTreeSet<String>) -> Option<(String, bool)> {
    if text(delta, "layer") == "promoted" {
        return None; // the same op again, now proven: said already
    }
    let op = delta.get("op")?;
    let key = text(op, "key");
    let kind = text(op, "kind");
    let mine = editing.contains(&key);
    let breaking = matches!(kind.as_str(), "rename" | "signature_change" | "delete" | "move");
    let via = delta.get("via").and_then(Value::as_array).and_then(|v| v.first()).and_then(Value::as_str).map(key_symbol);
    // a change to a function is news to the code that calls it, not to every
    // session that read the file it lives in (Study 8, second study: one
    // rename reached most of sixteen agents). Callers this session's files
    // import are told with their lines (protod `tell_callers`); here, only
    // what it is editing, or what its code depends on (`via`).
    if !mine && !(breaking && via.is_some()) {
        return None;
    }
    // a teammate's test is news only to a session editing that same test
    if !mine && crate::git::is_test_path(&key_path(&key)) {
        return None;
    }
    let tail = match (&via, mine) {
        (_, true) => ", which you are editing".to_string(),
        (Some(v), false) => format!(" (your {v} depends on it)"),
        _ => String::new(),
    };
    let line = format!("Δ {} {}{tail} in {}", who(op.get("author").unwrap_or(&Value::Null)), change_words(op), key_path(&key));
    // pushed at once only when it lands on what this session is editing
    Some((line, mine && (breaking || kind == "modify_body")))
}

/// Whether any of `theirs` (others' ops, as the server lists them) is
/// promoted: proven and pushed by its author. Drafts are only in their
/// author's checkout, and no pull can bring them in.
fn any_promoted(theirs: &[Value]) -> bool {
    theirs.iter().any(|t| text(t, "layer") == "promoted")
}

/// What to write against after `theirs`: the newest name or signature.
fn write_against(theirs: &[Value]) -> String {
    let mut target = String::new();
    for op in theirs {
        let public = op.get("public").cloned().unwrap_or(Value::Null);
        match text(op, "kind").as_str() {
            "rename" => target = text(&public, "new_name"),
            "signature_change" => target = text(&public, "new_signature"),
            "move" => target = text(&public, "new_path"),
            _ => {}
        }
    }
    target
}

/// A teammate's change this copy lacks and no pull can bring in (not
/// pushed yet, or not on the origin this checkout pulls from): what to
/// write against, never "pull".
fn not_pushed_line(theirs: &[Value], first: &Value, intent: &str, path: &str, landed: bool) -> String {
    let changes: Vec<String> = theirs.iter().take(3).map(change_words).collect();
    let who = who(first.get("author").unwrap_or(&Value::Null));
    let state = if landed {
        "landed it elsewhere: origin (as this checkout last fetched it) does not have it, so there is nothing to bring in"
    } else {
        "not pushed yet, so your copy does not have it"
    };
    let target = write_against(theirs);
    let what_now = if target.is_empty() {
        "keep your edit to your own part; landing merges both".to_string()
    } else {
        format!("write against `{target}`; landing will rebase onto it")
    };
    format!("Cork: {who} {}{intent} in {path} ({state}): {what_now}.", changes.join(", "))
}

/// A reject as a notice: who got there first, what they did, and what to do.
/// A conflict with work that is only in its author's checkout says nothing:
/// the gate already said it, and there is nothing to re-read.
pub fn reject_notice(reject: &Value) -> Option<String> {
    let reason = text(reject, "reason");
    let key = text(reject, "key");
    let path = key_path(&key);
    let symbol = key_symbol(&key);
    match reason.as_str() {
        "conflict" | "unknown_base" => {
            let theirs = reject.get("theirs").and_then(Value::as_array).cloned().unwrap_or_default();
            let first = theirs.first()?;
            if !any_promoted(&theirs) {
                return None;
            }
            let changes: Vec<String> = theirs.iter().take(3).map(change_words).collect();
            Some(format!(
                "Cork: {} {} first{}, and your copy of {path} did not have it when you wrote {symbol}. Your edit is on \
disk; Cork could not record it on top of theirs. Re-read {path}, keep both their change and yours, write it again, and \
tell the user.",
                who(reject.get("holder").or_else(|| first.get("author")).unwrap_or(&Value::Null)),
                changes.join(", "),
                intent_words(reject.get("their_intent"), &text(first, "intent_id")),
            ))
        }
        // their lease refused the record of the change, never the file: the
        // write is on disk, the gate already said who holds the code
        // (lines.rs), and the session's daemon records it once they let go
        // (protod.rs). "The server did not take your change; coordinate
        // before writing it again" read as the edit having failed (v104)
        _ => None,
    }
}

/// The gate's answer to check results, without looking at origin: a block
/// reason only when a result names someone working on it now (their lease)
/// or a pushed op of theirs this copy lacks. See `verdict`.
pub fn gate_verdict(results: &[Value]) -> Option<String> {
    verdict(results, None, false).block
}

/// What the gate says about check results.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Verdict {
    /// Stop the write (said once per fact by the caller).
    pub block: Option<String>,
    /// The fact the block states, for the caller's told-once.
    pub block_fact: String,
    /// The key the block is about (`code:tax.py::compute_tax`), and the
    /// agent session whose work it is: what the fix lines and the branch
    /// check need.
    pub block_key: String,
    pub block_by: String,
    /// Who that is, as a person reads it.
    pub block_who: String,
    /// Lines for the agent's context, each with its fact: changes it lacks
    /// that no pull can bring in.
    pub notices: Vec<(String, String)>,
}

/// The gate's answer to check results. Blocks only on what is certain and
/// what the agent can clear:
///
/// - another session holds a lease on the key (it is editing it now);
/// - a pushed (promoted) op of theirs this copy lacks, which origin has: a
///   pull and a re-read bring it in. With `root`, origin is looked at
///   (origin.rs); a promotion that never reached this checkout's origin is
///   the phantom block of Study 8, said as a line instead.
///
/// A copy behind a teammate's drafts (their work, only in their checkout) is
/// a line saying what to write against, never "pull". Anything else allows:
/// a stale base that rekeys or rebases, content the server never saw, an
/// invalid base, a timeout.
pub fn verdict(results: &[Value], root: Option<&Path>, lands: bool) -> Verdict {
    let mut out = Verdict::default();
    for r in results {
        let key = text(r, "key");
        let symbol = key_symbol(&key);
        match text(r, "status").as_str() {
            "leased" => {
                let Some(holder) = r.get("holder") else { continue };
                out.block = Some(format!(
                    "{} is editing {symbol} right now{}, not pushed yet. Keep your edit to your own lines of it, then retry \
the write and it goes through; landing merges both. If you both need the same lines, message them \
(`cork message --to {} \"...\"`) and agree who changes what.",
                    who(holder),
                    intent_words(r.get("intent"), ""),
                    if text(holder, "person").is_empty() { text(holder, "session") } else { text(holder, "person") },
                ));
                out.block_fact = format!("lease|{key}|{}", text(holder, "session"));
                out.block_key = key.clone();
                out.block_by = agent_session(&text(holder, "session")).to_string();
                out.block_who = who(holder);
                return out;
            }
            // a copy the server never saw while others have unpromoted work
            // on the key is a copy missing that work; with none, it is a
            // human's edit, and the write goes in over it
            "stale" | "unknown_base" if text(r, "would") == "conflict" || text(r, "status") == "unknown_base" => {
                let theirs = r.get("theirs").and_then(Value::as_array).cloned().unwrap_or_default();
                let Some(first) = theirs.first() else { continue };
                let intent = intent_words(r.get("intent"), &text(first, "intent_id"));
                let path = key_path(&key);
                let fact = format!("stale|{key}|{}", text(theirs.last().unwrap_or(first), "op_id"));
                if !any_promoted(&theirs) {
                    out.notices.push((fact, not_pushed_line(&theirs, first, &intent, &path, false)));
                    continue;
                }
                // pushed, by the protocol's record: is it on this checkout's origin?
                let promoted: Vec<&Value> = theirs.iter().filter(|t| text(t, "layer") == "promoted").collect();
                let on_origin = root.map(|root| {
                    promoted.iter().map(|op| crate::origin::Change::from_op(op).on_origin(root, &path)).collect::<Vec<_>>()
                });
                if on_origin.as_ref().is_some_and(|all| !all.is_empty() && all.iter().all(|o| *o == Some(false))) {
                    out.notices.push((fact, not_pushed_line(&theirs, first, &intent, &path, true)));
                    continue;
                }
                let changes: Vec<String> = theirs.iter().take(3).map(change_words).collect();
                let target = write_against(&theirs);
                out.block = Some(if lands {
                    let to = if target.is_empty() { "their change".to_string() } else { format!("`{target}`") };
                    format!(
                        "{} already {}{}, and pushed it; your copy of {} is older than that. Write against {to}: landing will \
rebase onto it when you push. Retry the write and it goes through.",
                        who(first.get("author").unwrap_or(&Value::Null)),
                        changes.join(", "),
                        intent,
                        path,
                    )
                } else {
                    format!(
                        "{} already {}{}, and pushed it; your copy of {} is older than that. Pull it (`git pull --rebase`, or re-read \
the file if you share a checkout), adapt to their change, then write again.",
                        who(first.get("author").unwrap_or(&Value::Null)),
                        changes.join(", "),
                        intent,
                        path,
                    )
                });
                out.block_fact = fact;
                out.block_key = key.clone();
                let by = promoted.last().and_then(|op| op.pointer("/author/session")).and_then(Value::as_str).unwrap_or("");
                out.block_by = agent_session(by).to_string();
                out.block_who = who(first.get("author").unwrap_or(&Value::Null));
                return out;
            }
            _ => {}
        }
    }
    out
}

// ------------------------------------------------------------ talking to the daemon

/// One request to the session's daemon, one answer. None when there is no
/// daemon or it does not answer in time.
#[cfg(unix)]
pub fn ask(env: &Env, session: &str, request: &Value, timeout: Duration) -> Option<Value> {
    use std::io::{BufRead, BufReader, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket_path(env, session)).ok()?;
    let _ = stream.set_write_timeout(Some(timeout));
    let _ = stream.set_read_timeout(Some(timeout.max(Duration::from_millis(1))));
    stream.write_all(format!("{request}\n").as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim()).ok()
}

#[cfg(not(unix))]
pub fn ask(_: &Env, _: &str, _: &Value, _: Duration) -> Option<Value> {
    None
}

/// The daemon is there to ask (its socket answers a connect).
#[cfg(unix)]
pub fn daemon_up(env: &Env, session: &str) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path(env, session)).is_ok()
}

#[cfg(not(unix))]
pub fn daemon_up(_: &Env, _: &str) -> bool {
    false
}

fn down_path(env: &Env) -> PathBuf {
    proto_dir(env).join("down.json")
}

/// A short-lived connection of the hook's own, for when there is no daemon.
/// Runs `work` on a thread and gives up at `budget`: a TCP connect to an
/// unreachable host may hang for a minute, and the hook must not. A failure
/// marks the server down for a minute, so the next writes do not pay again.
pub fn direct<T: Send + 'static>(
    url: &str, client: ClientConfig, env: &Env, budget: Duration,
    work: impl FnOnce(&mut BlockingClient, Duration) -> Option<T> + Send + 'static,
) -> Option<T> {
    if is_down(env, url) {
        return None;
    }
    let started = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    let url_owned = url.to_string();
    std::thread::spawn(move || {
        let answer = match BlockingClient::connect(&url_owned, client, false, budget) {
            Ok((mut c, _)) => {
                let left = budget.saturating_sub(started.elapsed());
                let out = work(&mut c, left);
                c.close();
                Ok(out)
            }
            Err(_) => Err(()),
        };
        let _ = tx.send(answer);
    });
    match rx.recv_timeout(budget) {
        Ok(Ok(out)) => out,
        _ => {
            mark_down(env, url, DOWN_FOR_S);
            None
        }
    }
}

/// Hooks stop trying `url` for `seconds`.
pub fn mark_down(env: &Env, url: &str, seconds: f64) {
    let mut down = config::load_json(&down_path(env));
    if let Some(map) = down.as_object_mut() {
        map.insert(url.to_string(), json!(now_s() + seconds));
    }
    save(&down_path(env), &down);
}

pub fn is_down(env: &Env, url: &str) -> bool {
    config::load_json(&down_path(env)).get(url).and_then(Value::as_f64).is_some_and(|until| until > now_s())
}

/// A connection's own session id: never the daemon's, whose hello would
/// supersede it and drop its leases.
pub fn direct_session(session: &str) -> String {
    format!("{session}.d{}", std::process::id())
}

/// Read frames until `pick` takes one; other frames are handed to `other`.
pub fn wait_for<T>(
    c: &mut BlockingClient, timeout: Duration, mut pick: impl FnMut(&ServerFrame) -> Option<T>,
    mut other: impl FnMut(ServerFrame),
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        match c.recv(left) {
            Ok(Some(f)) => match pick(&f) {
                Some(t) => return Some(t),
                None => other(f),
            },
            Ok(None) => {}
            Err(cork_protocol::ws_client::WsError::Decode(_)) => {}
            Err(_) => return None,
        }
    }
}

/// `check` the planned keys over `c`: the results as JSON.
pub fn check_over(c: &mut BlockingClient, planned: &[Planned], timeout: Duration, other: impl FnMut(ServerFrame)) -> Option<Vec<Value>> {
    let keys: Vec<Value> = planned.iter().map(|p| json!({"key": p.key, "base": p.base(), "kind": p.kind})).collect();
    check_keys(c, &keys, timeout, other)
}

/// `check` any keys (`{key, base, kind?}`; no kind is a reader's check), in
/// one frame: the results in the same order.
pub fn check_keys(c: &mut BlockingClient, keys: &[Value], timeout: Duration, other: impl FnMut(ServerFrame)) -> Option<Vec<Value>> {
    if keys.is_empty() {
        return Some(Vec::new());
    }
    let req = format!("g{}", c.now_ms());
    let frame = json!({"t": "check", "req": req, "keys": keys});
    let frame = cork_protocol::ClientFrame::decode(&frame.to_string()).ok()?;
    c.send(&frame).ok()?;
    wait_for(
        c,
        timeout,
        |f| match f {
            ServerFrame::CheckResult(r) if r.req.as_deref() == Some(req.as_str()) => {
                Some(r.results.iter().filter_map(|e| serde_json::to_value(e).ok()).collect())
            }
            ServerFrame::Error(e) if e.req.as_deref() == Some(req.as_str()) => Some(Vec::new()),
            _ => None,
        },
        other,
    )
}

/// Send `ops` (JSON with an `op_id` each) as one non-atomic batch and
/// collect an answer per op: `{"acks": [...], "rejects": [...]}`.
pub fn send_ops(c: &mut BlockingClient, ops: &[Value], timeout: Duration, mut other: impl FnMut(ServerFrame)) -> Option<Value> {
    let (mut acks, rejects, left) = send_ops_once(c, ops, timeout, &mut other)?;
    // a copy the server never saw, with nobody else's work on the key: a
    // human's edit, or a state never reported. The write goes in over it.
    let (over, mut rejects): (Vec<Value>, Vec<Value>) = rejects.into_iter().partition(|r| {
        text(r, "reason") == "unknown_base" && r.get("theirs").and_then(Value::as_array).is_none_or(|t| t.is_empty())
    });
    let again: Vec<Value> = over
        .iter()
        .filter_map(|r| {
            let mut op = ops.iter().find(|o| text(o, "op_id") == text(r, "op_id"))?.clone();
            op["op_id"] = json!(format!("{}.r", text(&op, "op_id")));
            op["base"] = json!({"version": r.pointer("/current/version").cloned().unwrap_or(json!(0)),
                                "hash": r.pointer("/current/hash").cloned().unwrap_or(json!(""))});
            Some(op)
        })
        .collect();
    if !again.is_empty() {
        match send_ops_once(c, &again, timeout, &mut other) {
            Some((a, r, _)) => {
                acks.extend(a);
                rejects.extend(r);
            }
            None => rejects.extend(over),
        }
    }
    Some(json!({"acks": acks, "rejects": rejects, "unanswered": left}))
}

type Answers = (Vec<Value>, Vec<Value>, BTreeSet<String>);

fn send_ops_once(c: &mut BlockingClient, ops: &[Value], timeout: Duration, other: &mut impl FnMut(ServerFrame)) -> Option<Answers> {
    let now = c.now_ms();
    let ops: Vec<Value> = ops
        .iter()
        .map(|o| {
            let mut o = o.clone();
            o["hlc"] = json!({"w": now, "c": 0});
            o
        })
        .collect();
    let batch = json!({"t": "batch", "batch_id": format!("{}.b", ops.first().map(|o| text(o, "op_id")).unwrap_or_default()),
                       "atomic": false, "ops": ops});
    let frame = cork_protocol::ClientFrame::decode(&batch.to_string()).ok()?;
    c.send(&frame).ok()?;
    let mut acks: Vec<Value> = Vec::new();
    let mut rejects: Vec<Value> = Vec::new();
    let mut left: BTreeSet<String> = ops.iter().map(|o| text(o, "op_id")).collect();
    while !left.is_empty() {
        let got = wait_for(
            c,
            timeout,
            |f| match f {
                ServerFrame::Ack(a) if left.contains(&a.op_id) => Some((a.op_id.clone(), true, serde_json::to_value(a).ok()?)),
                ServerFrame::Reject(r) if left.contains(&r.op_id) => Some((r.op_id.clone(), false, serde_json::to_value(r).ok()?)),
                ServerFrame::Error(e) if e.req.as_deref().is_some_and(|r| left.contains(r)) => {
                    Some((e.req.clone().unwrap_or_default(), false, json!({"reason": "error", "message": e.message})))
                }
                _ => None,
            },
            &mut *other,
        );
        let Some((id, ok, frame)) = got else { break };
        left.remove(&id);
        if ok {
            acks.push(frame)
        } else {
            rejects.push(frame)
        }
    }
    Some((acks, rejects, left))
}

/// Ops ready for the wire: an id each, from `prefix` and a counter.
/// Unique per write: the server answers a known id with its first ack, so
/// two writes must never share one (the clock's microseconds and a counter;
/// the prefix carries the session and the process).
pub fn with_ids(planned: &[Planned], prefix: &str) -> Vec<Value> {
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = format!("{}{n}", (now_s() * 1e6) as u64 % 1_000_000_000_000);
    planned
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut op = p.to_json();
            op["op_id"] = json!(format!("{}.{stamp}.{i}", op_prefix(prefix)));
            op
        })
        .collect()
}

fn op_prefix(prefix: &str) -> String {
    prefix.chars().map(|c| if c.is_ascii_alphanumeric() || "._:@-".contains(c) { c } else { '_' }).take(80).collect()
}

// ------------------------------------------------------------ the gate

/// What the gate needs to know about the session.
pub struct Ctx<'a> {
    pub root: &'a Path,
    pub cfg: &'a config::Config,
    pub env: &'a Env,
    pub session: &'a str,
}

/// What the gate says: a reason to stop the write, and lines for the
/// agent's context that let it through (a teammate changed code it calls).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Gated {
    pub block: Option<String>,
    pub notice: String,
    /// The agent session whose work the block is about ("" unknown): a
    /// session on another branch never holds a write (branches.rs).
    pub block_by: String,
    pub block_who: String,
    /// The key a block is about (`code:credits.py::balance_for`), and
    /// whether it is a lease: a lease on code a teammate is creating is no
    /// hold (gate.rs `being_created`).
    pub block_key: String,
    pub lease: bool,
}

/// One dependency check: the callee's key and this copy's content of it.
fn dep_key(d: &crate::deps::Dep) -> Value {
    json!({"key": d.key, "base": base_of(&d.base_hash)})
}

/// Who calls a dependency, for the daemon: `{key: [{path, called, signature}]}`.
fn callers_of(rel: &str, deps: &[crate::deps::Dep], into: &mut Map<String, Value>) {
    for d in deps {
        let row = into.entry(d.key.clone()).or_insert_with(|| json!([]));
        if let Some(rows) = row.as_array_mut() {
            rows.push(json!({"path": rel, "called": d.called, "signature": d.signature}));
        }
    }
}

/// The session's stated intent (its title), for the ops and leases it makes.
fn intent_path(env: &Env, session: &str) -> PathBuf {
    session_dir(env, session).join("intent.txt")
}

pub fn note_intent(env: &Env, session: &str, intent: &str) {
    let intent: String = intent.trim().chars().take(140).collect();
    if session.is_empty() || intent.is_empty() || session_intent(env, session) == intent {
        return;
    }
    let _ = std::fs::create_dir_all(session_dir(env, session));
    let _ = std::fs::write(intent_path(env, session), intent);
}

/// The workspace keeps titles off its servers: the session states no intent.
pub fn forget_intent(env: &Env, session: &str) {
    if !session.is_empty() {
        let _ = std::fs::remove_file(intent_path(env, session));
    }
}

pub fn session_intent(env: &Env, session: &str) -> String {
    if session.is_empty() {
        return String::new();
    }
    std::fs::read_to_string(intent_path(env, session)).unwrap_or_default().trim().to_string()
}

/// The gate's half (PreToolUse): remember each target as it is, work out
/// the ops the write makes and what the file will call, and `check` both:
/// the write's own keys as a writer, the callees as a reader (SPEC.md §9).
/// A conflict on its own keys blocks, as does a call the write adds to a
/// name a teammate removed or renamed (once); a teammate's change to a
/// function it calls is a notice. Runs to `deadline` through the daemon, or
/// within DIRECT_BUDGET on a connection of its own; anything else allows.
pub fn gate(ctx: &Ctx, tool: &str, tool_input: &Value, cwd: &Path, targets: &[PathBuf], started: Instant, deadline: Duration) -> Gated {
    if !matches!(tool, "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "apply_patch") && !text(tool_input, "command").contains("*** ") {
        return Gated::default();
    }
    let mut planned: Vec<Planned> = Vec::new();
    // per target: (rel, the write's own lines, what it calls)
    let mut calling: Vec<(String, BTreeSet<usize>, Vec<crate::deps::Dep>)> = Vec::new();
    // per target: the file as the write leaves it, and its own lines: what a
    // block's fix lines are read from (fixlines.rs)
    let mut afters: BTreeMap<String, (String, BTreeSet<usize>)> = BTreeMap::new();
    for target in targets {
        let Some(rel) = config::rel_path(target, ctx.root) else { continue };
        if crate::report::is_collide_artifact(&rel) || std::fs::metadata(target).is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let current = std::fs::read(target).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
        let before = shape_of(&rel, current.as_deref());
        remember_before(ctx.env, ctx.session, &rel, &before);
        if let Some(after) = predict_after(current.as_deref(), tool, tool_input, target, cwd) {
            planned.extend(plan(&rel, &before, &shape_of(&rel, after.as_deref())));
            if let Some(after) = after.as_deref() {
                let new_lines = crate::deps::new_lines(current.as_deref(), after);
                let deps = crate::deps::deps_of(ctx.root, &rel, after);
                if !deps.is_empty() {
                    calling.push((rel.clone(), new_lines.clone(), deps));
                }
                afters.insert(rel.clone(), (after.to_string(), new_lines));
            }
        }
    }
    if planned.is_empty() && calling.is_empty() {
        return Gated::default();
    }
    let planned = with_adopted(ctx.env, ctx.session, planned);
    let keys: Vec<Value> = planned.iter().map(Planned::to_json).collect();
    let mut deps: Vec<Value> = Vec::new();
    let mut callers: Map<String, Value> = Map::new();
    for (rel, _, ds) in &calling {
        for d in ds {
            if !deps.iter().any(|k| text(k, "key") == d.key) {
                deps.push(dep_key(d));
            }
        }
        callers_of(rel, ds, &mut callers);
    }
    let intent = session_intent(ctx.env, ctx.session);
    let left = deadline.saturating_sub(started.elapsed());
    let request = json!({"cmd": "gate", "ops": keys, "deps": deps, "callers": callers, "intent": intent});
    let (results, dep_results): (Vec<Value>, Vec<Value>) = match ask(ctx.env, ctx.session, &request, left) {
        Some(answer) => (
            answer.get("results").and_then(Value::as_array).cloned().unwrap_or_default(),
            answer.get("deps").and_then(Value::as_array).cloned().unwrap_or_default(),
        ),
        None if daemon_up(ctx.env, ctx.session) => return Gated::default(), // up but slow: allow
        None => {
            let url = ws_url(&ctx.cfg.server, ctx.env);
            if is_down(ctx.env, &url) {
                return Gated::default();
            }
            spawn_daemon(ctx.root, ctx.session, ctx.env);
            let client = client_config(ctx.root, ctx.cfg, &direct_session(ctx.session), crate::harness::current());
            let n = planned.len();
            let mut all: Vec<Value> = planned.iter().map(|p| json!({"key": p.key, "base": p.base(), "kind": p.kind})).collect();
            all.extend(deps);
            let Some(mut got) = direct(&url, client, ctx.env, DIRECT_BUDGET, move |c, left| check_keys(c, &all, left, |_| {})) else {
                return Gated::default();
            };
            let dep_results = got.split_off(n.min(got.len()));
            (got, dep_results)
        }
    };
    let results = without_commits_here(beyond_this_folder(results, ctx.root, ctx.session, ctx.env), ctx.root);
    let dep_results = without_commits_here(dep_results, ctx.root);
    let said = verdict(&results, Some(ctx.root), crate::land::lands(ctx.root, ctx.env));
    let mut told = crate::deps::Told::load(ctx.env, ctx.session);
    if said.block.is_some() || !said.notices.is_empty() {
        // the next write builds on the version the server named: the agent
        // was told to bring it in, or to write against it
        for r in &results {
            let named = r.get("theirs").and_then(Value::as_array).is_some_and(|t| !t.is_empty());
            if named && matches!(text(r, "status").as_str(), "stale" | "unknown_base") {
                adopt(ctx.env, ctx.session, &text(r, "key"), r.get("current").unwrap_or(&Value::Null));
            }
        }
    }
    // a block is said once per fact: the retry goes through, so no agent is
    // ever stuck on it (Study 8's phantom block held one agent four times)
    if let Some(reason) = said.block {
        if !told.blocked(&said.block_fact) {
            told.note(&said.block_fact, "", true);
            told.save();
            // the lines of this write their change makes wrong, and what each becomes
            let fix = results
                .iter()
                .find(|r| text(r, "key") == said.block_key)
                .and_then(|r| crate::deps::summarize(r.get("theirs").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]), who))
                .and_then(|change| crate::fixlines::Fix::from_change(&key_symbol(&said.block_key), &change))
                .zip(afters.get(&key_path(&said.block_key)))
                .map(|(fix, (after, new_lines))| crate::fixlines::render(&key_path(&said.block_key), after, new_lines, &[fix]))
                .unwrap_or_default();
            let reason = if fix.is_empty() { reason } else { format!("{reason}\n{fix}") };
            let lease = said.block_fact.starts_with("lease|");
            return Gated {
                block: Some(reason), notice: String::new(), block_by: said.block_by, block_who: said.block_who,
                block_key: said.block_key, lease,
            };
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for (fact, line) in said.notices {
        if !told.knows(&fact) {
            told.note(&fact, "", false);
            lines.push(line);
        }
    }
    let (findings, sessions): (Vec<crate::deps::Finding>, Vec<String>) = dep_findings(&calling, &dep_results, ctx.session).into_iter().unzip();
    if findings.is_empty() && lines.is_empty() {
        return Gated::default();
    }
    // whose work a block would be about: the first finding `speak` stops on
    let block_by = findings
        .iter()
        .zip(&sessions)
        .find(|(f, _)| f.block && !told.blocked(&f.fact))
        .map(|(_, s)| s.clone())
        .unwrap_or_default();
    let (block, notice) = crate::deps::speak(findings, &mut told);
    told.save();
    if !notice.is_empty() {
        lines.push(notice);
    }
    Gated { block, notice: lines.join("\n"), block_by, block_who: String::new(), block_key: String::new(), lease: false }
}

/// Check results without the changes made in this same working tree: by
/// this session or a helper of it (they share its id), or by another
/// session whose gate recorded this folder (sharedtree.rs). What they wrote
/// is on this disk already; a file without it lost it here (a person
/// deleted or rewrote it), so "not pushed yet, your copy does not have it"
/// was false, and so was "pull" (v99: a lead rebuilding an app its person
/// had deleted was told, on every file it and its helper wrote, of the
/// deleted build's drafts). Their live edits still reach this session as
/// Δ lines; a lease on what it is editing still holds.
fn beyond_this_folder(mut results: Vec<Value>, root: &Path, session: &str, env: &Env) -> Vec<Value> {
    let here = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut known: BTreeMap<String, bool> = BTreeMap::new();
    let mut in_this_folder = |author: &str| -> bool {
        let author = agent_session(author);
        if author.is_empty() {
            return false;
        }
        if author == agent_session(session) {
            return true;
        }
        *known.entry(author.to_string()).or_insert_with(|| {
            let state = config::load_json(&crate::check::state_path(author, env));
            state.get("root").and_then(Value::as_str).is_some_and(|theirs| {
                let theirs = Path::new(theirs);
                theirs == root || std::fs::canonicalize(theirs).is_ok_and(|t| t == here)
            })
        })
    };
    for r in &mut results {
        let Some(theirs) = r.get_mut("theirs").and_then(Value::as_array_mut) else { continue };
        theirs.retain(|t| !in_this_folder(t.pointer("/author/session").and_then(Value::as_str).unwrap_or("")));
    }
    results
}

/// Check results without commits this checkout already has. A commit that
/// reached the upstream without Cork is ingested as ops named
/// `git.<sha>.<n>` (`ingest_command`), which servers before landed history
/// (protocol SPEC §10.1) never promoted. So a
/// later write whose base the server never saw (a squash merge, a commit
/// it did not ingest) was told "kevin edited OnboardingFlow (not pushed
/// yet, so your copy does not have it)" about a three-week-old commit that
/// was already in the writer's history.
fn without_commits_here(mut results: Vec<Value>, root: &Path) -> Vec<Value> {
    let mut known: BTreeMap<String, bool> = BTreeMap::new();
    for r in &mut results {
        let Some(theirs) = r.get_mut("theirs").and_then(Value::as_array_mut) else { continue };
        theirs.retain(|t| {
            let id = text(t, "op_id");
            let Some(sha) = ingested_commit(&id) else { return true };
            !*known.entry(sha.to_string()).or_insert_with(|| {
                crate::git::git(root, &["merge-base", "--is-ancestor", sha, "HEAD"], Duration::from_millis(500)).is_some()
            })
        });
    }
    results
}

/// The commit an ingested op came from (`git.<sha>.<n>`, or `.r` re-sent).
fn ingested_commit(op_id: &str) -> Option<&str> {
    let sha = op_id.strip_prefix("git.")?.split('.').next()?;
    (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())).then_some(sha)
}

/// Ops by others: not this session's, nor one of its own short-lived connections'.
fn by_others<'a>(theirs: impl IntoIterator<Item = &'a Value>, session: &str) -> Vec<Value> {
    theirs
        .into_iter()
        .filter(|t| t.pointer("/author/session").and_then(Value::as_str).is_none_or(|s| s != session && !s.starts_with(&format!("{session}."))))
        .cloned()
        .collect()
}

/// The findings a reader's check answers make for the files being written,
/// each with the agent session whose change it is.
fn dep_findings(calling: &[(String, BTreeSet<usize>, Vec<crate::deps::Dep>)], dep_results: &[Value], session: &str) -> Vec<(crate::deps::Finding, String)> {
    let mut out = Vec::new();
    for r in dep_results {
        let key = text(r, "key");
        let theirs = by_others(r.get("theirs").and_then(Value::as_array).into_iter().flatten(), session);
        let Some(change) = crate::deps::summarize(&theirs, who) else { continue };
        let by = theirs.last().and_then(|op| op.pointer("/author/session")).and_then(Value::as_str).map(agent_session).unwrap_or("").to_string();
        for (rel, new_lines, deps) in calling {
            for d in deps.iter().filter(|d| d.key == key) {
                if let Some(f) = crate::deps::judge(d, &change, rel, new_lines) {
                    out.push((f, by.clone()));
                }
            }
        }
    }
    out
}

/// Start the session's daemon if it is not running (it is `cork listen`).
pub fn spawn_daemon(root: &Path, session: &str, env: &Env) {
    if !session.is_empty() && !daemon_up(env, session) {
        crate::listen::spawn_for_session(root, session, env);
    }
}

// ------------------------------------------------------------ the report

/// The report's half (PostToolUse): diff each target against what the gate
/// remembered, send the ops, and say what the server refused. The empty
/// string when there is nothing to say.
pub fn report(ctx: &Ctx, targets: &[PathBuf]) -> String {
    let mut planned: Vec<Planned> = Vec::new();
    for target in targets {
        let Some(rel) = config::rel_path(target, ctx.root) else { continue };
        let Some(before) = recall_before(ctx.env, ctx.session, &rel) else { continue };
        if std::fs::metadata(target).is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let now = std::fs::read(target).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
        let after = shape_of(&rel, now.as_deref());
        let ops = plan(&rel, &before, &after);
        // the next write in this session starts from here
        remember_before(ctx.env, ctx.session, &rel, &after);
        // a scratch file or build output is nobody's shared code
        if !ops.is_empty() && !crate::git::is_ignored(ctx.root, &rel) {
            planned.extend(ops);
        }
    }
    if planned.is_empty() {
        return String::new();
    }
    let planned = with_adopted(ctx.env, ctx.session, planned);
    // a change that breaks callers carries why it was made, for them
    let intent = session_intent(ctx.env, ctx.session);
    let planned: Vec<Planned> = planned
        .into_iter()
        .map(|p| if !intent.is_empty() && matches!(p.kind, "signature_change" | "rename" | "delete" | "move") { p.with("intent", &intent) } else { p })
        .collect();
    let ops: Vec<Value> = planned.iter().map(Planned::to_json).collect();
    let answer = match ask(ctx.env, ctx.session, &json!({"cmd": "ops", "ops": ops}), OPS_WAIT) {
        Some(answer) => answer,
        None if daemon_up(ctx.env, ctx.session) => return String::new(), // it spools the answer
        None => {
            let url = ws_url(&ctx.cfg.server, ctx.env);
            if is_down(ctx.env, &url) {
                return String::new();
            }
            spawn_daemon(ctx.root, ctx.session, ctx.env);
            let session = direct_session(ctx.session);
            let client = client_config(ctx.root, ctx.cfg, &session, crate::harness::current());
            let ops = with_ids(&planned, &session);
            let answer = direct(&url, client, ctx.env, DIRECT_BUDGET, move |c, left| send_ops(c, &ops, left, |_| {}));
            let Some(answer) = answer else { return String::new() };
            settle(ctx.env, ctx.session, &answer);
            answer
        }
    };
    let notices: Vec<String> =
        answer.get("rejects").and_then(Value::as_array).into_iter().flatten().filter_map(reject_notice).collect();
    notices.join("\n")
}

/// What an answer to a write's ops leaves behind: accepted ops are drafts
/// (and the keys they landed on need no adopted version any more); a
/// refused op's key is adopted at the version that refused it, which the
/// notice tells the agent to bring in.
pub fn settle(env: &Env, session: &str, answer: &Value) {
    add_drafts(env, session, &draft_rows(answer));
    let mut landed: BTreeSet<String> = BTreeSet::new();
    for ack in answer.get("acks").and_then(Value::as_array).into_iter().flatten() {
        landed.insert(text(ack, "key"));
        if let Some(to) = ack.pointer("/moved_to/key").and_then(Value::as_str) {
            landed.insert(to.to_string());
        }
    }
    forget_adopted(env, session, &landed);
    for reject in answer.get("rejects").and_then(Value::as_array).into_iter().flatten() {
        if matches!(text(reject, "reason").as_str(), "conflict" | "unknown_base") {
            adopt(env, session, &text(reject, "key"), reject.get("current").unwrap_or(&Value::Null));
        }
    }
}

/// The drafts an answer's acks make: `{op_id, key, path}` each.
pub fn draft_rows(answer: &Value) -> Vec<Value> {
    answer
        .get("acks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        // the key already held it (`same_as`): the server keeps no op to promote
        .filter(|a| text(a, "same_as").is_empty())
        .map(|a| json!({"op_id": text(a, "op_id"), "key": text(a, "key"), "path": key_path(&text(a, "key")), "ts": now_s()}))
        .collect()
}

/// A read: the daemon follows the file's symbols (and what they call, one
/// hop out, through the server's graph), and the functions in other files
/// it calls, found here from its imports: a teammate's new signature, rename
/// or delete there reaches this session at its next tool result, before it
/// writes against the old one. Only when a daemon runs; a read never waits
/// on the network.
pub fn follow_read(env: &Env, session: &str, root: &Path, rel: &str, content: &str) {
    if session.is_empty() || !daemon_up(env, session) {
        return;
    }
    let keys: Vec<String> = match shape_of(rel, Some(content)) {
        Shape::Code(symbols) => symbols.iter().take(MAX_READ_KEYS).map(|s| sym_key(rel, &s.name)).collect(),
        Shape::File(_) => vec![file_key(rel)],
        _ => return,
    };
    let mut callers: Map<String, Value> = Map::new();
    callers_of(rel, &crate::deps::deps_of(root, rel, content), &mut callers);
    let _ = ask(env, session, &json!({"cmd": "subscribe", "keys": keys, "callers": callers}), Duration::from_millis(50));
}

/// The session is over: its daemon releases what it holds and exits.
pub fn bye(env: &Env, session: &str) {
    if !session.is_empty() {
        let _ = ask(env, session, &json!({"cmd": "bye"}), Duration::from_millis(200));
    }
}

// ------------------------------------------------------------ promotion and trailers

/// The draft ops of this session on files the commits `range` change,
/// minus those a commit in the range already names.
pub fn drafts_in(root: &Path, env: &Env, session: &str, range: &str) -> Vec<String> {
    let changed: BTreeSet<String> = crate::git::git(root, &["diff", "--name-only", range], Duration::from_secs(5))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let named: BTreeSet<String> = trailer_ids(root, range).into_iter().collect();
    drafts(env, session)
        .iter()
        .filter(|d| changed.contains(&text(d, "path")))
        .map(|d| text(d, "op_id"))
        .filter(|id| !id.is_empty() && !named.contains(id))
        .collect()
}

/// Every op id the `Cork-Ops:` trailers of the commits in `range` name.
pub fn trailer_ids(root: &Path, range: &str) -> Vec<String> {
    let log = crate::git::git(root, &["log", "--format=%B%x00", range], Duration::from_secs(5)).unwrap_or_default();
    let mut ids: Vec<String> = Vec::new();
    for message in log.split('\0') {
        for id in cork_protocol::trailer::parse_trailer(message) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Before a landing pushes: name this session's ops in the tip commit's
/// trailer (`Cork-Ops:`), amending it. Only an unpushed commit is ever
/// amended, and only when it does not name them already.
pub fn stamp_trailer(root: &Path, env: &Env, session: &str, upstream: &str) -> Vec<String> {
    let ids = drafts_in(root, env, session, &format!("{upstream}..HEAD"));
    if ids.is_empty() {
        return ids;
    }
    let trailer = cork_protocol::trailer::format_trailer(&ids);
    // --only: the message changes, nothing staged (another session's work
    // in a shared checkout) rides along
    let mut args: Vec<String> = ["commit", "--amend", "--only", "--no-edit", "--no-verify", "--quiet"].iter().map(|s| s.to_string()).collect();
    for line in trailer.lines() {
        args.push("--trailer".into());
        args.push(line.to_string());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    if crate::git::git(root, &refs, Duration::from_secs(10)).is_none() {
        return Vec::new();
    }
    ids
}

/// After a push landed: promote the ops its commits name, and this
/// session's drafts on the files they change. Through the daemon, else on a
/// connection of its own.
pub fn promote(root: &Path, cfg: &config::Config, env: &Env, session: &str, ids: Vec<String>, evidence: Value) -> usize {
    if ids.is_empty() || !enabled(root, cfg, env) {
        return 0;
    }
    let request = json!({"cmd": "promote", "op_ids": ids, "evidence": evidence});
    let answer = match ask(env, session, &request, Duration::from_secs(4)) {
        Some(answer) => answer,
        None => {
            let client = client_config(root, cfg, &direct_session(session), crate::harness::current());
            let url = ws_url(&cfg.server, env);
            let ids = ids.clone();
            let evidence = evidence.clone();
            let promoted = direct(&url, client, env, Duration::from_secs(4), move |c, left| c.promote(ids, evidence, left).ok());
            match promoted {
                Some(p) => json!({"promoted": p.op_ids}),
                None => return 0,
            }
        }
    };
    let done: BTreeSet<String> =
        answer.get("promoted").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
    drop_drafts(env, session, &done);
    done.len()
}

/// A plain `git push` the report saw succeed (no landing ran): promote this
/// session's drafts on files that are committed and now on the upstream.
pub fn promote_after_push(root: &Path, cfg: &config::Config, env: &Env, session: &str) {
    if !enabled(root, cfg, env) || drafts(env, session).is_empty() {
        return;
    }
    let ahead = crate::git::git(root, &["rev-list", "--count", "@{u}..HEAD"], Duration::from_secs(3)).unwrap_or_default();
    if ahead.trim() != "0" {
        return;
    }
    let dirty: BTreeSet<String> = crate::git::git(root, &["status", "--porcelain", "--untracked-files=no"], Duration::from_secs(3))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.get(3..).map(str::to_string))
        .collect();
    let ids: Vec<String> =
        drafts(env, session).iter().filter(|d| !dirty.contains(&text(d, "path"))).map(|d| text(d, "op_id")).collect();
    let commit = crate::git::git(root, &["rev-parse", "HEAD"], Duration::from_secs(3)).unwrap_or_default();
    promote(root, cfg, env, session, ids, json!({"commit": commit.trim(), "via": "push"}));
}

// ------------------------------------------------------------ commits from elsewhere

const INGEST_EVERY_S: f64 = 30.0;
const INGEST_COMMITS: usize = 20;
const INGEST_FILES: usize = 30;

/// One checkout's watermark: its upstream as last seen, and when.
fn ingest_state_path(root: &Path, env: &Env) -> PathBuf {
    use sha2::{Digest, Sha256};
    let tag: String = Sha256::digest(root.to_string_lossy().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    proto_dir(env).join(format!("ingest-{tag}.json"))
}

/// The commits this machine already turned into ops, for every checkout
/// and worktree of the repo: kept per checkout, a new worktree made from an
/// older main sent every commit since then again.
fn ingested_path(repo_id: &str, env: &Env) -> PathBuf {
    use sha2::{Digest, Sha256};
    let tag: String = Sha256::digest(repo_id.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    proto_dir(env).join(format!("ingested-{tag}.json"))
}

/// Held while the ingested list is read and written, so two worktrees whose
/// upstreams moved at once never both claim a commit. Taken over when stale.
struct IngestLock(PathBuf);

impl IngestLock {
    fn take(path: PathBuf) -> IngestLock {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        let waited = std::time::Instant::now();
        loop {
            if std::fs::OpenOptions::new().write(true).create_new(true).open(&path).is_ok() {
                return IngestLock(path);
            }
            let stale = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(30));
            if stale || waited.elapsed() > Duration::from_secs(10) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for IngestLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The commits in `from..to` this machine has not turned into ops yet, with
/// their authors' emails, claimed as they are returned: once per machine,
/// whichever checkout or worktree saw them arrive. A commit that carries a
/// `Cork-Ops:` trailer is claimed and not returned: its ops reached the
/// server from the agent that made them.
pub fn claim_commits(root: &Path, repo_id: &str, from: &str, to: &str, env: &Env) -> Vec<(String, String)> {
    let range = format!("{from}..{to}");
    let commits = crate::git::git(root, &["rev-list", "--no-merges", "--reverse", "-n", &INGEST_COMMITS.to_string(), &range], Duration::from_secs(5))
        .unwrap_or_default();
    let repo = if repo_id.is_empty() { root.to_string_lossy().to_string() } else { repo_id.to_string() };
    let shared = ingested_path(&repo, env);
    let _lock = IngestLock::take(shared.with_extension("lock"));
    let listed = |path: &Path| -> Vec<String> {
        config::load_json(path).get("handled").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect()
    };
    // a checkout's own list, from before it was kept per repo, counts too
    let mut handled = listed(&shared);
    for commit in listed(&ingest_state_path(root, env)) {
        if !handled.contains(&commit) {
            handled.push(commit);
        }
    }
    let mut claimed = Vec::new();
    for commit in commits.lines().map(str::trim).filter(|c| !c.is_empty()) {
        if handled.iter().any(|h| h == commit) {
            continue;
        }
        handled.push(commit.to_string());
        let head = crate::git::git(root, &["log", "-1", "--format=%ae%x00%B", commit], Duration::from_secs(5)).unwrap_or_default();
        let (email, message) = head.split_once('\0').unwrap_or(("", ""));
        if !cork_protocol::trailer::parse_trailer(message).is_empty() {
            continue;
        }
        claimed.push((commit.to_string(), email.trim().to_string()));
    }
    let keep = handled.len().saturating_sub(2000);
    save(&shared, &json!({"handled": handled[keep..]}));
    claimed
}

/// Commits that arrived on the upstream since the last look (a fetch, a
/// pull): converted in the background into ops credited to their authors,
/// so the server's versions follow work that never went through an agent.
/// At most one `git rev-parse` per 30 s on the hot path.
pub fn maybe_ingest(root: &Path, cfg: &config::Config, env: &Env, session: &str) {
    if !enabled(root, cfg, env) {
        return;
    }
    let path = ingest_state_path(root, env);
    let mut state = config::load_json(&path);
    if state.get("ts").and_then(Value::as_f64).is_some_and(|t| now_s() - t < INGEST_EVERY_S) {
        return;
    }
    let upstream = crate::git::git(root, &["rev-parse", "@{u}"], Duration::from_secs(2)).unwrap_or_default().trim().to_string();
    let seen = text(&state, "upstream");
    state["ts"] = json!(now_s());
    state["upstream"] = json!(upstream);
    save(&path, &state);
    if upstream.is_empty() || seen.is_empty() || seen == upstream {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut command = std::process::Command::new(exe);
        command
            .args(["proto-ingest", &root.to_string_lossy(), &seen, &upstream, session])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let _ = command.spawn();
    }
}

/// The ops one commit makes, file by file, from its parent's content to its
/// own. Ids are the commit's own (`git.<sha>.<n>`), so the same commit sent
/// from two machines is one set of ops (the server answers a known id with
/// its first ack).
pub fn commit_ops(root: &Path, commit: &str, author_email: &str) -> Vec<Value> {
    let files = crate::git::git(root, &["diff-tree", "--no-commit-id", "--name-only", "-r", "--root", commit], Duration::from_secs(5))
        .unwrap_or_default();
    let show = |rev: &str, path: &str| -> Option<String> { crate::git::git(root, &["show", &format!("{rev}:{path}")], Duration::from_secs(5)) };
    let mut planned: Vec<Planned> = Vec::new();
    for rel in files.lines().filter(|l| !l.is_empty() && !crate::report::is_collide_artifact(l)).take(INGEST_FILES) {
        let before = shape_of(rel, show(&format!("{commit}^"), rel).as_deref());
        let after = shape_of(rel, show(commit, rel).as_deref());
        planned.extend(plan(rel, &before, &after));
    }
    let short: String = commit.chars().take(16).collect();
    planned
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut op = p.to_json();
            op["public"]["author_email"] = json!(author_email);
            op["op_id"] = json!(format!("git.{short}.{i}"));
            op
        })
        .collect()
}

/// `collide-hook proto-ingest <root> <from> <to> <session>`: the detached
/// half of `maybe_ingest`: the commits no checkout of this repo on this
/// machine sent yet (`claim_commits`). What the server already has (the
/// agent that wrote it reported it) it acks as the same result, and what
/// differs it takes as the truth: landed history (protocol SPEC §10.1).
pub fn ingest_command(args: &[String], env: &Env) -> i32 {
    let [root, from, to, session, ..] = args else { return 0 };
    let root = PathBuf::from(root);
    let cfg = config::config(Some(&root), env);
    if !enabled(&root, &cfg, env) {
        return 0;
    }
    let ops: Vec<Value> =
        claim_commits(&root, &cfg.repo_id, from, to, env).iter().flat_map(|(commit, email)| commit_ops(&root, commit, email)).collect();
    if ops.is_empty() {
        return 0;
    }
    let request = json!({"cmd": "ops", "ops": ops, "ingest": true});
    if ask(env, session, &request, Duration::from_secs(5)).is_none() {
        let client = client_config(&root, &cfg, &format!("{}.ingest", op_prefix(session)), "git");
        let url = ws_url(&cfg.server, env);
        let _ = direct(&url, client, env, Duration::from_secs(10), move |c, left| send_ops(c, &ops, left, |_| {}));
    }
    0
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_sessions_own_changes_never_come_back_as_a_teammates() {
        use serde_json::json;
        let d = |author: &str| json!({"op": {"key": "code:a.py::f", "kind": "rename", "author": {"session": author, "person": "kevin@x"}}});
        // its fallback writer, its commit ingest and itself
        assert!(own_delta(&d("s1.d4242"), "s1"));
        assert!(own_delta(&d("s1.ingest"), "s1"));
        assert!(own_delta(&d("s1"), "s1"));
        // the daemon's own connection id is a side session too
        assert!(own_delta(&d("s1"), "s1.d77"));
        // anyone else is news, even the same person in another session
        assert!(!own_delta(&d("s2"), "s1"));
        assert!(!own_delta(&d("s1x.d1"), "s1"));
        assert!(!own_delta(&d("s1.dev"), "s1"), "a session id that merely contains .d");
        assert!(!own_delta(&json!({"op": {"key": "k"}}), "s1"), "no author: not ours");
    }

    use super::*;

    const TAX: &str = "def compute_tax(amount, rate):\n    return round(amount * rate, 2)\n\n\ndef total(items):\n    return sum(items)\n";

    fn kinds(ops: &[Planned]) -> Vec<(String, &'static str)> {
        ops.iter().map(|o| (o.key.clone(), o.kind)).collect()
    }

    #[test]
    fn a_diff_becomes_one_typed_op_per_changed_symbol() {
        let before = shape_of("tax.py", Some(TAX));
        let body = TAX.replace("rate, 2)", "rate, 4)");
        let ops = plan("tax.py", &before, &shape_of("tax.py", Some(&body)));
        assert_eq!(kinds(&ops), vec![("code:tax.py::compute_tax".to_string(), "modify_body")]);
        let Shape::Code(b) = &before else { panic!() };
        assert_eq!(ops[0].base_hash, b[0].hash);
        assert_eq!(ops[0].to_json()["base"], json!({"version": 0, "hash": "", "content": b[0].hash}));

        let sig = TAX.replace("(amount, rate)", "(amount, rate, region)");
        let ops = plan("tax.py", &before, &shape_of("tax.py", Some(&sig)));
        assert_eq!(kinds(&ops), vec![("code:tax.py::compute_tax".to_string(), "signature_change")]);
        assert_eq!(ops[0].public["new_signature"], json!("def compute_tax(amount, rate, region)"));

        let renamed = TAX.replace("def compute_tax", "def tax_of");
        let ops = plan("tax.py", &before, &shape_of("tax.py", Some(&renamed)));
        assert_eq!(kinds(&ops), vec![("code:tax.py::compute_tax".to_string(), "rename")]);
        assert_eq!(ops[0].public["new_name"], json!("tax_of"));

        let added = format!("{TAX}\n\ndef refund(x):\n    return -x\n");
        let ops = plan("tax.py", &before, &shape_of("tax.py", Some(&added)));
        assert_eq!(kinds(&ops), vec![("code:tax.py::refund".to_string(), "insert")]);
        assert_eq!(ops[0].base_hash, "");

        let removed = "def total(items):\n    return sum(items)\n";
        let ops = plan("tax.py", &before, &shape_of("tax.py", Some(removed)));
        assert_eq!(kinds(&ops), vec![("code:tax.py::compute_tax".to_string(), "delete")]);
        assert_eq!(ops[0].to_json()["content_hash"], json!(""));

        // a renamed AND edited symbol is a delete and an insert: its shape moved
        let both = TAX.replace("def compute_tax", "def tax_of").replace("rate, 2)", "rate, 3)");
        let ops = plan("tax.py", &before, &shape_of("tax.py", Some(&both)));
        assert_eq!(kinds(&ops), vec![("code:tax.py::compute_tax".to_string(), "delete"), ("code:tax.py::tax_of".to_string(), "insert")]);

        // a whole new file, a deleted file, a file no parser knows
        assert_eq!(plan("tax.py", &Shape::Absent, &before).len(), 2);
        assert_eq!(kinds(&plan("tax.py", &before, &Shape::Absent)).len(), 2);
        let ops = plan("conf.toml", &shape_of("conf.toml", Some("a = 1\n")), &shape_of("conf.toml", Some("a = 2\n")));
        assert_eq!(kinds(&ops), vec![("file:conf.toml".to_string(), "file_replace")]);
        assert_eq!(kinds(&plan("conf.toml", &Shape::File("h".into()), &Shape::Absent)), vec![("file:conf.toml".to_string(), "delete")]);
        assert!(plan("conf.toml", &shape_of("conf.toml", Some("a\n")), &shape_of("conf.toml", Some("a  \r\n"))).is_empty(), "whitespace only");
        // a half-typed edit keys nothing
        assert!(plan("tax.py", &before, &shape_of("tax.py", Some("def (:\n"))).is_empty());
        // and a planned op survives the trip through the daemon
        let op = &plan("tax.py", &before, &shape_of("tax.py", Some(&sig)))[0];
        assert_eq!(&Planned::from_json(&op.to_json()).unwrap(), op);
        assert_eq!(Shape::from_json(&before.to_json()).unwrap(), before);
    }

    #[test]
    fn an_edit_is_worked_out_before_it_happens() {
        let dir = Path::new("/r");
        let target = Path::new("/r/tax.py");
        let edit = json!({"file_path": "/r/tax.py", "old_string": "rate, 2)", "new_string": "rate, 4)"});
        let after = predict_after(Some(TAX), "Edit", &edit, target, dir).unwrap().unwrap();
        assert!(after.contains("rate, 4)"));
        let touched = plan("tax.py", &shape_of("tax.py", Some(TAX)), &shape_of("tax.py", after.as_str().into()));
        assert_eq!(kinds(&touched), vec![("code:tax.py::compute_tax".to_string(), "modify_body")], "only the symbol the edit lands in");
        assert!(predict_after(Some(TAX), "Edit", &json!({"old_string": "nowhere", "new_string": "x"}), target, dir).is_none());
        let multi = json!({"edits": [{"old_string": "compute_tax", "new_string": "tax_of"}, {"old_string": "sum(items)", "new_string": "sum(items) or 0"}]});
        let after = predict_after(Some(TAX), "MultiEdit", &multi, target, dir).unwrap().unwrap();
        let ops = plan("tax.py", &shape_of("tax.py", Some(TAX)), &shape_of("tax.py", Some(&after)));
        assert_eq!(kinds(&ops), vec![("code:tax.py::compute_tax".to_string(), "rename"), ("code:tax.py::total".to_string(), "modify_body")]);
        let write = json!({"content": "x = 1\n"});
        assert_eq!(predict_after(Some(TAX), "Write", &write, target, dir), Some(Some("x = 1\n".to_string())));

        // a Codex patch: hunks applied by their context, other files ignored
        let patch = "*** Begin Patch\n*** Update File: tax.py\n@@ def total(items):\n-    return sum(items)\n+    return sum(items) + 1\n\
*** Add File: new.py\n+def fresh():\n+    pass\n*** Delete File: old.py\n*** End Patch\n";
        let input = json!({"command": patch});
        let after = predict_after(Some(TAX), "apply_patch", &input, target, dir).unwrap().unwrap();
        assert!(after.contains("return sum(items) + 1") && after.contains("rate, 2)"));
        assert_eq!(predict_after(None, "apply_patch", &input, Path::new("/r/new.py"), dir), Some(Some("def fresh():\n    pass\n".to_string())));
        assert_eq!(predict_after(Some("x"), "apply_patch", &input, Path::new("/r/old.py"), dir), Some(None));
        let stale = json!({"command": "*** Begin Patch\n*** Update File: tax.py\n-    return nothing\n+    x\n*** End Patch"});
        assert!(predict_after(Some(TAX), "apply_patch", &stale, target, dir).is_none());
    }

    /// What the server's gate is told about a write: the names it adds and
    /// the names it changes, never text.
    #[test]
    fn a_write_is_described_to_the_gate_by_name() {
        let dir = std::env::temp_dir().join(format!("proto-facts-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("tests"));
        let target = dir.join("tests/test_features.py");
        std::fs::write(&target, "def test_a():\n    assert 1\n").unwrap();
        let adds = json!({"file_path": target.to_string_lossy(), "old_string": "    assert 1\n",
                          "new_string": "    assert 1\n\n\ndef test_b():\n    assert 2\n"});
        assert_eq!(write_facts("Edit", &adds, &target, &dir, "tests/test_features.py"), Some(json!({"adds": ["test_b"], "changes": []})));
        let edits = json!({"file_path": target.to_string_lossy(), "old_string": "assert 1", "new_string": "assert 3"});
        assert_eq!(write_facts("Edit", &edits, &target, &dir, "tests/test_features.py"), Some(json!({"adds": [], "changes": ["test_a"]})));
        // an edit that does not apply says nothing: the server judges the file whole
        let stale = json!({"file_path": target.to_string_lossy(), "old_string": "nowhere", "new_string": "x"});
        assert_eq!(write_facts("Edit", &stale, &target, &dir, "tests/test_features.py"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The field report (v99): a lead and its helper rebuilt an app their
    /// person had deleted, in the same folder an earlier session built it
    /// in. Every write was told "kevin added mountIssues in issues.js (not
    /// pushed yet, so your copy does not have it)": that session's drafts.
    /// Work done in this folder is never news "not pushed yet" here.
    #[test]
    fn drafts_made_in_this_same_folder_are_never_not_pushed_news() {
        let home = std::env::temp_dir().join(format!("cork-folder-{}", std::process::id()));
        let (root, elsewhere) = (home.join("cork-linear-test"), home.join("clone"));
        for dir in [&root, &elsewhere] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), home.to_string_lossy().into_owned());
        std::fs::create_dir_all(config::data_dir(&env).join("check")).unwrap();
        let state = |session: &str, at: &Path| {
            std::fs::write(crate::check::state_path(session, &env), json!({"root": at.to_string_lossy()}).to_string()).unwrap();
        };
        state("9bd7bf3e-old", &root);
        state("b0b0b0b0-far", &elsewhere);
        let draft = |author: &str| json!({"op_id": format!("{author}.1"), "key": "code:static/js/views/issues.js::mountIssues",
            "kind": "insert", "public": {}, "author": {"session": author, "agent_id": "claude", "person": "kevin@x.com"},
            "hlc": {"w": 1, "c": 0}, "layer": "draft"});
        let check = |theirs: Vec<Value>| vec![json!({"key": "code:static/js/views/issues.js::mountIssues", "status": "stale",
            "would": "conflict", "theirs": theirs, "current": {"version": 3, "hash": "h"}})];
        // an earlier session in this folder, this session's own side session
        // (a helper shares its id): nothing to say
        for author in ["9bd7bf3e-old", "e7c3a2d4.d4242"] {
            let said = verdict(&beyond_this_folder(check(vec![draft(author)]), &root, "e7c3a2d4", &env), Some(&root), true);
            assert!(said.notices.is_empty() && said.block.is_none(), "{author}: {said:?}");
        }
        // a session in another clone: still news, its own and no one else's
        let said = verdict(&beyond_this_folder(check(vec![draft("9bd7bf3e-old"), draft("b0b0b0b0-far")]), &root, "e7c3a2d4", &env), Some(&root), true);
        assert_eq!(said.notices.len(), 1, "{said:?}");
        assert!(said.notices[0].0.ends_with("b0b0b0b0-far.1") && said.notices[0].1.contains("not pushed yet"), "{said:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_gate_blocks_only_when_the_server_names_someone() {
        let alice = json!({"session": "a1", "agent_id": "claude", "person": "alice@x.com"});
        let theirs = json!([{"op_id": "a1.1", "key": "code:tax.py::compute_tax", "version": 1, "kind": "rename",
            "public": {"new_name": "tax_of"}, "author": alice, "intent_id": "rename-tax", "hlc": {"w": 1, "c": 0}, "layer": "promoted"}]);
        let stale = json!({"key": "code:tax.py::compute_tax", "status": "stale", "would": "conflict", "theirs": theirs,
            "current": {"version": 1, "hash": "h"}});
        // the block names whose work it is (the branch check) and its key
        // (what its fix lines are read for)
        let full = verdict(std::slice::from_ref(&stale), None, false);
        assert_eq!((full.block_key.as_str(), full.block_by.as_str(), full.block_who.as_str()),
            ("code:tax.py::compute_tax", "a1", "alice@x.com (claude)"));
        let change = crate::deps::summarize(theirs.as_array().unwrap(), who).unwrap();
        let fix = crate::fixlines::Fix::from_change(&key_symbol(&full.block_key), &change).unwrap();
        assert_eq!(crate::fixlines::render("tax.py", "def compute_tax(a):\n    return a\n\n\nx = compute_tax(1)\n", &BTreeSet::new(), &[fix]),
            "In this write (tax.py):\n  line 1: `def compute_tax(a):` → `def tax_of(a):`\n  line 5: `compute_tax(1)` → `tax_of(1)`");
        let said = gate_verdict(&[stale]).unwrap();
        assert!(said.contains("alice@x.com (claude) already renamed compute_tax -> tax_of (their intent: rename-tax), and pushed it"), "{said}");
        assert!(said.contains("Pull it"));
        // a copy the server never saw, while alice has unpromoted work there
        let unknown = json!({"key": "code:tax.py::compute_tax", "status": "unknown_base", "theirs": theirs, "current": {"version": 1, "hash": "h"}});
        assert!(gate_verdict(&[unknown]).unwrap().contains("alice@x.com (claude) already renamed"));
        // the same rename only in alice's checkout (a draft): never a block,
        // never "pull", but what to write against
        let mut drafts = theirs.clone();
        drafts[0]["layer"] = json!("draft");
        let draft = json!({"key": "code:tax.py::compute_tax", "status": "stale", "would": "conflict", "theirs": drafts,
            "current": {"version": 1, "hash": "h"}});
        let said = verdict(&[draft], None, true);
        assert!(said.block.is_none(), "{said:?}");
        let (_, line) = &said.notices[0];
        assert!(line.contains("renamed compute_tax -> tax_of") && line.contains("not pushed yet")
            && line.contains("write against `tax_of`; landing will rebase onto it"), "{line}");
        assert!(!line.to_lowercase().contains("pull"), "{line}");
        let leased = json!({"key": "code:tax.py::compute_tax", "status": "leased", "holder": alice, "intent": {"text": "faster tax"},
            "current": {"version": 1, "hash": "h"}});
        let said = gate_verdict(&[leased]).unwrap();
        assert!(said.contains("alice@x.com (claude) is editing compute_tax right now (their intent: faster tax)"), "{said}");
        assert!(said.contains("--to alice@x.com"));
        assert_eq!(who(&json!({"person": "alice@x.com", "agent_id": "alice@x.com#s1"})), "alice@x.com");
        // a rename that the edit follows, content the server never saw, an
        // invalid base with no one to name: all allow
        for status in [json!({"status": "stale", "would": "rekey", "rekeyed_to": "code:tax.py::tax_of"}),
                       json!({"status": "unknown_base"}), json!({"status": "stale", "would": "conflict"}), json!({"status": "ok"})] {
            assert!(gate_verdict(&[status.clone()]).is_none(), "{status}");
        }
    }

    #[test]
    fn deltas_say_only_what_the_session_must_adapt_to() {
        let author = json!({"session": "b", "agent_id": "codex", "person": "bob@x.com"});
        let delta = |kind: &str, public: Value, via: Option<Value>| {
            let mut d = json!({"op": {"op_id": "b.1", "key": "code:tax.py::compute_tax", "kind": kind, "public": public, "author": author},
                "version": 2, "hash": "h", "seq": 1, "layer": "draft"});
            if let Some(v) = via {
                d["via"] = v;
            }
            d
        };
        let none = BTreeSet::new();
        let (line, push) = delta_line(&delta("signature_change", json!({"new_signature": "def compute_tax(a, r, region)"}),
            Some(json!(["code:app.py::checkout", "code:tax.py::compute_tax"]))), &none).unwrap();
        assert_eq!(line, "Δ bob@x.com (codex) changed the signature of compute_tax to `def compute_tax(a, r, region)` (your checkout depends on it) in tax.py");
        assert!(!push);
        // the same change to a session that only read tax.py: not news to it
        assert!(delta_line(&delta("signature_change", json!({"new_signature": "def compute_tax(a, r, region)"}), None), &none).is_none(),
            "a reader that calls nothing hears nothing");
        assert!(delta_line(&delta("modify_body", json!({}), None), &none).is_none(), "a body edit breaks no caller");
        let editing: BTreeSet<String> = ["code:tax.py::compute_tax".to_string()].into();
        let (line, push) = delta_line(&delta("modify_body", json!({}), None), &editing).unwrap();
        assert!(line.ends_with("edited compute_tax, which you are editing in tax.py") && push, "{line}");
        let mut promoted = delta("rename", json!({"new_name": "t"}), None);
        promoted["layer"] = json!("promoted");
        assert!(delta_line(&promoted, &none).is_none());
    }

    #[test]
    fn rejects_become_notices_and_spooled_lines_are_taken_once() {
        let reject = json!({"op_id": "s.1", "key": "code:tax.py::compute_tax", "reason": "conflict", "current": {"version": 2, "hash": "h"},
            "theirs": [{"op_id": "a.1", "key": "code:tax.py::compute_tax", "version": 2, "kind": "modify_body",
                "author": {"session": "a", "agent_id": "claude", "person": "alice@x.com"}, "hlc": {"w": 1, "c": 0}, "layer": "promoted"}],
            "holder": {"session": "a", "agent_id": "claude", "person": "alice@x.com"}, "their_intent": {"id": "i-9", "text": "round half even"}});
        let said = reject_notice(&reject).unwrap();
        assert!(said.starts_with("Cork: alice@x.com (claude) edited compute_tax first (their intent: round half even)"), "{said}");
        assert!(said.contains("Your edit is on disk") && !said.contains("did not take"), "{said}");
        // work only in alice's checkout: nothing to re-read, the gate said it
        let mut draft = reject.clone();
        draft["theirs"][0]["layer"] = json!("draft");
        assert!(reject_notice(&draft).is_none());
        assert!(reject_notice(&json!({"reason": "invalid", "key": "code:a.py::f"})).is_none());
        // a lease refuses Cork's record of the change, not the edit on disk:
        // nothing is said (the daemon records it when the lease goes)
        let mut leased = reject.clone();
        leased["reason"] = json!("leased");
        assert!(reject_notice(&leased).is_none());

        let home = std::env::temp_dir().join(format!("proto-spool-{}", std::process::id()));
        let mut env = Env::default();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        spool_line(&env, "s1", "delta", "Δ one");
        spool_line(&env, "s1", "delta", "Δ one");
        spool_line(&env, "s1", "notice", "Cork: refused");
        let said = take_notices(&env, "s1");
        assert!(said.starts_with("Cork: refused\nCork (live)") && said.ends_with("\nΔ one") && said.matches("Δ one").count() == 1, "{said}");
        assert_eq!(take_notices(&env, "s1"), "", "taken once");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_server_that_is_down_costs_one_bounded_try_then_nothing() {
        let home = std::env::temp_dir().join(format!("proto-down-{}", std::process::id()));
        let mut env = Env::default();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        // 10.255.255.1 is unroutable: a connect there hangs, the hook does not
        let started = Instant::now();
        let got = direct("ws://10.255.255.1:9/protocol/v1/ws", ClientConfig::default(), &env, Duration::from_millis(300), |_, _| Some(1));
        assert!(got.is_none());
        assert!(started.elapsed() < Duration::from_millis(600), "{:?}", started.elapsed());
        let again = Instant::now();
        assert!(direct("ws://10.255.255.1:9/protocol/v1/ws", ClientConfig::default(), &env, Duration::from_secs(5), |_, _| Some(1)).is_none());
        assert!(again.elapsed() < Duration::from_millis(20), "marked down: no second try");
        // a refused connect is quick too
        let refused = Instant::now();
        assert!(direct("ws://127.0.0.1:9/x", ClientConfig::default(), &env, Duration::from_secs(1), |_, _| Some(1)).is_none());
        assert!(refused.elapsed() < Duration::from_millis(500));
        assert_eq!(ws_url("https://mcp.corkcli.com/", &Env::default()), "wss://mcp.corkcli.com/protocol/v1/ws");
        assert_eq!(ws_url("http://127.0.0.1:8080", &Env::default()), "ws://127.0.0.1:8080/protocol/v1/ws");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn trailers_name_the_ops_a_commit_carries() {
        let ids = vec!["s.1.0".to_string(), "s.1.1".to_string()];
        let message = format!("Fix tax\n\nCo-Authored-By: x\n{}", cork_protocol::trailer::format_trailer(&ids));
        assert_eq!(cork_protocol::trailer::parse_trailer(&message), ids);
        assert_eq!(key_path("code:pkg/tax.py::compute_tax"), "pkg/tax.py");
        assert_eq!(key_path("file:conf.toml"), "conf.toml");
        assert_eq!(sym_key("a.rs", "impl Foo"), "code:a.rs::impl_Foo");
    }

    /// The field report (v102): a write in a fresh worktree was told
    /// "kevin@x.com edited OnboardingFlow (not pushed yet, so your
    /// copy does not have it)". The op was `git.aa7b2ef671296ea0.25.r`: a
    /// commit three weeks old, ingested from main, and in the worktree's own
    /// history. A commit this copy has is never news; one it lacks still is.
    #[test]
    fn an_ingested_commit_this_copy_already_has_is_never_not_pushed_news() {
        let root = std::env::temp_dir().join(format!("proto-ingested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(&root).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "a@x.com"]);
        git(&["config", "user.name", "a"]);
        std::fs::write(root.join("Flow.tsx"), "export function OnboardingFlow() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "onboarding"]);
        let here: String = git(&["rev-parse", "HEAD"]).chars().take(16).collect();
        let op = |id: &str, session: &str| json!({"op_id": id, "key": "code:Flow.tsx::OnboardingFlow", "kind": "modify_body",
            "public": {}, "author": {"session": session, "agent_id": "claude", "person": "kevin@x.com"}, "hlc": {"w": 1, "c": 0}, "layer": "draft"});
        let check = |theirs: Vec<Value>| vec![json!({"key": "code:Flow.tsx::OnboardingFlow", "status": "unknown_base",
            "would": "conflict", "theirs": theirs, "current": {"version": 3, "hash": "h"}})];
        // ingested from main and in this history, as sent and as re-sent over an unknown base
        for id in [format!("git.{here}.25"), format!("git.{here}.25.r")] {
            let said = verdict(&without_commits_here(check(vec![op(&id, &format!("git.{here}"))]), &root), Some(&root), true);
            assert!(said.notices.is_empty() && said.block.is_none(), "{id}: {said:?}");
        }
        // a commit this copy lacks, and a teammate's draft, are still told
        let elsewhere = "0123456789abcdef";
        let theirs = vec![op(&format!("git.{elsewhere}.1"), &format!("git.{elsewhere}")), op("b0b0.7.1", "b0b0")];
        let kept = without_commits_here(check(theirs), &root);
        assert_eq!(kept[0]["theirs"].as_array().unwrap().len(), 2);
        assert!(ingested_commit("git.zzzz.1").is_none() && ingested_commit("b0b0.7.1").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The field report (v107): after two PRs merged, a session ingested
    /// their commits, then a new worktree of the same repo (made from an
    /// older main) ingested the first again. A commit is claimed once per
    /// machine, whichever checkout or worktree sees it arrive.
    #[test]
    fn a_second_worktree_never_ingests_a_commit_again() {
        let base = std::env::temp_dir().join(format!("proto-claim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&base, &["init", "-q", "-b", "main", "repo"]);
        let repo = base.join("repo");
        git(&repo, &["config", "user.email", "kevin@x.com"]);
        git(&repo, &["config", "user.name", "kevin"]);
        let commit = |name: &str, message: &str| {
            std::fs::write(repo.join(name), format!("export const {} = 1;\n", name.replace('.', "_"))).unwrap();
            git(&repo, &["add", "."]);
            git(&repo, &["commit", "-qm", message]);
            git(&repo, &["rev-parse", "HEAD"])
        };
        let seed = commit("seed.ts", "seed");
        // a worktree made now, on the older main
        git(&repo, &["worktree", "add", "-q", "--detach", &base.join("wt").to_string_lossy(), &seed]);
        let channels = commit("channels.ts", "Channels");
        let agents = commit("cli.ts", "Channels CLI\n\nCork-Ops: s1.4.0,s1.4.1");
        let mut env = Env::new();
        env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
        let id = "github.com/acme/web";
        // the main checkout sees both commits arrive; the agent's own carries its ops
        let first = claim_commits(&repo, id, &seed, &agents, &env);
        assert_eq!(first, vec![(channels.clone(), "kevin@x.com".to_string())]);
        // the worktree's upstream moves over the same commits: nothing to send
        assert!(claim_commits(&base.join("wt"), id, &seed, &agents, &env).is_empty());
        // what is new to the machine still goes, once
        let next = commit("modal.ts", "Route modal");
        assert_eq!(claim_commits(&base.join("wt"), id, &seed, &next, &env), vec![(next.clone(), "kevin@x.com".to_string())]);
        assert!(claim_commits(&repo, id, &seed, &next, &env).is_empty());
        // another repo is another list
        assert_eq!(claim_commits(&repo, "github.com/acme/other", &agents, &next, &env).len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_ack_of_what_the_key_already_held_is_no_draft() {
        let answer = json!({"acks": [
            {"op_id": "s1.1.0", "key": "code:a.py::f", "version": 2},
            {"op_id": "s1.1.1", "key": "code:a.py::g", "version": 5, "same_as": "s2.9.0"},
        ]});
        let rows = draft_rows(&answer);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["op_id"], json!("s1.1.0"));
    }

    #[test]
    fn a_landing_names_its_drafts_in_the_unpushed_tip_once() {
        let base = std::env::temp_dir().join(format!("proto-trailer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (remote, root) = (base.join("remote.git"), base.join("work"));
        std::fs::create_dir_all(&root).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&base, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "a@x.com"]);
        git(&root, &["config", "user.name", "a"]);
        std::fs::write(root.join("tax.py"), TAX).unwrap();
        std::fs::write(root.join("other.py"), "x = 1\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "start"]);
        git(&root, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&root, &["push", "-qu", "origin", "main"]);
        std::fs::write(root.join("tax.py"), TAX.replace("rate, 2)", "rate, 4)")).unwrap();
        git(&root, &["commit", "-qam", "Round to 4\n\nCo-Authored-By: someone"]);
        let pushed = git(&root, &["rev-parse", "origin/main"]);

        let mut env = Env::default();
        env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
        add_drafts(&env, "s1", &[json!({"op_id": "s1.1.0", "key": "code:tax.py::compute_tax", "path": "tax.py"}),
                                 json!({"op_id": "s1.1.1", "key": "code:other.py::x", "path": "other.py"})]);
        // only the op on a file the unpushed commits change
        assert_eq!(stamp_trailer(&root, &env, "s1", "origin/main"), vec!["s1.1.0".to_string()]);
        let message = git(&root, &["log", "-1", "--format=%B"]);
        assert!(message.contains("Co-Authored-By: someone") && message.ends_with("Cork-Ops: s1.1.0"), "{message}");
        assert_eq!(trailer_ids(&root, "origin/main..HEAD"), vec!["s1.1.0".to_string()]);
        // named once: a second landing attempt amends nothing
        let tip = git(&root, &["rev-parse", "HEAD"]);
        assert!(stamp_trailer(&root, &env, "s1", "origin/main").is_empty());
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), tip);
        // what was pushed is never rewritten
        assert_eq!(git(&root, &["rev-parse", "origin/main"]), pushed);

        // a commit made elsewhere, without a trailer: its ops, credited to
        // its author, with ids of its own (the same from any machine)
        std::fs::write(root.join("tax.py"), TAX.replace("def total(", "def grand_total(").replace("rate, 2)", "rate, 4)")).unwrap();
        std::fs::write(root.join("conf.toml"), "a = 1\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["-c", "user.email=human@x.com", "commit", "-qm", "By hand"]);
        let sha = git(&root, &["rev-parse", "HEAD"]);
        let ops = commit_ops(&root, &sha, "human@x.com");
        let got: Vec<(String, String)> = ops.iter().map(|o| (text(o, "key"), text(o, "kind"))).collect();
        assert_eq!(got, vec![("file:conf.toml".to_string(), "file_replace".to_string()),
                             ("code:tax.py::total".to_string(), "rename".to_string())], "{ops:?}");
        assert_eq!(ops[1]["public"]["author_email"], json!("human@x.com"));
        assert_eq!(ops[1]["op_id"], json!(format!("git.{}.1", &sha[..16])));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_planned_op_keeps_an_adopted_base() {
        let home = std::env::temp_dir().join(format!("proto-adopt-{}", std::process::id()));
        let mut env = Env::default();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        let before = shape_of("tax.py", Some(TAX));
        let planned = plan("tax.py", &before, &shape_of("tax.py", Some(&TAX.replace("rate, 2)", "rate, 4)"))));
        assert!(with_adopted(&env, "s", planned.clone())[0].adopted.is_none());
        adopt(&env, "s", "code:tax.py::compute_tax", &json!({"version": 3, "hash": "h3", "promoted_version": 0}));
        let adopted = with_adopted(&env, "s", planned.clone());
        let Shape::Code(b) = &before else { panic!() };
        assert_eq!(adopted[0].to_json()["base"], json!({"version": 3, "hash": "h3", "content": b[0].hash}));
        assert_eq!(Planned::from_json(&adopted[0].to_json()).unwrap(), adopted[0]);
        // an accepted op on the key: the content base again
        settle(&env, "s", &json!({"acks": [{"op_id": "s.1", "key": "code:tax.py::compute_tax", "version": 4, "hash": "h4"}]}));
        assert!(with_adopted(&env, "s", planned)[0].adopted.is_none());
        let _ = std::fs::remove_dir_all(&home);
    }
}
