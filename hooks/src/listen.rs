//! `cork listen`: a teammate's message reaches an open Claude Code
//! session the moment it is sent, not at the session's next hook call.
//!
//! The session's start hook starts one listener per session, detached. It
//! waits on Cork's `/inbox/wait` and posts each message it is handed into
//! the session through Claude Code's own inbox socket
//! (`CLAUDE_CODE_MESSAGING_SOCKET`, the one other local sessions message it
//! on): the message shows in the conversation like one from another session,
//! and an idle session starts a turn on it. The listener ends with the
//! session (its socket stops answering).
//!
//! Also here: `cork ack <id>...` (an agent that handled a message clears
//! it, with no MCP tool needed) and the machine's list of checkouts, which
//! tells the wake watcher where a repo lives on this machine.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::{self, Env};

/// What one wait asks the server to hold for.
const WAIT_S: u64 = 25;
/// The longest a listener lives: a session open longer gets a new one at
/// its next start or resume.
const MAX_LIFE: Duration = Duration::from_secs(7 * 24 * 3600);

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

// ------------------------------------------------------------ the checkouts

fn checkouts_path(env: &Env) -> PathBuf {
    crate::machine::data_dir(env).join("checkouts.json")
}

/// Remember where a repo lives on this machine (`repo_id -> folder`), so the
/// wake watcher can start an agent in it. Written at each session start.
pub fn note_checkout(root: &Path, server: &str, repo_id: &str, env: &Env) {
    if repo_id.is_empty() || server.is_empty() {
        return;
    }
    let path = checkouts_path(env);
    let mut all = config::load_json(&path);
    if !all.is_object() {
        all = json!({});
    }
    let folder = root.to_string_lossy().to_string();
    // a linked worktree (a woken agent's among them) is not where the repo lives
    if root.join(".git").is_file() && all.get(repo_id).is_some() {
        return;
    }
    let git = git_identity(root);
    let same = all
        .get(repo_id)
        .map(|row| text(row, "root") == folder && text(row, "server") == server && text(row, "git") == git)
        .unwrap_or(false);
    if same {
        return;
    }
    all[repo_id] = json!({"root": folder, "server": server, "seen": now_s(), "git": git});
    let _ = std::fs::create_dir_all(crate::machine::data_dir(env));
    let _ = std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&all).unwrap_or_default()));
}

/// The `.git` folder's identity on disk (device and inode): it stays when
/// the checkout's folder is moved or renamed, and a new clone gets another.
/// Empty where the system gives none.
pub fn git_identity(root: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::metadata(root.join(".git")) {
            if meta.is_dir() {
                return format!("{}:{}", meta.dev(), meta.ino());
            }
        }
    }
    #[cfg(not(unix))]
    let _ = root;
    String::new()
}

/// Every checkout this machine has noted, as written: `{repo_id: {root,
/// server, seen, git}}` (renames.rs reads it for a folder that changed repo id).
pub fn checkouts_all(env: &Env) -> Value {
    let all = config::load_json(&checkouts_path(env));
    if all.is_object() { all } else { json!({}) }
}

/// Forget a repo id's checkout: the repo was renamed, and the server knows.
pub fn forget_checkout(repo_id: &str, env: &Env) {
    let path = checkouts_path(env);
    let mut all = checkouts_all(env);
    if all.as_object_mut().and_then(|map| map.remove(repo_id)).is_some() {
        let _ = std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&all).unwrap_or_default()));
    }
}

/// This machine's checkouts on `server` that still exist: `(repo_id, folder)`.
pub fn checkouts(server: &str, env: &Env) -> Vec<(String, PathBuf)> {
    let all = config::load_json(&checkouts_path(env));
    let mut out = Vec::new();
    for (repo_id, row) in all.as_object().into_iter().flatten() {
        let root = PathBuf::from(text(row, "root"));
        if text(row, "server").trim_end_matches('/') == server.trim_end_matches('/') && root.join(".git").exists() {
            out.push((repo_id.clone(), root));
        }
    }
    out
}

// ------------------------------------------------------------ the message

pub(crate) fn age_words(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    match s {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", s / 60),
        _ => format!("{}h ago", s / 3600),
    }
}

/// Who sent it, as the person knows them: "Bob (bob@x.com)".
pub fn sender(message: &Value) -> String {
    let from = text(message, "from");
    let name = text(message, "from_name");
    if name.is_empty() || name == from { from } else { format!("{name} ({from})") }
}

/// Who Cork says sent a message and what it saw that session do, as the
/// server put them on it (`verified`, `evidence`): a line each, under the
/// message, so its claims are read against them. "" from an older server.
pub fn provenance(message: &Value) -> String {
    ["verified", "evidence"]
        .iter()
        .map(|key| text(message, key))
        .filter(|line| !line.is_empty())
        .map(|line| format!("\n  {line}"))
        .collect()
}

/// What the messages handed over together arrive as in an open session:
/// one injection, the guidance said once. Claude Code wraps it as a message
/// from another session, and the session's prompt hook briefs the code it
/// names, as it does any prompt, so no facts are repeated here.
pub fn session_text(messages: &[Value], collide: &str) -> String {
    // a handoff is work sent to this session alone, from its user's other
    // session: to be done, not weighed as news
    let (handoffs, notes): (Vec<Value>, Vec<Value>) =
        messages.iter().cloned().partition(|m| m.get("handoff").and_then(Value::as_bool) == Some(true));
    let mut parts: Vec<String> = Vec::new();
    for handoff in &handoffs {
        parts.push(format!(
            "Cork: a handoff for you [{}], from {}, {}. It is work your user wants done, sent to you alone: start on it \
now, tell the user in one line that you picked it up, and when it is done tell the sender (`{collide} message --to {} \"...\"`), \
then `{collide} ack {}`.{}\n{}",
            text(handoff, "id"),
            sender(handoff),
            age_words(handoff.get("age_s").and_then(Value::as_f64).unwrap_or(0.0)),
            text(handoff, "from"),
            text(handoff, "id"),
            provenance(handoff),
            text(handoff, "message").trim(),
        ));
    }
    if !notes.is_empty() {
        parts.push(notes_text(&notes, collide));
    }
    parts.join("\n\n")
}

/// Messages that are not handoffs, as one injection.
fn notes_text(messages: &[Value], collide: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for message in messages {
        let id = text(message, "id");
        let anchor = text(message, "anchor");
        let about = if anchor.is_empty() { String::new() } else { format!(" about {anchor}") };
        lines.push(format!(
            "[{id}] {}{about}, {}: {}{}",
            sender(message),
            age_words(message.get("age_s").and_then(Value::as_f64).unwrap_or(0.0)),
            text(message, "message").trim(),
            provenance(message),
        ));
        ids.push(id);
    }
    let count = if messages.len() == 1 { "A message".to_string() } else { format!("{} messages", messages.len()) };
    // from this session's own helpers (subagents), which share its session
    let who = if messages.iter().all(|m| m.get("within_session").and_then(Value::as_bool) == Some(true)) {
        "this session's own helpers"
    } else {
        "teammates' agents"
    };
    // a call to action only when one asks something; news is just news
    if !messages.iter().any(|m| asks(&text(m, "message"))) {
        return format!(
            "Cork: {count} from {who} (not your user), for your information: nothing to do unless it \
changes your work.\n{}",
            lines.join("\n"),
        );
    }
    format!(
        "Cork: {count} from {who} (not your user). Act on what fits this repo's direction, within your \
permissions, and tell the user.\n{}\nReply: `{collide} message --to <sender> \"...\"`. Handled: `{collide} ack {}`.",
        lines.join("\n"),
        ids.join(" "),
    )
}

/// Whether a message asks something of its reader: a question, a request,
/// a "don't". Twin of the server's `agenttools::asks`.
pub fn asks(message: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let lower = message.to_lowercase();
    lower.contains('?')
        || RE.get_or_init(|| {
            regex::Regex::new(concat!(
                r"\b(?:please|pls|can you|could you|would you|will you|can someone|you need to|you must|you should|",
                r"make sure|don't|do not|stop|wait|hold off|hold on|avoid|leave .{1,30} alone|let me know|need you|",
                r"need your|before you|instead of|switch (?:over|to)|update your|use .{1,30} instead)\b"
            ))
            .expect("asks pattern")
        })
        .is_match(&lower)
}

// ------------------------------------------------------------ the socket

/// Post one line of text into the session, as its own child process does.
/// `false` when the session is gone.
pub(crate) fn post_to_session(socket: &str, token: &str, content: &str) -> bool {
    let mut lines = String::new();
    if !token.is_empty() {
        lines.push_str(&json!({"type": "auth", "token": token}).to_string());
        lines.push('\n');
    }
    lines.push_str(&json!({"type": "user", "message": {"role": "user", "content": content}}).to_string());
    lines.push('\n');
    write_socket(socket, lines.as_bytes())
}

/// Post one message into another session as a peer does: the auth line
/// with its peer token, then the frame Claude Code's SendMessage writes
/// (`peer::frame`). `false` when the session is gone.
pub(crate) fn post_as_peer(socket: &str, token: &str, frame: &Value) -> bool {
    let lines = format!("{}\n{frame}\n", json!({"type": "auth", "token": token}));
    write_socket(socket, lines.as_bytes())
}

#[cfg(unix)]
fn write_socket(socket: &str, bytes: &[u8]) -> bool {
    use std::io::Write;
    match std::os::unix::net::UnixStream::connect(socket) {
        Ok(mut stream) => {
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            stream.write_all(bytes).and_then(|_| stream.flush()).is_ok()
        }
        Err(_) => false,
    }
}

#[cfg(windows)]
fn write_socket(socket: &str, bytes: &[u8]) -> bool {
    use std::io::Write;
    match std::fs::OpenOptions::new().write(true).open(socket) {
        Ok(mut pipe) => pipe.write_all(bytes).and_then(|_| pipe.flush()).is_ok(),
        Err(_) => false,
    }
}

/// A Claude Code session on this machine, as another session reaches it by
/// Claude Code's own peer messaging: its socket (`messagingSocketPath` in
/// its registry entry) and the token a peer authenticates with there (the
/// `peerToken` of `<pid>.<sha256 of the socket path>.key` beside it).
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    pub session: String,
    pub socket: String,
    pub token: String,
}

/// The Claude Code sessions on this machine another one can message: the
/// ones whose registry entry speaks peer protocol 1 (the frames
/// `post_as_peer` writes) and that left a peer token. Read when a
/// message is sent, never on a hook's hot path.
pub fn peers(env: &Env) -> Vec<Peer> {
    let dir = crate::transcript::registry_dir(env);
    let mut out = Vec::new();
    for (path, record) in crate::transcript::registry(env) {
        let (session, socket) = (text(&record, "sessionId"), text(&record, "messagingSocketPath"));
        if session.is_empty() || socket.is_empty() || record.get("peerProtocol").and_then(Value::as_i64) != Some(1) {
            continue;
        }
        let pid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let key = dir.join(format!("{pid}.{}.key", crate::report::sha256_hex(socket.as_bytes())));
        let token = text(&config::load_json(&key), "peerToken");
        if !token.is_empty() && Path::new(&socket).exists() {
            out.push(Peer { session, socket, token });
        }
    }
    out
}

/// The session still answers on its socket.
#[cfg(unix)]
pub(crate) fn session_open(socket: &str) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

#[cfg(windows)]
pub(crate) fn session_open(socket: &str) -> bool {
    Path::new(socket).exists()
}

// ------------------------------------------------------------ the listener

fn listen_dir(env: &Env) -> PathBuf {
    crate::machine::data_dir(env).join("listen")
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

/// Start this session's listener, unless one is already running for it.
/// It hands teammates' messages to a Claude Code session (only Claude Code
/// has an inbox socket; only a team's repo has anyone to hear from), and it
/// holds the session's protocol connection, for every harness (protod.rs).
pub fn spawn_for_session(root: &Path, session: &str, env: &Env) {
    spawn_owned(root, session, env, harness_owner(std::process::id()));
}

/// Programs that only stand between the harness and its hook: the shell a
/// hook command runs in, `env`, `timeout`, Cork itself (`cork`, or the
/// `collide` of an install from before the rename).
const WRAPPERS: [&str; 13] =
    ["sh", "bash", "zsh", "dash", "fish", "ksh", "env", "timeout", "nice", "nohup", "cork", "collide", "collide-hook"];

/// The harness process this hook runs under (Claude Code, Codex, Cursor),
/// as (pid, program name): the nearest ancestor that is not a wrapper. A
/// listener lives only as long as it does. None when the ancestry cannot be
/// read or runs out at init.
pub(crate) fn harness_owner(from: u32) -> Option<(u32, String)> {
    owner_from(from, ancestor)
}

fn owner_from(from: u32, parent_of: impl Fn(u32) -> Option<(u32, String)>) -> Option<(u32, String)> {
    let mut pid = from;
    for _ in 0..12 {
        let (up, name) = parent_of(pid)?;
        if up <= 1 {
            return None;
        }
        let base = name.rsplit('/').next().unwrap_or(&name).trim_start_matches('-').to_ascii_lowercase();
        if !WRAPPERS.contains(&base.as_str()) {
            return Some((up, name));
        }
        pid = up;
    }
    None
}

/// Whether the harness process is still the one that started the listener:
/// alive, and still running the same program (a pid the system handed to
/// another process since does not keep a listener alive).
pub(crate) fn owner_alive(owner: &(u32, String)) -> bool {
    #[cfg(unix)]
    {
        let Ok(out) = std::process::Command::new("ps").args(["-o", "comm=", "-p", &owner.0.to_string()]).output() else {
            return true; // cannot tell: never end a live session's listener on a guess
        };
        let now = String::from_utf8_lossy(&out.stdout).trim().to_string();
        !now.is_empty() && now == owner.1
    }
    #[cfg(windows)]
    {
        pid_alive(owner.0)
    }
}

fn spawn_owned(root: &Path, session: &str, env: &Env, owner: Option<(u32, String)>) {
    let socket = config::get(env, "CLAUDE_CODE_MESSAGING_SOCKET");
    let off = config::get(env, "CORK_LISTEN");
    if session.is_empty() || off == "0" {
        return;
    }
    // another agent started from inside a Claude Code session inherits its
    // socket: only Claude Code's own hooks may listen on it, and only on the
    // socket of the Claude Code process that runs THIS session
    let pid_file = listen_dir(env).join(format!("{}.pid", safe(session)));
    let inbox = !socket.is_empty()
        && crate::harness::current() == "claude"
        && !std::fs::read_to_string(&pid_file).ok().and_then(|p| p.trim().parse::<u32>().ok()).is_some_and(pid_alive)
        && owns_socket(socket, std::process::id());
    let cfg = config::config(Some(root), env);
    let protocol = crate::proto::enabled(root, &cfg, env) && !crate::proto::daemon_up(env, session);
    if !inbox && !protocol {
        return;
    }
    let Ok(me) = std::env::current_exe() else { return };
    let _ = std::fs::create_dir_all(listen_dir(env));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(listen_dir(env).join("listen.log"));
    let mut command = std::process::Command::new(me);
    command.arg("listen").arg(session).arg("--harness").arg(crate::harness::current());
    // the harness it serves: when that process is gone, so is the session
    // (a killed Codex session sends no SessionEnd and has no socket to watch)
    if let Some((pid, name)) = &owner {
        command.arg("--owner").arg(pid.to_string()).arg("--owner-name").arg(name);
    }
    if !inbox {
        command.arg("--protocol-only");
        // a socket that is not this session's own (inherited from the
        // session that started this agent, in another repo): nothing of
        // this repo may be posted into it, deltas included
        command.env_remove("CLAUDE_CODE_MESSAGING_SOCKET").env_remove("CLAUDE_CODE_MESSAGING_TOKEN");
    }
    command.current_dir(root).stdin(std::process::Stdio::null());
    match log.and_then(|f| f.try_clone().map(|g| (f, g))) {
        Ok((out, err)) => {
            command.stdout(out).stderr(err);
        }
        Err(_) => {
            command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    if let Ok(child) = command.spawn() {
        if inbox {
            let _ = std::fs::write(&pid_file, child.id().to_string());
        }
    }
}

/// Whether `socket` (CLAUDE_CODE_MESSAGING_SOCKET as this process sees it)
/// belongs to the Claude Code process this hook runs under, walking up from
/// `from`. Claude Code names its socket after its own pid
/// (`/tmp/cc-socks/<pid>.sock`), and every process it starts inherits the
/// variable: an agent started from inside another session (`claude -p`, a
/// harness, a study) inherits that session's socket while its hooks run in
/// a different repo. Posting there put one repo's messages and Δ lines into
/// a session working in another repo. Only the nearest Claude Code ancestor
/// may own it; a socket named some other way is trusted as before (Windows
/// pipes), and an ancestry that cannot be read is not.
pub fn owns_socket(socket: &str, from: u32) -> bool {
    let stem = Path::new(socket).file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let Ok(owner) = stem.parse::<u32>() else { return cfg!(windows) || !socket.ends_with(".sock") };
    // pid 1 (init, launchd) is never Claude Code: on a Linux runner it is
    // the test's own grandparent, and it read as the owner
    if owner <= 1 {
        return false;
    }
    owner_in_ancestry(owner, from, |pid| ancestor(pid))
}

/// Walk up from `from`: true at `owner`, false at the first Claude Code
/// process that is not it (the nearest one runs this session), or when the
/// walk runs out.
fn owner_in_ancestry(owner: u32, from: u32, parent_of: impl Fn(u32) -> Option<(u32, String)>) -> bool {
    let mut pid = from;
    for _ in 0..12 {
        if pid == owner {
            return true;
        }
        let Some((up, name)) = parent_of(pid) else { return false };
        if up == owner {
            return true;
        }
        let base = name.rsplit('/').next().unwrap_or(&name).to_ascii_lowercase();
        if base.contains("claude") {
            return false;
        }
        if up <= 1 {
            return false;
        }
        pid = up;
    }
    false
}

/// A process's parent and the parent's program name.
pub(crate) fn ancestor(pid: u32) -> Option<(u32, String)> {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("ps").args(["-o", "ppid=", "-p", &pid.to_string()]).output().ok()?;
        let up: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        let named = std::process::Command::new("ps").args(["-o", "comm=", "-p", &up.to_string()]).output().ok()?;
        Some((up, String::from_utf8_lossy(&named.stdout).trim().to_string()))
    }
    #[cfg(windows)]
    {
        let _ = pid;
        None
    }
}

fn safe(session: &str) -> String {
    session.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

fn stamp_line(line: &str) {
    eprintln!("{:.0} {line}", now_s());
}

/// `cork listen <session>`, run detached by the session's start hook.
/// With `--print` (Hermes's plugin runs it) each batch is a JSON line on
/// stdout for the agent's own process to inject, and the listener ends when
/// that process does.
pub fn run(args: &[String], env: &Env) -> i32 {
    let session = args.iter().find(|a| !a.starts_with("--")).cloned().unwrap_or_default();
    let print = args.iter().any(|a| a == "--print");
    let protocol_only = args.iter().any(|a| a == "--protocol-only");
    let socket = config::get(env, "CLAUDE_CODE_MESSAGING_SOCKET").to_string();
    let token = config::get(env, "CLAUDE_CODE_MESSAGING_TOKEN").to_string();
    if session.is_empty() {
        eprintln!("usage: cork listen <session> [--print] (run by the session's start hook)");
        return 2;
    }
    let parent = parent_pid();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|at| args.get(at + 1)).cloned();
    let owner = flag("--owner").and_then(|p| p.parse::<u32>().ok()).zip(flag("--owner-name"));
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(root) = config::find_repo_root(&[Some(cwd)]) else { return 0 };
    let cfg = config::config(Some(&root), env);
    // the local server (the free version) runs a session's protocol daemon
    // only, and only when the protocol is switched on there
    if !cfg.usable() || (cfg.machine_local && !(protocol_only && crate::proto::enabled(&root, &cfg, env))) {
        return 0;
    }
    // the session's protocol connection: on a thread of its own beside the
    // inbox, or alone when there is no inbox to wait on
    let claude_socket = if crate::harness::current() == "claude" { socket.clone() } else { String::new() };
    let daemon = crate::protod::Options {
        root: root.clone(),
        session: session.clone(),
        agent: crate::harness::current().to_string(),
        socket: claude_socket.clone(),
        socket_token: token.clone(),
        owner: owner.clone(),
    };
    let inbox = !protocol_only && (print || !claude_socket.is_empty());
    if !inbox {
        crate::protod::run(&daemon, env);
        return 0;
    }
    if crate::proto::enabled(&root, &cfg, env) {
        let env = env.clone();
        std::thread::spawn(move || crate::protod::run(&daemon, &env));
    }
    let pid_file = listen_dir(env).join(format!("{}.pid", safe(&session)));
    let _ = std::fs::create_dir_all(listen_dir(env));
    let _ = std::fs::write(&pid_file, std::process::id().to_string());
    let collide = crate::machine::message_command(env).trim_end_matches(" message").to_string();
    let program = std::env::current_exe().ok();
    let built = program.as_ref().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    let born = Instant::now();
    let mut backoff = Duration::from_secs(5);
    stamp_line(&format!("listening for session {session} in {}", cfg.repo_id));
    loop {
        let open = if print { parent_pid() == parent && parent != Some(1) } else { session_open(&socket) };
        if !open || born.elapsed() > MAX_LIFE || owner.as_ref().is_some_and(|o| !owner_alive(o)) {
            break;
        }
        // an update replaced the program: the new one takes over, serving
        // the same harness process (this one's parent is long gone)
        let now_built = program.as_ref().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        if built.is_some() && now_built.is_some() && now_built != built {
            let _ = std::fs::remove_file(&pid_file);
            spawn_owned(&root, &session, env, owner.clone());
            return 0;
        }
        let payload = json!({"repo_id": cfg.repo_id, "session": session, "role": "session", "wait_s": WAIT_S});
        let answer = crate::http::post(
            &cfg.server, "/inbox/wait", &cfg.token, &crate::report::user_agent(), &payload,
            Duration::from_secs(WAIT_S + 15),
        );
        let Ok(answer) = answer else {
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(300));
            continue;
        };
        if answer.get("ok").and_then(Value::as_bool) != Some(true) {
            // refused (signed out, not a member, a free machine): look again later
            std::thread::sleep(Duration::from_secs(300));
            continue;
        }
        backoff = Duration::from_secs(5);
        let mut messages: Vec<Value> = answer.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
        if !messages.is_empty() {
            // a burst (three quick lines from one person) goes in as one turn, not three
            std::thread::sleep(Duration::from_secs(3));
            let more = json!({"repo_id": cfg.repo_id, "session": session, "role": "session", "wait_s": 0});
            if let Ok(again) = crate::http::post(&cfg.server, "/inbox/wait", &cfg.token, &crate::report::user_agent(), &more, Duration::from_secs(10)) {
                messages.extend(again.get("messages").and_then(Value::as_array).cloned().unwrap_or_default());
            }
            let said = session_text(&messages, &collide);
            // the person hears of a handoff taken, or a teammate's note to a
            // session sitting idle, when they asked to (pings.rs)
            let idle = crate::pings::idle(&session, env);
            for message in &messages {
                if message.get("handoff").and_then(Value::as_bool) == Some(true) {
                    crate::pings::took_handoff(env, &root, &session, &text(message, "from"), &text(message, "message"));
                } else if idle && message.get("within_session").and_then(Value::as_bool) != Some(true) {
                    let first: String = text(message, "message").chars().take(120).collect();
                    crate::pings::ping(env, crate::pings::Kind::Message, &root, &format!("Cork: a message from {}", sender(message)), &first);
                }
            }
            let delivered = if print { print_line(&said) } else { into_session(&session, &socket, &token, &messages, &collide, env) };
            let ids: Vec<String> = messages.iter().map(|m| text(m, "id")).collect();
            stamp_line(&format!("messages {}: {}", ids.join(","), if delivered { "delivered" } else { "session gone" }));
        }
    }
    let _ = std::fs::remove_file(&pid_file);
    0
}

/// The messages the listener took, into its session. A session that asks
/// before acting takes a message from its own listener as it comes. One
/// that runs without asking holds a message unless its sender attests the
/// same permission class (peer.rs), and a listener is not its own session's
/// child once its hook has exited: so each message from a Claude Code
/// session on this machine whose class matches goes in as that session's,
/// attested, and the rest wait for the session's next hook output, which
/// needs no one's approval (and is never lost).
fn into_session(session: &str, socket: &str, token: &str, messages: &[Value], collide: &str, env: &Env) -> bool {
    let mine = crate::peer::mode_class(&crate::peer::mode_of(session, env));
    if mine == Some("prompting") {
        return post_to_session(socket, token, &session_text(messages, collide));
    }
    let peers = peers(env);
    let own = peers.iter().find(|p| p.session == session);
    let mut by_sender: std::collections::BTreeMap<String, Vec<Value>> = std::collections::BTreeMap::new();
    let mut held: Vec<Value> = Vec::new();
    for message in messages {
        let stamped = text(message, "sender_session");
        let sender = if stamped.is_empty() { text(message, "from_session") } else { stamped };
        let sender = sender.split('/').next().unwrap_or("").to_string();
        let same = mine.is_some() && !sender.is_empty() && crate::peer::mode_class(&crate::peer::mode_of(&sender, env)) == mine;
        if same && own.is_some() && peers.iter().any(|p| p.session == sender) {
            by_sender.entry(sender).or_default().push(message.clone());
        } else {
            held.push(message.clone());
        }
    }
    let mut posted = true;
    for (sender, said) in by_sender {
        let (Some(own), Some(from)) = (own, peers.iter().find(|p| p.session == sender)) else { continue };
        let text = session_text(&said, collide);
        let address = crate::peer::address(&from.socket);
        let name = crate::transcript::session_name(&sender, false, env);
        match crate::peer::envelope(&address, &name, mine.unwrap_or(""), &text) {
            Some(content) => posted &= post_as_peer(socket, &own.token, &crate::peer::frame(&address, &content)),
            None => held.extend(said),
        }
    }
    if !held.is_empty() {
        crate::peer::hold(session, &session_text(&held, collide), env);
    }
    posted
}

/// The process that started this one, while it lives (an orphan is
/// re-parented, to launchd or init).
fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("ps").args(["-o", "ppid=", "-p", &std::process::id().to_string()]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(windows)]
    {
        None
    }
}

fn print_line(said: &str) -> bool {
    use std::io::Write;
    let mut out = std::io::stdout();
    writeln!(out, "{}", json!({"text": said})).and_then(|_| out.flush()).is_ok()
}

/// For a harness reached only as its agent stops (Cursor's `stop` hook):
/// the messages addressed to this person waiting now, taken, as the text to
/// continue with. `None` when there are none.
pub fn followup(hook_input: &Value, env: &Env) -> Option<String> {
    let session = text(hook_input, "session_id");
    let cwd = PathBuf::from(text(hook_input, "cwd"));
    let root = config::find_repo_root(&[(!cwd.as_os_str().is_empty()).then_some(cwd)])?;
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() || cfg.machine_local || session.is_empty() {
        return None;
    }
    let payload = json!({"repo_id": cfg.repo_id, "session": session, "role": "session", "wait_s": 0});
    let answer = crate::http::post(&cfg.server, "/inbox/wait", &cfg.token, &crate::report::user_agent(), &payload,
        Duration::from_secs(4)).ok()?;
    let messages: Vec<Value> = answer.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    if messages.is_empty() {
        return None;
    }
    let collide = crate::machine::message_command(env).trim_end_matches(" message").to_string();
    Some(session_text(&messages, &collide))
}

// ------------------------------------------------------------ ack

/// `cork ack <id>...`: the messages are handled; they stop coming back.
pub fn ack(args: &[String], env: &Env) -> i32 {
    let ids: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if ids.is_empty() {
        println!("usage: cork ack <message id>...");
        return 2;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = config::find_repo_root(&[Some(cwd)]);
    let cfg = config::config(root.as_deref(), env);
    if !cfg.usable() {
        println!("Cork: run this inside a repository Cork covers.");
        return 1;
    }
    let (server, token, repo_id) = (cfg.server, cfg.token, cfg.repo_id);
    let payload = json!({"repo_id": repo_id, "ack": ids});
    match crate::http::post(&server, "/inbox", &token, &crate::report::user_agent(), &payload, Duration::from_secs(8)) {
        Ok(answer) if answer.get("ok").and_then(Value::as_bool) == Some(true) => {
            println!("Cork: done; {} message(s) still waiting.", answer.get("messages").and_then(Value::as_array).map(Vec::len).unwrap_or(0));
            0
        }
        _ => {
            println!("Cork: could not reach Cork; the message stays in the inbox.");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The leak: an agent started from inside another Claude Code session
    /// inherits that session's inbox socket, and its hooks posted its own
    /// repo's messages and Δ lines into it. Only the nearest Claude Code
    /// ancestor owns the socket it names.
    #[test]
    fn only_the_claude_code_process_running_this_session_owns_its_socket() {
        // hook 100 <- sh 90 <- claude (the child agent) 80 <- zsh 70 <- claude (the user's session) 60
        let tree = |pid: u32| -> Option<(u32, String)> {
            match pid {
                100 => Some((90, "/bin/sh".into())),
                90 => Some((80, "/Users/me/.local/bin/claude".into())),
                80 => Some((70, "-zsh".into())),
                70 => Some((60, "claude".into())),
                60 => Some((1, "launchd".into())),
                _ => None,
            }
        };
        assert!(owner_in_ancestry(80, 100, tree), "the child's own socket");
        assert!(!owner_in_ancestry(60, 100, tree), "inherited from the session that started the child");
        assert!(!owner_in_ancestry(4242, 100, tree), "nobody's");
        // the real process: its own pid is its own; pid 1 is not
        assert!(owns_socket(&format!("/tmp/cc-socks/{}.sock", std::process::id()), std::process::id()));
        assert!(!owns_socket("/tmp/cc-socks/1.sock", std::process::id()));
    }

    /// Study 9: `cork listen` outlived the Codex sessions it served. The
    /// listener now knows the harness process it serves (the nearest
    /// ancestor of the hook that is not a shell or Cork), and ends when
    /// that process is gone or another program has its pid.
    #[test]
    fn the_listener_ends_with_the_harness_process_it_serves() {
        // hook 100 <- sh 90 <- codex 80 <- zsh 70 <- launchd 1
        let tree = |pid: u32| -> Option<(u32, String)> {
            match pid {
                100 => Some((90, "/bin/sh".into())),
                90 => Some((80, "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex".into())),
                80 => Some((70, "-zsh".into())),
                70 => Some((1, "launchd".into())),
                // a detached sender: re-parented to launchd
                200 => Some((1, "launchd".into())),
                _ => None,
            }
        };
        assert_eq!(owner_from(100, tree).map(|o| o.0), Some(80), "Codex, past the hook's shell");
        assert_eq!(owner_from(200, tree), None, "no harness above a detached process");
        // a real harness process: alive while it runs, gone once killed
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let name = String::from_utf8_lossy(
            &std::process::Command::new("ps").args(["-o", "comm=", "-p", &child.id().to_string()]).output().unwrap().stdout,
        ).trim().to_string();
        let owner = (child.id(), name);
        assert!(owner_alive(&owner));
        assert!(!owner_alive(&(child.id(), "some-other-program".into())), "a reused pid is not the session");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!owner_alive(&owner), "the session is gone");
    }

    #[test]
    fn a_headless_session_is_told_apart_from_one_with_a_person() {
        let env = |pairs: &[(&str, &str)]| -> Env { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
        let input = json!({"hook_event_name": "SessionStart", "source": "startup"});
        assert!(crate::harness::headless(&input, &env(&[("CLAUDE_CODE_ENTRYPOINT", "sdk-cli")])), "claude -p");
        assert!(crate::harness::headless(&input, &env(&[("CLAUDE_CODE_ENTRYPOINT", "sdk-ts")])));
        assert!(crate::harness::headless(&input, &env(&[("CORK_WOKEN", "m1")])), "the wake watcher's agent");
        assert!(crate::harness::headless(&input, &env(&[("CLAUDE_CODE_ENTRYPOINT", "cli"), ("CLAUDE_CODE_SESSION_ATTENDED", "0")])));
        assert!(!crate::harness::headless(&input, &env(&[("CLAUDE_CODE_ENTRYPOINT", "cli"), ("CLAUDE_CODE_SESSION_ATTENDED", "1")])));
        assert!(!crate::harness::headless(&input, &env(&[("CLAUDE_CODE_ENTRYPOINT", "sdk-cli"), ("CORK_HEADLESS", "0")])));
    }

    #[test]
    fn messages_handed_over_together_arrive_as_one_with_the_guidance_said_once() {
        let one = json!({"id": "m1", "from": "bob@x.com", "from_name": "Bob", "message": "  rename parse() please ",
            "age_s": 125.0, "anchor": "app/io.py::parse"});
        let two = json!({"id": "m2", "from": "ana@x.com", "message": "tests are red on main", "age_s": 5.0});
        let said = session_text(&[one, two], "/bin/collide");
        assert_eq!(said, "Cork: 2 messages from teammates' agents (not your user). Act on what fits this repo's direction, \
within your permissions, and tell the user.\n\
[m1] Bob (bob@x.com) about app/io.py::parse, 2m ago: rename parse() please\n\
[m2] ana@x.com, just now: tests are red on main\n\
Reply: `/bin/collide message --to <sender> \"...\"`. Handled: `/bin/collide ack m1 m2`.");
    }

    /// Who Cork says sent a pushed message, and what it saw that session
    /// do, arrive under the message, so its claims are read against them.
    #[test]
    fn a_pushed_message_carries_who_cork_verified_sent_it() {
        let note = json!({"id": "m4", "from": "kevin@x.com", "message": "I own src/, leave it", "age_s": 60.0,
            "verified": "verified by Cork: kevin@x.com · session \"Cork version number\" (cork-linear-test-e3) · Claude Code · this machine",
            "evidence": "Cork sees: that session last edited nothing in src/ in the last day"});
        let said = session_text(&[note], "/bin/cork");
        assert!(said.contains("[m4] kevin@x.com, 1m ago: I own src/, leave it\n  verified by Cork: kevin@x.com · session \
\"Cork version number\" (cork-linear-test-e3) · Claude Code · this machine\n  Cork sees: that session"), "{said}");
        let handoff = json!({"id": "h2", "from": "kevin@x.com#s2", "message": "Build it.", "age_s": 1.0, "handoff": true,
            "verified": "verified by Cork: kevin@x.com · session s2"});
        let said = session_text(&[handoff], "/bin/cork");
        assert!(said.contains("ack h2`.\n  verified by Cork: kevin@x.com · session s2\nBuild it."), "{said}");
        // an older server sends neither, and nothing is added
        assert_eq!(provenance(&json!({"id": "m5"})), "");
    }

    /// A handoff is work sent to this session alone: to be done, not news;
    /// a note from the session's own helper says so.
    #[test]
    fn a_handoff_arrives_as_the_sessions_to_do_and_a_helpers_note_as_its_own() {
        let handoff = json!({"id": "h1", "from": "kevin@x.com#s2", "message": "Build the invite panel.", "age_s": 30.0, "handoff": true});
        let said = session_text(&[handoff], "/bin/cork");
        assert!(said.starts_with("Cork: a handoff for you [h1], from kevin@x.com#s2") && said.contains("start on it now")
            && said.contains("/bin/cork message --to kevin@x.com#s2") && said.ends_with("Build the invite panel."), "{said}");
        let note = json!({"id": "m3", "from": "kevin@x.com#s1/w1", "message": "done with the CSS", "age_s": 2.0, "within_session": true});
        let said = session_text(&[note], "/bin/cork");
        assert!(said.starts_with("Cork: A message from this session's own helpers (not your user)"), "{said}");
    }

    #[cfg(unix)]
    #[test]
    fn a_message_is_written_to_the_session_socket_as_an_auth_line_and_a_user_line() {
        use std::io::Read;
        let dir = std::env::temp_dir().join(format!("collide-listen-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("s.sock");
        let _ = std::fs::remove_file(&path);
        let server = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let socket = path.to_string_lossy().to_string();
        let writer = std::thread::spawn(move || post_to_session(&socket, "tok", "hello"));
        let (mut stream, _) = server.accept().unwrap();
        let mut got = String::new();
        stream.read_to_string(&mut got).unwrap();
        assert!(writer.join().unwrap());
        let lines: Vec<Value> = got.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0], json!({"type": "auth", "token": "tok"}));
        assert_eq!(lines[1], json!({"type": "user", "message": {"role": "user", "content": "hello"}}));
        assert!(!post_to_session(&dir.join("gone.sock").to_string_lossy(), "", "x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The field report (v104): a session in bypass permissions held every
    /// message Cork's listener posted into it, "from an unidentified session
    /// ... the sender did not attest its permission mode". A message from a
    /// Claude Code session on this machine in the same class now goes in
    /// as that session's, attested; one whose sender cannot attest (a
    /// teammate elsewhere) waits for the session's next hook output.
    #[cfg(unix)]
    #[test]
    fn a_bypass_session_takes_attested_messages_and_the_rest_wait_for_its_hooks() {
        use std::io::Read;
        let dir = std::env::temp_dir().join(format!("cork-into-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        env.insert("CLAUDE_CONFIG_DIR".into(), dir.to_string_lossy().into_owned());
        let register = |pid: &str, session: &str, name: &str, socket: &str| {
            let record = json!({"pid": pid, "sessionId": session, "name": name, "messagingSocketPath": socket, "peerProtocol": 1});
            std::fs::write(sessions.join(format!("{pid}.json")), record.to_string()).unwrap();
            let key = format!("{pid}.{}.key", crate::report::sha256_hex(socket.as_bytes()));
            std::fs::write(sessions.join(key), json!({"peerToken": format!("peer-{pid}")}).to_string()).unwrap();
        };
        let mine = dir.join("r.sock");
        let theirs = dir.join("s.sock");
        std::fs::write(&theirs, "").unwrap();
        let server = std::os::unix::net::UnixListener::bind(&mine).unwrap();
        let (mine, theirs) = (mine.to_string_lossy().into_owned(), theirs.to_string_lossy().into_owned());
        register("100", "rcp", "repo-rc", &mine);
        register("200", "snd", "repo-35", &theirs);
        crate::peer::note_mode("rcp", "bypassPermissions", &env);
        crate::peer::note_mode("snd", "bypassPermissions", &env);
        let messages = vec![
            json!({"id": "m1", "from": "kevin@x.com", "sender_session": "snd", "message": "the API is up", "age_s": 1.0}),
            json!({"id": "m2", "from": "ana@x.com", "sender_session": "far", "message": "main is red", "age_s": 1.0}),
        ];
        let (socket, env2) = (mine.clone(), env.clone());
        let writer = std::thread::spawn(move || into_session("rcp", &socket, "child", &messages, "/bin/cork", &env2));
        let (mut stream, _) = server.accept().unwrap();
        let mut got = String::new();
        stream.read_to_string(&mut got).unwrap();
        assert!(writer.join().unwrap());
        let lines: Vec<Value> = got.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0], json!({"type": "auth", "token": "peer-100"}), "as a peer, with the recipient's peer token");
        let content = lines[1]["message"]["content"].as_str().unwrap();
        let from = crate::peer::address(&theirs);
        assert!(content.starts_with(&format!("<cross-session-message from=\"{from}\" from-name=\"repo-35\" from-mode=\"bypass\">\n")), "{content}");
        assert!(content.contains("[m1]") && !content.contains("[m2]"), "{content}");
        assert_eq!(lines[1]["from"], json!(from));
        // the teammate's waits for the next hook, whole
        let held = crate::peer::take_held("rcp", &env);
        assert!(held.contains("[m2]") && held.contains("main is red"), "{held}");
        assert_eq!(crate::peer::take_held("rcp", &env), "", "taken once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Another Claude Code session on this machine is reached on the socket
    /// its registry names, with the peer token kept beside it; one that
    /// speaks another peer protocol, or left no token, is not.
    #[cfg(unix)]
    #[test]
    fn a_peer_is_found_by_its_registry_entry_and_its_key_file() {
        let dir = std::env::temp_dir().join(format!("cork-peers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let mut env = Env::new();
        env.insert("CLAUDE_CONFIG_DIR".into(), dir.to_string_lossy().into_owned());
        let socket = |pid: &str| {
            let path = dir.join(format!("{pid}.sock"));
            std::fs::write(&path, "").unwrap();
            path.to_string_lossy().into_owned()
        };
        let entry = |pid: &str, session: &str, socket: &str, protocol: i64| {
            let record = json!({"pid": pid, "sessionId": session, "messagingSocketPath": socket, "peerProtocol": protocol});
            std::fs::write(sessions.join(format!("{pid}.json")), record.to_string()).unwrap();
        };
        let key = |pid: &str, socket: &str, token: &str| {
            let name = format!("{pid}.{}.key", crate::report::sha256_hex(socket.as_bytes()));
            std::fs::write(sessions.join(name), json!({"peerToken": token, "pidDomain": "darwin"}).to_string()).unwrap();
        };
        let (a, b, c) = (socket("4572"), socket("5038"), socket("7000"));
        entry("4572", "s-e3", &a, 1);
        key("4572", &a, "tok-e3");
        entry("5038", "s-0e", &b, 1); // no key file
        entry("7000", "s-new", &c, 2);
        key("7000", &c, "tok-new");
        assert_eq!(peers(&env), vec![Peer { session: "s-e3".into(), socket: a, token: "tok-e3".into() }]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
