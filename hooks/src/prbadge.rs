//! Cork's badge on the pull requests agents open.
//!
//! An agent asked to end a pull request with the badge drops it when its
//! harness says no attribution lines (Claude Code's `attribution.pr: ""`),
//! so where the hooks see a shell command's output (Claude Code, Codex) the
//! hook adds it instead: a command that ran `gh pr create` and printed the
//! new pull request's address starts a detached `cork pr-badge <url>`,
//! which adds the badge line to its description with the person's own
//! `gh`. Nothing waits on it and nothing is blocked: whatever `gh` cannot do
//! is skipped and written to the audit log.
//!
//! Once per pull request: each one handled is kept in `pr-badge.json`, so a
//! badge the person took out stays out. The GitHub App adds the same line
//! where it is installed; both look for the badge first, so whichever comes
//! second does nothing. `cork badge off` and `CORK_PR_BADGE=0` stop it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use regex::Regex;
use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::machine::{pr_badge_on, PR_BADGE_LINE};

/// What marks a description as carrying the badge already, whoever put it there.
const MARK: &str = "corkcli.com/badge.svg";
/// Pull requests remembered as handled; the oldest go first.
const KEEP: usize = 2000;

/// The harnesses whose hooks see what a shell command printed: there the
/// hook adds the badge, and the agent is not asked to.
pub fn covers(harness: &str) -> bool {
    matches!(harness, "claude" | "codex")
}

fn pr_url_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"https://[A-Za-z0-9.-]+(?::\d+)?/[^/\s]+/[^/\s]+/pull/\d+").unwrap())
}

/// Whether one part of a shell command runs `gh pr create` (gh by any
/// path, after any `VAR=value` settings or `env`), and if so whether it
/// asked for `--web`, which opens a browser and makes nothing.
fn creates_pr(part: &str) -> Option<bool> {
    let words: Vec<&str> = part
        .split_whitespace()
        .skip_while(|w| matches!(*w, "env" | "command" | "exec" | "time") || (w.contains('=') && !w.starts_with('-')))
        .collect();
    let gh = words.first()?;
    if !(*gh == "gh" || gh.ends_with("/gh") || gh.ends_with("gh.exe")) || words.get(1..3) != Some(&["pr", "create"][..]) {
        return None;
    }
    let web = words[3..].iter().any(|w| {
        *w == "--web" || (w.starts_with('-') && !w.starts_with("--") && w.len() > 1 && w[1..].chars().all(|c| c.is_ascii_alphabetic()) && w.contains('w'))
    });
    Some(web)
}

/// What the command printed: its stdout where the harness keeps it apart
/// (Claude Code), else all of it (Codex).
fn printed(hook_input: &Value) -> String {
    match hook_input.get("tool_response") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(map)) => ["stdout", "output"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// The pull request a shell command just opened: it ran `gh pr create`
/// (anywhere in a compound command, not with `--web`) and printed the new
/// pull request's address. One that already existed (`gh` names it after
/// "already exists") is not one it opened.
pub fn opened_by(command: &str, hook_input: &Value) -> Option<String> {
    if !command.contains("pr create") {
        return None;
    }
    let parts = command.split(['\n', ';', '&', '|', '(', ')']);
    let mut found = false;
    for part in parts {
        match creates_pr(part) {
            Some(true) => return None,
            Some(false) => found = true,
            None => {}
        }
    }
    if !found {
        return None;
    }
    let output = printed(hook_input);
    let mut url = None;
    let mut previous = "";
    for line in output.lines() {
        if !previous.contains("already exists") && !line.contains("already exists") {
            if let Some(m) = pr_url_re().find_iter(line).last() {
                url = Some(m.as_str().to_string());
            }
        }
        previous = line;
    }
    url
}

/// PostToolUse of a shell command: a pull request it opened gets the badge
/// from a detached `cork pr-badge`, so the hook returns at once.
pub fn after_shell(hook_input: &Value, env: &Env) {
    let command = hook_input.pointer("/tool_input/command").and_then(Value::as_str).unwrap_or("");
    let Some(url) = opened_by(command, hook_input) else { return };
    if !pr_badge_on(env) || handled(&url, env) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut child = Command::new(exe);
    child.arg("pr-badge").arg(&url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let cwd = hook_input.get("cwd").and_then(Value::as_str).unwrap_or("");
    if !cwd.is_empty() && Path::new(cwd).is_dir() {
        child.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        child.process_group(0);
    }
    let _ = child.spawn();
}

/// What `add` did with a pull request.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `cork badge off` or `CORK_PR_BADGE=0`
    Off,
    /// handled before: added then, or found with the badge, or closed
    Handled,
    NoGh,
    /// `gh` refused or said something unreadable
    Failed,
    Closed,
    Present,
    Added,
}

/// `cork pr-badge <url>`: run detached by the hook; says nothing.
pub fn command(args: &[String], env: &Env) -> i32 {
    let url = args.first().map(String::as_str).unwrap_or("");
    if !pr_url_re().find(url).is_some_and(|m| m.as_str() == url) {
        return 2;
    }
    add(url, env);
    0
}

/// The badge line onto a pull request's description, once, if it is open
/// and has none.
pub fn add(url: &str, env: &Env) -> Outcome {
    if !pr_badge_on(env) {
        return Outcome::Off;
    }
    if handled(url, env) {
        return Outcome::Handled;
    }
    let Some(gh) = find_gh(env) else {
        audit(url, "pr view", json!({"ok": false, "why": "gh is not installed"}));
        return Outcome::NoGh;
    };
    let viewed = Command::new(&gh)
        .args(["pr", "view", url, "--json", "body,isDraft,state"])
        .stdin(Stdio::null())
        .output();
    let pr = match viewed {
        Ok(out) if out.status.success() => serde_json::from_slice::<Value>(&out.stdout).unwrap_or(Value::Null),
        Ok(out) => {
            audit(url, "pr view", json!({"ok": false, "why": first_line(&out.stderr)}));
            return Outcome::Failed;
        }
        Err(e) => {
            audit(url, "pr view", json!({"ok": false, "why": e.to_string()}));
            return Outcome::Failed;
        }
    };
    let Some(body) = pr.get("body").and_then(Value::as_str) else {
        audit(url, "pr view", json!({"ok": false, "why": "no description in gh's answer"}));
        return Outcome::Failed;
    };
    if pr.get("state").and_then(Value::as_str) != Some("OPEN") {
        remember(url, env);
        return Outcome::Closed;
    }
    if body.contains(MARK) {
        remember(url, env);
        return Outcome::Present;
    }
    let next = if body.trim().is_empty() { PR_BADGE_LINE.to_string() } else { format!("{}\n\n{PR_BADGE_LINE}", body.trim_end()) };
    let draft = pr.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
    let edited = Command::new(&gh)
        .args(["pr", "edit", url, "--body-file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(next.as_bytes())?;
            }
            child.wait_with_output()
        });
    match edited {
        Ok(out) if out.status.success() => {
            remember(url, env);
            audit(url, "pr edit", json!({"ok": true, "added": PR_BADGE_LINE, "draft": draft}));
            Outcome::Added
        }
        Ok(out) => {
            audit(url, "pr edit", json!({"ok": false, "why": first_line(&out.stderr)}));
            Outcome::Failed
        }
        Err(e) => {
            audit(url, "pr edit", json!({"ok": false, "why": e.to_string()}));
            Outcome::Failed
        }
    }
}

fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(200).collect()
}

/// What Cork asked of GitHub on the person's behalf, in the audit log.
fn audit(url: &str, method: &str, body: Value) {
    crate::audit::record("gh", method, url, Some(&body), body.to_string().len());
}

/// The GitHub CLI: `CORK_GH` when set (and only it), else `gh` on the
/// PATH, else where Homebrew and Linux packages put it (an agent app
/// started from the Dock may run its hooks with a short PATH).
fn find_gh(env: &Env) -> Option<PathBuf> {
    let named = config::get(env, "CORK_GH");
    if !named.is_empty() {
        return Path::new(named).is_file().then(|| PathBuf::from(named));
    }
    let name = if cfg!(windows) { "gh.exe" } else { "gh" };
    let on_path = std::env::split_paths(config::get(env, "PATH")).map(|dir| dir.join(name));
    let usual = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/home/linuxbrew/.linuxbrew/bin"].into_iter().map(|dir| Path::new(dir).join(name));
    on_path.chain(usual.filter(|_| cfg!(unix))).find(|p| p.is_file())
}

fn state_path(env: &Env) -> PathBuf {
    crate::machine::data_dir(env).join("pr-badge.json")
}

fn handled(url: &str, env: &Env) -> bool {
    config::load_json(&state_path(env)).get(url).is_some()
}

fn remember(url: &str, env: &Env) {
    let mut seen = config::load_json(&state_path(env)).as_object().cloned().unwrap_or_default();
    seen.insert(url.to_string(), json!(crate::journal::now_s().round()));
    if seen.len() > KEEP {
        let mut by_age: Vec<(String, f64)> = seen.iter().map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0))).collect();
        by_age.sort_by(|a, b| a.1.total_cmp(&b.1));
        for (old, _) in by_age.into_iter().take(seen.len() - KEEP) {
            seen.remove(&old);
        }
    }
    let _ = std::fs::create_dir_all(crate::machine::data_dir(env));
    let _ = std::fs::write(state_path(env), Value::Object(seen).to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://github.com/acme/shop/pull/7";

    fn bash(command: &str, stdout: &str) -> Value {
        json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {"command": command},
               "tool_response": {"stdout": stdout, "stderr": ""}})
    }

    /// A home of its own and a `gh` on its PATH that serves `state` and
    /// `body`, and records each call (and what an edit was handed) in `calls`.
    fn setup(name: &str, state: &str, body: &str) -> (PathBuf, Env) {
        let base = std::env::temp_dir().join(format!("cork-prbadge-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let view = json!({"body": body, "isDraft": false, "state": state}).to_string();
        std::fs::write(base.join("view.json"), view).unwrap();
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> '{calls}'\ncase \"$1 $2\" in\n  \"pr view\") cat '{view}' ;;\n  \"pr edit\") cat > '{edited}' ;;\nesac\nexit 0\n",
            calls = base.join("calls").display(),
            view = base.join("view.json").display(),
            edited = base.join("edited").display(),
        );
        let gh = bin.join("gh");
        std::fs::write(&gh, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut env = Env::new();
        env.insert("HOME".into(), base.to_string_lossy().to_string());
        env.insert("CORK_HOME".into(), base.to_string_lossy().to_string());
        env.insert("PATH".into(), bin.to_string_lossy().to_string());
        (base, env)
    }

    fn calls(base: &Path) -> String {
        std::fs::read_to_string(base.join("calls")).unwrap_or_default()
    }

    #[test]
    fn the_command_that_opened_a_pull_request_is_read_from_its_output() {
        let made = "Creating pull request for feat into main in acme/shop\n\nhttps://github.com/acme/shop/pull/7\n";
        assert_eq!(opened_by("gh pr create --fill", &bash("gh pr create --fill", made)).as_deref(), Some(URL));
        // a compound command, gh by its path, the body in a heredoc
        let compound = "git push -u origin feat && /opt/homebrew/bin/gh pr create --fill";
        let pushed = format!("remote: Create a pull request for 'feat' on GitHub by visiting:\nremote:   https://github.com/acme/shop/pull/new/feat\n{made}");
        assert_eq!(opened_by(compound, &bash(compound, &pushed)).as_deref(), Some(URL));
        let heredoc = "gh pr create --title x --body \"$(cat <<'EOF'\nSummary\nEOF\n)\"";
        assert_eq!(opened_by(heredoc, &bash(heredoc, made)).as_deref(), Some(URL));
        // Codex keeps one output, as a string or under `output`
        let codex = json!({"tool_input": {"command": "gh pr create --fill"}, "tool_response": {"output": made}});
        assert_eq!(opened_by("gh pr create --fill", &codex).as_deref(), Some(URL));
        // --web opens a browser and makes nothing; -w, and -dw, are the same
        for web in ["gh pr create --web", "gh pr create -w", "gh pr create -dw --fill"] {
            assert_eq!(opened_by(web, &bash(web, made)), None, "{web}");
        }
        // one that already existed is not one it opened
        let exists = "a pull request for branch \"feat\" into branch \"main\" already exists:\nhttps://github.com/acme/shop/pull/7\n";
        assert_eq!(opened_by("gh pr create --fill; true", &bash("gh pr create --fill; true", exists)), None);
        // settings before it, and a subshell
        let set = "cd shop && (GH_TOKEN=x gh pr create --fill)";
        assert_eq!(opened_by(set, &bash(set, made)).as_deref(), Some(URL));
        // other commands that print an address, or name it without running it
        assert_eq!(opened_by("gh pr view 7", &bash("gh pr view 7", made)), None);
        assert_eq!(opened_by("echo gh pr create", &bash("echo gh pr create", made)), None);
        assert_eq!(opened_by("gh pr create --fill", &bash("gh pr create --fill", "")), None);
    }

    #[test]
    fn the_badge_is_added_once_and_a_removed_one_stays_out() {
        let (base, env) = setup("once", "OPEN", "Summary\n\n- refunds\n");
        let compound = "git push -u origin feat && gh pr create --fill";
        let url = opened_by(compound, &bash(compound, &format!("To github.com:acme/shop.git\n{URL}\n"))).unwrap();
        assert_eq!(add(&url, &env), Outcome::Added);
        let asked = calls(&base);
        assert!(asked.contains("pr view https://github.com/acme/shop/pull/7 --json body,isDraft,state"), "{asked}");
        assert!(asked.contains("pr edit https://github.com/acme/shop/pull/7 --body-file -"), "{asked}");
        let edited = std::fs::read_to_string(base.join("edited")).unwrap();
        assert_eq!(edited, format!("Summary\n\n- refunds\n\n{PR_BADGE_LINE}"));
        // the person takes it out: the pull request is not touched again
        std::fs::remove_file(base.join("calls")).unwrap();
        assert_eq!(add(URL, &env), Outcome::Handled);
        assert_eq!(calls(&base), "", "gh is not even asked");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_badge_already_there_or_a_closed_pull_request_is_left_alone() {
        let (base, env) = setup("present", "OPEN", &format!("Summary\n\n{PR_BADGE_LINE}"));
        assert_eq!(add(URL, &env), Outcome::Present);
        assert!(!calls(&base).contains("pr edit"));
        assert_eq!(add(URL, &env), Outcome::Handled);
        let _ = std::fs::remove_dir_all(&base);
        let (base, env) = setup("closed", "MERGED", "Summary");
        assert_eq!(add(URL, &env), Outcome::Closed);
        assert!(!calls(&base).contains("pr edit"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn switched_off_or_with_no_gh_nothing_happens() {
        let (base, mut env) = setup("off", "OPEN", "Summary");
        crate::machine::save_settings(&env, &json!({"mode": "local", "pr_badge": false}));
        assert_eq!(add(URL, &env), Outcome::Off);
        crate::machine::save_settings(&env, &json!({"mode": "local"}));
        env.insert("CORK_PR_BADGE".into(), "0".into());
        assert_eq!(add(URL, &env), Outcome::Off);
        assert_eq!(calls(&base), "");
        env.remove("CORK_PR_BADGE");
        // gh missing: skipped, and tried again should it ever be asked
        env.insert("CORK_GH".into(), base.join("no-gh").to_string_lossy().to_string());
        assert_eq!(add(URL, &env), Outcome::NoGh);
        assert!(!handled(URL, &env));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_hook_starts_nothing_for_a_command_that_opened_no_pull_request() {
        let (base, env) = setup("hook", "OPEN", "Summary");
        // --web: nothing to add to, so nothing is started
        after_shell(&bash("gh pr create --web", "Opening github.com/acme/shop/compare/main...feat in your browser.\n"), &env);
        assert_eq!(calls(&base), "");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_a_pull_request_address_is_taken_as_the_argument() {
        let env = Env::new();
        assert_eq!(command(&["https://github.com/acme/shop/pull/new/feat".to_string()], &env), 2);
        assert_eq!(command(&["--help".to_string()], &env), 2);
        assert_eq!(command(&[], &env), 2);
    }
}
