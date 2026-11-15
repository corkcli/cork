//! Cork installed once per machine, for every repo on it.
//!
//! `cork install` (what https://corkcli.com/install.sh runs) puts the
//! one `cork` program (these hooks and the local server) in
//! `~/.cork/bin`, registers the hooks at the USER level of each agent
//! tool (Claude Code's `~/.claude/settings.json`), and writes
//! `~/.cork/machine.json`. From then on every repo the agents open is
//! covered, with no per-repo setup:
//!
//! - a repo still carrying the old per-repo setup (hook scripts or a
//!   binary under `.collide/`, wired from its own `.claude/settings.json`)
//!   keeps running those until it is migrated: the machine's hooks stand
//!   down there, so no event is handled twice. `cork install` run at the
//!   repo's root (what `setup` hands out) and `cork migrate` retire that
//!   setup and stage the removal (see `retire_repo_setup`);
//! - any other repo is named by its git origin and served by the machine's
//!   mode: "local" (no account: the local server on 127.0.0.1, nothing
//!   leaves the machine) or "cloud" (signed in, on any plan: Cork's servers,
//!   with the credential `login` saved).
//!
//! No account is "local": the hooks never call out. Signing in joins the
//! workspace on Free too; Free is metered in agent hours, not machines.
//!
//! A machine installed before the product was renamed (Collide to Cork)
//! keeps its `~/.collide` folder and its `~/.collide/bin/collide`, which its
//! agent tools' hook entries run; a `cork` is placed beside that `collide`
//! (the same program) so `cork ...` works there too. Nothing is moved.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::{self, Env};

pub const DEFAULT_PORT: u16 = 47_600;
/// The free version's one program: these hooks and the local server.
pub const PROGRAM: &str = "cork";
/// Its name before the rename: what an install from then still runs, and
/// what its hook entries name (`~/.collide/bin/collide`).
pub const OLD_PROGRAM: &str = "collide";

/// Whether a program's file stem is the one program, under either name.
pub fn is_program_name(stem: &str) -> bool {
    stem == PROGRAM || stem == OLD_PROGRAM
}

static SERVES_LOCAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called by the `cork` program (cork-rs with the hooks linked in):
/// this binary can also run the local server, as `cork local`.
pub fn set_serves_local() {
    SERVES_LOCAL.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn serves_local() -> bool {
    SERVES_LOCAL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether this is the free version's `cork` program.
pub fn serves_local_pub() -> bool {
    serves_local()
}

/// `~/.cork/bin/cork`: where the free version lives on the machine
/// (`~/.collide/bin/collide` on an install from before the rename, whose
/// hook entries run that path).
pub fn program_path(env: &Env) -> PathBuf {
    bin_dir(env).join(exe(if config::legacy_layout(env) { OLD_PROGRAM } else { PROGRAM }))
}

/// The `cork` an agent or a person runs: `~/.cork/bin/cork`, or the one
/// placed beside an old install's `collide` (see [`place_cork_beside`]).
pub fn cork_path(env: &Env) -> PathBuf {
    bin_dir(env).join(exe(PROGRAM))
}

/// One program under both names. On macOS and Linux `collide` is a link to
/// `cork` in the same folder ([`move_to_cork`]), so the old name's hook
/// entries, a `collide` typed at a terminal and the `cork` an agent runs are
/// one file, and one update replaces it. Where links cannot be made (Windows,
/// which keeps its `~/.collide`), an install from before the rename gets a
/// `cork` copied beside its `collide`, as the install copies itself; a `cork`
/// already the same size is left alone. Called by the install, by the first
/// session a newer program runs (`refresh_settings`) and after a self-update.
pub fn place_cork_beside(env: &Env) {
    if !config::legacy_layout(env) {
        #[cfg(unix)]
        {
            let bin = bin_dir(env);
            let _ = link_old_name(&bin);
            // a copy an installer renamed aside to replace a running one:
            // only Windows needs that, a running program here keeps its file
            for name in [PROGRAM, OLD_PROGRAM] {
                let _ = std::fs::remove_file(bin.join(format!("{name}.old")));
            }
        }
        return;
    }
    let (from, to) = (program_path(env), cork_path(env));
    if !from.is_file() || from == to {
        return;
    }
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).ok();
    if size(&from) == size(&to) {
        return;
    }
    let _ = copy_executable(&from, &to);
}

/// What the machine-level hook commands set, so they can stand down where
/// a repo runs Cork's hooks itself (`CORK_MACHINE`, or the `COLLIDE_MACHINE`
/// an install from before the rename sets; `config::get` reads either).
pub const MACHINE_FLAG: &str = "CORK_MACHINE";

/// Windows: a program this one starts inherits every inheritable handle it
/// holds, its own stdout pipe included. Something started to outlive the
/// hook (the local server, an index, the tests of a change, an update) then
/// keeps that pipe open, and whatever waits for the hook's output (Claude
/// Code, Codex, Cursor) waits until that program exits: a stalled hook.
/// Called once at start: the standard handles stop being inheritable. A
/// child still gets its own stdin/stdout/stderr, which Rust duplicates for
/// it explicitly; it no longer gets this process's.
#[cfg(windows)]
pub fn no_inherit_std() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> *mut core::ffi::c_void;
        fn SetHandleInformation(handle: *mut core::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 0x1;
    // STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE
    for which in [-10i32 as u32, -11i32 as u32, -12i32 as u32] {
        // SAFETY: GetStdHandle takes no pointers; SetHandleInformation only
        // changes a flag on the handle it is given, which may be null or
        // invalid and then fails harmlessly.
        unsafe {
            let handle = GetStdHandle(which);
            if !handle.is_null() && handle as isize != -1 {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(not(windows))]
pub fn no_inherit_std() {}

/// `~/.cork`, or `~/.collide` on an install from before the rename.
pub fn data_dir(env: &Env) -> PathBuf {
    config::data_dir(env)
}

pub fn bin_dir(env: &Env) -> PathBuf {
    data_dir(env).join("bin")
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

pub fn local_dir(env: &Env) -> PathBuf {
    let given = config::get(env, "CORK_LOCAL_DIR");
    if given.is_empty() { data_dir(env).join("local") } else { PathBuf::from(given) }
}

/// `~/.cork/machine.json`: `{"mode": "local"|"cloud", "server_url",
/// "workspace", "account": {...}}`. Empty when Cork was never installed
/// for the machine.
pub fn settings(env: &Env) -> Value {
    config::load_json(&data_dir(env).join("machine.json"))
}

pub fn save_settings(env: &Env, value: &Value) -> bool {
    let path = data_dir(env).join("machine.json");
    let _ = std::fs::create_dir_all(data_dir(env));
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(value).unwrap_or_default())).is_ok()
}

pub fn installed(env: &Env) -> bool {
    settings(env).get("mode").and_then(Value::as_str).is_some()
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// A machine working with its team: the cloud's address and this machine's
/// credential for it. None on a free (local) machine.
pub fn cloud_account(env: &Env) -> Option<(String, String)> {
    let machine = settings(env);
    if text(&machine, "mode") != "cloud" {
        return None;
    }
    let account = machine.get("account")?;
    let server = config::canonical_server(&text(account, "server_url"));
    let creds = config::load_json(&config::credentials_path(env));
    let token = config::resolve_token(&creds, &server, &text(account, "workspace"));
    (!server.is_empty() && !token.is_empty()).then_some((server, token))
}

/// The local server's address and key, as its files say.
pub fn local_endpoint(env: &Env) -> (String, String) {
    let dir = local_dir(env);
    let port = std::fs::read_to_string(dir.join("port"))
        .ok()
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let token = std::fs::read_to_string(dir.join("token")).unwrap_or_default().trim().to_string();
    (format!("http://127.0.0.1:{port}"), token)
}

/// A repo's id when it carries no Cork config: its git origin
/// (`github.com/owner/name`), else `local/<folder>`.
pub fn repo_id_for(root: &Path) -> String {
    let origin = crate::git::origin_repo_id(root);
    if !origin.is_empty() {
        return origin;
    }
    let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if name.is_empty() { String::new() } else { format!("local/{name}") }
}

/// A repo a team can share: named by a hosted remote, `host.tld/owner/name`
/// (GitHub, GitLab, Bitbucket, a company's own). A folder, a git repo with
/// no remote, or one whose remote is another folder on this machine is not:
/// there is no one else to share it with.
pub fn is_hosted(repo_id: &str) -> bool {
    let parts: Vec<&str> = repo_id.split('/').collect();
    parts.len() >= 3
        && parts[0].contains('.')
        && parts.iter().all(|p| !p.is_empty() && !p.chars().any(char::is_whitespace))
}

/// The config for a repo with none of its own, from the machine's mode.
/// `None` when Cork was not installed for the machine. On a signed-in
/// machine, only a repo with a hosted remote goes to the workspace; any
/// other git repo stays on this machine, as with no account.
pub fn repo_config(root: &Path, env: &Env) -> Option<config::Config> {
    let machine = settings(env);
    let mode = text(&machine, "mode");
    if mode.is_empty() {
        return None;
    }
    let repo_id = repo_id_for(root);
    let cloud = mode == "cloud" && is_hosted(&repo_id);
    let machine_local = !cloud;
    let (server, token) = if cloud {
        let server = config::canonical_server(&text(&machine, "server_url"));
        let creds = config::load_json(&config::credentials_path(env));
        let token = config::resolve_token(&creds, &server, &text(&machine, "workspace"));
        (server, token)
    } else {
        local_endpoint(env)
    };
    Some(config::Config { server, token, repo_id, verify: String::new(), local_parse: true, machine_local })
}

/// True when this repo runs Cork's hooks from its own settings, for the
/// agent tool running now, and those hooks can work: the machine's hooks
/// then stay silent, or every event would be handled twice.
///
/// Only the running tool's own file counts (Claude Code's said nothing about
/// a Codex session), and only when the repo's hook has something to run and
/// a server to report to. A repo set up the old way and since half removed
/// (its `.collide/` gone, its settings left) silenced the machine's hooks
/// while its own did nothing: no agent in it reported at all.
pub fn repo_runs_its_own(root: &Path) -> bool {
    let file = match crate::harness::current() {
        "codex" => root.join(".codex").join("hooks.json"),
        "cursor" => root.join(".cursor").join("hooks.json"),
        _ => root.join(".claude").join("settings.json"),
    };
    let settings = std::fs::read_to_string(file).unwrap_or_default();
    let mentions = ["collide-hook", ".collide/report_hook.py", "collide-report-hook"].iter().any(|m| settings.contains(m));
    let collide = root.join(".collide");
    let runnable = collide.join("bin").join("collide-hook").exists()
        || collide.join("bin").join("collide-hook.exe").exists()
        || collide.join("report_hook.py").exists();
    let configured = config::load_json(&collide.join("config.json"))
        .get("server_url").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    mentions && runnable && configured
}

/// The local server's own id for a data directory: the same hash it
/// reports on /health (see cork-rs `local::instance_id`).
fn instance_id_of(dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(dir.to_string_lossy().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The ids this account's local server may answer with: its folder's, and,
/// on a machine that moved to `~/.cork`, the `~/.collide/local` a server
/// started before the move still calls it (the same folder, through the
/// link). Without the second, the hooks would start a second server on the
/// same data beside the one already running.
fn instance_ids(env: &Env) -> Vec<String> {
    let dir = local_dir(env);
    let home = PathBuf::from(crate::check::home(env));
    let mut ids = vec![instance_id_of(&dir)];
    if dir == home.join(".cork").join("local") {
        ids.push(instance_id_of(&home.join(".collide").join("local")));
    }
    ids
}

/// A local Cork answers at `base`, and it is this account's (another
/// person on the same computer runs their own, on another port).
fn healthy(base: &str, env: &Env) -> bool {
    crate::http::audit_raw("GET", &format!("{base}/health"), "");
    let id = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(400))
        .build()
        .get(&format!("{base}/health"))
        .call()
        .ok()
        .and_then(|r| r.into_json::<Value>().ok())
        .map(|v| text(&v, "local_id"))
        .unwrap_or_default();
    !id.is_empty() && instance_ids(env).contains(&id)
}

/// Make sure the local server is up, starting it when it is not. Waits at
/// most `wait` for it to answer; returns whether it does.
pub fn ensure_local(env: &Env, wait: Duration) -> bool {
    let (base, _) = local_endpoint(env);
    if healthy(&base, env) {
        return true;
    }
    let server = {
        let installed = program_path(env);
        if installed.exists() {
            installed
        } else if serves_local() {
            match std::env::current_exe() {
                Ok(me) => me,
                Err(_) => return false,
            }
        } else {
            return false;
        }
    };
    let dir = local_dir(env);
    let _ = std::fs::create_dir_all(&dir);
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("server.log"));
    let mut command = Command::new(&server);
    command.arg("local").stdin(Stdio::null());
    match log.and_then(|f| f.try_clone().map(|g| (f, g))) {
        Ok((out, err)) => {
            command.stdout(out).stderr(err);
        }
        Err(_) => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    // outlives the hook, and the agent session it serves
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
    if command.spawn().is_err() {
        return false;
    }
    let started = Instant::now();
    while started.elapsed() < wait {
        std::thread::sleep(Duration::from_millis(50));
        let (base, _) = local_endpoint(env);
        if healthy(&base, env) {
            return true;
        }
    }
    false
}

/// Called before a `gate` or `report` event is handled. A machine-level
/// hook (`CORK_MACHINE=1`) in a repo that runs Cork's hooks itself
/// exits here, silent. A session or a prompt in a repo the local server
/// serves makes sure it is running first.
pub fn before_event(stdin_data: &str, env: &Env) -> Option<i32> {
    let input: Value = crate::harness::normalize(serde_json::from_str(stdin_data).unwrap_or(Value::Null));
    let cwd = {
        let given = text(&input, "cwd");
        if given.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(given) }
    };
    let project = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project.is_empty()).then(|| PathBuf::from(project));
    let root = config::find_repo_root(&[Some(cwd), project])?;
    if config::get(env, MACHINE_FLAG) == "1" && repo_runs_its_own(&root) {
        return Some(0);
    }
    let event = text(&input, "hook_event_name");
    note_agent_process(&text(&input, "session_id"), env);
    let repo_cfg = config::repo_config(&root);
    let own = repo_cfg.get("server_url").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    if own {
        return None;
    }
    let Some(cfg) = repo_config(&root, env) else { return None };
    // which repo this session is in, for the status line (statusline.rs)
    crate::statusline::note(&root, &cfg.repo_id, &text(&input, "session_id"), &event, env);
    if cfg.machine_local && matches!(event.as_str(), "SessionStart" | "UserPromptSubmit") {
        let wait = if event == "SessionStart" { Duration::from_secs(4) } else { Duration::from_millis(1500) };
        if !ensure_local(env, wait) {
            return None;
        }
    }
    // any first hook call in a repo maps it, local or cloud: a session start
    // does it itself, so every other event (the first prompt, read or edit
    // of a session that was open before Cork was installed, or before
    // the machine moved to its team) does it once, in the background
    if event != "SessionStart" && cfg.usable() {
        index_once(&root, &cfg.server, env);
    }
    None
}

/// Map the repo this process runs in, once per repo and server on this
/// machine, in the background: what a first hook call does, for callers
/// that are not a hook (the MCP bridge). A repo that runs Cork's own
/// hooks, or a machine with nothing installed, is left alone.
pub fn index_here(env: &Env) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let project = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project.is_empty()).then(|| PathBuf::from(project));
    let Some(root) = config::find_repo_root(&[Some(cwd), project]) else { return };
    let repo_cfg = config::repo_config(&root);
    if repo_cfg.get("server_url").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
        return;
    }
    let Some(cfg) = repo_config(&root, env) else { return };
    if cfg.usable() {
        index_once(&root, &cfg.server, env);
    }
}

/// Which server a repo's map was sent to, per repo: a machine that moves
/// from local to its team's servers maps the repo again there.
fn indexed_marker(root: &Path, server: &str, env: &Env) -> PathBuf {
    use sha2::{Digest, Sha256};
    let seed = format!("{}\n{}", root.to_string_lossy(), server.trim_end_matches('/'));
    let key: String = Sha256::digest(seed.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    local_dir(env).join("indexed").join(key)
}

/// A session that was already open when Cork was installed (or when the
/// machine moved to its team) missed its session start, the step that maps
/// the repo's code. Its first hook call maps it instead, in the background,
/// once per repo and server on this machine.
fn index_once(root: &Path, server: &str, env: &Env) {
    let marker = indexed_marker(root, server, env);
    if marker.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(marker.parent().unwrap_or(Path::new(".")));
    let _ = std::fs::write(&marker, root.to_string_lossy().as_bytes());
    let Ok(me) = std::env::current_exe() else { return };
    let mut command = Command::new(me);
    command.arg("index").arg(root).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// A session start maps the repo itself: the first prompt need not.
pub fn mark_indexed(root: &Path, server: &str, env: &Env) {
    let marker = indexed_marker(root, server, env);
    let _ = std::fs::create_dir_all(marker.parent().unwrap_or(Path::new(".")));
    let _ = std::fs::write(&marker, root.to_string_lossy().as_bytes());
}

// ------------------------------------------------------------------ install

pub(crate) fn claude_settings_path(env: &Env) -> PathBuf {
    PathBuf::from(crate::check::home(env)).join(".claude").join("settings.json")
}

/// The mark every machine-level entry carries (`# cork-machine`), or the
/// `# collide-machine` of an install from before the rename: either is ours.
pub(crate) const MACHINE_MARK: &str = "# cork-machine";
pub(crate) const OLD_MACHINE_MARK: &str = "# collide-machine";

/// Whether a hook entry, or a whole settings file, carries the machine's mark.
pub(crate) fn has_machine_mark(text: &str) -> bool {
    text.contains(MACHINE_MARK) || text.contains(OLD_MACHINE_MARK)
}

/// The machine-level hook commands, one per kind: every event but the gate
/// goes to `report`. They find the binary in `~/.cork/bin`, say nothing
/// and exit 0 when it is missing, and mark themselves so they can stand down
/// in a repo that runs Cork itself.
fn machine_command(sub: &str, env: &Env) -> String {
    machine_command_for(sub, "claude", env)
}

/// The same for another agent tool, which hands its events over in its own
/// dialect (`--harness codex|cursor`). An install from before the rename
/// keeps its entries byte for byte (`~/.collide/bin/collide`,
/// `CORK_MACHINE`): Codex trusts each hook by its hash, and a changed
/// entry is skipped until the person approves it again in /hooks.
fn machine_command_for(sub: &str, harness: &str, env: &Env) -> String {
    machine_command_in(sub, harness, config::legacy_layout(env))
}

/// The same, written the way an install from before the rename wrote it
/// (`legacy`) or the way it is written now.
fn machine_command_in(sub: &str, harness: &str, legacy: bool) -> String {
    let flag = if harness == "claude" { String::new() } else { format!(" --harness {harness}") };
    let (path, var, mark) = if legacy {
        ("$HOME/.collide/bin/collide", "CORK_MACHINE", OLD_MACHINE_MARK)
    } else {
        ("$HOME/.cork/bin/cork", MACHINE_FLAG, MACHINE_MARK)
    };
    let run = if sub == "gate" {
        format!("exec env {var}=1 \"$b\" gate{flag}")
    } else {
        format!("env {var}=1 \"$b\" report{flag} || true")
    };
    format!("b=\"{path}\"; [ -x \"$b\" ] || b=\"$b.exe\"; [ -x \"$b\" ] || exit 0; {run}; exit 0 {mark}")
}

/// How a tool's hooks file is merged with the machine's entries.
pub type Merge = fn(&str, &Env) -> String;

/// Claude Code's events and matchers, as the repo-level settings carry them.
const CLAUDE_EVENTS: &[(&str, Option<&str>, &str)] = &[
    ("SessionStart", None, "report"),
    ("UserPromptSubmit", None, "report"),
    // Read: a big mapped file's outline before its whole text (reads.rs)
    ("PreToolUse", Some("Edit|Write|MultiEdit|Bash|Read"), "gate"),
    ("PostToolUse", Some("Edit|Write|MultiEdit|NotebookEdit|Read|Grep|Glob|Bash"), "report"),
    ("PostToolUseFailure", Some("Edit|Write|MultiEdit|NotebookEdit|Read|Grep|Glob|Bash"), "report"),
    ("PostToolBatch", None, "report"),
    ("Stop", None, "report"),
    // a turn that ended on an API error fires this INSTEAD of Stop: a usage
    // limit ("You've hit your session limit · resets 4:20pm") arrives only here
    ("StopFailure", None, "report"),
    // a subagent's messages are in its own transcript: billed as it ends
    ("SubagentStop", None, "report"),
    ("SessionEnd", None, "report"),
];

fn is_machine_entry(entry: &Value) -> bool {
    has_machine_mark(&entry.to_string())
}

/// `~/.claude/settings.json` with Cork's machine hooks in, everything the
/// person had kept as it was, and an older copy of ours replaced.
pub fn merge_claude_settings(existing: &str, env: &Env) -> String {
    let mut current: serde_json::Map<String, Value> =
        serde_json::from_str::<Value>(existing).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let mut hooks = current.get("hooks").and_then(Value::as_object).cloned().unwrap_or_default();
    for (event, matcher, sub) in CLAUDE_EVENTS {
        let mut kept: Vec<Value> = hooks
            .get(*event)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| !is_machine_entry(entry))
            .collect();
        let mut entry = json!({"hooks": [{"type": "command", "command": machine_command(sub, env), "timeout": 30}]});
        if let Some(matcher) = matcher {
            entry["matcher"] = json!(matcher);
        }
        kept.push(entry);
        hooks.insert((*event).to_string(), Value::Array(kept));
    }
    current.insert("hooks".into(), Value::Object(hooks));
    format!("{}\n", serde_json::to_string_pretty(&Value::Object(current)).unwrap_or_default())
}

/// The same file with Cork's machine hooks taken out again.
pub fn remove_from_claude_settings(existing: &str) -> String {
    let mut current: serde_json::Map<String, Value> =
        serde_json::from_str::<Value>(existing).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    if let Some(hooks) = current.get_mut("hooks").and_then(Value::as_object_mut) {
        for entries in hooks.values_mut() {
            if let Some(list) = entries.as_array_mut() {
                list.retain(|entry| !is_machine_entry(entry));
            }
        }
        hooks.retain(|_, entries| entries.as_array().map_or(true, |l| !l.is_empty()));
    }
    format!("{}\n", serde_json::to_string_pretty(&Value::Object(current)).unwrap_or_default())
}

/// Codex's events, as its per-repo hooks carry them (`apply_patch` is its
/// file edit, `shell` its terminal).
const CODEX_EVENTS: &[(&str, Option<&str>, &str)] = &[
    ("SessionStart", None, "report"),
    ("UserPromptSubmit", None, "report"),
    ("PreToolUse", Some("^(apply_patch|Edit|Write|MultiEdit|shell)$"), "gate"),
    // Codex names its terminal `Bash` now. A separate entry, so the one above
    // stays byte-identical: Codex trusts each hook by its hash, and a changed
    // entry is skipped until the person approves it again in /hooks
    ("PreToolUse", Some("^Bash$"), "gate"),
    ("PostToolUse", Some("^(apply_patch|Edit|Write|MultiEdit|Read|Grep|Glob|Bash|shell)$"), "report"),
    ("Stop", None, "report"),
    // a turn that failed (Codex's usage limit: "try again at Nov 15th, 2026
    // 3:09 PM") sends no Stop; the session's end is where it is read, and
    // where the session stops being counted
    ("SessionEnd", None, "report"),
];

/// What Codex calls each of Cork's hooks where it asks the person to
/// trust them (its review lists a hook by its `statusMessage`, else "Hook 3").
/// An install from before the rename keeps the words it was trusted with:
/// the label is part of the entry Codex hashes.
fn codex_label(event: &str, matcher: Option<&str>, legacy: bool) -> String {
    let what = match (event, matcher) {
        ("SessionStart", _) => "brief the agent at session start",
        ("UserPromptSubmit", _) => "brief the agent on each prompt",
        ("PreToolUse", Some("^Bash$")) => "check terminal writes against teammates' work",
        ("PreToolUse", _) => "check file edits against teammates' work",
        ("PostToolUse", _) => "report what the agent read and changed",
        ("Stop", _) => "record the end of each turn",
        ("SessionEnd", _) => "record the end of the session",
        _ => "",
    };
    let name = if legacy { "Collide" } else { "Cork" };
    if what.is_empty() { name.to_string() } else { format!("{name}: {what}") }
}

/// `~/.codex/hooks.json` with Cork's machine hooks in: Codex's format is
/// Claude Code's (a `hooks` object of event -> matcher entries).
pub fn merge_codex_hooks(existing: &str, env: &Env) -> String {
    // Codex trusts each hook by its exact text: entries written before the
    // rename stay as they were once the machine moves to ~/.cork (they run
    // ~/.collide/bin/collide, which is then a link to cork) rather than be
    // skipped until the person approves them again in /hooks
    let legacy = config::legacy_layout(env) || existing.contains(OLD_MACHINE_MARK);
    let mut current: serde_json::Map<String, Value> =
        serde_json::from_str::<Value>(existing).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let mut hooks = current.get("hooks").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut cleared: Vec<&str> = Vec::new();
    for (event, matcher, sub) in CODEX_EVENTS {
        // an event's old machine entries go once, before its first new one
        let fresh = !cleared.contains(event);
        cleared.push(event);
        let mut kept: Vec<Value> = hooks.get(*event).and_then(Value::as_array).cloned().unwrap_or_default()
            .into_iter().filter(|entry| !fresh || !is_machine_entry(entry)).collect();
        let mut entry = json!({"hooks": [{"type": "command", "command": machine_command_in(sub, "codex", legacy), "timeout": 30,
                                           "statusMessage": codex_label(event, *matcher, legacy)}]});
        if let Some(matcher) = matcher {
            entry["matcher"] = json!(matcher);
        }
        kept.push(entry);
        hooks.insert((*event).to_string(), Value::Array(kept));
    }
    current.insert("hooks".into(), Value::Object(hooks));
    format!("{}\n", serde_json::to_string_pretty(&Value::Object(current)).unwrap_or_default())
}

/// Cursor's events (camelCase, one command per entry).
const CURSOR_EVENTS: &[(&str, Option<&str>, &str)] = &[
    ("sessionStart", None, "report"),
    ("beforeSubmitPrompt", None, "report"),
    ("preToolUse", Some("^(Write|Edit|MultiEdit|Delete|Shell)$"), "gate"),
    ("postToolUse", Some("^(Write|Edit|MultiEdit|Delete|Read|Glob|Grep|Shell)$"), "report"),
    ("stop", None, "report"),
    ("sessionEnd", None, "report"),
];

/// `~/.cursor/hooks.json` with Cork's machine hooks in.
pub fn merge_cursor_hooks(existing: &str, env: &Env) -> String {
    let mut current: serde_json::Map<String, Value> =
        serde_json::from_str::<Value>(existing).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    current.entry("version").or_insert(json!(1));
    let mut hooks = current.get("hooks").and_then(Value::as_object).cloned().unwrap_or_default();
    for (event, matcher, sub) in CURSOR_EVENTS {
        let mut kept: Vec<Value> = hooks.get(*event).and_then(Value::as_array).cloned().unwrap_or_default()
            .into_iter().filter(|entry| !is_machine_entry(entry)).collect();
        let mut entry = json!({"command": machine_command_for(sub, "cursor", env), "timeout": 30});
        if let Some(matcher) = matcher {
            entry["matcher"] = json!(matcher);
        }
        kept.push(entry);
        hooks.insert((*event).to_string(), Value::Array(kept));
    }
    current.insert("hooks".into(), Value::Object(hooks));
    format!("{}\n", serde_json::to_string_pretty(&Value::Object(current)).unwrap_or_default())
}

// ------------------------------------------------------------ AGENTS.md

/// The markers around the section: Cork's, and Collide's from before the
/// rename, which the server's protocol block shares (cork-rs blocks.rs).
/// Either is found and replaced; only Cork's is written.
pub const SECTION_START: &str = "<!-- cork:start -->";
pub const SECTION_END: &str = "<!-- cork:end -->";
const OLD_SECTION_START: &str = "<!-- collide:start -->";
const OLD_SECTION_END: &str = "<!-- collide:end -->";

/// Whether a file already carries the section, under either marker.
pub fn has_agents_section(text: &str) -> bool {
    text.contains(SECTION_START) || text.contains(OLD_SECTION_START)
}

/// The section a repo's AGENTS.md carries for the agents Cork connects on
/// a machine. Committed and shared, so it names no path on this machine.
pub const AGENTS_SECTION: &str = "<!-- cork:start -->
## Cork: the agents in this repo work as a team

Other agents may be editing this repo at the same time. Cork's hooks keep
you in step with them, with no calls from you:

- A **Cork briefing** arrives at session start and with prompts: the
  signatures, callers and recent changes of the code you are about to touch.
  Build on it instead of re-reading files.
- The **gate** checks every write. When it holds one, it says whose live
  change it protects: adapt to it, then retry.
- Lines starting with **Δ** in a tool result are other agents' changes since
  your last step: adapt to them, never undo them.
- A **lint** finding means another agent renamed, removed or re-signatured
  something you use: fix your side.
- Commit with `cork commit --mine -m \"...\"`: it commits only the lines
  you wrote, never another agent's in the same file.
- `git push` lands through Cork: it brings in the others' commits and
  pushes once, so a push needs no pull or retry from you.
- When asked to tell the others something, run
  `cork message --about path.py::symbol \"...\"`: it reaches the agents
  working on that code (`--to agent-xxxx` for one agent). Do not announce
  your plan. Messages for you arrive in your tool results.
- The `cork` MCP tools answer what a briefing did not: `get_symbol`,
  `blast_radius` before changing something shared, `recap` for what changed
  and why, `remember` for a decision about specific code.
<!-- cork:end -->";

/// For a repo Cork covers through this machine's install: when its
/// AGENTS.md has no Cork section, the ask to add one. Told to one session
/// per repo, once, so agents starting together do not all edit the file
/// (and only to a session with a person at it; see `harness::headless`).
pub fn agents_md_ask(root: &Path, env: &Env) -> Option<String> {
    let own = config::repo_config(&root);
    if own.get("server_url").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
        return None; // a repo still on the old per-repo setup has the section from it
    }
    let path = root.join("AGENTS.md");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if has_agents_section(&current) {
        return None;
    }
    use sha2::{Digest, Sha256};
    // by repo, not folder: twelve clones of one repo are one ask
    let key: String = Sha256::digest(repo_id_for(root).as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let marker = local_dir(env).join("agents-md").join(key);
    // asked once per repo on this machine: a person who said nothing, or
    // no, was not asked again the next day (agents carried the ask into
    // every day's sessions)
    if marker.exists() {
        return None;
    }
    let _ = std::fs::create_dir_all(marker.parent()?);
    let _ = std::fs::write(&marker, format!("{:.0}", now_s()));
    let file = if current.trim().is_empty() { "an AGENTS.md (it has none)" } else { "its AGENTS.md, after what is already there" };
    Some(format!(
        "Cork: this repo's agents have no written note on working alongside each other. When it fits, ask the user \
whether to add Cork's section to {file}; it tells every agent here (Claude Code, Codex, Cursor) how the briefings, the \
gate, messages and landing work. If they say yes, run: {}{} agents-md (it adds the section and changes nothing else), \
then commit the file.",
        home_prefix(env),
        command_path(env).display()
    ))
}

/// `cork agents-md`: add Cork's section to this repo's AGENTS.md,
/// after everything already in it (or replace an older section, Cork's or
/// Collide's from before the rename, in place). Nothing else in the file
/// changes.
pub fn agents_md(env: &Env) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = config::find_repo_root(&[Some(cwd.clone())]).unwrap_or(cwd);
    let path = root.join("AGENTS.md");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = with_agents_section(&current);
    if updated == current {
        println!("Cork: AGENTS.md already has Cork's section.");
        return 0;
    }
    if std::fs::write(&path, &updated).is_err() {
        println!("Cork: could not write {}.", path.display());
        return 1;
    }
    let _ = env;
    println!("Cork: added its section to {}. Nothing else in the file changed; commit it when you are ready.", path.display());
    0
}

/// The first section in `text` at or after `from`, under either marker:
/// where it starts and where it ends (past its end marker).
fn find_section(text: &str, from: usize) -> Option<(usize, usize)> {
    let rest = &text[from..];
    let mut best: Option<(usize, usize)> = None;
    for (open, close) in [(SECTION_START, SECTION_END), (OLD_SECTION_START, OLD_SECTION_END)] {
        let Some(a) = rest.find(open) else { continue };
        let Some(b) = rest[a..].find(close) else { continue };
        let span = (from + a, from + a + b + close.len());
        if best.map_or(true, |(start, _)| span.0 < start) {
            best = Some(span);
        }
    }
    best
}

/// The file with Cork's section: an existing one (Cork's, or Collide's from
/// before the rename) replaced in place, any further copy taken out so the
/// file never carries two, otherwise added at the end.
fn with_agents_section(current: &str) -> String {
    if let Some((a, b)) = find_section(current, 0) {
        let mut out = format!("{}{AGENTS_SECTION}", &current[..a]);
        let mut at = b;
        while let Some((c, d)) = find_section(current, at) {
            out.push_str(current[at..c].trim_end_matches([' ', '\n']));
            at = d;
        }
        out.push_str(&current[at..]);
        return out;
    }
    let body = current.trim_end();
    if body.is_empty() { format!("{AGENTS_SECTION}\n") } else { format!("{body}\n\n{AGENTS_SECTION}\n") }
}

// ------------------------------------------------------------ MCP tools

pub(crate) const CODEX_MCP_MARK: &str = "# cork-machine: Cork's MCP tools";
/// The mark an install from before the rename wrote above its table.
const OLD_CODEX_MCP_MARK: &str = "# collide-machine: Collide's MCP tools";

/// The name Cork's MCP server is registered under in each agent tool.
pub const MCP_NAME: &str = "cork";
/// The names a Cork server may already be registered under: Cork's, and
/// Collide's from before the rename (by `install`, or by the person with
/// `claude mcp add collide --url ...`). One is replaced by the other, never
/// both kept: two copies of every tool.
pub const MCP_NAMES: [&str; 2] = ["cork", "collide"];

/// How an agent tool starts Cork's MCP tools: this program, `mcp`.
pub(crate) fn mcp_entry(env: &Env) -> Value {
    let mut entry = json!({"type": "stdio", "command": program_path(env).to_string_lossy(), "args": ["mcp"]});
    let cork_home = config::get(env, "CORK_HOME");
    if !home_prefix(env).is_empty() {
        entry["env"] = json!({"CORK_HOME": cork_home});
    }
    entry
}

/// A JSON file with an `mcpServers` map (Claude Code's ~/.claude.json,
/// Cursor's ~/.cursor/mcp.json) with Cork's entry set, or removed when
/// `entry` is None. An entry under Cork's old name goes either way.
/// Everything else in the file is kept; a file that is not JSON is left
/// alone (None).
pub(crate) fn merge_mcp_json(existing: &str, entry: Option<&Value>) -> Option<String> {
    let mut current: Value = if existing.trim().is_empty() { json!({}) } else { serde_json::from_str(existing).ok()? };
    let map = current.as_object_mut()?;
    match entry {
        Some(entry) => {
            let servers = map.entry("mcpServers").or_insert_with(|| json!({})).as_object_mut()?;
            for name in MCP_NAMES {
                servers.remove(name);
            }
            servers.insert(MCP_NAME.into(), entry.clone());
        }
        None => {
            if let Some(servers) = map.get_mut("mcpServers").and_then(Value::as_object_mut) {
                for name in MCP_NAMES {
                    servers.remove(name);
                }
            }
        }
    }
    Some(format!("{}\n", serde_json::to_string_pretty(&current).ok()?))
}

/// Codex's ~/.codex/config.toml with Cork's MCP server, or without it.
pub(crate) fn merge_codex_mcp(existing: &str, entry: Option<&Value>) -> String {
    // our block runs from the mark to the next table or the end. A `cork`
    // or `collide` server the person added themselves (`codex mcp add
    // collide --url ...`) goes too: a second [mcp_servers.cork] is a
    // duplicate key, and Codex refuses to start at all over it; a
    // [mcp_servers.collide] beside it is every tool twice
    let our_table = |line: &str| {
        let t = line.trim();
        MCP_NAMES.iter().any(|name| {
            t == format!("[mcp_servers.{name}]") || t == format!("[mcp_servers.\"{name}\"]")
                || t.starts_with(&format!("[mcp_servers.{name}.")) || t.starts_with(&format!("[mcp_servers.\"{name}\"."))
        })
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut ours = false;
    for line in existing.lines() {
        if line.trim() == CODEX_MCP_MARK || line.trim() == OLD_CODEX_MCP_MARK || our_table(line) {
            ours = true;
            continue;
        }
        if ours && line.trim_start().starts_with('[') {
            ours = false;
        }
        if !ours {
            kept.push(line);
        }
    }
    let mut out = kept.join("\n").trim_end().to_string();
    if let Some(entry) = entry {
        let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("{CODEX_MCP_MARK}\n[mcp_servers.{MCP_NAME}]\ncommand = {}\nargs = [\"mcp\"]\n",
            quote(entry["command"].as_str().unwrap_or(""))));
        if let Some(home) = entry.pointer("/env/CORK_HOME").and_then(Value::as_str) {
            out.push_str(&format!("env = {{ CORK_HOME = {} }}\n", quote(home)));
        }
    } else if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// The MCP configs Cork fills, beside the hooks: (name, file, is_toml).
fn mcp_files(env: &Env, with_claude: bool) -> Vec<(&'static str, PathBuf, bool)> {
    let home = PathBuf::from(crate::check::home(env));
    let mut out = Vec::new();
    for (name, _, _) in tools(env) {
        match name {
            "Claude Code" if with_claude => out.push((name, home.join(".claude.json"), false)),
            "Codex" => out.push((name, home.join(".codex").join("config.toml"), true)),
            "Cursor" => out.push((name, home.join(".cursor").join("mcp.json"), false)),
            _ => {}
        }
    }
    out
}

/// Claude Code rewrites ~/.claude.json while it runs, so its own command
/// changes it when it is installed. False when there is no `claude` to ask.
/// Taking it out takes out both names (Cork's, and Collide's from before
/// the rename).
fn claude_mcp(env: &Env, entry: Option<&Value>) -> bool {
    let run = |args: &[&str]| {
        let mut command = Command::new(exe("claude"));
        command.args(args).env("HOME", crate::check::home(env)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        command.status()
    };
    match entry {
        Some(entry) => match run(&["mcp", "add-json", "-s", "user", MCP_NAME, &entry.to_string()]) {
            Ok(status) => status.success(),
            Err(_) => false,
        },
        // removing what was never there fails, and is still done
        None => MCP_NAMES.iter().fold(false, |any, name| run(&["mcp", "remove", "-s", "user", name]).is_ok() || any),
    }
}

/// Add (or with `entry` None, take out) Cork's MCP tools for each tool.
fn write_mcp(env: &Env, entry: Option<&Value>, with_claude: bool) {
    if with_claude && config::get(env, "CORK_CLAUDE_CLI") != "0" && entry.is_some() {
        let _ = claude_mcp(env, None);
    }
    for (name, path, toml) in mcp_files(env, with_claude) {
        // a tool the person took Cork out of gets nothing back
        if entry.is_some() && crate::harnesses::opted_out_name(name, env) {
            continue;
        }
        if name == "Claude Code" && config::get(env, "CORK_CLAUDE_CLI") != "0" && claude_mcp(env, entry) {
            continue;
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if entry.is_none() && existing.is_empty() {
            continue;
        }
        let merged = if toml { Some(merge_codex_mcp(&existing, entry)) } else { merge_mcp_json(&existing, entry) };
        if let Some(merged) = merged {
            if merged != existing {
                let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
                let _ = std::fs::write(&path, merged);
            }
        }
    }
}

/// The agent tools on this machine whose user-level hooks Cork fills:
/// (name, file, merge). Claude Code always; Codex and Cursor when present.
fn tools(env: &Env) -> Vec<(&'static str, PathBuf, Merge)> {
    let mut out: Vec<(&'static str, PathBuf, Merge)> =
        vec![("Claude Code", claude_settings_path(env), merge_claude_settings)];
    for key in ["codex", "cursor"] {
        if crate::harnesses::present(key, env) {
            if let Some(file) = crate::harnesses::hook_file(key, env) {
                out.push(file);
            }
        }
    }
    out
}

/// A program of that name on the PATH this process was handed (the
/// environment it runs with, so a test's scratch PATH is the whole PATH).
pub(crate) fn on_path(name: &str, env: &Env) -> bool {
    std::env::split_paths(config::get(env, "PATH")).any(|dir| !dir.as_os_str().is_empty() && dir.join(exe(name)).is_file())
}

/// The self-update replaces the program, not the hook entries in each
/// tool's settings: the first session a newer program runs re-merges the
/// files Cork already manages there (the merge replaces an older copy of
/// its own entries and keeps everything else), so hook events added in a
/// later version are registered without a reinstall. A file without
/// Cork's entries (the plugin's --no-settings install) is left alone.
pub fn refresh_settings(env: &Env) {
    let machine = settings(env);
    let version = crate::report::HOOK_VERSION as u64;
    if machine.get("settings_version").and_then(Value::as_u64).unwrap_or(0) >= version {
        return;
    }
    // a machine from before the rename moves to ~/.cork (macOS, Linux), so
    // the entries merged below name the new paths
    #[cfg(unix)]
    {
        let _ = move_to_cork(env);
        repoint_path_lines(env);
    }
    // `cork` and `collide`: one program under both names
    place_cork_beside(env);
    let mut came_after = false;
    for (name, path, merge) in tools(env) {
        // a tool the person took Cork out of is left alone
        if crate::harnesses::opted_out_name(name, env) {
            continue;
        }
        let existing = match std::fs::read_to_string(&path) {
            Ok(existing) => existing,
            // an agent tool that came after Cork (the ChatGPT app's Codex,
            // Cursor) has no hooks file yet: the harness check fills it below,
            // as the install would have had the tool been there then. Never
            // Claude Code's settings: without them it may run Cork as a
            // plugin, and a second copy of the hooks would fire twice.
            Err(_) => {
                came_after |= name != "Claude Code";
                continue;
            }
        };
        if !has_machine_mark(&existing) {
            continue;
        }
        let merged = merge(&existing, env);
        if merged != existing {
            let _ = std::fs::write(&path, merged);
        }
    }
    // one detection and one record for tools that came after Cork
    // (harnesses.rs): what it adds is recorded and said once, and a tool
    // whose record is `off` is never filled again
    if came_after {
        let _ = crate::harnesses::scan(env, 0.0, false);
    }
    // Hermes's plugin, kept current where it is in. Adding Cork to a
    // Hermes (or Codex, or Cursor) that came after it is the harness check's
    // job (harnesses.rs), which leaves alone a tool the person took it out of
    if installed(env) && hermes_installed(env) {
        let _ = install_hermes(env);
    }
    // a wake watcher from before its launcher moves onto it (wake.rs)
    crate::wake::refresh_service(env);
    // a machine set up under one of the hosted server's older names keeps
    // its address as Cork's now (config::canonical_server)
    move_to_cloud_address(env);
    // read again: the harness check above may have written its records
    let mut machine = settings(env);
    if let Some(map) = machine.as_object_mut() {
        map.insert("settings_version".into(), json!(version));
    }
    save_settings(env, &machine);
}

/// machine.json's server addresses, written as config::canonical_server
/// reads them, so what `cork status` and every tool say matches what is used.
fn move_to_cloud_address(env: &Env) {
    let mut machine = settings(env);
    let mut changed = false;
    let mut fix = |slot: Option<&mut Value>| {
        if let Some(Value::String(url)) = slot {
            let now = config::canonical_server(url);
            if !url.is_empty() && now != *url {
                *url = now;
                changed = true;
            }
        }
    };
    fix(machine.get_mut("server_url"));
    fix(machine.get_mut("account").and_then(|a| a.get_mut("server_url")));
    if changed {
        save_settings(env, &machine);
    }
}

pub(crate) fn copy_executable(from: &Path, to: &Path) -> Result<(), String> {
    if from == to || same_file(from, to) {
        return Ok(());
    }
    let staged = to.with_extension("new");
    std::fs::copy(from, &staged).map_err(|e| format!("{}: {e}", from.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    // a running copy on Windows cannot be replaced, only renamed aside
    if to.exists() {
        let aside = to.with_extension("old");
        let _ = std::fs::remove_file(&aside);
        let _ = std::fs::rename(to, &aside);
    }
    std::fs::rename(&staged, to).map_err(|e| format!("{}: {e}", to.display()))
}

/// `cork install [--no-settings] [--claim CODE --server URL]`: set
/// Cork up for every repo on this machine. The program copies itself to
/// `~/.cork/bin/cork`: it is both the hooks and the local server. On an
/// install from before the rename it goes to `~/.collide/bin/collide`, which
/// that machine's hook entries run, with a `cork` beside it.
pub fn install(args: &[String], env: &Env) -> i32 {
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_default();
    if !serves_local() {
        println!("Cork install: this is the per-repo hook. Install the free version with: curl -fsSL https://corkcli.com/install.sh | sh");
        return 1;
    }
    // `--harness codex`: Cork back into one tool the person had taken it
    // out of (or into one it has not found yet), and nothing else
    if args.iter().any(|a| a == "--harness") {
        return crate::harnesses::enable_command(&arg("--harness"), env);
    }
    // a machine set up before the rename moves to ~/.cork first (macOS,
    // Linux), taking an installer's download there with it; where it cannot
    // move, that download settles into the ~/.collide the machine is set up in
    #[cfg(unix)]
    let moved = move_to_cork(env);
    #[cfg(not(unix))]
    let moved: Option<PathBuf> = None;
    let me = moved.or_else(|| settle_layout(env)).unwrap_or_else(|| std::env::current_exe().unwrap_or_default());
    let bin = bin_dir(env);
    if std::fs::create_dir_all(&bin).is_err() {
        println!("Cork install: could not create {}.", bin.display());
        return 1;
    }
    if let Err(problem) = copy_executable(&me, &program_path(env)) {
        println!("Cork install: could not place the program ({problem}).");
        return 1;
    }
    // `cork` and `collide`: one program under both names
    place_cork_beside(env);
    // an install from before the merge: its two binaries make way
    for old in ["collide-hook", "collide-server"] {
        let _ = std::fs::remove_file(bin.join(exe(old)));
    }

    // the machine's mode: local unless a paid login already made it cloud
    // (a file with no mode yet may hold a switch set before the install)
    let mut machine = settings(env);
    if !machine.is_object() {
        machine = json!({});
    }
    if text(&machine, "mode").is_empty() {
        machine["mode"] = json!("local");
    }
    if let Some(map) = machine.as_object_mut() {
        map.insert("settings_version".into(), json!(crate::report::HOOK_VERSION as u64));
    }
    if !save_settings(env, &machine) {
        println!("Cork install: could not write {}.", tilde(&data_dir(env).join("machine.json"), env));
        return 1;
    }
    machine_id(env);

    // Claude Code: the user-level settings apply to every project. The
    // plugin brings its own hooks and installs with --no-settings.
    // the plugin brings Claude Code's hooks itself and installs with
    // --no-settings; the other tools are still Cork's to fill
    let quiet = args.iter().any(|a| a == "--no-settings");
    let mut covered: Vec<String> = Vec::new();
    for (name, path, merge) in tools(env) {
        if quiet && name == "Claude Code" {
            crate::harnesses::record(env, "claude", "plugin", "install");
            continue;
        }
        // a tool the person took Cork out of stays out (`--harness` brings it back)
        if crate::harnesses::opted_out_name(name, env) {
            continue;
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        if std::fs::write(&path, merge(&existing, env)).is_err() {
            println!("Cork install: could not update {}.", path.display());
            return 1;
        }
        crate::harnesses::record_name(env, name, "install");
        covered.push(format!("{name} ({})", path.display()));
    }
    if !crate::harnesses::opted_out("hermes", env) {
        if let Some(hermes) = install_hermes(env) {
            crate::harnesses::record(env, "hermes", "on", "install");
            covered.push(hermes);
        }
    }

    // the MCP tools (get_symbol, blast_radius, recap, ...) from the local
    // server, for every tool; the plugin brings Claude Code's itself
    write_mcp(env, Some(&mcp_entry(env)), !quiet);

    let running = ensure_local(env, Duration::from_secs(5));
    if quiet {
        return 0;
    }
    // run at a repo's root (what `setup` hands out), the repo's old
    // per-repo setup makes way: the machine's hooks take its place
    let retired = std::env::current_dir()
        .ok()
        .and_then(|d| git_root(&d))
        .map(|root| retire_repo_setup(&root))
        .unwrap_or_default();
    let (base, _) = local_endpoint(env);
    let tell_badge = badge_told_once(env);
    let paint = Paint::new(env);
    // installed from `setup`: the code links this machine to that account
    let server = arg("--server").trim_end_matches('/').to_string();
    let code = arg("--claim");
    let claim = (!code.is_empty() && !server.is_empty()).then(|| claim_machine(&server, &code, env));
    let not_linked = |why: &str| {
        println!("Cork is installed, but not linked to your account{}. Link it any time with: {}",
            if why.is_empty() { String::new() } else { format!(" ({why})") }, paint.command(&cork_command(env, "login")));
    };
    // run by the npx wrapper or the install script, which print the banner
    // and the last word: what the install did, as a person reads it (who,
    // then what, then where), each thing said once
    if config::get(env, "CORK_STYLE") == "1" {
        let had_account = settings(env).get("account").is_some_and(Value::is_object);
        let linked = match &claim {
            Some(Ok(claimed)) => link(&server, claimed, env),
            _ => 0,
        };
        say_installed(&covered, had_account, tell_badge, env);
        say_retired(&retired);
        if let Some(Err(why)) = &claim {
            not_linked(why);
        }
        return linked;
    }
    println!("Cork is installed for every repo on this machine.");
    println!("  hooks:  {}", covered.join(", "));
    println!("  server: {} ({})", base, if running { "running" } else { "starts with the next session" });
    println!("  data:   {} (stays on this machine)", local_dir(env).display());
    if tell_badge {
        println!("  PRs:    end with a \"coordinated by cork\" badge (turn off: {})", paint.command(&cork_command(env, "badge off")));
    }
    println!("Start or restart your agent sessions; agents in the same repo now see each other's work and messages.");
    say_retired(&retired);
    match &claim {
        Some(Ok(claimed)) => return link(&server, claimed, env),
        Some(Err(why)) => {
            not_linked(why);
            return 0;
        }
        None => {}
    }
    println!("To work across your machines and with other people, sign in, free: {}", paint.command(&cork_command(env, "login")));
    0
}

/// `setup`'s single-use code, spent: the account's answer to link this
/// machine with, or why it was not taken.
fn claim_machine(server: &str, code: &str, env: &Env) -> Result<Value, String> {
    let mut body = machine_body(env);
    body["code"] = json!(code);
    crate::http::audit_raw("POST", &format!("{server}/machine/claim"), &body.to_string());
    let claimed = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
        .post(&format!("{server}/machine/claim"))
        .send_json(body)
        .ok()
        .and_then(|r| r.into_json::<Value>().ok())
        .unwrap_or(Value::Null);
    if claimed.get("ok").and_then(Value::as_bool) == Some(true) { Ok(claimed) } else { Err(text(&claimed, "error")) }
}

/// "Claude Code, Codex and Cursor".
fn and_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The repo the install was run in, as it says it: `watching acme`, with
/// the agents at work in it on this machine when there are any (what the
/// hooks recorded for the status line; never a guess).
fn watching(env: &Env) -> Option<String> {
    let root = config::find_repo_root(&[std::env::current_dir().ok()])?;
    let repo_id = repo_id_for(&root);
    let name = repo_id.rsplit('/').next().filter(|n| !n.is_empty()).map(str::to_string)
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().to_string()))?;
    Some(match crate::statusline::sessions_here(&repo_id, &root, env).len() {
        0 => format!("watching {name}"),
        1 => format!("watching {name}, 1 agent online"),
        n => format!("watching {name}, {n} agents online"),
    })
}

/// The styled install's lines: who this machine is signed in as (unless
/// the sign-in just said it), the agent tools whose hooks are on, the repo
/// it was run in, and (the first install only) the pull-request badge.
fn say_installed(covered: &[String], signed_in: bool, badge: bool, env: &Env) {
    let paint = Paint::new(env);
    let tick = paint.yellow("✓");
    if signed_in {
        let user = settings(env).get("account").map(|a| text(a, "user")).unwrap_or_default();
        println!("  {tick} signed in{}", if user.is_empty() { String::new() } else { format!(" as {user}") });
    }
    let tools: Vec<String> = covered.iter().map(|c| c.split(" (").next().unwrap_or(c).to_string()).collect();
    if tools.is_empty() {
        println!("  {tick} installed; Cork is off in every agent tool here ({} says which)", paint.command("cork status"));
    } else {
        println!("  {tick} hooks on for {}", and_list(&tools));
    }
    let mut after: Vec<&str> = Vec::new();
    if tools.iter().any(|t| t == "Claude Code") {
        after.push("open Claude Code sessions pick it up now");
    }
    if tools.iter().any(|t| t == "Codex") {
        after.push("in Codex, start a new session and approve Cork's hooks in /hooks");
    }
    if !after.is_empty() {
        println!("    {}", paint.dim(&after.join("; ")));
    }
    if let Some(line) = watching(env) {
        println!("  {tick} {line}");
    }
    if badge {
        println!("  {tick} pull requests end with a \"coordinated by cork\" badge {}", paint.dim("· turn off: cork badge off"));
    }
}

/// A machine set up before the rename keeps `~/.collide` for as long as no
/// `~/.cork` exists beside it (`config::data_dir_in`, and the local server
/// picks the same). An installer that put the new program in a fresh
/// `~/.cork/bin` on such a machine would flip it to an empty folder: no
/// mode, no account, no local history. So when `~/.cork` holds nothing but
/// that download and `~/.collide` holds the machine's setup, the program
/// moves itself to `~/.collide/bin/cork` (a rename, which works for a
/// running program on Windows too) and the stray `~/.cork` goes. Returns
/// where the running program now is, or None when there was nothing to do.
pub(crate) fn settle_layout(env: &Env) -> Option<PathBuf> {
    let home = PathBuf::from(crate::check::home(env));
    let (cork, collide) = (home.join(".cork"), home.join(".collide"));
    if !collide.join("machine.json").is_file() || !cork.is_dir() {
        return None;
    }
    if !only_download(&cork) {
        return None;
    }
    let me = std::env::current_exe().ok()?;
    let target = collide.join("bin").join(exe(PROGRAM));
    let moved = if me.starts_with(&cork) {
        std::fs::create_dir_all(collide.join("bin")).ok()?;
        let aside = target.with_extension("old");
        let _ = std::fs::remove_file(&aside);
        if target.exists() {
            let _ = std::fs::rename(&target, &aside);
        }
        std::fs::rename(&me, &target).ok()?;
        Some(target)
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&cork);
    moved
}

/// Whether a `~/.cork` holds nothing but what an installer downloaded into
/// it (`bin/cork`): no setup, no account, no local history.
fn only_download(cork: &Path) -> bool {
    let names = |dir: &Path| -> Option<Vec<String>> {
        Some(std::fs::read_dir(dir).ok()?.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
    };
    let Some(top) = names(cork) else { return false };
    let programs = names(&cork.join("bin")).unwrap_or_default();
    top.iter().all(|n| n == "bin") && programs.iter().all(|n| n.starts_with(PROGRAM) || n.starts_with(OLD_PROGRAM))
}

/// Whether two paths are one file, through any link.
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

/// `to` becomes the same file as `from` (a hard link, else a copy), in one
/// rename: whatever runs `to` meanwhile finds the old file or the new one.
#[cfg(unix)]
fn place_same(from: &Path, to: &Path) -> bool {
    if same_file(from, to) {
        return true;
    }
    let staged = to.with_extension("new");
    let _ = std::fs::remove_file(&staged);
    (std::fs::hard_link(from, &staged).is_ok() || std::fs::copy(from, &staged).is_ok()) && std::fs::rename(&staged, to).is_ok()
}

/// `bin/collide` becomes a link to `bin/cork` beside it, in one rename. A
/// relative link, so it holds under either folder name.
#[cfg(unix)]
fn link_old_name(bin: &Path) -> bool {
    let (program, old) = (bin.join(PROGRAM), bin.join(OLD_PROGRAM));
    if !program.is_file() {
        return false;
    }
    if std::fs::read_link(&old).is_ok_and(|to| to == Path::new(PROGRAM)) {
        return true;
    }
    let staged = bin.join(format!("{OLD_PROGRAM}.link"));
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(PROGRAM, &staged).is_ok() && std::fs::rename(&staged, &old).is_ok()
}

/// Held while one process moves the machine: two sessions starting at once
/// both run the first session's refresh. One left behind by a crash is
/// stale after a minute.
#[cfg(unix)]
struct MoveLock(PathBuf);

#[cfg(unix)]
impl Drop for MoveLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
fn move_lock(home: &Path) -> Option<MoveLock> {
    let path = home.join(".cork-move.lock");
    for _ in 0..2 {
        if std::fs::OpenOptions::new().write(true).create_new(true).open(&path).is_ok() {
            return Some(MoveLock(path));
        }
        let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if age.is_some_and(|age| age < Duration::from_secs(60)) {
            return None;
        }
        let _ = std::fs::remove_file(&path);
    }
    None
}

/// The PATH lines an installer wrote for `~/.collide/bin`, pointed at
/// `~/.cork/bin` once the machine has moved (macOS, Linux), so a new
/// terminal finds `cork` in its own folder, not through the old name's link.
/// Only Cork's own lines change: the export (or fish's `fish_add_path`) and
/// the `# Collide` comment an installer put above it. A file that already
/// names `~/.cork/bin` loses the old lines instead. Nothing is touched when
/// Cork lives outside the home folder (`CORK_HOME`).
#[cfg(unix)]
pub(crate) fn repoint_path_lines(env: &Env) {
    let home = config::get(env, "HOME");
    if home.is_empty() || crate::check::home(env) != home || config::legacy_layout(env) {
        return;
    }
    const OLD: [&str; 2] = [r#"export PATH="$HOME/.collide/bin:$PATH""#, r#"fish_add_path "$HOME/.collide/bin""#];
    const NEW: [&str; 2] = [r#"export PATH="$HOME/.cork/bin:$PATH""#, r#"fish_add_path "$HOME/.cork/bin""#];
    let home = PathBuf::from(home);
    for name in [".zshrc", ".bashrc", ".bash_profile", ".profile", ".config/fish/conf.d/collide.fish", ".config/fish/conf.d/cork.fish"] {
        let path = home.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if !OLD.iter().any(|old| text.contains(old)) {
            continue;
        }
        let has_new = NEW.iter().any(|new| text.contains(new));
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let bare = line.trim();
            let next_is_old = lines.get(i + 1).is_some_and(|next| OLD.contains(&next.trim()));
            if matches!(bare, "# Collide" | "# Cork") && next_is_old {
                if !has_new {
                    out.push_str(&line.replace("# Collide", "# Cork"));
                }
            } else if OLD.contains(&bare) {
                if !has_new {
                    out.push_str(&line.replace("/.collide/bin", "/.cork/bin"));
                }
            } else {
                out.push_str(line);
            }
            i += 1;
        }
        if out != text {
            // written in place: a dotfile that is a link keeps being one
            let _ = std::fs::write(&path, out);
        }
    }
}

/// A machine set up before the rename moves to `~/.cork` (macOS and Linux).
/// The folder is renamed whole, so its setup, account and local history come
/// along, and `~/.collide` is left as a link to it: whatever still names the
/// old paths keeps working through it (hook entries an agent tool trusts by
/// their exact text, as Codex does; sessions, listeners and a local server
/// already running; the wake watcher launchd starts; a PATH line in a
/// shell's startup file). In `bin/`, `cork` is the program and `collide` a
/// link to it: one program under two names, never two copies.
///
/// Each step leaves a machine that works. The program is put in place as
/// `cork` first, inside the old folder; `collide` becomes the link next, so
/// the old layout runs the same file; only then is the folder renamed, with
/// the link to it ready to take its name, and a rename that fails puts the
/// folder back. Nothing moves when `~/.cork` holds anything but an
/// installer's download (two setups are never merged): that download is
/// what becomes `cork`. Returns where the running program is now when it
/// was the download or in the moved folder (the install copies from it).
#[cfg(unix)]
pub(crate) fn move_to_cork(env: &Env) -> Option<PathBuf> {
    let home = PathBuf::from(crate::check::home(env));
    let (cork, collide) = (home.join(".cork"), home.join(".collide"));
    let is_folder = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    if !is_folder(&collide) || !collide.join("machine.json").is_file() {
        return None;
    }
    let _lock = move_lock(&home)?;
    let me = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok()).unwrap_or_default();
    let old_bin = collide.join("bin");
    // an installer's download into a fresh ~/.cork is set aside, to become the program
    let ran_from = |dir: &Path| std::fs::canonicalize(dir).is_ok_and(|dir| me.starts_with(dir));
    let was_running = ran_from(&cork) || ran_from(&old_bin);
    let aside = home.join(format!(".cork-download-{}", std::process::id()));
    let downloaded = std::fs::symlink_metadata(&cork).is_ok();
    if downloaded && (!is_folder(&cork) || !only_download(&cork) || std::fs::rename(&cork, &aside).is_err()) {
        return None;
    }
    // the program as `cork`, inside the old folder: the download, else the
    // copy running now, else the one the old hook entries run
    let program = old_bin.join(PROGRAM);
    let download = aside.join("bin").join(PROGRAM);
    let source = if download.is_file() {
        download
    } else if me.parent().is_some_and(|dir| same_file(dir, &old_bin)) && me.is_file() {
        me.clone()
    } else {
        old_bin.join(OLD_PROGRAM)
    };
    let placed = source.is_file() && (same_file(&source, &program) || {
        let _ = std::fs::create_dir_all(&old_bin);
        if source.starts_with(&aside) { std::fs::rename(&source, &program).is_ok() } else { place_same(&source, &program) }
    });
    if !placed {
        // the download goes back where the installer put it
        let _ = std::fs::rename(&aside, &cork);
        return None;
    }
    let _ = std::fs::remove_dir_all(&aside);
    let _ = link_old_name(&old_bin);
    // the folder, with a link ready to take its old name at once
    let link = home.join(format!(".collide-link-{}", std::process::id()));
    let _ = std::fs::remove_file(&link);
    if std::os::unix::fs::symlink(".cork", &link).is_err() {
        return was_running.then_some(program);
    }
    if std::fs::rename(&collide, &cork).is_err() {
        let _ = std::fs::remove_file(&link);
        return was_running.then_some(program);
    }
    if std::fs::rename(&link, &collide).is_err() {
        let _ = std::fs::rename(&cork, &collide);
        let _ = std::fs::remove_file(&link);
        return was_running.then_some(program);
    }
    was_running.then(|| cork.join("bin").join(PROGRAM))
}

/// Color for a person at a terminal: the brand's yellow for what matters, dim
/// for labels. Off when output is piped or NO_COLOR is set. Only for lines
/// printed to a person: what the hooks hand an agent never carries it.
pub(crate) struct Paint {
    on: bool,
}

impl Paint {
    pub(crate) fn new(env: &Env) -> Self {
        use std::io::IsTerminal;
        Paint { on: std::io::stdout().is_terminal() && config::get(env, "NO_COLOR").is_empty() }
    }
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.on { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
    }
    pub(crate) fn yellow(&self, text: &str) -> String {
        self.wrap("38;2;255;198;26", text)
    }
    pub(crate) fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }
    /// A cork command for the person to run, so it stands out of the sentence.
    pub(crate) fn command(&self, text: &str) -> String {
        self.wrap("1;38;2;255;198;26", text)
    }
    /// A line written elsewhere (for a person), with each cork command in it
    /// painted: `cork upgrade`, `~/.cork/bin/cork login`, `cork install
    /// --harness codex`.
    pub(crate) fn commands(&self, line: &str) -> String {
        if !self.on {
            return line.to_string();
        }
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| {
            regex::Regex::new(concat!(
                r"(?:CORK_HOME=\S+ )?(?:[\w~.:/\\-]*[/\\])?\bcork(?:\.exe)? ",
                r"(?:install --harness \w+|wake (?:on|off)|statusline (?:on|off)|",
                r"login|logout|status|update|upgrade|sync|add|install|uninstall|rollback|migrate|audit)\b"
            ))
            .expect("command pattern")
        })
        .replace_all(line, |c: &regex::Captures| self.command(&c[0]))
        .into_owned()
    }
}

// ------------------------------------------------------------ the PR badge

/// What an agent ends each pull request it opens with, the way Claude Code
/// signs its own: Cork's badge, linking to the site.
pub const PR_BADGE_LINE: &str = "[![coordinated by cork](https://corkcli.com/badge.svg)](https://corkcli.com)";

/// Whether pull requests agents open here get the badge (added by the hooks,
/// or asked of the agent where they cannot add it, prbadge.rs): on unless
/// `cork badge off` (machine.json `pr_badge`) or `CORK_PR_BADGE=0`.
pub fn pr_badge_on(env: &Env) -> bool {
    config::get(env, "CORK_PR_BADGE").trim() != "0" && settings(env).get("pr_badge").and_then(Value::as_bool) != Some(false)
}

/// True the first time the install can say the badge is on: recorded, so
/// a later install does not say it again.
fn badge_told_once(env: &Env) -> bool {
    let mut machine = settings(env);
    if !machine.is_object() || machine.get("pr_badge").is_some() || !pr_badge_on(env) {
        return false;
    }
    machine["pr_badge"] = json!(true);
    save_settings(env, &machine)
}

/// `cork badge on|off|status`.
pub fn badge(args: &[String], env: &Env) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    if matches!(sub, "on" | "off") {
        let mut machine = settings(env);
        if !machine.is_object() {
            machine = json!({});
        }
        machine["pr_badge"] = json!(sub == "on");
        if !save_settings(env, &machine) {
            println!("Cork: could not write {}.", tilde(&data_dir(env).join("machine.json"), env));
            return 1;
        }
    } else if sub != "status" {
        println!("usage: cork badge on|off|status\n\n\
When on, your agents end each pull request they open with Cork's badge:\n  {PR_BADGE_LINE}");
        return 2;
    }
    if pr_badge_on(env) {
        println!("Cork: the PR badge is on. Pull requests your agents open with `gh pr create` get this line, once \
(remove it and it stays removed):\n  {PR_BADGE_LINE}\n`cork badge off` stops it.");
    } else if config::get(env, "CORK_PR_BADGE").trim() == "0" {
        println!("Cork: the PR badge is off here: CORK_PR_BADGE=0 is set in this environment.");
    } else {
        println!("Cork: the PR badge is off. It is not added to your agents' pull requests, and agents are not asked to \
add it. `cork badge on` brings it back.");
    }
    0
}

/// A path under the home folder, written the short way (`~/.cork/local`).
fn tilde(path: &Path, env: &Env) -> String {
    let home = config::get(env, "HOME");
    let shown = path.display().to_string();
    match shown.strip_prefix(home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => shown,
    }
}

// ------------------------------------------- retiring the per-repo setup

/// What the old per-repo setup committed under `.collide/`: the hook
/// scripts, the pinned native hook with its installer and self-update
/// stamp, the binary itself and the git pre-commit shim. `config.json` is
/// handled on its own (see `SETUP_CONFIG_KEYS`).
const REPO_SETUP_FILES: &[&str] = &[
    "report_hook.py", "gate_hook.py", "pre_commit.py", "wake.py", "hooks.lock", "install-native-hook.sh",
    "native", "bin", "githooks",
];
/// The keys setup wrote into `.collide/config.json`. The machine names the
/// server, the workspace and the repo (by its origin) now; any other key is
/// the repo's own choice (`verify`, `auto_pull`, `land`, `protocol`) and is
/// kept in place, since the hooks still read it.
const SETUP_CONFIG_KEYS: &[&str] = &["server_url", "repo_id", "workspace", "local_parse"];
/// Where each agent tool's per-repo hooks were wired.
const REPO_SETTINGS: &[&str] = &[".claude/settings.json", ".codex/hooks.json", ".cursor/hooks.json"];

/// A hook command of the old per-repo setup, in any dialect. The machine's
/// own (`# collide-machine`) never counts.
fn is_repo_hook_command(text: &str) -> bool {
    !has_machine_mark(text)
        && text.contains("collide")
        && (text.contains(".collide/report_hook.py")
            || text.contains(".collide/gate_hook.py")
            || text.contains("collide-hook")
            || text.contains("collide-report-hook")
            || text.contains("collide-gate-hook"))
}

fn mentions_repo_hook(node: &Value) -> bool {
    match node {
        Value::String(text) => is_repo_hook_command(text),
        Value::Array(items) => items.iter().any(mentions_repo_hook),
        Value::Object(map) => map.values().any(mentions_repo_hook),
        _ => false,
    }
}

/// A repo settings file with the old per-repo hook entries taken out and
/// everything else kept: `None` when it had none, `Some("")` when nothing
/// would be left (the file goes), else the new content.
pub fn strip_repo_hooks(existing: &str) -> Option<String> {
    let mut current = serde_json::from_str::<Value>(existing).ok()?.as_object()?.clone();
    let hooks = current.get("hooks")?.as_object()?.clone();
    let mut kept = serde_json::Map::new();
    let mut removed = false;
    for (event, entries) in hooks {
        let Some(list) = entries.as_array() else {
            kept.insert(event, entries);
            continue;
        };
        let left: Vec<Value> = list.iter().filter(|entry| !mentions_repo_hook(entry)).cloned().collect();
        removed |= left.len() != list.len();
        if !left.is_empty() {
            kept.insert(event, Value::Array(left));
        }
    }
    if !removed {
        return None;
    }
    if kept.is_empty() {
        current.remove("hooks");
    } else {
        current.insert("hooks".into(), Value::Object(kept));
    }
    // a hooks file the setup created holds nothing but its dialect's version
    let empty = current.is_empty() || (current.len() == 1 && current.contains_key("version"));
    Some(if empty { String::new() } else { format!("{}\n", serde_json::to_string_pretty(&Value::Object(current)).unwrap_or_default()) })
}

/// Whether this repo still carries the old per-repo setup anywhere.
pub fn has_repo_setup(root: &Path) -> bool {
    let collide = root.join(".collide");
    REPO_SETUP_FILES.iter().any(|name| collide.join(name).exists())
        || config::load_json(&collide.join("config.json"))
            .as_object()
            .is_some_and(|m| m.keys().any(|k| SETUP_CONFIG_KEYS.contains(&k.as_str())))
        || REPO_SETTINGS.iter().any(|rel| {
            std::fs::read_to_string(root.join(rel)).ok().and_then(|text| strip_repo_hooks(&text)).is_some()
        })
}

fn git_quiet(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args)
        .stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Take the old per-repo setup out of this repo: the files under `.collide/`
/// it committed, its hook entries in each agent tool's repo settings, its
/// git pre-commit wiring and its `.gitignore` line. What the repo chose for
/// itself in `.collide/config.json` stays. Every change is staged, never
/// committed. Returns the paths changed, repo-relative.
pub fn retire_repo_setup(root: &Path) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    let collide = root.join(".collide");
    for name in REPO_SETUP_FILES {
        let path = collide.join(name);
        let gone = if path.is_dir() {
            std::fs::remove_dir_all(&path).is_ok()
        } else {
            path.exists() && std::fs::remove_file(&path).is_ok()
        };
        if gone {
            changed.push(format!(".collide/{name}"));
        }
    }
    let config_path = collide.join("config.json");
    if let Some(map) = config::load_json(&config_path).as_object().filter(|_| config_path.is_file()) {
        if map.keys().any(|k| SETUP_CONFIG_KEYS.contains(&k.as_str())) {
            let own: serde_json::Map<String, Value> = map
                .iter()
                .filter(|(k, _)| !SETUP_CONFIG_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let done = if own.is_empty() {
                std::fs::remove_file(&config_path).is_ok()
            } else {
                std::fs::write(&config_path, format!("{}\n", serde_json::to_string_pretty(&Value::Object(own)).unwrap_or_default())).is_ok()
            };
            if done {
                changed.push(".collide/config.json".into());
            }
        }
    }
    if std::fs::read_dir(&collide).map(|mut d| d.next().is_none()).unwrap_or(false) {
        let _ = std::fs::remove_dir(&collide);
    }
    for rel in REPO_SETTINGS {
        let path = root.join(rel);
        let Some(stripped) = std::fs::read_to_string(&path).ok().and_then(|text| strip_repo_hooks(&text)) else { continue };
        let done = if stripped.is_empty() { std::fs::remove_file(&path).is_ok() } else { std::fs::write(&path, stripped).is_ok() };
        if done {
            changed.push((*rel).to_string());
        }
    }
    // the pre-commit shim's wiring is local git config, not a file
    if git_quiet(root, &["config", "--get", "core.hooksPath"]).is_some_and(|p| p.trim_end_matches('/') == ".collide/githooks") {
        let _ = git_quiet(root, &["config", "--unset", "core.hooksPath"]);
    }
    let gitignore = root.join(".gitignore");
    if let Ok(text) = std::fs::read_to_string(&gitignore) {
        let lines: Vec<&str> = text.lines().collect();
        let kept: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(i, line)| {
                let entry = line.trim().trim_end_matches('/') == ".collide/bin";
                let comment = line.trim_start().starts_with("# per-platform native hook")
                    && lines.get(i + 1).is_some_and(|next| next.trim().trim_end_matches('/') == ".collide/bin");
                !entry && !comment
            })
            .map(|(_, line)| *line)
            .collect();
        if kept.len() != lines.len() {
            let body = if kept.is_empty() { String::new() } else { format!("{}\n", kept.join("\n")) };
            if std::fs::write(&gitignore, body).is_ok() {
                changed.push(".gitignore".into());
            }
        }
    }
    // staged one path at a time: an ignored file that was never tracked
    // (the binary) matches nothing, and must not fail the rest
    for rel in &changed {
        let _ = git_quiet(root, &["add", "-A", "--", rel]);
    }
    changed
}

/// What was retired, in words for whoever ran the command.
fn say_retired(changed: &[String]) {
    if changed.is_empty() {
        return;
    }
    println!(
        "Cork: removed this repo's old per-repo setup and staged the change ({}). Commit it, e.g. \
git commit -m \"Cork runs per machine now\", so teammates' agents move to the machine install too.",
        changed.join(", ")
    );
}

/// The git repository `dir` is in, by its `.git`.
fn git_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// `cork migrate`: retire the old per-repo setup of the repo this runs
/// in, once Cork is installed for the machine (which takes its place).
pub fn migrate(env: &Env) -> i32 {
    let Some(root) = std::env::current_dir().ok().and_then(|d| git_root(&d)) else {
        println!("Cork migrate: run this inside the git repository to migrate.");
        return 1;
    };
    if !installed(env) {
        println!("Cork migrate: Cork is not installed on this machine yet, and it takes the place of the \
per-repo setup. Install it first (call Cork's setup tool and run the command it returns), then run this again.");
        return 1;
    }
    let changed = retire_repo_setup(&root);
    if changed.is_empty() {
        println!("Cork migrate: {} has no per-repo setup left; the machine's hooks cover it.", root.display());
    } else {
        say_retired(&changed);
    }
    0
}

/// `cork uninstall`: take the machine hooks out. The local data stays.
pub fn uninstall(env: &Env) -> i32 {
    for (_, path, _) in tools(env) {
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let _ = std::fs::write(&path, remove_from_claude_settings(&existing));
        }
    }
    // Cork's status line too (`cork statusline on`); the person's own stays
    let claude = claude_settings_path(env);
    if let Some(rest) = std::fs::read_to_string(&claude).ok().and_then(|t| crate::statusline::without_statusline(&t)) {
        let _ = std::fs::write(&claude, rest);
    }
    write_mcp(env, None, true);
    crate::wake::off(env);
    uninstall_hermes(env);
    let path = data_dir(env).join("machine.json");
    let _ = std::fs::remove_file(path);
    println!("Cork's machine hooks and its MCP entry are removed. Your local data is kept in {}.", local_dir(env).display());
    println!("Added Cork (or Collide, its name before) as a connector on claude.ai too? Claude Code loads those as well: remove it under Settings, Connectors on claude.ai.");
    println!("Restart open agent sessions: they keep the hooks they started with.");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_is_named_by_id_or_by_name_and_a_file_by_its_extension() {
        let list = json!({"channels": [{"id": "c0123456789ab", "name": "general"}, {"id": "c0123456789ac", "name": "release-notes"}]});
        assert_eq!(text(&channel_in(&list, "#general").unwrap(), "id"), "c0123456789ab");
        assert_eq!(text(&channel_in(&list, "General").unwrap(), "id"), "c0123456789ab");
        assert_eq!(text(&channel_in(&list, "Release Notes").unwrap(), "id"), "c0123456789ac", "spaces are dashes");
        assert_eq!(text(&channel_in(&list, "c0123456789ac").unwrap(), "name"), "release-notes");
        assert!(channel_in(&list, "#nowhere").is_none());
        assert!(channel_in(&json!({}), "general").is_none());
        assert_eq!(attach_type("shots/Before.PNG"), "image/png");
        assert_eq!(attach_type("clip.mov"), "video/quicktime");
        assert_eq!(attach_type("plan.pdf"), "application/pdf");
        assert_eq!(attach_type("Makefile"), "application/octet-stream");
        assert_eq!(attach_type("page.html"), "application/octet-stream", "never claimed as a page: the server refuses it anyway");
    }

    #[test]
    fn cork_commands_are_painted_for_a_person_and_never_for_an_agent() {
        let on = Paint { on: true };
        let line = on.commands("more hours (cork upgrade); sign in: ~/.cork/bin/cork login; or now: cork install --harness codex");
        assert!(line.contains("\x1b[1;38;2;255;198;26mcork upgrade\x1b[0m"), "{line}");
        assert!(line.contains("\x1b[1;38;2;255;198;26m~/.cork/bin/cork login\x1b[0m"), "{line}");
        assert!(line.contains("\x1b[1;38;2;255;198;26mcork install --harness codex\x1b[0m"), "{line}");
        // words that only look like it stay as they are
        assert_eq!(on.commands("cork is up to date; the corkcli.com page"), "cork is up to date; the corkcli.com page");
        // piped or NO_COLOR: plain
        assert_eq!(Paint { on: false }.commands("run cork login"), "run cork login");
        // what the hooks hand an agent is built without it
        let env: Env = [("HOME".to_string(), "/h".to_string())].into_iter().collect();
        assert!(!cork_command(&env, "login").contains('\x1b') && !free_note(&env).contains('\x1b'));
    }

    #[test]
    fn the_help_lists_update_beside_upgrade() {
        let env: Env = [("NO_COLOR".to_string(), "1".to_string())].into_iter().collect();
        let help = help(&env);
        assert!(help.contains("  cork update  "), "{help}");
        assert!(help.contains("a new cork version is cork update"), "{help}");
        assert!(help.contains("  cork statusline on  "), "{help}");
        assert_eq!(and_list(&["Claude Code".into(), "Codex".into(), "Cursor".into()]), "Claude Code, Codex and Cursor");
        assert_eq!(and_list(&["Claude Code".into()]), "Claude Code");
    }

    #[test]
    fn status_says_the_plan_its_hours_and_who_spent_most() {
        // Nov 12 2026, a pool of 2 units, 37.5 h of 200 used, running out before Dec 1
        let now = 1_794_470_000.0;
        let hours = json!({
            "pool": {"kind": "units", "units": 2, "hours": 200, "free_hours": 10},
            "metered_s": 135_000, "limit_s": 720_000, "resets": 1_796_083_200.0, "exhausted": false,
            "pace": {"runs_out_at": 1_795_900_000.0},
            "by_member": [{"name": "", "email": "alice@example.com", "s": 108_000}, {"name": "Carol", "email": "c@x", "s": 27_000}],
        });
        let lines = hours_lines(&hours, "units", now);
        assert_eq!(lines[0], "Plan: 2 units (200 agent hours a month, pooled) · 38 h of 200 h used (18%) this month · resets Dec 1");
        assert!(lines[1].contains("run out about Nov 2"), "{lines:?}");
        assert_eq!(lines[2], "  Most hours this month: alice 30 h, Carol 7.5 h");
        // Free, used up: the wait and the way out
        let free = json!({"pool": {"kind": "free", "units": 0, "free_hours": 10}, "metered_s": 36_000, "limit_s": 36_000,
                          "resets": 1_796_083_200.0, "exhausted": true, "by_member": []});
        let lines = hours_lines(&free, "free", now);
        assert!(lines[0].starts_with("Plan: Free (10 agent hours a month for each person) · 10 h of 10 h used (100%)"), "{lines:?}");
        assert!(lines[1].contains("new ones wait until Dec 1 or until units are added"), "{lines:?}");
        // an earlier seat plan: unlimited, and when its price lock ends
        let seats = json!({"pool": {"kind": "unlimited", "units": 0}, "metered_s": 3_600, "limit_s": null, "resets": 1_796_083_200.0,
                           "sunset": {"ends": 1_825_027_200.0}});
        let lines = hours_lines(&seats, "team", now);
        assert_eq!(lines[0], "Plan: Team plan (unlimited agent hours) · 1.0 h used this month · resets Dec 1");
        assert!(lines[1].contains("locked until Nov 1"), "{lines:?}");
        // an admin's comp says so, whatever plan it rides on
        let comp = json!({"pool": {"kind": "unlimited", "units": 0}, "metered_s": 29_520, "limit_s": null, "resets": 1_796_083_200.0, "comped": true});
        assert_eq!(hours_lines(&comp, "team", now)[0], "Plan: Complimentary (unlimited agent hours) · 8.2 h used this month · resets Dec 1");
        // nothing from the server: nothing said
        assert!(hours_lines(&Value::Null, "", now).is_empty());
    }

    #[test]
    fn a_dead_processs_session_record_is_swept_and_a_live_one_kept() {
        let dir = std::env::temp_dir().join(format!("cork-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
        for pid in ["101", "102"] {
            let path = dir.join(pid);
            std::fs::write(&path, "dead-session 1\n").unwrap();
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        }
        std::fs::write(dir.join("103"), "live-session 2\n").unwrap();
        sweep_agent_records(&dir, now_s());
        assert!(!dir.join("101").exists() && !dir.join("102").exists());
        assert!(dir.join("103").exists());
        // within the hour nothing is read again
        std::fs::write(dir.join("104"), "x 1\n").unwrap();
        std::fs::File::options().write(true).open(dir.join("104")).unwrap().set_modified(old).unwrap();
        sweep_agent_records(&dir, now_s());
        assert!(dir.join("104").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_service_line_says_what_the_status_page_says() {
        let ok = json!({"status": {"indicator": "none", "description": "All systems operational"},
                        "components": [{"name": "Website", "status": "operational"}]});
        assert_eq!(service_line_from(&ok).unwrap(), "Cork service: operational");
        let degraded = json!({"status": {"indicator": "minor", "description": "Degraded performance"},
                              "components": [{"name": "Website", "status": "operational"},
                                             {"name": "Hook updates", "status": "degraded_performance"}]});
        assert_eq!(service_line_from(&degraded).unwrap(), "Cork service: degraded (Hook updates: degraded; see corkcli.com/status)");
        let down = json!({"status": {"indicator": "critical", "description": "Major outage"},
                          "components": [{"name": "API and MCP server", "status": "major_outage"},
                                         {"name": "Dashboard", "status": "partial_outage"}]});
        assert_eq!(
            service_line_from(&down).unwrap(),
            "Cork service: major outage (API and MCP server: major outage, Dashboard: partial outage; see corkcli.com/status)"
        );
        assert_eq!(service_line_from(&json!({"nope": true})), None, "not a status page: nothing said");
    }

    #[test]
    fn the_install_says_the_badge_is_on_once_and_never_when_it_is_off() {
        let home = std::env::temp_dir().join(format!("cork-badge-told-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        env.insert("CORK_HOME".into(), home.to_string_lossy().to_string());
        save_settings(&env, &json!({"mode": "local"}));
        assert!(badge_told_once(&env));
        assert!(!badge_told_once(&env), "said once");
        assert_eq!(settings(&env)["pr_badge"], true);
        // switched off before the install: nothing to say
        save_settings(&env, &json!({"mode": "local", "pr_badge": false}));
        assert!(!badge_told_once(&env) && !pr_badge_on(&env));
        save_settings(&env, &json!({"mode": "local"}));
        env.insert("CORK_PR_BADGE".into(), "0".into());
        assert!(!badge_told_once(&env));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_service_line_is_only_for_collides_own_servers_and_fails_open() {
        let mut env = Env::new();
        assert_eq!(status_url(&env, "https://mcp.corkcli.com").as_deref(), Some("https://corkcli.com/status.json"));
        assert_eq!(status_url(&env, "https://api.corkcli.com/").as_deref(), Some("https://corkcli.com/status.json"));
        assert_eq!(status_url(&env, "https://collide.example.com"), None, "another server has no status page here");
        assert_eq!(status_url(&env, "http://127.0.0.1:47600"), None);
        // an address that cannot answer: nothing said, quickly, nothing cached
        let home = std::env::temp_dir().join(format!("collide-service-line-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        env.insert("CORK_HOME".into(), home.to_string_lossy().to_string());
        env.insert("CORK_STATUS_URL".into(), "http://127.0.0.1:9/status.json".into());
        let started = Instant::now();
        assert_eq!(service_line(&env, "https://mcp.corkcli.com"), None);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!data_dir(&env).join("service-status.json").exists());
        // a fresh answer is reused for a minute
        std::fs::create_dir_all(data_dir(&env)).unwrap();
        std::fs::write(
            data_dir(&env).join("service-status.json"),
            json!({"url": "http://127.0.0.1:9/status.json", "at": now_s(), "line": "Cork service: operational"}).to_string(),
        )
        .unwrap();
        assert_eq!(service_line(&env, "https://mcp.corkcli.com").as_deref(), Some("Cork service: operational"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_merge_keeps_everything_else_and_replaces_its_own() {
        let theirs = r#"{"model": "opus", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}}"#;
        let once = merge_claude_settings(theirs, &Env::new());
        let twice = merge_claude_settings(&once, &Env::new());
        assert_eq!(once, twice, "installing twice changes nothing");
        let parsed: Value = serde_json::from_str(&twice).unwrap();
        assert_eq!(parsed["model"], json!("opus"));
        let stop = parsed["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "theirs kept, ours added once: {stop:?}");
        let removed: Value = serde_json::from_str(&remove_from_claude_settings(&twice)).unwrap();
        assert_eq!(removed["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(removed["hooks"].get("SessionStart").is_none());
        // the gate hears Reads too: a big mapped file's outline comes first (reads.rs)
        let gate = parsed["hooks"]["PreToolUse"][0]["matcher"].as_str().unwrap();
        assert!(gate.split('|').any(|tool| tool == "Read"), "{gate}");
    }

    #[test]
    fn a_codex_installed_after_collide_gets_its_hooks_at_the_next_refresh() {
        let home = std::env::temp_dir().join(format!("collide-refresh-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        env.insert("CORK_HOME".into(), home.to_string_lossy().to_string());
        save_settings(&env, &json!({"mode": "cloud", "settings_version": 1}));

        refresh_settings(&env);
        let codex: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".codex/hooks.json")).unwrap()).unwrap();
        assert!(codex["hooks"]["PostToolUse"].to_string().contains("report --harness codex"), "{codex}");
        // Claude Code's settings are never made from nothing: it may run the plugin
        assert!(!home.join(".claude/settings.json").exists());
        // the harness check's record: said once at the next session start
        assert_eq!(settings(&env)["harnesses"]["codex"]["state"], json!("on"), "{}", settings(&env));
        assert_eq!(settings(&env)["settings_version"], json!(crate::report::HOOK_VERSION));
        // the person takes Cork out of Codex: a later refresh leaves it out
        std::fs::remove_file(home.join(".codex/hooks.json")).unwrap();
        crate::harnesses::record(&env, "codex", "off", "you");
        let mut machine = settings(&env);
        machine["settings_version"] = json!(1);
        save_settings(&env, &machine);
        refresh_settings(&env);
        assert!(!home.join(".codex/hooks.json").exists(), "an opted-out tool is never filled again");
        assert_eq!(settings(&env)["harnesses"]["codex"]["state"], json!("off"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_newer_program_brings_the_hook_list_up_to_date_once() {
        let home = std::env::temp_dir().join(format!("collide-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        env.insert("CORK_HOME".into(), home.to_string_lossy().to_string());
        // an install from an older version: one event of ours, one of theirs
        let old = json!({"model": "opus", "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": machine_command("report", &env)}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}});
        let path = home.join(".claude").join("settings.json");
        std::fs::write(&path, old.to_string()).unwrap();
        save_settings(&env, &json!({"mode": "local"}));

        refresh_settings(&env);
        let now: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for (event, _, _) in CLAUDE_EVENTS {
            assert!(now["hooks"][*event].to_string().contains("# cork-machine"), "{event} registered: {now}");
        }
        assert_eq!(now["model"], json!("opus"));
        assert!(now["hooks"]["Stop"].to_string().contains("say done"), "theirs kept");
        assert_eq!(settings(&env)["settings_version"], json!(crate::report::HOOK_VERSION as u64));

        // once current, nothing is rewritten; a file Cork does not manage is never touched
        std::fs::write(&path, r#"{"hooks": {}}"#).unwrap();
        refresh_settings(&env);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"hooks": {}}"#);
        save_settings(&env, &json!({"mode": "local"}));
        refresh_settings(&env);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"hooks": {}}"#, "no Cork entries: left alone");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_half_removed_repo_setup_does_not_silence_the_machine_hooks() {
        let root = std::env::temp_dir().join(format!("collide-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(root.join(".codex")).unwrap();
        std::fs::write(root.join(".claude/settings.json"), r#"{"hooks": {"x": ".collide/report_hook.py"}}"#).unwrap();
        std::fs::write(root.join(".codex/hooks.json"), r#"{"hooks": {"x": ".collide/report_hook.py"}}"#).unwrap();
        assert!(!repo_runs_its_own(&root), "settings naming a hook that is not there run nothing");
        std::fs::create_dir_all(root.join(".collide")).unwrap();
        std::fs::write(root.join(".collide/report_hook.py"), "").unwrap();
        assert!(!repo_runs_its_own(&root), "a hook with no server to report to reports nothing");
        std::fs::write(root.join(".collide/config.json"), r#"{"server_url": "https://mcp.corkcli.com"}"#).unwrap();
        assert!(repo_runs_its_own(&root), "a working repo setup keeps the machine's hooks out of its way");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Migrating: the per-repo setup goes (scripts, binary, pin, its hook
    /// entries, its .gitignore line), the developer's own hooks and the
    /// repo's own config choices stay, and the change is staged, not committed.
    #[test]
    fn the_old_per_repo_setup_is_retired_and_staged_and_nothing_else_moves() {
        let root = std::env::temp_dir().join(format!("collide-retire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".collide/bin")).unwrap();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(root.join(".cursor")).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git").arg("-C").arg(&root).args(args).stdout(Stdio::null()).stderr(Stdio::null())
                .status().map(|s| s.success()).unwrap_or(false);
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        for name in ["report_hook.py", "gate_hook.py", "hooks.lock", "install-native-hook.sh"] {
            std::fs::write(root.join(".collide").join(name), "x\n").unwrap();
        }
        std::fs::write(root.join(".collide/bin/collide-hook"), "binary").unwrap();
        std::fs::write(root.join(".collide/config.json"),
            r#"{"server_url": "https://mcp.corkcli.com", "repo_id": "github.com/o/r", "workspace": "w1", "local_parse": true, "verify": "cargo check"}"#).unwrap();
        let repo_hook = r#"b="${CLAUDE_PROJECT_DIR:-.}/.collide/bin/collide-hook"; [ -x "$b" ] || b="$(command -v collide-hook 2>/dev/null)"; f="${CLAUDE_PROJECT_DIR:-.}/.collide/report_hook.py""#;
        std::fs::write(root.join(".claude/settings.json"), serde_json::to_string_pretty(&json!({
            "permissions": {"allow": ["mcp__collide"]},
            "hooks": {
                "PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": repo_hook}]},
                                {"matcher": "Read", "hooks": [{"type": "command", "command": "npm run lint"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": repo_hook}]}],
            }})).unwrap()).unwrap();
        std::fs::write(root.join(".cursor/hooks.json"), serde_json::to_string_pretty(&json!({
            "version": 1, "hooks": {"stop": [{"command": format!("{repo_hook} --harness cursor"), "timeout": 5}]}})).unwrap()).unwrap();
        std::fs::write(root.join(".gitignore"), "node_modules/\n# per-platform native hook, installed by Cork\n.collide/bin/\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "the old setup"]);
        assert!(has_repo_setup(&root));
        assert!(repo_runs_its_own(&root), "until it is retired, the repo's own hooks run and the machine's stand down");

        let changed = retire_repo_setup(&root);
        for path in [".collide/report_hook.py", ".collide/gate_hook.py", ".collide/hooks.lock", ".collide/install-native-hook.sh",
                     ".collide/bin", ".collide/config.json", ".claude/settings.json", ".cursor/hooks.json", ".gitignore"] {
            assert!(changed.iter().any(|c| c == path), "{path} in {changed:?}");
        }
        // the developer's own hook and permission stay; only ours went
        let settings: Value = serde_json::from_str(&std::fs::read_to_string(root.join(".claude/settings.json")).unwrap()).unwrap();
        assert_eq!(settings["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
        assert!(settings["hooks"]["PostToolUse"].to_string().contains("npm run lint"));
        assert!(settings["hooks"].get("Stop").is_none());
        assert_eq!(settings["permissions"]["allow"], json!(["mcp__collide"]));
        // a hooks file holding only Cork's entries goes altogether
        assert!(!root.join(".cursor/hooks.json").exists());
        // what the repo chose for itself stays; what setup wrote goes
        assert_eq!(config::load_json(&root.join(".collide/config.json")), json!({"verify": "cargo check"}));
        assert_eq!(std::fs::read_to_string(root.join(".gitignore")).unwrap(), "node_modules/\n");
        assert!(!has_repo_setup(&root) && !repo_runs_its_own(&root), "the machine's hooks take over");
        // staged for the person to commit, never committed
        let staged = Command::new("git").arg("-C").arg(&root).args(["diff", "--cached", "--name-only"]).output().unwrap();
        let staged = String::from_utf8_lossy(&staged.stdout).to_string();
        for path in [".collide/report_hook.py", ".collide/config.json", ".claude/settings.json", ".cursor/hooks.json", ".gitignore"] {
            assert!(staged.lines().any(|l| l == path), "{path} staged: {staged}");
        }
        assert_eq!(retire_repo_setup(&root), Vec::<String>::new(), "a second run finds nothing");
        // a settings file with none of ours is never rewritten
        assert_eq!(strip_repo_hooks(r#"{"hooks": {"Stop": [{"hooks": [{"command": "say done"}]}]}}"#), None);
        assert_eq!(strip_repo_hooks(&merge_claude_settings("", &Env::new())), None, "the machine's own entries are not the repo's");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn codex_and_cursor_get_the_machine_hooks_in_their_own_dialects() {
        let codex: Value = serde_json::from_str(&merge_codex_hooks(r#"{"hooks": {"Stop": [{"hooks": [{"command": "mine"}]}]}}"#, &Env::new())).unwrap();
        assert_eq!(codex["hooks"]["Stop"].as_array().unwrap().len(), 2, "theirs kept");
        assert!(codex["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap().contains("gate --harness codex"));
        assert_eq!(merge_codex_hooks(&merge_codex_hooks("", &Env::new()), &Env::new()), merge_codex_hooks("", &Env::new()), "installing twice changes nothing");
        let gates = codex["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(gates.len(), 2, "the write gate and the shell gate");
        assert_eq!(gates[0]["matcher"], "^(apply_patch|Edit|Write|MultiEdit|shell)$", "the trusted entry is unchanged");
        assert_eq!(gates[1]["matcher"], "^Bash$");
        let cursor: Value = serde_json::from_str(&merge_cursor_hooks("", &Env::new())).unwrap();
        assert_eq!(cursor["version"], json!(1));
        assert!(cursor["hooks"]["beforeSubmitPrompt"][0]["command"].as_str().unwrap().contains("report --harness cursor"));
        let removed: Value = serde_json::from_str(&remove_from_claude_settings(&merge_cursor_hooks("", &Env::new()))).unwrap();
        assert!(removed["hooks"].as_object().unwrap().is_empty(), "{removed}");
    }

    #[test]
    fn the_mcp_tools_are_added_beside_everything_else_and_taken_out_cleanly() {
        let entry = json!({"type": "stdio", "command": "/h/.collide/bin/collide", "args": ["mcp"]});
        let theirs = r#"{"numStartups": 9, "mcpServers": {"github": {"command": "gh-mcp"}}}"#;
        let added: Value = serde_json::from_str(&merge_mcp_json(theirs, Some(&entry)).unwrap()).unwrap();
        assert_eq!(added["numStartups"], json!(9));
        assert_eq!(added["mcpServers"]["github"]["command"], json!("gh-mcp"));
        assert_eq!(added["mcpServers"]["cork"]["args"], json!(["mcp"]));
        let removed: Value = serde_json::from_str(&merge_mcp_json(&added.to_string(), None).unwrap()).unwrap();
        assert!(removed["mcpServers"].get("cork").is_none() && removed["mcpServers"].get("github").is_some());
        // an entry under the name from before the rename is replaced, never doubled
        let old = r#"{"mcpServers": {"collide": {"type": "http", "url": "https://mcp.collidemcp.com/mcp"}, "github": {"command": "gh-mcp"}}}"#;
        let moved: Value = serde_json::from_str(&merge_mcp_json(old, Some(&entry)).unwrap()).unwrap();
        assert!(moved["mcpServers"].get("collide").is_none(), "{moved}");
        assert_eq!(moved["mcpServers"]["cork"]["args"], json!(["mcp"]));
        assert_eq!(moved["mcpServers"].as_object().unwrap().len(), 2);
        let gone: Value = serde_json::from_str(&merge_mcp_json(old, None).unwrap()).unwrap();
        assert!(gone["mcpServers"].get("collide").is_none() && gone["mcpServers"].get("github").is_some());
        assert!(merge_mcp_json("not json", Some(&entry)).is_none(), "a file that is not JSON is left alone");

        let toml = "model = \"o4\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let theirs = "model = \"o3\"\n\n[mcp_servers.collide]\nurl = \"https://api.corkcli.com/mcp\"\n\n[mcp_servers.collide.env]\nX = \"1\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let taken = merge_codex_mcp(theirs, Some(&entry));
        assert_eq!(taken.matches("[mcp_servers.cork]").count(), 1, "one cork table, never a duplicate key: {taken}");
        assert!(!taken.contains("[mcp_servers.collide]"), "the old name is replaced, not kept beside it: {taken}");
        assert!(!taken.contains("api.corkcli.com") && !taken.contains("[mcp_servers.collide.env]"));
        assert!(taken.contains("[mcp_servers.other]") && taken.contains("model = \"o3\""), "everything else stays");
        let once = merge_codex_mcp(toml, Some(&entry));
        assert_eq!(merge_codex_mcp(&once, Some(&entry)), once, "installing twice changes nothing");
        assert!(once.contains("[mcp_servers.other]") && once.contains("[mcp_servers.cork]\ncommand = \"/h/.collide/bin/collide\""), "{once}");
        assert_eq!(merge_codex_mcp(&once, None).trim(), toml.trim());
        // the block an install from before the rename wrote, mark and all
        let old = format!("{toml}\n{OLD_CODEX_MCP_MARK}\n[mcp_servers.collide]\ncommand = \"/h/.collide/bin/collide\"\nargs = [\"mcp\"]\n");
        let moved = merge_codex_mcp(&old, Some(&entry));
        assert!(!moved.contains("collide-machine") && !moved.contains("[mcp_servers.collide]") && moved.matches(CODEX_MCP_MARK).count() == 1, "{moved}");
        assert_eq!(merge_codex_mcp(&old, None).trim(), toml.trim());
    }

    #[test]
    fn agents_md_is_asked_for_once_per_repo_and_never_once_it_is_there() {
        let base = std::env::temp_dir().join(format!("collide-agentsmd-{}", std::process::id()));
        let repo = base.join("app");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "# House rules\nUse tabs.\n").unwrap();
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), base.join("home").to_string_lossy().to_string());
        let ask = agents_md_ask(&repo, &env).expect("first session is asked");
        assert!(ask.contains("ask the user") && ask.contains("agents-md"), "{ask}");
        let added = with_agents_section("# House rules\nUse tabs.\n");
        assert!(added.starts_with("# House rules\nUse tabs.\n\n<!-- cork:start -->"), "{added}");
        assert_eq!(with_agents_section(&added), added, "adding twice changes nothing");
        let older = added.replace("work as a team", "work together (old)");
        assert_eq!(with_agents_section(&older), added, "an older section is replaced in place");
        assert!(agents_md_ask(&repo, &env).is_none(), "the next session starting alongside is not");
        let _ = std::fs::remove_dir_all(base.join("home"));
        std::fs::write(repo.join("AGENTS.md"), format!("# House rules\n\n{AGENTS_SECTION}\n")).unwrap();
        assert!(agents_md_ask(&repo, &env).is_none(), "added: never asked again");
        // a section under the old markers counts as there, and is replaced by one
        std::fs::write(repo.join("AGENTS.md"), "<!-- collide:start -->\nold\n<!-- collide:end -->\n").unwrap();
        assert!(agents_md_ask(&repo, &env).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_old_collide_section_becomes_one_cork_section() {
        let old = "# Rules\n\n<!-- collide:start -->\n## Collide: old words\n<!-- collide:end -->\n\nMore rules.\n";
        let now = with_agents_section(old);
        assert_eq!(now, format!("# Rules\n\n{AGENTS_SECTION}\n\nMore rules.\n"));
        assert!(!now.contains("collide:start") && now.matches(SECTION_START).count() == 1);
        assert_eq!(with_agents_section(&now), now, "once replaced, nothing more changes");
        // both blocks present (an old one left beside a new one): one is left
        let both = format!("{old}\n{AGENTS_SECTION}\n");
        let one = with_agents_section(&both);
        assert_eq!(one.matches(SECTION_START).count(), 1, "{one}");
        assert!(!one.contains("collide:start") && one.contains("More rules."), "{one}");
        // the server's protocol block under the old markers is the same slot
        let server = "<!-- collide:start -->\n## Collide: cross-agent collision awareness (v26)\n<!-- collide:end -->";
        assert_eq!(with_agents_section(server), AGENTS_SECTION);
        assert!(has_agents_section(server) && has_agents_section(AGENTS_SECTION) && !has_agents_section("# none"));
    }

    #[test]
    fn a_stray_cork_download_on_an_old_machine_does_not_flip_its_folder() {
        let home = std::env::temp_dir().join(format!("cork-settle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        std::fs::create_dir_all(home.join(".collide")).unwrap();
        std::fs::write(home.join(".collide").join("machine.json"), r#"{"mode": "cloud"}"#).unwrap();
        // something of the person's own in ~/.cork: left alone
        std::fs::create_dir_all(home.join(".cork").join("notes")).unwrap();
        assert!(settle_layout(&env).is_none());
        assert!(home.join(".cork").exists());
        // only an installer's download (not this test binary): the stray folder goes
        std::fs::remove_dir_all(home.join(".cork").join("notes")).unwrap();
        std::fs::create_dir_all(home.join(".cork").join("bin")).unwrap();
        std::fs::write(home.join(".cork").join("bin").join("cork"), "x").unwrap();
        assert!(settle_layout(&env).is_none(), "the running program was not in it");
        assert!(!home.join(".cork").exists());
        assert!(config::legacy_layout(&env), "the machine stays in ~/.collide");
        assert_eq!(text(&settings(&env), "mode"), "cloud");
        // a machine with nothing in ~/.collide is a fresh one: nothing to settle
        let _ = std::fs::remove_file(home.join(".collide").join("machine.json"));
        std::fs::create_dir_all(home.join(".cork").join("bin")).unwrap();
        assert!(settle_layout(&env).is_none());
        assert!(home.join(".cork").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_install_from_before_the_rename_keeps_its_hook_entries_byte_for_byte() {
        let home = std::env::temp_dir().join(format!("cork-legacy-entries-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".collide").join("bin")).unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        assert!(config::legacy_layout(&env));
        // exactly what installs before the rename wrote: Codex trusts it by hash
        let report = "b=\"$HOME/.collide/bin/collide\"; [ -x \"$b\" ] || b=\"$b.exe\"; [ -x \"$b\" ] || exit 0; env CORK_MACHINE=1 \"$b\" report --harness codex || true; exit 0 # collide-machine";
        assert_eq!(machine_command_for("report", "codex", &env), report);
        assert_eq!(codex_label("SessionStart", None, true), "Collide: brief the agent at session start");
        let codex = merge_codex_hooks("", &env);
        assert!(codex.contains("Collide: brief the agent at session start") && !codex.contains("cork"), "{codex}");
        assert_eq!(program_path(&env), home.join(".collide").join("bin").join(exe("collide")));
        assert_eq!(cork_path(&env), home.join(".collide").join("bin").join(exe("cork")));
        // a newer program puts a `cork` beside the old `collide`
        std::fs::write(program_path(&env), "the program").unwrap();
        place_cork_beside(&env);
        assert_eq!(std::fs::read_to_string(cork_path(&env)).unwrap(), "the program");
        assert_eq!(command_path(&env), cork_path(&env));
        // an old entry is found and replaced by the merge, never doubled
        let merged = merge_claude_settings(&merge_claude_settings("", &env), &env);
        let parsed: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["hooks"]["Stop"].as_array().unwrap().len(), 1, "{merged}");
        // a fresh home: the new layout, the new entries, and they replace an old one
        let fresh = std::env::temp_dir().join(format!("cork-fresh-entries-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fresh);
        let mut new_env = Env::new();
        new_env.insert("HOME".into(), fresh.to_string_lossy().to_string());
        assert!(!config::legacy_layout(&new_env));
        assert_eq!(program_path(&new_env), fresh.join(".cork").join("bin").join(exe("cork")));
        let command = machine_command_for("gate", "claude", &new_env);
        assert!(command.contains("$HOME/.cork/bin/cork") && command.contains("CORK_MACHINE=1") && command.ends_with("# cork-machine"), "{command}");
        assert_eq!(codex_label("Stop", None, false), "Cork: record the end of each turn");
        let replaced: Value = serde_json::from_str(&merge_claude_settings(&merged, &new_env)).unwrap();
        let stop = replaced["hooks"]["Stop"].to_string();
        assert_eq!(replaced["hooks"]["Stop"].as_array().unwrap().len(), 1, "{stop}");
        assert!(stop.contains("# cork-machine") && !stop.contains("# collide-machine"));
        assert_eq!(remove_from_claude_settings(&merged).matches("collide-machine").count(), 0);
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&fresh);
    }

    #[cfg(unix)]
    fn old_machine(name: &str) -> (PathBuf, Env) {
        let home = std::env::temp_dir().join(format!("cork-move-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let old = home.join(".collide");
        std::fs::create_dir_all(old.join("bin")).unwrap();
        std::fs::create_dir_all(old.join("local")).unwrap();
        std::fs::write(old.join("machine.json"), r#"{"mode": "cloud", "settings_version": 94}"#).unwrap();
        std::fs::write(old.join("local").join("collide.db"), "history").unwrap();
        // what installs before this one left: the program twice, as two copies
        std::fs::write(old.join("bin").join("collide"), "v94").unwrap();
        std::fs::write(old.join("bin").join("cork"), "v94").unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        (home, env)
    }

    #[cfg(unix)]
    /// The PATH lines the installers wrote follow the move to `~/.cork`:
    /// a new terminal runs `cork` from its own folder. Only Cork's lines
    /// change; a file that already names `~/.cork/bin` loses the old one.
    #[cfg(unix)]
    #[test]
    fn the_shells_path_lines_follow_the_move() {
        let (home, env) = old_machine("pathlines");
        let zshrc = "alias ll='ls -l'\n\n# Collide\nexport PATH=\"$HOME/.collide/bin:$PATH\"\nexport EDITOR=vim\n";
        let bashrc = "# Collide\nexport PATH=\"$HOME/.collide/bin:$PATH\"\n\n# Cork\nexport PATH=\"$HOME/.cork/bin:$PATH\"\n";
        std::fs::write(home.join(".zshrc"), zshrc).unwrap();
        std::fs::write(home.join(".bashrc"), bashrc).unwrap();
        std::fs::create_dir_all(home.join(".config/fish/conf.d")).unwrap();
        std::fs::write(home.join(".config/fish/conf.d/collide.fish"), "fish_add_path \"$HOME/.collide/bin\"\n").unwrap();
        // still on ~/.collide: nothing changes yet
        repoint_path_lines(&env);
        assert_eq!(std::fs::read_to_string(home.join(".zshrc")).unwrap(), zshrc);
        let _ = move_to_cork(&env);
        repoint_path_lines(&env);
        assert_eq!(
            std::fs::read_to_string(home.join(".zshrc")).unwrap(),
            "alias ll='ls -l'\n\n# Cork\nexport PATH=\"$HOME/.cork/bin:$PATH\"\nexport EDITOR=vim\n"
        );
        assert_eq!(std::fs::read_to_string(home.join(".bashrc")).unwrap(), "\n# Cork\nexport PATH=\"$HOME/.cork/bin:$PATH\"\n");
        assert_eq!(std::fs::read_to_string(home.join(".config/fish/conf.d/collide.fish")).unwrap(), "fish_add_path \"$HOME/.cork/bin\"\n");
        // Cork elsewhere (CORK_HOME): the shell's files are not its to change
        let mut elsewhere = env.clone();
        elsewhere.insert("CORK_HOME".into(), home.join("other").to_string_lossy().to_string());
        std::fs::write(home.join(".profile"), "export PATH=\"$HOME/.collide/bin:$PATH\"\n").unwrap();
        repoint_path_lines(&elsewhere);
        assert!(std::fs::read_to_string(home.join(".profile")).unwrap().contains(".collide/bin"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_old_machine_moves_to_cork_and_its_old_paths_keep_working() {
        let (home, env) = old_machine("old");
        assert!(config::legacy_layout(&env));
        // the Codex entries it was trusted with, and Claude Code's, before the move
        let codex_before = merge_codex_hooks("", &env);
        let claude_before = merge_claude_settings("", &env);
        assert!(move_to_cork(&env).is_none(), "this test binary was not in the folder");
        // one real folder, the old name a link to it
        assert!(std::fs::symlink_metadata(home.join(".cork")).unwrap().is_dir());
        assert_eq!(std::fs::read_link(home.join(".collide")).unwrap(), Path::new(".cork"));
        assert!(!config::legacy_layout(&env));
        assert_eq!(text(&settings(&env), "mode"), "cloud", "the setup came along");
        assert_eq!(std::fs::read_to_string(local_dir(&env).join("collide.db")).unwrap(), "history");
        // one program: `collide` is a link to `cork`, through either folder name
        let bin = home.join(".cork").join("bin");
        assert_eq!(std::fs::read_link(bin.join("collide")).unwrap(), Path::new("cork"));
        assert_eq!(std::fs::read_to_string(home.join(".collide/bin/collide")).unwrap(), "v94");
        assert!(same_file(&home.join(".collide/bin/collide"), &program_path(&env)));
        assert_eq!(program_path(&env), bin.join("cork"));
        // Claude Code's entries move to the new paths; Codex keeps the exact
        // text it trusts, which still runs the program through the links
        let claude_after = merge_claude_settings(&claude_before, &env);
        assert!(claude_after.contains("$HOME/.cork/bin/cork") && !claude_after.contains(".collide/bin"), "{claude_after}");
        assert_eq!(merge_codex_hooks(&codex_before, &env), codex_before);
        assert!(merge_codex_hooks("", &env).contains("$HOME/.cork/bin/cork"), "a new Codex setup gets the new entries");
        // a local server started before the move answers with the old folder's id: still ours
        assert!(instance_ids(&env).contains(&instance_id_of(&home.join(".collide").join("local"))));
        // done once: a second run finds nothing to move
        assert!(move_to_cork(&env).is_none());
        assert_eq!(std::fs::read_link(home.join(".collide")).unwrap(), Path::new(".cork"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn an_installers_download_becomes_the_program_when_the_machine_moves() {
        let (home, env) = old_machine("download");
        std::fs::create_dir_all(home.join(".cork").join("bin")).unwrap();
        std::fs::write(home.join(".cork").join("bin").join("cork"), "v95").unwrap();
        let _ = move_to_cork(&env);
        let bin = home.join(".cork").join("bin");
        assert_eq!(std::fs::read_to_string(bin.join("cork")).unwrap(), "v95");
        assert_eq!(std::fs::read_to_string(bin.join("collide")).unwrap(), "v95", "the old name runs the new program");
        assert_eq!(text(&settings(&env), "mode"), "cloud");
        assert!(std::fs::read_dir(&home).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".cork-download")));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn a_cork_folder_of_the_persons_own_is_never_merged_into() {
        let (home, env) = old_machine("own");
        std::fs::create_dir_all(home.join(".cork").join("notes")).unwrap();
        assert!(move_to_cork(&env).is_none());
        assert!(std::fs::symlink_metadata(home.join(".collide")).unwrap().is_dir(), "the old folder stays where it is");
        assert!(home.join(".cork").join("notes").is_dir() && !home.join(".cork").join("machine.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn a_fresh_machine_gets_the_old_name_as_a_link() {
        let home = std::env::temp_dir().join(format!("cork-fresh-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".cork").join("bin")).unwrap();
        std::fs::write(home.join(".cork").join("bin").join("cork"), "v95").unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        place_cork_beside(&env);
        assert_eq!(std::fs::read_link(home.join(".cork/bin/collide")).unwrap(), Path::new("cork"));
        // an old copy where the link belongs is replaced by it
        std::fs::remove_file(home.join(".cork/bin/collide")).unwrap();
        std::fs::write(home.join(".cork/bin/collide"), "v80").unwrap();
        place_cork_beside(&env);
        assert_eq!(std::fs::read_to_string(home.join(".cork/bin/collide")).unwrap(), "v95");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A stand-in cloud that answers /machine/status with `paid` (and `cloud`,
    /// as servers since v100 do), once per request.
    fn status_server(paid: bool, cloud: Option<bool>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                // the whole request (headers, then the body they announce) before answering
                let mut got: Vec<u8> = Vec::new();
                let mut buffer = [0u8; 8192];
                loop {
                    let Ok(n) = stream.read(&mut buffer) else { break };
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&got).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length: usize = text[..end].lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                            .unwrap_or(0);
                        if got.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let mut body = json!({"ok": true, "paid": paid, "plan": if paid { "team" } else { "free" }});
                if let Some(cloud) = cloud {
                    body["cloud"] = json!(cloud);
                }
                let body = body.to_string();
                let _ = write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            }
        });
        base
    }

    #[test]
    fn a_signed_in_machine_works_with_its_workspace_on_free_too() {
        let home = std::env::temp_dir().join(format!("collide-lapse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        env.insert("CORK_HOME".into(), home.to_string_lossy().to_string());
        let machine = |server: &str, mode: &str| {
            json!({"mode": mode, "server_url": server, "workspace": "w1",
                   "account": {"user": "a@x", "workspace": "w1", "workspace_name": "Acme", "server_url": server, "checked": 0.0}})
        };
        let creds = |server: &str| std::fs::write(config::credentials_path(&env), json!({format!("{server}#w1"): "cat_x"}).to_string()).unwrap();
        std::fs::create_dir_all(home.join(".collide")).unwrap();

        // signed in on Free before v100, so kept local: it moves over at its next check
        let free = status_server(false, Some(true));
        creds(&free);
        assert!(save_settings(&env, &machine(&free, "local")));
        let told = plan_check(&env).expect("the move is told");
        assert!(told.contains("switched to Acme") && told.contains("on Free"), "{told}");
        assert_eq!(text(&settings(&env), "mode"), "cloud");
        assert!(plan_check(&env).is_none(), "checked: not again for 15 minutes");

        // on the cloud, Free keeps it there: units ending is not a reason to leave
        assert!(save_settings(&env, &machine(&free, "cloud")));
        assert!(plan_check(&env).is_none());
        assert_eq!(text(&settings(&env), "mode"), "cloud");

        // a server from before v100 (no `cloud`) lets an unpaid machine go: the free version carries on
        let old = status_server(false, None);
        creds(&old);
        assert!(save_settings(&env, &machine(&old, "cloud")));
        let told = plan_check(&env).expect("the move back is told");
        assert!(told.contains("no longer takes this machine") && told.contains("free and local"), "{told}");
        assert_eq!(text(&settings(&env), "mode"), "local");
        assert!(settings(&env).get("account").is_some(), "the account is kept");

        // paid, from either server: the machine is on the cloud
        let paid = status_server(true, None);
        creds(&paid);
        assert!(save_settings(&env, &machine(&paid, "local")));
        let told = plan_check(&env);
        assert!(told.as_deref().unwrap_or("").contains("switched to Acme"), "{told:?} {}", settings(&env));
        assert_eq!(text(&settings(&env), "mode"), "cloud");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn only_a_hosted_remote_is_a_repo_a_team_can_share() {
        for shared in ["github.com/acme/api", "gitlab.com/group/sub/app", "git.acme.io/team/app"] {
            assert!(is_hosted(shared), "{shared}");
        }
        for local in ["local/my-app", "my-app", "acme/api", "C:/Users/me/app", ""] {
            assert!(!is_hosted(local), "{local}");
        }
    }

    #[test]
    fn a_message_command_with_settings_in_front_is_still_signed_by_its_agent() {
        let input = |command: &str| json!({"tool_name": "Bash", "session_id": "s-1", "tool_input": {"command": command}}).to_string();
        for command in ["~/.collide/bin/collide message hi", "CORK_HOME=/tmp/h /tmp/h/.collide/bin/collide message hi",
                        "~/.cork/bin/cork message hi", "CORK_HOME=/tmp/h /tmp/h/.cork/bin/cork message hi", "cork message hi",
                        "~/.collide/bin/cork message hi"] {
            let out = message_rewrite(&input(command)).expect(command);
            assert!(out.contains("CORK_SESSION=s-1"), "{out}");
        }
        assert!(message_rewrite(&input("COLLIDE_SESSION=s-0 cork message hi")).is_none(), "stamped under the old name");
        for command in [
            "collide message \"renamed amount_due -> checkout_total; switch over\"",
            "collide message 'a | b & c' 2>&1",
            "collide message \\\n  \"hi there\"",
        ] {
            assert!(message_rewrite(&input(command)).is_some(), "{command}");
        }
        assert!(message_rewrite(&input("collide message \"$(cat ~/.ssh/id_rsa)\"")).is_none());
        assert!(message_rewrite(&input("collide message hi > /etc/x")).is_none());
        assert!(message_rewrite(&input("cd x\ncollide message hi")).is_none());
        assert!(message_rewrite(&input("CORK_HOME=/tmp/h ls")).is_none());
        assert!(message_rewrite(&input("collide message hi && rm -rf x")).is_none());
    }

    /// `cork handoff` is signed like a message; a helper (a subagent, whose
    /// hook input names its `agent_id`) signs as itself too, so its message
    /// never comes back to it and reaches its lead.
    #[test]
    fn a_handoff_and_a_helpers_message_are_signed_by_their_agent() {
        let out = message_rewrite(&json!({"tool_name": "Bash", "session_id": "s-1",
            "tool_input": {"command": "cork handoff \"Build the invite panel\""}}).to_string()).expect("handoff");
        assert!(out.contains("CORK_SESSION=s-1 cork handoff") && out.contains("a handoff to another agent"), "{out}");
        let out = message_rewrite(&json!({"tool_name": "Bash", "session_id": "s-1", "agent_id": "a7f3c2",
            "tool_input": {"command": "cork message --about static/css/app.css hi"}}).to_string()).expect("helper");
        assert!(out.contains("CORK_SESSION=s-1 CORK_WORKER=a7f3c2 cork message"), "{out}");
        assert!(message_rewrite(&json!({"tool_name": "Bash", "session_id": "s-1",
            "tool_input": {"command": "cork status"}}).to_string()).is_none());
    }

    #[test]
    fn a_repo_without_an_origin_is_named_by_its_folder() {
        let dir = std::env::temp_dir().join(format!("collide-machine-{}", std::process::id())).join("my-app");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(repo_id_for(&dir), "local/my-app");
    }
}

// ------------------------------------------------------------ welcome

/// What an agent in a repo on the free version is told when its session
/// starts: the others are on this machine, and how to reach them.
pub fn free_note(env: &Env) -> String {
    format!(
        "Cork (free, on this machine): other agents working in this repo on this machine see your edits as you make them, \
and you see theirs. To tell them something, run: {} \"...\" (add --to agent-xxxx for one agent, \
or --about path.py::symbol for whoever is on that code).",
        message_command(env)
    )
}

fn welcomed_path(session: &str, env: &Env) -> Option<PathBuf> {
    let name: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(64).collect();
    (!name.is_empty()).then(|| local_dir(env).join("welcomed").join(name))
}

/// A session that started with Cork's session start has heard what it is.
pub fn mark_welcomed(session: &str, env: &Env) {
    if !installed(env) {
        return;
    }
    let Some(path) = welcomed_path(session, env) else { return };
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, "1");
    // a week on, no session still needs its marker
    let week = Duration::from_secs(7 * 86_400);
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let old = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|age| age > week);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A session already open when Cork was installed missed that start:
/// Claude Code picks the new hooks up mid-session, so its next prompt is
/// told, once, that Cork is on and what changes.
pub fn welcome_once(hook_input: &Value, env: &Env) -> Option<String> {
    if !installed(env) {
        return None;
    }
    let path = welcomed_path(&text(hook_input, "session_id"), env)?;
    if path.exists() {
        return None;
    }
    let cwd = {
        let given = text(hook_input, "cwd");
        if given.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(given) }
    };
    let root = config::find_repo_root(&[Some(cwd)])?;
    if repo_runs_its_own(&root) {
        return None; // its own hooks greeted it at its start
    }
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return None;
    }
    mark_welcomed(&text(hook_input, "session_id"), env);
    Some(if cfg.machine_local {
        format!("Cork was just installed on this machine and is on for this session from now on. {}", free_note(env))
    } else {
        "Cork was just installed on this machine and is on for this session from now on: your edits reach your team's \
workspace as you make them, briefings arrive with your prompts, and the write gate checks every edit against teammates' \
live work. Tell the user this in one line."
            .to_string()
    })
}

// ------------------------------------------------------------ agent messages

/// The program an agent or a person is told to run: the machine's `cork`
/// (beside an old install's `collide` once it was placed there), else the
/// program the hooks run.
pub fn command_path(env: &Env) -> PathBuf {
    let cork = cork_path(env);
    let program = program_path(env);
    if cork.is_file() || !program.is_file() { cork } else { program }
}

/// `~/.cork/bin/cork <sub>`, written the short way, for a line a person reads.
pub fn cork_command(env: &Env, sub: &str) -> String {
    format!("{}{} {sub}", home_prefix(env), tilde(&command_path(env), env))
}

/// The exact command an agent on this machine runs to message the others:
/// this program's own path (its `cork` when one sits beside it), with its
/// home when that is not the default (a `~/.cork/bin/cork` that does not
/// exist reads as no way to talk).
pub fn message_command(env: &Env) -> String {
    own_command(env, "message")
}

/// `<this program> <sub>`, exactly as an agent on this machine runs it: the
/// running program's own path (its `cork` when one sits beside an old
/// `collide`), else the machine's; a repo's old `collide-hook` runs as is.
pub fn own_command(env: &Env, sub: &str) -> String {
    let program = std::env::current_exe()
        .ok()
        .and_then(|me| {
            let stem = me.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            if is_program_name(&stem) {
                let cork = me.with_file_name(exe(PROGRAM));
                Some(if cork.is_file() { cork } else { me })
            } else if stem == "collide-hook" {
                Some(me)
            } else {
                None
            }
        })
        .unwrap_or_else(|| command_path(env));
    format!("{}{} {sub}", home_prefix(env), program.display())
}

/// `CORK_HOME=... ` when Cork's home is not the default one, for a
/// command an agent runs in its own shell, which does not carry it.
pub fn home_prefix(env: &Env) -> String {
    let cork_home = config::get(env, "CORK_HOME");
    let home = config::get(env, "HOME");
    if !cork_home.is_empty() && cork_home != home { format!("CORK_HOME={cork_home} ") } else { String::new() }
}

/// Whether a command's program word is Cork's (`cork`, the `collide` of an
/// install from before the rename, or a repo's old `collide-hook`), by
/// name or by path.
fn is_our_program(word: &str) -> bool {
    ["cork", "cork.exe", "collide", "collide.exe", "collide-hook", "collide-hook.exe"]
        .iter()
        .any(|name| word == *name || word.ends_with(&format!("/{name}")) || word.ends_with(&format!("\\{name}")))
}

/// Whether a command already carries the session stamp, under either name.
fn stamped(command: &str) -> bool {
    command.contains("CORK_SESSION=") || command.contains("COLLIDE_SESSION=")
}

/// PreToolUse (Bash): an agent about to run `cork message ...` gets
/// its session stamped on the command (`CORK_SESSION=...`), so the
/// message is signed by that agent rather than by the person. Only a command
/// that is nothing but the message is rewritten and let through: anything
/// chained onto it (`;`, `&&`, pipes, redirects, substitutions) is left
/// exactly as it was, for the harness's own permission check.
pub fn message_rewrite(stdin_data: &str) -> Option<String> {
    let input: Value = serde_json::from_str(stdin_data).ok()?;
    if input.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let command = input.pointer("/tool_input/command").and_then(Value::as_str)?.trim().to_string();
    let session = text(&input, "session_id");
    if session.is_empty() || stamped(&command) {
        return None;
    }
    // a line continuation is a space; `2>&1` only joins its own output
    let command = command.replace("\\\n", " ").replace(" 2>&1", "");
    // `VAR=value` settings may come first; the program is the word after them
    let words: Vec<&str> = command.split_whitespace().collect();
    let at = words.iter().position(|w| {
        let name = w.split('=').next().unwrap_or("");
        !(w.contains('=') && !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
    });
    let Some(at) = at else { return None };
    let first = words[at];
    let sub = words.get(at + 1).copied().unwrap_or("");
    if !is_our_program(first) || !matches!(sub, "message" | "handoff") {
        return None;
    }
    if chains_anything(&command) {
        return None;
    }
    let safe = |id: &str| -> String { id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect() };
    // a helper (a subagent) runs in its lead's session: its own id too, so
    // its message is signed by that helper and never handed back to it
    let worker = safe(&text(&input, "agent_id"));
    let stamp = if worker.is_empty() {
        format!("CORK_SESSION={}", safe(&session))
    } else {
        format!("CORK_SESSION={} CORK_WORKER={worker}", safe(&session))
    };
    let mut updated = input.get("tool_input").cloned().unwrap_or(json!({}));
    updated["command"] = json!(format!("{stamp} {command}"));
    let reason = if sub == "handoff" { "Cork: a handoff to another agent." } else { "Cork: a message to the other agents." };
    Some(
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "permissionDecisionReason": reason,
            "updatedInput": updated,
        }})
        .to_string(),
    )
}

/// PreToolUse (Bash): `cork commit --mine ...` (or `cork status
/// --mine`) gets its session stamped (`CORK_SESSION=...`), so it knows
/// whose lines are whose. A chain after it is fine: the stamp sets only the
/// first command. A session that asks before commands still asks.
pub fn commit_rewrite(stdin_data: &str) -> Option<String> {
    let input: Value = serde_json::from_str(stdin_data).ok()?;
    if input.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let command = input.pointer("/tool_input/command").and_then(Value::as_str)?.trim().to_string();
    let session = text(&input, "session_id");
    if session.is_empty() || stamped(&command) || command.contains("--session") {
        return None;
    }
    let first_line = command.lines().next().unwrap_or("");
    let words: Vec<&str> = first_line.split_whitespace().collect();
    let at = words.iter().position(|w| {
        let name = w.split('=').next().unwrap_or("");
        !(w.contains('=') && !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
    })?;
    let first = words[at];
    let is_ours = is_our_program(first);
    let sub = words.get(at + 1).copied().unwrap_or("");
    let mine = words.get(at + 2..).unwrap_or_default().iter().take_while(|w| !matches!(**w, "&&" | ";" | "||" | "|")).any(|w| *w == "--mine");
    if !is_ours || !matches!(sub, "commit" | "status") || !mine {
        return None;
    }
    let safe_session: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let mut updated = input.get("tool_input").cloned().unwrap_or(json!({}));
    updated["command"] = json!(format!("CORK_SESSION={safe_session} {command}"));
    let decision = if input.get("permission_mode").and_then(Value::as_str) == Some("bypassPermissions") { "allow" } else { "ask" };
    Some(
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": "Cork: commit only the lines this session wrote.",
            "updatedInput": updated,
        }})
        .to_string(),
    )
}

/// Whether a command does anything besides run one program: chaining,
/// pipes, redirects, substitutions or a second line, outside quotes. Text in
/// single quotes is inert; in double quotes only a substitution is live.
fn chains_anything(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let (mut single, mut double) = (false, false);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if !single => i += 1,
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '`' if !single => return true,
            '$' if !single && chars.get(i + 1) == Some(&'(') => return true,
            ';' | '&' | '|' | '>' | '<' | '\n' if !single && !double => return true,
            _ => {}
        }
        i += 1;
    }
    single || double
}

/// Where the hooks note which agent session each harness process runs.
fn agents_dir(env: &Env) -> PathBuf {
    local_dir(env).join("agents")
}

/// A process's parent and its program name, or None when unknown.
#[cfg(unix)]
fn parent_of(pid: u32) -> Option<(u32, String)> {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // pid (comm) state ppid ...: comm may hold spaces, so split at the last ')'
        let close = stat.rfind(')')?;
        let comm = stat[stat.find('(')? + 1..close].to_string();
        let ppid = stat[close + 1..].split_whitespace().nth(1)?.parse().ok()?;
        return Some((ppid, comm));
    }
    let out = Command::new("ps").args(["-o", "ppid=,comm=", "-p", &pid.to_string()]).output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (ppid, comm) = line.split_once(char::is_whitespace)?;
    Some((ppid.trim().parse().ok()?, comm.trim().rsplit('/').next().unwrap_or("").to_string()))
}

#[cfg(not(unix))]
fn parent_of(_pid: u32) -> Option<(u32, String)> {
    None
}

fn is_shell(comm: &str) -> bool {
    let name = comm.trim_start_matches('-');
    matches!(name, "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh")
}

/// How long a session counts as at work in its harness process: one
/// process may host several agents at once (Cursor's chats share its app).
const AGENT_PROCESS_WINDOW_S: f64 = 120.0;

/// A hook runs as a child of the agent's harness (sometimes through a
/// shell). Note when each session last ran in that process, so a `cork
/// message` the agent runs, a descendant of the same process, is signed by
/// the agent even when its command was not stamped with the session.
fn note_agent_process(session: &str, env: &Env) {
    if session.is_empty() || session.contains(char::is_whitespace) {
        return;
    }
    let Some((mut harness, comm)) = parent_of(std::process::id()) else { return };
    if is_shell(&comm) {
        let Some((up, _)) = parent_of(harness) else { return };
        harness = up;
    }
    if harness <= 1 {
        return;
    }
    let path = agents_dir(env).join(harness.to_string());
    let stamp = now_s();
    let mut seen: Vec<(String, f64)> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(' ').and_then(|(s, t)| Some((s.to_string(), t.parse().ok()?))))
        .filter(|(s, t)| s != session && stamp - t < AGENT_PROCESS_WINDOW_S)
        .collect();
    seen.push((session.to_string(), stamp));
    let body: String = seen.iter().map(|(s, t)| format!("{s} {t:.0}\n")).collect();
    let _ = std::fs::create_dir_all(agents_dir(env));
    let _ = std::fs::write(&path, body);
    sweep_agent_records(&agents_dir(env), stamp);
}

/// How often the records of exited processes are swept, and how many go
/// in one sweep (a hook's time is short; a backlog clears over a few).
const AGENT_SWEEP_EVERY_S: f64 = 3600.0;
const AGENT_SWEEP_MAX: usize = 5000;

/// Take out the records no live session is in any more. Left behind, every
/// pid ends up with one (the system reuses them), and a `cork message`
/// looking for its own session found a dead process's record first.
fn sweep_agent_records(dir: &Path, stamp: f64) {
    let marker = dir.join(".swept");
    let swept = std::fs::metadata(&marker).and_then(|m| m.modified()).ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64());
    if swept.is_some_and(|t| stamp - t < AGENT_SWEEP_EVERY_S) {
        return;
    }
    let _ = std::fs::write(&marker, "");
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let stale = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|t| stamp - t.as_secs_f64() > AGENT_PROCESS_WINDOW_S);
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path == marker || !stale(&path) {
            continue;
        }
        let _ = std::fs::remove_file(&path);
        removed += 1;
        if removed >= AGENT_SWEEP_MAX {
            // the rest go next time: the marker is cleared so that is soon
            let _ = std::fs::remove_file(&marker);
            break;
        }
    }
}

/// The agent session this command runs under: CORK_SESSION when stamped,
/// else the one session at work in the nearest ancestor process a hook
/// noted. Two at work in one process: no way to tell, so no session.
pub(crate) fn running_session(env: &Env) -> String {
    let given = config::get(env, "CORK_SESSION");
    if !given.is_empty() {
        return given.to_string();
    }
    let stamp = now_s();
    let mut pid = std::process::id();
    for _ in 0..8 {
        let Some((up, _)) = parent_of(pid) else { break };
        if up <= 1 {
            break;
        }
        if let Ok(body) = std::fs::read_to_string(agents_dir(env).join(up.to_string())) {
            let live: Vec<&str> = body
                .lines()
                .filter_map(|line| line.split_once(' '))
                .filter(|(_, t)| t.parse::<f64>().is_ok_and(|t| stamp - t < AGENT_PROCESS_WINDOW_S))
                .map(|(s, _)| s)
                .collect();
            // nothing live: a record left by an earlier process that had
            // this pid (pids are reused), so the search goes on up
            match live.len() {
                0 => {}
                1 => return live[0].to_string(),
                _ => return String::new(),
            }
        }
        pid = up;
    }
    String::new()
}

/// `cork message [--to AGENT | --about path.py::symbol] TEXT`: tell
/// the other agents in this repo something. With neither flag it goes to
/// every agent at work in the repo right now. With `--channel NAME` (and
/// any `--attach PATH`) it is a post in one of the workspace's channels
/// instead (`post_to_channel`).
pub fn message(args: &[String], env: &Env) -> i32 {
    send_to_agents(args, env, false)
}

/// The largest file `--attach` sends: the paid plans' limit (the server
/// says no sooner on Free).
const ATTACH_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// The type a file is sent as, from its name. The server reads the bytes
/// and decides; this only says what the file claims to be.
fn attach_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

/// The channel `named` names in a channel list: by id, or by name with or
/// without its `#`, in any case.
fn channel_in(list: &Value, named: &str) -> Option<Value> {
    let name = named.trim().trim_start_matches('#').to_lowercase().replace(' ', "-");
    list.get("channels")?.as_array()?.iter().find(|c| text(c, "id") == named.trim() || text(c, "name") == name).cloned()
}

/// `cork message --channel NAME [--attach PATH]... "words"`: a post in one
/// of the workspace's channels, signed by this agent (its session, as the
/// hooks stamped it), seen by every teammate who can see the channel and
/// heard by the agents it @mentions. `--to @lee` mentions people. Each file
/// goes up first and the post carries them: shared because it was asked,
/// so its bytes leave the machine as they are.
fn post_to_channel(env: &Env, named: &str, said: &str, to: &str, attach: &[String]) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(root) = config::find_repo_root(&[Some(cwd)]) else {
        println!("Cork: run this inside a repository Cork covers.");
        return 1;
    };
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() || cfg.machine_local {
        println!("Cork: channels need a Cork account; this repository is not connected to one.");
        return 1;
    }
    let ua = crate::report::user_agent();
    let budget = Duration::from_secs(10);
    // the workspace this repo's credential belongs to
    let me = crate::http::get(&cfg.server, "/api/me", &cfg.token, &ua, budget).unwrap_or(Value::Null);
    let workspace = text(&me, "workspace");
    if workspace.is_empty() {
        println!("Cork: the message was not sent (this credential names no workspace: run `cork login` again).");
        return 1;
    }
    let base = format!("/api/workspaces/{workspace}");
    let list = crate::http::get(&cfg.server, &format!("{base}/channels"), &cfg.token, &ua, budget).unwrap_or(Value::Null);
    let Some(channel) = channel_in(&list, named) else {
        let names: Vec<String> = list
            .get("channels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| format!("#{}", text(c, "name")))
            .collect();
        let known = if names.is_empty() { String::new() } else { format!(": the channels you can see are {}", names.join(", ")) };
        println!("Cork: the message was not sent (no channel called #{}{known}).", named.trim_start_matches('#'));
        return 1;
    };
    let (id, name) = (text(&channel, "id"), text(&channel, "name"));
    let mut files: Vec<String> = Vec::new();
    for path in attach {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if size > ATTACH_MAX_BYTES {
            println!("Cork: the message was not sent ({path} is over 100 MB).");
            return 1;
        }
        let Ok(bytes) = std::fs::read(path) else {
            println!("Cork: the message was not sent (could not read {path}).");
            return 1;
        };
        let file_name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let query: String = file_name
            .bytes()
            .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
            .collect();
        let endpoint = format!("{base}/channels/{id}/files?name={query}");
        match crate::http::send(&cfg.server, &endpoint, &cfg.token, &ua, attach_type(path), &bytes, Duration::from_secs(300)) {
            Ok((200, answer)) if !text(&answer, "id").is_empty() => files.push(text(&answer, "id")),
            Ok((_, answer)) => {
                let why = text(&answer, "error");
                println!("Cork: the message was not sent ({file_name}: {}).", if why.is_empty() { "the upload was refused" } else { &why });
                return 1;
            }
            Err(()) => {
                println!("Cork: the message was not sent (could not reach the server to upload {file_name}).");
                return 1;
            }
        }
    }
    // `--to @lee, @max/2`: mentions, before the words
    let pings: Vec<String> = to
        .split([',', ' '])
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| if h.starts_with('@') { h.to_string() } else { format!("@{h}") })
        .filter(|h| !said.contains(h.as_str()))
        .collect();
    let words = if pings.is_empty() { said.trim().to_string() } else { format!("{} {}", pings.join(" "), said.trim()) };
    let payload = crate::privacy::outbound(&cfg.server, "/channel-message", &json!({
        "text": words, "attachments": files, "session": running_session(env), "agent": "cork-cli",
    }));
    let endpoint = format!("{base}/channels/{id}/messages");
    match crate::http::send(&cfg.server, &endpoint, &cfg.token, &ua, "application/json", payload.to_string().as_bytes(), budget) {
        Ok((200, answer)) => {
            let heard = answer.get("delivered_to").and_then(Value::as_u64).unwrap_or(0);
            let with = match files.len() {
                0 => String::new(),
                1 => " with 1 file".to_string(),
                n => format!(" with {n} files"),
            };
            let reach = match heard {
                0 => String::new(),
                1 => "; 1 person's agents hear it".to_string(),
                n => format!("; {n} people's agents hear it"),
            };
            println!("Cork: posted in #{name}{with}{reach}.");
            0
        }
        Ok((_, answer)) => {
            let why = text(&answer, "error");
            println!("Cork: the message was not sent{}.", if why.is_empty() { String::new() } else { format!(" ({why})") });
            1
        }
        Err(()) => {
            println!("Cork: the message was not sent (could not reach the server).");
            1
        }
    }
}

/// `cork handoff [--to AGENT] SPEC`: work the person wants another agent
/// to do, as a self-contained spec. It goes to ONE agent: their other
/// session open in this repo (the one `--to` names), which starts on it by
/// itself; with none open it waits, and the next session started here
/// takes it.
pub fn handoff(args: &[String], env: &Env) -> i32 {
    send_to_agents(args, env, true)
}

fn send_to_agents(args: &[String], env: &Env, handoff: bool) -> i32 {
    let mut to = String::new();
    let mut about = String::new();
    let mut retract = String::new();
    let mut show = String::new();
    let mut status = false;
    let mut channel = String::new();
    let mut attach: Vec<String> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // a post in one of the workspace's channels, and files with it
            "--channel" if !handoff => {
                channel = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--attach" if !handoff => {
                attach.extend(args.get(i + 1).cloned());
                i += 2;
            }
            // what this agent sent here lately, and how far each got
            "--status" if !handoff => {
                status = true;
                i += 1;
            }
            "--to" => {
                to = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--about" => {
                about = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            // take one back: its id from the send, or "last"
            "--retract" => {
                retract = args.get(i + 1).cloned().unwrap_or_else(|| "last".to_string());
                i += 2;
            }
            // one handed over cut, read whole, with who sent it
            "--show" if !handoff => {
                show = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            word => {
                words.push(word.to_string());
                i += 1;
            }
        }
    }
    let said = words.join(" ");
    let asks_help = matches!(said.trim(), "--help" | "-h" | "help");
    if !channel.trim().is_empty() && !asks_help && (!said.trim().is_empty() || !attach.is_empty()) {
        return post_to_channel(env, channel.trim(), &said, &to, &attach);
    }
    if retract.is_empty() && show.trim().is_empty() && !status && (said.trim().is_empty() || asks_help) {
        if handoff {
            println!("usage: cork handoff [--to AGENT] \"the spec: goal, context, decisions, files, what done looks like\"");
            println!("       it goes to one agent: your other session open in this repo, or the next one you start here");
        } else {
            println!("usage: cork message [--to AGENT | --about path.py::symbol] \"what to tell them\"");
            println!("       cork message --channel NAME [--to @user] [--attach PATH]... \"words\"");
            println!("                                        a post in a workspace channel, with files");
            println!("       cork message --retract ID|last   take one back");
            println!("       cork message --status            what you sent: when each was delivered and seen");
            println!("       cork message --show ID           one you were handed cut, whole, with who sent it");
        }
        return 2;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(root) = config::find_repo_root(&[Some(cwd)]) else {
        println!("Cork: run this inside a repository Cork covers.");
        return 1;
    };
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        println!("Cork: this repository is not connected, so there is no one to tell.");
        return 1;
    }
    let mut payload = json!({
        "repo_id": cfg.repo_id, "session": running_session(env),
        "text": said, "to": to, "about": about, "retract": retract,
    });
    // a helper inside the session, as the hooks stamped it
    let worker = config::get(env, "CORK_WORKER");
    if !worker.is_empty() {
        payload["worker"] = json!(worker);
    }
    if handoff {
        payload["handoff"] = json!(true);
    }
    if status {
        payload["status"] = json!(true);
    }
    if !show.trim().is_empty() {
        payload["show"] = json!(show.trim());
    }
    // a message to one agent, from a Claude Code session: the other Claude
    // Code sessions on this machine, which it can also reach by Claude
    // Code's own peer messaging (`deliver_to_peers`), as long as this one can
    // attest its permission class and theirs is the same: a message across
    // classes waits for approval there, and Cork's own delivery needs none
    let me = running_session(env);
    let class = crate::peer::mode_class(&crate::peer::mode_of(&me, env));
    let peers = if retract.is_empty() && !status && show.trim().is_empty() && (handoff || !to.is_empty()) && class.is_some() {
        crate::listen::peers(env)
    } else {
        Vec::new()
    };
    if !me.is_empty() && peers.iter().any(|p| p.session == me) {
        let sessions: Vec<&str> = peers.iter().map(|p| p.session.as_str()).collect();
        let others = crate::peer::same_class(&me, &sessions, env);
        if !others.is_empty() {
            payload["peers"] = json!(others);
        }
    }
    let answer = crate::http::post(&cfg.server, "/message", &cfg.token, &crate::report::user_agent(), &payload, Duration::from_secs(5))
        .unwrap_or(Value::Null);
    if answer.get("ok").and_then(Value::as_bool) != Some(true) {
        let why = text(&answer, "error");
        let what = if !retract.is_empty() {
            "the message was not taken back"
        } else if !show.trim().is_empty() {
            "the message could not be shown"
        } else if handoff {
            "the handoff was not sent"
        } else {
            "the message was not sent"
        };
        // the server's own words when it says why nothing was sent (with
        // the agents who are here, for --to), so the agent can pick one
        if why.starts_with("nothing was sent") {
            println!("Cork: {why}");
        } else {
            println!("Cork: {what}{}.", if why.is_empty() { String::new() } else { format!(" ({why})") });
        }
        return 1;
    }
    if !retract.is_empty() {
        println!("Cork: {}.", text(&answer, "note"));
        return 0;
    }
    if !show.trim().is_empty() {
        println!("{}", text(&answer, "text"));
        return 0;
    }
    if status {
        let lines: Vec<&str> = answer.get("lines").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        if lines.is_empty() {
            println!("Cork: you have sent no message in this repo lately.");
        } else {
            println!("Cork: what you sent, newest first:\n{}", lines.iter().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n"));
        }
        return 0;
    }
    // a reply to whoever handed this session work: the handoff is done
    if !handoff && !to.is_empty() {
        crate::pings::replied(env, &running_session(env), &to);
    }
    deliver_to_peers(&answer, &peers, &cfg, &me, env);
    // how to take it back, and how to see it arrive, said once with the receipt
    let id = text(&answer, "message_id");
    let back = if id.is_empty() {
        String::new()
    } else {
        format!(" To take it back: cork message --retract {id}; to see it delivered and seen: cork message --status")
    };
    // each by its names (title and Claude Code name) when the server gives them
    let recipients: Vec<String> = answer
        .get("sent_to_names")
        .or_else(|| answer.get("sent_to"))
        .or_else(|| answer.get("delivered_to"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let note = text(&answer, "note");
    if !note.is_empty() {
        println!("Cork: {note}");
    } else if handoff {
        // one recipient, named, so the agent can tell the user where it went
        let to = recipients.first().map(String::as_str).unwrap_or("another agent");
        println!("Cork: handed off to {to}; it starts on it by itself.{back}");
    } else if recipients.is_empty() {
        println!("Cork: sent.{back}");
    } else {
        // a count, not sixteen session ids the agent then carries along
        if recipients.len() <= 2 {
            println!("Cork: sent to {}.{back}", recipients.join(", "));
        } else {
            println!("Cork: sent to {} agents.{back}", recipients.len());
        }
    }
    0
}

/// The message just sent, handed straight to its recipient through Claude
/// Code's own peer messaging too, when the recipient is a Claude Code
/// session on this machine: the server held it back for this (`peer` in
/// its answer) and hears whether the post went through. Not, or never
/// said: it goes the usual way. Through: Cork's copy stays held until the
/// recipient's hooks find it in its transcript (`transcript::peer_seen`),
/// then it is delivered and no hook prints it again; found nowhere within
/// the window, the usual way delivers it after all. Cork's inbox stays the
/// record: receipts, `--retract` and `--show` work on it as on any message.
fn deliver_to_peers(answer: &Value, peers: &[crate::listen::Peer], cfg: &config::Config, me: &str, env: &Env) {
    let collide = message_command(env).trim_end_matches(" message").to_string();
    // signed as this session, with its real permission class (peer.rs)
    let Some(own) = peers.iter().find(|p| p.session == me) else { return };
    let class = crate::peer::mode_class(&crate::peer::mode_of(me, env)).unwrap_or("");
    let from = crate::peer::address(&own.socket);
    let name = crate::transcript::session_name(me, false, env);
    for handed in answer.get("peer").and_then(Value::as_array).into_iter().flatten() {
        let session = text(handed, "session");
        let Some(peer) = peers.iter().find(|p| p.session == session) else { continue };
        let Some(message) = handed.get("message").filter(|m| m.is_object()) else { continue };
        let said = crate::listen::session_text(std::slice::from_ref(message), &collide);
        let id = text(message, "id");
        let landed = match crate::peer::envelope(&from, &name, class, &said) {
            Some(content) => crate::listen::post_as_peer(&peer.socket, &peer.token, &crate::peer::frame(&from, &content)),
            None => false,
        };
        // the socket never answers: the recipient's hooks look for it in its
        // transcript, and until they find it Cork keeps its copy held
        if landed {
            crate::transcript::note_peer_sent(&peer.session, &id, env);
        }
        let done = json!({"repo_id": cfg.repo_id, "session": me, "peer_done": id, "to": text(handed, "to"), "landed": landed});
        let _ = crate::http::post(&cfg.server, "/message", &cfg.token, &crate::report::user_agent(), &done, Duration::from_secs(3));
    }
}

/// A hook response's waiting messages, for the context the hook prints.
pub fn inbox(response: &Value) -> String {
    text(response, "inbox_note")
}

// ------------------------------------------------------------ the summary

/// The least time between two daily summaries.
const SUMMARY_EVERY_S: f64 = 20.0 * 3600.0;

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// "1.4M", "82k", "640".
pub fn tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// What the local server says happened since `since`, or `None`.
pub fn fetch_summary(cfg: &config::Config, since: f64, wait: Duration) -> Option<Value> {
    let answer = crate::http::post(&cfg.server, "/local/summary", &cfg.token, &crate::report::user_agent(), &json!({"since": since}), wait).ok()?;
    (answer.get("ok").and_then(Value::as_bool) == Some(true)).then_some(answer)
}

/// The summary in one line a person reads.
pub fn summary_line(summary: &Value, period: &str) -> String {
    let saved = number(summary, "tokens_saved");
    let usd = summary.get("est_usd_saved").and_then(Value::as_f64).unwrap_or(0.0);
    let skipped = number(summary, "messages_not_sent");
    let (agents, repos, changes, said) =
        (number(summary, "agents"), number(summary, "repos"), number(summary, "changes"), number(summary, "agent_messages"));
    let plural = |n: u64, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    let mut parts: Vec<String> = Vec::new();
    if saved > 0 {
        parts.push(format!(
            "your agents skipped {} and about {} tokens (~${usd:.2} at cache-read rates)",
            plural(skipped, "message", "messages"), tokens(saved)
        ));
    }
    if changes > 0 {
        let who = if agents > 1 { format!("{agents} agents") } else { "your agents".to_string() };
        let mut shared = format!("{who} shared {} across {}", plural(changes, "change", "changes"), plural(repos.max(1), "repo", "repos"));
        if said > 0 {
            shared.push_str(&format!(" and sent each other {}", plural(said, "message", "messages")));
        }
        parts.push(shared);
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("Cork {period}: {}.", parts.join("; "))
}

/// Once a day, at a session start in a repo the local server covers: what
/// Cork did since the last summary. `None` when it is not time yet, or
/// there is nothing to say.
pub fn daily_summary(cfg: &config::Config, env: &Env) -> Option<String> {
    if !cfg.machine_local {
        return None;
    }
    let path = local_dir(env).join("summary.json");
    let last = config::load_json(&path).get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let now = now_s();
    if now - last < SUMMARY_EVERY_S {
        return None;
    }
    let since = if last > 0.0 { last } else { now - 86_400.0 };
    let summary = fetch_summary(cfg, since, Duration::from_millis(800))?;
    let _ = std::fs::write(&path, json!({"ts": now}).to_string());
    let line = summary_line(&summary, "since yesterday on this machine");
    (!line.is_empty()).then_some(line)
}

/// `cork status`: what this machine runs, and what Cork did on
/// it in the last day and in all.
pub fn status(env: &Env) -> i32 {
    let machine = settings(env);
    let mode = text(&machine, "mode");
    if mode.is_empty() {
        println!("Cork is not installed for this machine. Install it with: curl -fsSL https://corkcli.com/install.sh | sh");
        return 0;
    }
    // which agent tools are here, and whether Cork is on in each (a tool
    // installed since is brought in first, unless the person took it out)
    let _ = crate::harnesses::scan(env, 0.0, true);
    let paint = Paint::new(env);
    let agents = paint.commands(&crate::harnesses::status_lines(env));
    if mode == "cloud" {
        let server = config::canonical_server(&text(&machine, "server_url"));
        let name = machine.get("account").map(|a| text(a, "workspace_name")).filter(|n| !n.is_empty())
            .unwrap_or_else(|| "your workspace".to_string());
        println!("Cork: this machine works with {name} ({server}).");
        if let Some(line) = service_line(env, &server) {
            println!("{line}");
        }
        // the subscription: its hours this month and who spent the most
        if let Some((cloud, token)) = cloud_account(env) {
            let answer = crate::http::post(&cloud, "/machine/status", &token, &crate::report::user_agent(), &json!({}), Duration::from_millis(1500));
            for line in answer.map(|a| hours_lines(a.get("hours").unwrap_or(&Value::Null), &text(&a, "plan"), now_s())).unwrap_or_default() {
                println!("{}", paint.commands(&line));
            }
        }
        // what the hooks' requests cost this machine's agents in waiting
        if let Some(line) = crate::audit::latency_line(now_s() - 86_400.0) {
            println!("{line}");
        }
        println!("{agents}");
        return 0;
    }
    let running = ensure_local(env, Duration::from_secs(3));
    let (base, token) = local_endpoint(env);
    println!("Cork (free, on this machine): {}", if running { format!("running at {base}") } else { "not running; it starts with the next agent session".to_string() });
    println!("  data: {}", local_dir(env).display());
    // a machine signed in to an account also hears how the hosted service is
    if let Some(account) = machine.get("account").filter(|a| a.is_object()) {
        let server = config::canonical_server(&text(account, "server_url"));
        if let Some(line) = service_line(env, if server.is_empty() { CLOUD } else { &server }) {
            println!("{line}");
        }
    }
    println!("{agents}");
    if !running {
        return 0;
    }
    let cfg = config::Config { server: base, token, repo_id: String::new(), verify: String::new(), local_parse: true, machine_local: true };
    for (period, since) in [("in the last day", now_s() - 86_400.0), ("since install", 0.0)] {
        if let Some(summary) = fetch_summary(&cfg, since, Duration::from_secs(5)) {
            let line = summary_line(&summary, period);
            println!("  {}", if line.is_empty() { format!("Cork {period}: nothing yet.") } else { line });
        }
    }
    if machine.get("account").is_none() {
        println!(
            "To work across your machines and with other people (the dashboard, prompt matching by meaning), sign in, free: {}",
            paint.command(&cork_command(env, "login"))
        );
    }
    0
}

/// A unit, in a few words.
const UNITS_PITCH_SHORT: &str = "100 agent hours a month each, pooled across the workspace";
/// What a unit buys, said wherever a machine is offered one.
const UNITS_PITCH: &str = "Units: 100 agent hours a month each, pooled across the workspace; $20 a unit ($17 from the 11th, $15 from the 51st), 15% off yearly.";

/// "Nov 28": a day as people write it.
fn day_label(ts: f64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (_, month, day) = crate::audit::civil((ts / 86_400.0).floor() as i64);
    format!("{} {day}", MONTHS[(month as usize).saturating_sub(1).min(11)])
}

fn hours_label(seconds: i64) -> String {
    let hours = seconds as f64 / 3600.0;
    if hours < 10.0 { format!("{hours:.1} h") } else { format!("{hours:.0} h") }
}

/// `cork status`'s lines about the subscription, from the server's hours
/// summary (the dashboard's hours card) and the plan's name: the plan and
/// its pool, what is used this month, when it resets or runs out, and the
/// members who spent the most. None when the server sent none.
pub fn hours_lines(hours: &Value, plan: &str, now: f64) -> Vec<String> {
    let Some(pool) = hours.get("pool").filter(|p| p.is_object()) else { return Vec::new() };
    let int = |v: &Value, key: &str| v.get(key).and_then(Value::as_i64).unwrap_or(0);
    let used = int(hours, "metered_s");
    let limit = hours.get("limit_s").and_then(Value::as_i64);
    let resets = hours.get("resets").and_then(Value::as_f64).unwrap_or(0.0);
    let units = int(pool, "units");
    let plan = match text(pool, "kind").as_str() {
        "units" => format!("{units} unit{} ({} agent hours a month, pooled)", if units == 1 { "" } else { "s" }, int(pool, "hours")),
        "free" => format!("Free ({} agent hours a month for each person)", int(pool, "free_hours")),
        // an admin's comp, never billed
        "unlimited" if hours.get("comped").and_then(Value::as_bool) == Some(true) => "Complimentary (unlimited agent hours)".to_string(),
        // a seat plan held until its end date, named as the dashboard names it
        "unlimited" => format!("{} plan (unlimited agent hours)", match plan {
            "team" => "Team",
            "business" => "Business",
            "founding" => "Founding team",
            "pro" => "Pro",
            _ => "An earlier",
        }),
        _ => return Vec::new(),
    };
    let spent = match limit.filter(|l| *l > 0) {
        Some(limit) => format!("{} of {} used ({}%)", hours_label(used), hours_label(limit), used * 100 / limit),
        None => format!("{} used", hours_label(used)),
    };
    let mut lines = vec![format!("Plan: {plan} · {spent} this month · resets {}", day_label(resets))];
    let runs_out = hours.pointer("/pace/runs_out_at").and_then(Value::as_f64).filter(|t| *t > now && *t < resets);
    if hours.get("exhausted").and_then(Value::as_bool) == Some(true) {
        lines.push(format!(
            "  The month's hours are used up: sessions already running finish, new ones wait until {} or until units are added (cork upgrade).",
            day_label(resets)
        ));
    } else if let Some(at) = runs_out {
        lines.push(format!("  At this month's pace the hours run out about {}.", day_label(at)));
    }
    if let Some(ends) = hours.get("sunset").and_then(|s| s.get("ends")).and_then(Value::as_f64) {
        lines.push(format!("  Its price stays locked until {}; then the workspace moves to units.", day_label(ends)));
    }
    let members: Vec<String> = hours.get("by_member").and_then(Value::as_array).into_iter().flatten()
        .filter(|m| int(m, "s") > 0)
        .map(|m| {
            let who = [text(m, "name"), text(m, "email").split('@').next().unwrap_or("").to_string()]
                .into_iter().find(|w| !w.is_empty()).unwrap_or_else(|| "someone".to_string());
            format!("{who} {}", hours_label(int(m, "s")))
        })
        .collect();
    if !members.is_empty() {
        lines.push(format!("  Most hours this month: {}", members.join(", ")));
    }
    lines
}

// ------------------------------------------------------------ the service

/// How long `cork status` reuses what it last read of the status page.
const SERVICE_CACHE_S: f64 = 60.0;

/// Where Cork's hosted service says how it is doing: CORK_STATUS_URL,
/// else corkcli.com's for a machine on Cork's own servers (or on
/// collidemcp.com, the address from before the rename); None for any other
/// server.
fn status_url(env: &Env, server: &str) -> Option<String> {
    let given = config::get(env, "CORK_STATUS_URL");
    if !given.is_empty() {
        return Some(given.to_string());
    }
    let host = server.split("://").nth(1).unwrap_or("").split(['/', ':']).next().unwrap_or("").to_lowercase();
    let ours = ["corkcli.com", "collidemcp.com"].iter().any(|d| host == *d || host.ends_with(&format!(".{d}")));
    ours.then(|| "https://corkcli.com/status.json".to_string())
}

/// The one line `cork status` says about the service, from the status
/// page's JSON: "Cork service: operational", or the state and what is
/// not operational, pointing at the page.
pub fn service_line_from(doc: &Value) -> Option<String> {
    let indicator = doc.pointer("/status/indicator")?.as_str()?;
    if indicator == "none" {
        return Some("Cork service: operational".to_string());
    }
    let description = doc.pointer("/status/description").and_then(Value::as_str).unwrap_or("");
    let words = |status: &str| match status {
        "degraded_performance" => "degraded".to_string(),
        "partial_outage" => "partial outage".to_string(),
        "major_outage" => "major outage".to_string(),
        other => other.replace('_', " "),
    };
    let not_ok: Vec<String> = doc
        .get("components")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| text(c, "status") != "operational")
        .map(|c| format!("{}: {}", text(c, "name"), words(&text(c, "status"))))
        .collect();
    let state = if description == "Degraded performance" || description.is_empty() {
        "degraded".to_string()
    } else {
        description.to_lowercase()
    };
    let mut inside = not_ok.join(", ");
    if !inside.is_empty() {
        inside.push_str("; ");
    }
    Some(format!("Cork service: {state} ({inside}see corkcli.com/status)"))
}

/// The service line, read at most once a minute with a two-second budget.
/// Fails open: None when the page cannot be read, and `cork status`
/// then says nothing about the service.
pub fn service_line(env: &Env, server: &str) -> Option<String> {
    let url = status_url(env, server)?;
    let cache = data_dir(env).join("service-status.json");
    let now = now_s();
    let cached = config::load_json(&cache);
    let fresh = cached.get("url").and_then(Value::as_str) == Some(url.as_str())
        && now - cached.get("at").and_then(Value::as_f64).unwrap_or(0.0) < SERVICE_CACHE_S;
    if fresh {
        return cached.get("line").and_then(Value::as_str).map(str::to_string);
    }
    crate::http::audit_raw("GET", &url, "");
    let doc: Value = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(2))
        .build()
        .get(&url)
        .set("User-Agent", "cork-cli (cork status)")
        .call()
        .ok()?
        .into_json()
        .ok()?;
    let line = service_line_from(&doc)?;
    let _ = std::fs::create_dir_all(data_dir(env));
    let _ = std::fs::write(&cache, json!({"url": url, "at": now, "line": line}).to_string());
    Some(line)
}

// ------------------------------------------------------------ the account

const CLOUD: &str = "https://mcp.corkcli.com";
/// How often a signed-in machine asks about its workspace's plan: a machine
/// signed in before v100 on Free (kept local then) moves over at its next
/// check.
const PLAN_CHECK_EVERY_S: f64 = 15.0 * 60.0;

/// Where this machine's binaries update from: its account's server, else
/// Cork's.
pub fn update_server(env: &Env) -> String {
    let from_account = settings(env).get("account").map(|a| text(a, "server_url")).unwrap_or_default();
    if from_account.is_empty() { CLOUD.to_string() } else { config::canonical_server(&from_account) }
}

/// This machine's id (made at install, kept) and its name.
pub fn machine_id(env: &Env) -> String {
    let mut machine = settings(env);
    let known = text(&machine, "machine_id");
    if !known.is_empty() {
        return known;
    }
    // the id kept beside the credential outlives machine.json, which an
    // uninstall removes: a reinstall is the same machine, not a second one
    let mut id = crate::report::machine_id(env);
    if id.is_empty() {
        use sha2::{Digest, Sha256};
        let seed = format!("{:?}{}{}", std::time::SystemTime::now(), std::process::id(), crate::check::home(env));
        id = Sha256::digest(seed.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    }
    if let Some(map) = machine.as_object_mut() {
        map.insert("machine_id".into(), json!(id));
        save_settings(env, &machine);
    }
    id
}

fn machine_name(env: &Env) -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        let value = config::get(env, key);
        if !value.is_empty() {
            return value.to_string();
        }
    }
    Command::new("hostname")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

fn local_cfg(env: &Env) -> config::Config {
    let (server, token) = local_endpoint(env);
    config::Config { server, token, repo_id: String::new(), verify: String::new(), local_parse: true, machine_local: true }
}

/// What this machine has done locally, for its record on the account.
fn local_stats(env: &Env) -> Value {
    ensure_local(env, Duration::from_secs(3));
    fetch_summary(&local_cfg(env), 0.0, Duration::from_secs(5))
        .map(|s| {
            json!({"tokens_saved": s["tokens_saved"], "est_usd_saved": s["est_usd_saved"], "agents": s["agents"],
                   "repos": s["repos"], "changes": s["changes"], "agent_messages": s["agent_messages"]})
        })
        .unwrap_or_else(|| json!({}))
}

fn machine_body(env: &Env) -> Value {
    json!({"machine": machine_id(env), "name": machine_name(env), "os": std::env::consts::OS, "stats": local_stats(env)})
}

/// Whether a sign-in's or a plan check's answer puts the machine on Cork's
/// servers: `cloud`, any plan (servers since v100), else a paid workspace.
fn joins(answer: &Value) -> bool {
    answer.get("cloud").and_then(Value::as_bool) == Some(true) || answer.get("paid").and_then(Value::as_bool) == Some(true)
}

/// Signed in: from now on this machine's hooks use Cork's servers.
fn switch_to_cloud(env: &Env, account: &Value) {
    let mut machine = settings(env);
    if let Some(map) = machine.as_object_mut() {
        map.insert("mode".into(), json!("cloud"));
        map.insert("server_url".into(), json!(text(account, "server_url")));
        map.insert("workspace".into(), json!(text(account, "workspace")));
    }
    save_settings(env, &machine);
}

/// The server no longer takes this machine: it works on its own again, the
/// account kept so it can come straight back.
fn switch_to_local(env: &Env) {
    let mut machine = settings(env);
    if let Some(map) = machine.as_object_mut() {
        map.insert("mode".into(), json!("local"));
    }
    save_settings(env, &machine);
}

/// `cork login` on a machine with Cork installed: sign in, record
/// the machine on the account, move it to Cork's servers and sync its local
/// history there, on any plan.
pub fn login(args: &[String], env: &Env) -> i32 {
    login_as(args, env, "Cork CLI: login")
}

fn login_as(args: &[String], env: &Env, client_name: &str) -> i32 {
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_default();
    let server = {
        let given = arg("--server");
        let from_env = config::get(env, "CORK_SERVER_URL").to_string();
        (if !given.is_empty() { given } else if !from_env.is_empty() { from_env } else { CLOUD.to_string() })
            .trim_end_matches('/')
            .to_string()
    };
    let signed_in = crate::login::sign_in_as(&server, env, client_name);
    focus_terminal(env);
    let access = match signed_in {
        Ok(access) => access,
        Err(problem) => {
            println!("Cork login: {problem}.");
            return 1;
        }
    };
    crate::http::audit_raw("POST", &format!("{server}/machine/connect"), &machine_body(env).to_string());
    let connected = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
        .post(&format!("{server}/machine/connect"))
        .set("Authorization", &format!("Bearer {access}"))
        .send_json(machine_body(env))
        .ok()
        .and_then(|r| r.into_json::<Value>().ok())
        .unwrap_or(Value::Null);
    if connected.get("ok").and_then(Value::as_bool) != Some(true) {
        let why = text(&connected, "error");
        println!("Cork login: this machine could not be connected{}.", if why.is_empty() { String::new() } else { format!(" ({why})") });
        return 1;
    }
    link(&server, &connected, env)
}

/// Back to the app the login was run from, once the browser is done with
/// it: macOS names that app in `__CFBundleIdentifier` (Terminal, iTerm,
/// Ghostty, VS Code, Cursor...). Elsewhere the person switches back.
fn focus_terminal(env: &Env) {
    let app = config::get(env, "__CFBundleIdentifier").to_string();
    if cfg!(target_os = "macos") && !app.is_empty() && app.chars().all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c)) {
        let _ = Command::new("open").args(["-b", &app]).status();
    }
}

/// What a sign-in or a claim answered, kept: the credential merged with any
/// others this machine holds, the account in machine.json, and the machine
/// on the workspace's servers with its history synced.
fn link(server: &str, connected: &Value, env: &Env) -> i32 {
    // the credential, merged with any others this machine holds
    let fragment: serde_json::Map<String, Value> = connected
        .get("credentials")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter(|(k, _)| k.starts_with(&server)).map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let scratch = std::env::temp_dir().join(format!("collide-machine-login-{}.json", std::process::id()));
    let saved = std::fs::write(&scratch, Value::Object(fragment).to_string()).is_ok()
        && crate::report::add_credentials(&scratch.to_string_lossy(), env) == 0;
    let _ = std::fs::remove_file(&scratch);
    if !saved {
        println!("Cork login: signed in, but the credential could not be saved to {}.", tilde(&config::credentials_path(env), env));
        return 1;
    }
    let account = json!({
        "user": text(&connected, "user"), "workspace": text(&connected, "workspace"),
        "workspace_name": text(&connected, "workspace_name"), "server_url": text(&connected, "server_url"),
        "dashboard_url": text(&connected, "dashboard_url"), "checked": now_s(),
    });
    let mut machine = settings(env);
    let before = machine.get("account").map(|a| text(a, "server_url")).unwrap_or_default();
    if machine.as_object().map_or(true, |m| m.is_empty()) {
        machine = json!({"mode": "local"});
    }
    machine["account"] = account.clone();
    save_settings(env, &machine);
    // a different server may offer a different version: check it now
    let after = text(&account, "server_url");
    if !after.is_empty() && after != before {
        crate::selfupdate::server_changed(&after, env);
    }
    let name = {
        let named = text(&account, "workspace_name");
        if named.is_empty() { text(&account, "workspace") } else { named }
    };
    let who = {
        let user = text(&account, "user");
        if user.is_empty() { String::new() } else { format!(" as {user}") }
    };
    // from an install (the npx wrapper's sign-in, setup's code): one line
    // in its style, and the sync says only what went wrong
    let styled = config::get(env, "CORK_STYLE") == "1";
    let paint = Paint::new(env);
    if !joins(connected) {
        if styled {
            println!("  {} signed in{who} {}", paint.yellow("✓"), paint.dim(&format!("· this machine stays local for now: {name}'s server did not take it")));
        } else {
            println!("Signed in{who}. This machine stays local for now: {name}'s server did not take it.");
        }
        return 0;
    }
    switch_to_cloud(env, &account);
    if styled {
        println!("  {} signed in{who}", paint.yellow("✓"));
    } else if connected.get("paid").and_then(Value::as_bool) == Some(true) {
        println!("Signed in{who}. This machine now works with {name}; syncing its local history...");
    } else {
        println!(
            "Signed in{who}. This machine now works with {name}: your agents here and on every machine you sign in on \
see each other's work. {name} is on Free, metered in agent hours ({} shows this month's). Syncing its local history...",
            paint.command(&cork_command(env, "status"))
        );
    }
    // signed in either way: a history that could not be read now is sent
    // by `cork sync` later, and a new machine has none to send
    let _ = sync_said(env, !styled);
    0
}

/// `cork sync`: send this machine's local record to its workspace,
/// batch by batch, from where the last sync stopped.
pub fn sync(env: &Env) -> i32 {
    sync_said(env, true)
}

/// The same; with `say` false only what went wrong is said.
fn sync_said(env: &Env, say: bool) -> i32 {
    let paint = Paint::new(env);
    let machine = settings(env);
    let account = machine.get("account").cloned().unwrap_or(Value::Null);
    let server = text(&account, "server_url");
    if server.is_empty() {
        println!("Cork sync: sign in first with: {}", paint.command(&cork_command(env, "login")));
        return 1;
    }
    let creds = config::load_json(&config::credentials_path(env));
    let token = config::resolve_token(&creds, &server, &text(&account, "workspace"));
    if !ensure_local(env, Duration::from_secs(5)) {
        if !local_dir(env).join("collide.db").exists() {
            if say {
                println!("Cork sync: this machine has no local history yet, so there is nothing to send.");
            }
            return 0;
        }
        println!("Cork sync: the local server did not start, so its history could not be read. Run it again: {}", paint.command(&cork_command(env, "sync")));
        return 1;
    }
    let local = local_cfg(env);
    let cursor_path = local_dir(env).join("sync.json");
    let mut cursor = config::load_json(&cursor_path).get("cursor").cloned().unwrap_or_else(|| json!({}));
    let id = machine_id(env);
    let (mut rows, mut notes) = (0u64, 0u64);
    loop {
        let batch = crate::http::post(&local.server, "/local/export", &local.token, &crate::report::user_agent(),
            &json!({"cursor": cursor}), Duration::from_secs(30))
            .unwrap_or(Value::Null);
        if batch.get("ok").and_then(Value::as_bool) != Some(true) {
            println!("Cork sync: could not read the local record.");
            return 1;
        }
        let more = batch.get("more").and_then(Value::as_bool).unwrap_or(false);
        let body = json!({"machine": id, "repos": batch["repos"], "final": !more, "all_repos": batch["all_repos"]});
        let answer = crate::http::post(&server, "/machine/import", &token, &crate::report::user_agent(), &body, Duration::from_secs(60))
            .unwrap_or(Value::Null);
        if answer.get("ok").and_then(Value::as_bool) != Some(true) {
            let why = text(&answer, "error");
            println!("Cork sync: stopped{}. Run {} to continue.", if why.is_empty() { String::new() } else { format!(" ({why})") }, paint.command(&cork_command(env, "sync")));
            return 1;
        }
        rows += answer.get("rows").and_then(Value::as_u64).unwrap_or(0);
        notes += answer.get("notes").and_then(Value::as_u64).unwrap_or(0);
        cursor = batch.get("cursor").cloned().unwrap_or(cursor);
        let _ = std::fs::write(&cursor_path, json!({"cursor": cursor, "ts": now_s()}).to_string());
        if !more {
            break;
        }
    }
    if say {
        println!("Cork sync: {rows} entries of history and {notes} notes from this machine are in your workspace now.");
    }
    0
}

/// At a session start on a signed-in machine, now and then: does the
/// workspace take it? A machine still local (signed in on Free before v100)
/// moves to Cork's servers and syncs in the background, with nothing for
/// the person to do. Returns the line to tell them, once.
pub fn plan_check(env: &Env) -> Option<String> {
    let mut machine = settings(env);
    let mode = text(&machine, "mode");
    if mode != "local" && mode != "cloud" {
        return None;
    }
    let account = machine.get("account").cloned()?;
    let checked = account.get("checked").and_then(Value::as_f64).unwrap_or(0.0);
    if now_s() - checked < PLAN_CHECK_EVERY_S {
        return None;
    }
    machine["account"]["checked"] = json!(now_s());
    save_settings(env, &machine);
    let server = text(&account, "server_url");
    let creds = config::load_json(&config::credentials_path(env));
    let token = config::resolve_token(&creds, &server, &text(&account, "workspace"));
    if server.is_empty() || token.is_empty() {
        return None;
    }
    // no answer (offline, the cloud down): nothing changes until the next check
    // a machine on the cloud has no local record to count: no local server for it
    let body = if mode == "cloud" {
        json!({"machine": machine_id(env), "name": machine_name(env), "os": std::env::consts::OS})
    } else {
        machine_body(env)
    };
    let answer = crate::http::post(&server, "/machine/status", &token, &crate::report::user_agent(), &body, Duration::from_millis(1500))
        .ok()?;
    let paid = answer.get("paid").and_then(Value::as_bool) == Some(true);
    let name = {
        let named = text(&account, "workspace_name");
        if named.is_empty() { "your workspace".to_string() } else { named }
    };
    if mode == "cloud" {
        if joins(&answer) {
            return None;
        }
        // the server let the machine go: the free version carries on, so no agent stops working
        switch_to_local(env);
        return Some(format!(
            "Cork: {name} no longer takes this machine, so it is back on Cork, free and local: agents here still see each \
other's work and message each other, but not other machines'. Tell the user this in one line."
        ));
    }
    if !joins(&answer) {
        return None;
    }
    switch_to_cloud(env, &account);
    // the history goes over in the background; the session does not wait
    if let Ok(exe) = std::env::current_exe() {
        let mut command = Command::new(exe);
        command.arg("sync").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let _ = command.spawn();
    }
    Some(if paid {
        format!(
            "Cork: this machine has switched to {name} and is syncing its local history there. From this session on, \
agents on your other machines and your teammates' see this one's work too. Tell the user this in one line."
        )
    } else {
        format!(
            "Cork: this machine has switched to {name} on Cork's servers and is syncing its local history there. From this \
session on, agents on your other signed-in machines see this one's work too. {name} is on Free, metered in agent hours. Tell the user \
this in one line."
        )
    })
}

/// The commands a person runs, for `cork` with no command (cork.js and the
/// CLI docs list the same).
const COMMANDS: &[(&str, &str)] = &[
    ("cork status", "what Cork did here, and whether it is running"),
    ("cork message", "tell the other agents in this repo something"),
    ("cork handoff", "hand work to your other session (or the next one) as a spec"),
    ("cork commit --mine -m MSG", "commit only the lines this session wrote"),
    ("cork trunk on|off|status", "trunk mode: agents work on one branch, and Cork lands the ones they make into it"),
    ("cork branches", "the agents' branches: open, landed, conflicted; land or delete one"),
    ("cork promote", "open or update the one pull request from the trunk to main (Cork never merges it)"),
    ("cork wake on", "start an agent when a teammate messages you and none is running"),
    ("cork statusline on", "Cork in Claude Code's status line: agents here, writes it stopped today"),
    ("cork badge off", "stop ending your agents' pull requests with Cork's badge"),
    ("cork today", "your day: what your agents did, what Cork caught, what is left open"),
    ("cork pings on|off", "desktop notices when something matters (catches and finished handoffs by default)"),
    ("cork sound on|off", "a short sound when Cork blocks a collision (off by default)"),
    ("cork reads full|outline", "whether a big mapped file's whole-file Read gets its outline first (outline by default)"),
    ("cork audit", "exactly what this machine sent to Cork (kept here, never uploaded)"),
    ("cork login", "link this machine to your Cork account"),
    ("cork update", "get the newest version of cork now"),
    ("cork upgrade", "add units: 100 agent hours a month each, pooled (a new cork version is cork update)"),
    ("cork add", "share this repo with your workspace (with units)"),
    ("cork migrate", "retire this repo's old per-repo Cork setup (.collide/ hooks)"),
    ("cork rollback", "go back to the version before the last update"),
    ("cork uninstall", "take Cork's hooks out (your data stays)"),
];

/// `cork` with no command: what it is and the commands, each painted.
pub fn help(env: &Env) -> String {
    let paint = Paint::new(env);
    let mut out = vec!["cork: Cork for every repo on this machine.".to_string(), String::new()];
    for (command, what) in COMMANDS {
        let pad = " ".repeat(20usize.saturating_sub(command.len()).max(2));
        out.push(format!("  {}{pad}{}", paint.command(command), paint.commands(what)));
    }
    out.push(String::new());
    out.push("The agents' hooks run it as: cork report | cork gate".to_string());
    out.join("\n")
}

/// `cork upgrade`: units for the machine's workspace. Signs in first when
/// the machine is not linked yet (which joins it to the workspace), then
/// opens the billing page for its workspace.
pub fn upgrade(args: &[String], env: &Env) -> i32 {
    let paint = Paint::new(env);
    // people reach for `upgrade` to get a new version: that is `update`
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "usage: {} [--server URL]\n\n\
More agent hours for your workspace: opens its billing page for units ({UNITS_PITCH_SHORT}), signing this \
machine in first if it is not.\nA new version of the cork program is {}, not upgrade.",
            paint.command("cork upgrade"),
            paint.command("cork update")
        );
        return 0;
    }
    if settings(env).get("account").is_none() {
        let code = login_as(args, env, "Cork CLI: upgrade");
        if code != 0 {
            return code;
        }
    }
    let machine = settings(env);
    let account = machine.get("account").cloned().unwrap_or(Value::Null);
    let name = {
        let named = text(&account, "workspace_name");
        if named.is_empty() { "your workspace".to_string() } else { named }
    };
    let site = {
        let given = text(&account, "dashboard_url");
        // a sign-in from before the rename saved collidemcp.com, which only redirects here now
        let given = given.trim_end_matches('/').replace("://collidemcp.com", "://corkcli.com").replace("://www.collidemcp.com", "://corkcli.com");
        if given.is_empty() { "https://corkcli.com".to_string() } else { given }
    };
    let url = format!("{site}/dashboard/billing?workspace={}", text(&account, "workspace"));
    println!("Opening units for {name}: {url}\n{UNITS_PITCH}\nThey cover every machine signed in to {name} as soon as they are paid for.");
    println!("{}", paint.dim(&format!("(A new version of cork itself: {}.)", paint.command("cork update"))));
    if config::get(env, "CORK_NO_BROWSER") != "1" {
        crate::login::open_browser(&url);
    }
    // the next session start checks right away, not in 15 minutes
    let mut machine = settings(env);
    if machine.get("account").is_some() {
        machine["account"]["checked"] = json!(0.0);
        save_settings(env, &machine);
    }
    0
}

/// `cork add`: this repo joins the team's workspace, the person's yes to
/// the question their agent asked. Only a repo on GitHub (or another git
/// host) can: no one else could be working in any other.
pub fn add(env: &Env) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(root) = config::find_repo_root(&[Some(cwd)]) else {
        println!("Cork: run this inside the git repository to add.");
        return 1;
    };
    let repo_id = repo_id_for(&root);
    if !is_hosted(&repo_id) {
        println!(
            "Cork: this repository has no remote on GitHub (or another git host), so it cannot be shared with a team. \
Push it to GitHub first; until then Cork keeps working for it on this machine."
        );
        return 1;
    }
    let cfg = config::config(Some(&root), env);
    if cfg.machine_local {
        println!(
            "Cork: on the free version every repository on this machine is already covered. \
To share {repo_id} with a team: {}", Paint::new(env).command(&cork_command(env, "upgrade"))
        );
        return 0;
    }
    if !cfg.usable() {
        println!("Cork: this machine is not signed in. Run: {}", Paint::new(env).command(&cork_command(env, "login")));
        return 1;
    }
    let answer = crate::http::post(&cfg.server, "/repo/add", &cfg.token, &crate::report::user_agent(),
        &json!({"repo_id": cfg.repo_id, "session": config::get(env, "CORK_SESSION")}), Duration::from_secs(10))
        .unwrap_or(Value::Null);
    if answer.get("ok").and_then(Value::as_bool) == Some(true) {
        let name = text(&answer, "workspace_name");
        println!("Cork: {} is in {} now; agents working in it share with the team.", cfg.repo_id,
            if name.is_empty() { "your workspace".to_string() } else { name });
        return 0;
    }
    let why = text(&answer, "error");
    println!("Cork: {} was not added{}.", cfg.repo_id, if why.is_empty() { String::new() } else { format!(" ({why})") });
    1
}

// ------------------------------------------------------------ Hermes

const HERMES_PLUGIN: &str = include_str!("../assets/hermes/__init__.py");
const HERMES_MANIFEST: &str = include_str!("../assets/hermes/plugin.yaml");

/// The plugin's name in Hermes (`~/.hermes/plugins/cork`), and where an
/// install from before the rename put it (`collide`): moved on the next
/// install or refresh, never both left switched on.
const HERMES_NAME: &str = "cork";
const OLD_HERMES_NAME: &str = "collide";

/// Whether Cork's Hermes plugin is in, under either name.
pub(crate) fn hermes_installed(env: &Env) -> bool {
    [HERMES_NAME, OLD_HERMES_NAME].iter().any(|n| hermes_home(env).join("plugins").join(n).join("__init__.py").exists())
}

pub(crate) fn hermes_home(env: &Env) -> PathBuf {
    let given = config::get(env, "HERMES_HOME");
    if given.is_empty() { PathBuf::from(crate::check::home(env)).join(".hermes") } else { PathBuf::from(given) }
}

/// The `hermes` program: on PATH, else where its installer puts it.
fn hermes_program(env: &Env) -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH")
        .and_then(|paths| std::env::split_paths(&paths).map(|d| d.join(exe("hermes"))).find(|p| p.is_file()));
    on_path.or_else(|| {
        let local = PathBuf::from(crate::check::home(env)).join(".local").join("bin").join(exe("hermes"));
        local.is_file().then_some(local)
    })
}

/// Hermes runs Cork through a plugin (its hooks and the messages pushed
/// into its open conversations): written into ~/.hermes/plugins/cork,
/// switched on with Hermes's own commands the first time (a plugin is off
/// until enabled, and its message injection is off until granted). Later
/// calls keep the files current. `None` where Hermes is not installed.
pub fn install_hermes(env: &Env) -> Option<String> {
    install_hermes_with(env, true)
}

/// The same; with `wait` false (a hook, on its time budget) Hermes's own
/// commands that switch the plugin on run detached, after the hook is done.
pub(crate) fn install_hermes_with(env: &Env, wait: bool) -> Option<String> {
    let home = hermes_home(env);
    if !home.is_dir() {
        return None;
    }
    let dir = home.join("plugins").join(HERMES_NAME);
    let old = home.join("plugins").join(OLD_HERMES_NAME);
    let first = !dir.join("__init__.py").exists();
    std::fs::create_dir_all(&dir).ok()?;
    // the plugin under its name from before the rename makes way (Hermes
    // is told to switch it off where the new one is switched on)
    if old.join("__init__.py").exists() {
        let _ = std::fs::remove_dir_all(&old);
    }
    for (name, body) in [("__init__.py", HERMES_PLUGIN), ("plugin.yaml", HERMES_MANIFEST)] {
        if std::fs::read_to_string(dir.join(name)).ok().as_deref() != Some(body) {
            std::fs::write(dir.join(name), body).ok()?;
        }
    }
    if first {
        if wait {
            enable_hermes(env);
        } else {
            crate::harnesses::spawn_detached(env, &["harnesses", "hermes-enable"]);
        }
    }
    Some(format!("Hermes ({})", dir.display()))
}

/// Hermes's own commands: the plugin on, and its message injection granted.
pub(crate) fn enable_hermes(env: &Env) {
    if let Some(hermes) = hermes_program(env) {
        let quiet = |args: &[&str]| {
            let _ = Command::new(&hermes).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        };
        quiet(&["plugins", "disable", OLD_HERMES_NAME]);
        quiet(&["plugins", "enable", HERMES_NAME]);
        quiet(&["config", "set", &format!("plugins.entries.{HERMES_NAME}.allow_gateway_injection"), "true"]);
    }
}

fn uninstall_hermes(env: &Env) {
    for name in [HERMES_NAME, OLD_HERMES_NAME] {
        let dir = hermes_home(env).join("plugins").join(name);
        if !dir.exists() {
            continue;
        }
        if let Some(hermes) = hermes_program(env) {
            let _ = Command::new(&hermes).args(["plugins", "disable", name]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
