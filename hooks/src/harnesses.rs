//! Agent tools installed after Cork.
//!
//! `cork install` fills the hooks and MCP config of every agent tool it
//! finds then. A person who installs Codex, Cursor or Hermes later would
//! have to run it again, and a tool without Cork's hooks never runs
//! Cork, so the check runs from what already does:
//!
//! - every session start of any tool that has Cork, at most once an hour
//!   per machine (`~/.collide/harnesses-checked`), on the hook's own time:
//!   the merges are a few small file writes, and Hermes's own commands that
//!   switch its plugin on run detached;
//! - the long-lived processes the install starts (the local server, and the
//!   `cork mcp` bridge each agent session keeps open), every ten minutes,
//!   for a person who from now on only uses the new tool.
//!
//! A tool found without Cork's entries gets the same merges `install`
//! does (hooks and MCP, everything the person had kept), and its next
//! session start says so once. What Cork did per tool is kept in
//! `~/.collide/machine.json` under `harnesses`; a tool whose Cork
//! entries the person removed after Cork added them is recorded as
//! `off` and never touched again, until `cork install --harness NAME`.
//! Claude Code is never added here: `install` always fills it, unless the
//! plugin owns its hooks (`--no-settings`), and then Cork keeps out.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::machine;

/// A session start checks at most this often.
pub const SESSION_EVERY_S: f64 = 3600.0;
/// The long-lived processes check this often.
pub const WATCH_EVERY_S: f64 = 600.0;

/// The tools Cork adds itself to when they come after it.
pub const LATER: &[&str] = &["codex", "cursor", "hermes"];
/// Every tool `cork status` reports on.
const ALL: &[&str] = &["claude", "codex", "cursor", "hermes"];

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn home(env: &Env) -> PathBuf {
    PathBuf::from(crate::check::home(env))
}

pub fn display(key: &str) -> &'static str {
    match key {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "cursor" => "Cursor",
        "hermes" => "Hermes",
        _ => "",
    }
}

fn key_for(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude code" | "claude" | "claude-code" => Some("claude"),
        "codex" => Some("codex"),
        "cursor" => Some("cursor"),
        "hermes" => Some("hermes"),
        _ => None,
    }
}

/// The folder a tool keeps its user settings in: gone means the tool was
/// reset or uninstalled, not that Cork was taken out of it.
fn config_dir(key: &str, env: &Env) -> PathBuf {
    match key {
        "claude" => home(env).join(".claude"),
        "codex" => home(env).join(".codex"),
        "cursor" => home(env).join(".cursor"),
        _ => machine::hermes_home(env),
    }
}

/// The tool is on this machine: its settings folder, or its program on PATH.
pub fn present(key: &str, env: &Env) -> bool {
    let dir = config_dir(key, env).is_dir();
    match key {
        "claude" => dir || machine::on_path("claude", env),
        "codex" => dir || machine::on_path("codex", env),
        "cursor" => dir || machine::on_path("cursor", env) || machine::on_path("cursor-agent", env),
        // Hermes's plugin goes into its home, so the home must be there
        "hermes" => dir,
        _ => false,
    }
}

/// The user-level hooks file of a tool and the merge that fills it.
pub fn hook_file(key: &str, env: &Env) -> Option<(&'static str, PathBuf, machine::Merge)> {
    match key {
        "claude" => Some(("Claude Code", machine::claude_settings_path(env), machine::merge_claude_settings as machine::Merge)),
        "codex" => Some(("Codex", home(env).join(".codex").join("hooks.json"), machine::merge_codex_hooks as machine::Merge)),
        "cursor" => Some(("Cursor", home(env).join(".cursor").join("hooks.json"), machine::merge_cursor_hooks as machine::Merge)),
        _ => None,
    }
}

/// The MCP config of a tool: (file, is_toml).
fn mcp_file(key: &str, env: &Env) -> Option<(PathBuf, bool)> {
    match key {
        "claude" => Some((home(env).join(".claude.json"), false)),
        "codex" => Some((home(env).join(".codex").join("config.toml"), true)),
        "cursor" => Some((home(env).join(".cursor").join("mcp.json"), false)),
        _ => None,
    }
}

#[cfg(test)]
fn hermes_plugin(env: &Env) -> PathBuf {
    machine::hermes_home(env).join("plugins").join("cork").join("__init__.py")
}

/// Cork's hooks are in the tool (for Hermes, its plugin).
pub fn hooks_on(key: &str, env: &Env) -> bool {
    if key == "hermes" {
        return machine::hermes_installed(env);
    }
    hook_file(key, env)
        .and_then(|(_, path, _)| std::fs::read_to_string(path).ok())
        .is_some_and(|text| machine::has_machine_mark(&text))
}

/// Cork's MCP server is in the tool's MCP config (put there by `install`,
/// by the person, or by a plugin).
pub fn mcp_on(key: &str, env: &Env) -> bool {
    let Some((path, toml)) = mcp_file(key, env) else { return false };
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    // under Cork's name, or Collide's from before the rename
    if toml {
        return text.lines().any(|line| {
            let t = line.trim();
            machine::MCP_NAMES.iter().any(|name| t == format!("[mcp_servers.{name}]") || t == format!("[mcp_servers.\"{name}\"]"))
        });
    }
    let parsed = serde_json::from_str::<Value>(&text).ok();
    machine::MCP_NAMES.iter().any(|name| parsed.as_ref().and_then(|v| v.get("mcpServers")?.get(*name)).is_some())
}

// ------------------------------------------------------------ the record

fn records(env: &Env) -> serde_json::Map<String, Value> {
    machine::settings(env).get("harnesses").and_then(Value::as_object).cloned().unwrap_or_default()
}

fn record_of(key: &str, env: &Env) -> Value {
    records(env).get(key).cloned().unwrap_or(Value::Null)
}

fn state_of(key: &str, env: &Env) -> String {
    record_of(key, env).get("state").and_then(Value::as_str).unwrap_or("").to_string()
}

/// Read, change and write one tool's record in machine.json, keeping
/// everything else in the file as it is now.
fn update(env: &Env, key: &str, change: impl FnOnce(&mut serde_json::Map<String, Value>)) {
    let mut machine = machine::settings(env);
    if !machine.is_object() {
        machine = json!({});
    }
    let mut all = machine.get("harnesses").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut one = all.get(key).and_then(Value::as_object).cloned().unwrap_or_default();
    change(&mut one);
    all.insert(key.to_string(), Value::Object(one));
    machine["harnesses"] = Value::Object(all);
    machine::save_settings(env, &machine);
}

/// What Cork did in a tool: `state` is "on", "off" (the person took it
/// out), "plugin" (Claude Code's hooks are the plugin's) or "gone" (the
/// tool itself was removed); `by` is "install", "detected" or "you".
pub fn record(env: &Env, key: &str, state: &str, by: &str) {
    let now = now_s().round();
    update(env, key, |one| {
        one.insert("state".into(), json!(state));
        one.insert("by".into(), json!(by));
        one.insert("at".into(), json!(now));
        one.insert("told".into(), json!(true));
        one.remove("removed_at");
        one.remove("from");
    });
}

/// `record` for a tool named as `install` names it.
pub fn record_name(env: &Env, name: &str, by: &str) {
    if let Some(key) = key_for(name) {
        record(env, key, "on", by);
    }
}

/// The person took Cork out of this tool: it is left alone.
pub fn opted_out(key: &str, env: &Env) -> bool {
    state_of(key, env) == "off"
}

pub fn opted_out_name(name: &str, env: &Env) -> bool {
    key_for(name).is_some_and(|key| opted_out(key, env))
}

// ------------------------------------------------------------ adding

/// The same merges `install` does, for one tool: its hooks and its MCP
/// entry (the machine's, local or cloud), everything else kept.
fn add(key: &str, env: &Env, wait: bool) -> bool {
    if key == "hermes" {
        return machine::install_hermes_with(env, wait).is_some();
    }
    let Some((_, path, merge)) = hook_file(key, env) else { return false };
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let merged = merge(&existing, env);
    if merged != existing {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        if std::fs::write(&path, merged).is_err() {
            return false;
        }
    }
    if let Some((path, toml)) = mcp_file(key, env) {
        let entry = machine::mcp_entry(env);
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let merged = if toml { Some(machine::merge_codex_mcp(&existing, Some(&entry))) } else { machine::merge_mcp_json(&existing, Some(&entry)) };
        if let Some(merged) = merged.filter(|m| *m != existing) {
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
            let _ = std::fs::write(&path, merged);
        }
    }
    true
}

fn stamp_path(env: &Env) -> PathBuf {
    machine::data_dir(env).join("harnesses-checked")
}

/// Look at every tool Cork adds itself to; returns the ones it added.
/// Skipped when the machine looked less than `every_s` ago (0: always).
/// `wait` false keeps it on a hook's budget (Hermes's commands detached).
pub fn scan(env: &Env, every_s: f64, wait: bool) -> Vec<&'static str> {
    if !machine::installed(env) {
        return Vec::new();
    }
    let stamp = stamp_path(env);
    let now = now_s();
    if every_s > 0.0 {
        let last: f64 = std::fs::read_to_string(&stamp).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0.0);
        if now - last < every_s {
            return Vec::new();
        }
    }
    // stamped first: sessions starting together do not all look
    let _ = std::fs::create_dir_all(machine::data_dir(env));
    let _ = std::fs::write(&stamp, format!("{now:.0}"));
    let mut added = Vec::new();
    for &key in LATER {
        let state = state_of(key, env);
        if state == "off" {
            continue; // the person's call: never touched
        }
        if !present(key, env) {
            if state == "on" {
                record(env, key, "gone", "detected"); // a reinstall brings it back in
            }
            continue;
        }
        let hooks = hooks_on(key, env);
        if state == "on" {
            if !hooks && config_dir(key, env).is_dir() {
                // Cork's entries were in, and the person took them out
                update(env, key, |one| {
                    one.insert("state".into(), json!("off"));
                    one.insert("removed_at".into(), json!(now.round()));
                });
            } else if !hooks {
                record(env, key, "gone", "detected");
            }
            continue;
        }
        if hooks {
            // in from an install before these records: kept as it is
            record(env, key, "on", "install");
            continue;
        }
        // found without Cork's hooks: new since the install, or with only
        // Cork's MCP tools in it (added by hand or by an older install)
        let mcp_only = mcp_on(key, env);
        if add(key, env, wait) {
            update(env, key, |one| {
                one.insert("state".into(), json!("on"));
                one.insert("by".into(), json!("detected"));
                one.insert("at".into(), json!(now.round()));
                one.insert("told".into(), json!(false));
                one.insert("from".into(), json!(if mcp_only { "mcp-only" } else { "none" }));
                one.remove("removed_at");
            });
            added.push(key);
        }
    }
    added
}

/// What the person is told about a tool Cork added itself to.
pub fn notice_for(key: &str, mcp_only: bool) -> String {
    let had = if mcp_only { " (it had Cork's MCP tools but not its hooks)" } else { "" };
    let next = match key {
        "codex" => "Codex asks you to approve the hooks once: start a new Codex session and approve them in /hooks.",
        "cursor" => "Restart Cursor, or open a new session, to load them.",
        "hermes" => "Start a new Hermes conversation to load the plugin.",
        _ => "",
    };
    format!(
        "Cork added itself to {}{had}. {next} Don't want it there? Remove Cork's entries and it stays out.",
        display(key)
    )
}

/// The tools Cork added itself to that the person has not heard about:
/// one line, and they are marked told.
pub fn pending_notice(env: &Env) -> Option<String> {
    let mut lines = Vec::new();
    for (key, one) in records(env) {
        let detected = one.get("by").and_then(Value::as_str) == Some("detected")
            && one.get("state").and_then(Value::as_str) == Some("on")
            && one.get("told").and_then(Value::as_bool) == Some(false);
        let Some(key) = key_for(&key).filter(|_| detected) else { continue };
        let mcp_only = one.get("from").and_then(Value::as_str) == Some("mcp-only");
        lines.push(notice_for(key, mcp_only));
        update(env, key, |one| {
            one.insert("told".into(), json!(true));
        });
    }
    (!lines.is_empty()).then(|| lines.join(" "))
}

/// At a session start (report.rs): look for tools installed since, at most
/// once an hour, and say once what Cork added itself to. The line is
/// held for a session that will show it (one in a repo Cork serves).
pub fn session_start(hook_input: &Value, env: &Env) -> Option<String> {
    if !machine::installed(env) {
        return None;
    }
    let _ = scan(env, SESSION_EVERY_S, false);
    let cwd = hook_input.get("cwd").and_then(Value::as_str).filter(|s| !s.is_empty()).map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let project = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project.is_empty()).then(|| PathBuf::from(project));
    let root = config::find_repo_root(&[Some(cwd), project])?;
    if !config::config(Some(&root), env).usable() {
        return None;
    }
    pending_notice(env).map(|line| format!("{line} Tell the user this in one line."))
}

/// One look from a long-lived process (at most every ten minutes per
/// machine, whichever process looks).
pub fn watch_tick(env: &Env) -> Vec<&'static str> {
    scan(env, WATCH_EVERY_S, true)
}

/// The local server and the MCP bridge run this: a background thread that
/// looks every ten minutes while the process lives. Fail-open: a thread
/// that cannot start is no check.
pub fn watch(env: Env) {
    if config::get(&env, "CORK_HARNESS_WATCH") == "0" {
        return;
    }
    // the session that started this process has just looked, or is about to
    let _ = std::thread::Builder::new().name("collide-harnesses".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_secs(WATCH_EVERY_S as u64));
        if machine::installed(&env) {
            let _ = watch_tick(&env);
        }
    });
}

/// Start this program detached with `args` (it outlives the hook).
pub(crate) fn spawn_detached(env: &Env, args: &[&str]) {
    let program = machine::program_path(env);
    let program = if program.exists() {
        program
    } else if machine::serves_local_pub() {
        match std::env::current_exe() {
            Ok(me) => me,
            Err(_) => return,
        }
    } else {
        return;
    };
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let _ = command.spawn();
}

// ------------------------------------------------------------ commands

/// `cork harnesses [scan|hermes-enable]`: the check by hand (what the
/// hooks and the background processes run), and Hermes's own commands
/// started detached by a session start.
pub fn command(args: &[String], env: &Env) -> i32 {
    match args.first().map(String::as_str).unwrap_or("scan") {
        "hermes-enable" => {
            machine::enable_hermes(env);
            0
        }
        _ => {
            let added = scan(env, 0.0, true);
            println!("{}", machine::Paint::new(env).commands(&status_lines(env)));
            if let Some(line) = pending_notice(env) {
                println!("{line}");
            }
            let _ = added;
            0
        }
    }
}

/// `cork install --harness NAME`: Cork back into a tool the person
/// took it out of (or into one it has not added itself to yet).
pub fn enable_command(name: &str, env: &Env) -> i32 {
    let Some(key) = key_for(name).filter(|k| *k != "claude") else {
        if key_for(name) == Some("claude") {
            println!("Cork install: Claude Code's hooks come with {} itself (or with Cork's plugin).", machine::Paint::new(env).command("cork install"));
        } else {
            println!("Cork install: --harness takes codex, cursor or hermes.");
        }
        return 1;
    };
    if !machine::installed(env) {
        println!("Cork install: Cork is not installed on this machine yet. Install it with: curl -fsSL https://corkcli.com/install.sh | sh");
        return 1;
    }
    if key == "hermes" && !config_dir(key, env).is_dir() {
        println!("Cork install: no Hermes on this machine ({} is missing).", config_dir(key, env).display());
        return 1;
    }
    if !add(key, env, true) {
        println!("Cork install: could not update {}'s settings.", display(key));
        return 1;
    }
    record(env, key, "on", "you");
    let after = match key {
        "codex" => " Codex asks you to approve the hooks once: start a new Codex session and approve them in /hooks.",
        "cursor" => " Restart Cursor, or open a new session, to load them.",
        _ => " Start a new Hermes conversation to load the plugin.",
    };
    println!("Cork is on in {} again.{after}", display(key));
    0
}

fn ago(at: f64) -> String {
    let days = ((now_s() - at) / 86_400.0).floor().max(0.0) as u64;
    match days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        n => format!("{n} days ago"),
    }
}

/// `cork status`'s lines: each agent tool found, and whether Cork is
/// on in it.
pub fn status_lines(env: &Env) -> String {
    let mut lines = vec!["  agents:".to_string()];
    let mut notes = Vec::new();
    for &key in ALL {
        let one = record_of(key, env);
        let state = one.get("state").and_then(Value::as_str).unwrap_or("");
        let at = one.get("at").and_then(Value::as_f64).unwrap_or(0.0);
        let hooks = hooks_on(key, env);
        let mcp = mcp_on(key, env);
        let said = if key == "claude" && state == "plugin" {
            "on, through Cork's plugin".to_string()
        } else if state == "off" {
            let when = one.get("removed_at").and_then(Value::as_f64).map(ago).unwrap_or_default();
            format!("off: you took Cork out ({when}), so it stays out. Turn it back on: cork install --harness {key}")
        } else if hooks && (mcp || key == "hermes") {
            if key == "hermes" { "on (plugin)".to_string() } else { "on (hooks and MCP tools)".to_string() }
        } else if hooks {
            "on (hooks; no MCP tools)".to_string()
        } else if mcp {
            format!("MCP tools only, no hooks: Cork adds them at the next session start (or now: cork install --harness {key})")
        } else if !present(key, env) {
            "not installed".to_string()
        } else if key == "claude" {
            "found, Cork not in it (run: cork install)".to_string()
        } else {
            format!("found, Cork not in it yet: it adds itself at the next session start (or now: cork install --harness {key})")
        };
        lines.push(format!("    {:<12} {said}", display(key)));
        if state == "on" && one.get("by").and_then(Value::as_str) == Some("detected") && now_s() - at < 7.0 * 86_400.0 {
            notes.push(format!("  {} ({})", notice_for(key, one.get("from").and_then(Value::as_str) == Some("mcp-only")), ago(at)));
        }
    }
    lines.extend(notes);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine with Cork installed in a scratch HOME: no PATH, so only
    /// the folders made here count as tools.
    fn machine_env(tag: &str) -> (PathBuf, Env) {
        let home = std::env::temp_dir().join(format!("collide-harnesses-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        env.insert("PATH".into(), String::new());
        machine::save_settings(&env, &json!({"mode": "local"}));
        (home, env)
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    fn codex_after_install(home: &Path) {
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/hooks.json"), r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}}"#).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "model = \"o3\"\n\n[mcp_servers.other]\ncommand = \"other\"\n").unwrap();
    }

    #[test]
    fn codex_installed_after_collide_gets_hooks_and_mcp_at_the_next_session_start() {
        let (home, env) = machine_env("added");
        codex_after_install(&home);
        assert!(!hooks_on("codex", &env));
        assert_eq!(scan(&env, SESSION_EVERY_S, false), vec!["codex"]);
        let hooks: Value = serde_json::from_str(&read(&home.join(".codex/hooks.json"))).unwrap();
        assert!(hooks["hooks"]["SessionStart"].to_string().contains("# cork-machine"), "{hooks}");
        assert!(hooks["hooks"]["Stop"].to_string().contains("say done"), "theirs kept: {hooks}");
        let toml = read(&home.join(".codex/config.toml"));
        assert!(toml.contains("model = \"o3\"") && toml.contains("[mcp_servers.other]"), "theirs kept: {toml}");
        assert!(toml.contains("[mcp_servers.cork]") && toml.contains("args = [\"mcp\"]"), "{toml}");
        assert_eq!(state_of("codex", &env), "on");
        // Claude Code is never written here, plugin or not
        assert!(!home.join(".claude/settings.json").exists());
        // idempotent: a second look changes nothing
        let (before_hooks, before_toml) = (read(&home.join(".codex/hooks.json")), toml);
        assert!(scan(&env, 0.0, false).is_empty());
        assert_eq!(read(&home.join(".codex/hooks.json")), before_hooks);
        assert_eq!(read(&home.join(".codex/config.toml")), before_toml);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn removed_entries_stay_removed_until_the_person_turns_them_back_on() {
        let (home, env) = machine_env("optout");
        codex_after_install(&home);
        scan(&env, 0.0, false);
        // the person takes Cork's hooks out again
        let without = machine::remove_from_claude_settings(&read(&home.join(".codex/hooks.json")));
        std::fs::write(home.join(".codex/hooks.json"), &without).unwrap();
        assert!(scan(&env, 0.0, false).is_empty());
        assert_eq!(read(&home.join(".codex/hooks.json")), without, "not added back");
        assert!(opted_out("codex", &env));
        assert!(record_of("codex", &env).get("removed_at").is_some());
        assert!(status_lines(&env).contains("cork install --harness codex"));
        // and never touched again
        std::fs::write(home.join(".codex/hooks.json"), "{}").unwrap();
        scan(&env, 0.0, true);
        assert_eq!(read(&home.join(".codex/hooks.json")), "{}");
        // `cork install --harness codex` brings it back
        assert_eq!(enable_command("codex", &env), 0);
        assert!(hooks_on("codex", &env) && mcp_on("codex", &env));
        assert_eq!(state_of("codex", &env), "on");
        assert!(scan(&env, 0.0, false).is_empty(), "on again: nothing more to add");
        assert!(hooks_on("codex", &env));
        assert_eq!(enable_command("claude", &env), 1, "Claude Code is install's, not --harness's");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_check_runs_at_most_once_an_hour_from_sessions_and_every_ten_minutes_from_the_background() {
        let (home, env) = machine_env("throttle");
        assert!(scan(&env, SESSION_EVERY_S, false).is_empty());
        codex_after_install(&home);
        assert!(scan(&env, SESSION_EVERY_S, false).is_empty(), "looked a moment ago");
        assert!(watch_tick(&env).is_empty(), "the background looks every ten minutes too");
        // eleven minutes on: the background finds it, a session would not yet
        let stamp = stamp_path(&env);
        std::fs::write(&stamp, format!("{:.0}", now_s() - 660.0)).unwrap();
        assert!(scan(&env, SESSION_EVERY_S, false).is_empty());
        assert_eq!(watch_tick(&env), vec!["codex"], "the background check adds it");
        // and an hour on, a session looks again
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        std::fs::write(&stamp, format!("{:.0}", now_s() - 3700.0)).unwrap();
        assert_eq!(scan(&env, SESSION_EVERY_S, false), vec!["cursor"]);
        let mcp: Value = serde_json::from_str(&read(&home.join(".cursor/mcp.json"))).unwrap();
        assert!(mcp["mcpServers"]["cork"]["args"] == json!(["mcp"]), "{mcp}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn nothing_happens_on_a_machine_without_collide_or_to_claude_code_under_the_plugin() {
        let (home, env) = machine_env("plugin");
        record(&env, "claude", "plugin", "install");
        codex_after_install(&home);
        scan(&env, 0.0, false);
        assert!(!home.join(".claude/settings.json").exists(), "the plugin owns Claude Code's hooks");
        assert!(!home.join(".claude.json").exists());
        assert!(status_lines(&env).contains("on, through Cork's plugin"));
        // no machine install: no check at all
        let _ = std::fs::remove_file(machine::data_dir(&env).join("machine.json"));
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        assert!(scan(&env, 0.0, false).is_empty());
        assert!(!home.join(".cursor/hooks.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn mcp_only_gets_its_hooks_and_the_person_hears_once() {
        let (home, env) = machine_env("notice");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "[mcp_servers.cork]\nurl = \"https://mcp.corkcli.com/mcp\"\n").unwrap();
        assert!(mcp_on("codex", &env) && !hooks_on("codex", &env));
        assert!(status_lines(&env).contains("MCP tools only"));
        // a session start outside any repo Cork serves holds the line
        let input = json!({"cwd": home.to_string_lossy()});
        assert!(session_start(&input, &env).is_none());
        assert!(hooks_on("codex", &env), "the session start added the hooks");
        assert_eq!(record_of("codex", &env)["from"], json!("mcp-only"));
        // the next session start in a repo Cork serves says it, once
        let repo = home.join("work").join("app");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        // set up for Cork in the repo itself: a bare git repo counts only
        // when the machine running the test has Cork installed
        // (find_repo_root reads the process's own HOME), which a CI runner does not
        std::fs::create_dir_all(repo.join(".collide")).unwrap();
        std::fs::write(repo.join(".collide/config.json"), "{}").unwrap();
        std::fs::create_dir_all(machine::local_dir(&env)).unwrap();
        std::fs::write(machine::local_dir(&env).join("token"), "t0k").unwrap();
        let input = json!({"cwd": repo.to_string_lossy(), "hook_event_name": "SessionStart"});
        let told = session_start(&input, &env).expect("told once");
        assert!(told.starts_with("Cork added itself to Codex (it had Cork's MCP tools but not its hooks). \
Codex asks you to approve the hooks once: start a new Codex session and approve them in /hooks."), "{told}");
        assert!(told.ends_with("Tell the user this in one line."), "{told}");
        assert!(session_start(&input, &env).is_none(), "and only once");
        assert!(pending_notice(&env).is_none());
        assert!(status_lines(&env).contains("Cork added itself to Codex"), "status repeats it for a week");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_tool_removed_entirely_comes_back_in_when_reinstalled_and_hermes_too() {
        let (home, env) = machine_env("gone");
        codex_after_install(&home);
        scan(&env, 0.0, false);
        std::fs::remove_dir_all(home.join(".codex")).unwrap();
        scan(&env, 0.0, false);
        assert_eq!(state_of("codex", &env), "gone", "an uninstalled tool is not an opt-out");
        codex_after_install(&home);
        assert_eq!(scan(&env, 0.0, false), vec!["codex"]);
        // Hermes: its plugin is its hooks; deleting it is taking Cork out
        std::fs::create_dir_all(home.join(".hermes")).unwrap();
        assert_eq!(scan(&env, 0.0, false), vec!["hermes"]);
        assert!(hermes_plugin(&env).exists());
        std::fs::remove_dir_all(home.join(".hermes/plugins/cork")).unwrap();
        scan(&env, 0.0, false);
        assert!(opted_out("hermes", &env) && !hermes_plugin(&env).exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
