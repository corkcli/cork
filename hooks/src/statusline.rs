//! `cork statusline`: Cork where people already look, in Claude Code's
//! status line: `cork · 3 agents here · 2 catches today`.
//!
//! Claude Code runs the status line's command after each message, with the
//! session (`cwd`, `session_id`, ...) as JSON on stdin, so it must answer in
//! a few milliseconds and never wait on a network. It reads two small
//! records the hooks keep on this machine as they run anyway:
//!
//! - `here/<session>`: the repo each agent session works in, touched by its
//!   hooks at most every half minute. A session counts as here for ten
//!   minutes after its last hook, and leaves at its end.
//! - `catches`: one line for each write the gate stopped (a teammate's live
//!   change, a numbered file someone else took), counted once per file and
//!   session over the local day.
//!
//! `cork statusline on` puts the command in `~/.claude/settings.json` when
//! no status line is set there: one of the person's own is never replaced.
//! `off` takes out only Cork's.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

use crate::config::{self, Env};

/// How long a session counts as at work in its repo after its last hook.
const HERE_S: u64 = 600;
/// How often a session's record is touched: hooks fire on every tool call.
const TOUCH_S: u64 = 30;
/// Past this size the catches log keeps only its last two days.
const CATCHES_MAX: u64 = 64 * 1024;

fn dir(env: &Env) -> PathBuf {
    crate::machine::local_dir(env).join("statusline")
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Seconds since a file last changed; None when it is not there.
fn age(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    Some(SystemTime::now().duration_since(modified).map(|d| d.as_secs()).unwrap_or(0))
}

fn session_file(session: &str, env: &Env) -> Option<PathBuf> {
    let name: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(64).collect();
    (!name.is_empty()).then(|| dir(env).join("here").join(name))
}

/// A hook ran for `session` in this repo: it is here (at its end, gone).
pub fn note(root: &Path, repo_id: &str, session: &str, event: &str, env: &Env) {
    let Some(path) = session_file(session, env) else { return };
    if event == "SessionEnd" {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let body = format!("{repo_id}\n{}\n", root.display());
    if age(&path).is_some_and(|a| a < TOUCH_S) && std::fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
        return;
    }
    let folder = dir(env).join("here");
    let _ = std::fs::create_dir_all(&folder);
    let _ = std::fs::write(&path, body);
    // sessions that ended without a SessionEnd (a killed process) go
    for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
        if age(&entry.path()).is_some_and(|a| a > 6 * HERE_S) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The sessions at work in a repo on this machine now: by its id, else
/// (no id) by its folder.
pub fn sessions_here(repo_id: &str, root: &Path, env: &Env) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(dir(env).join("here")).into_iter().flatten().flatten() {
        if !age(&entry.path()).is_some_and(|a| a < HERE_S) {
            continue;
        }
        let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
        let mut lines = body.lines();
        let (id, folder) = (lines.next().unwrap_or(""), lines.next().unwrap_or(""));
        let same = if repo_id.is_empty() { Path::new(folder) == root } else { id == repo_id };
        if same {
            out.insert(entry.file_name().to_string_lossy().to_string());
        }
    }
    out
}

/// This machine's offset from UTC in seconds, as `date` says it (a
/// catch is rare enough to ask); 0 where it cannot be read.
fn utc_offset() -> i64 {
    #[cfg(unix)]
    {
        let said = std::process::Command::new("date").arg("+%z").output().ok();
        let text = said.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        let (sign, digits) = match (text.strip_prefix('-'), text.strip_prefix('+')) {
            (Some(rest), _) => (-1, rest),
            (_, Some(rest)) => (1, rest),
            _ => return 0,
        };
        if digits.len() == 4 && digits.chars().all(|c| c.is_ascii_digit()) {
            let (h, m) = (digits[..2].parse::<i64>().unwrap_or(0), digits[2..].parse::<i64>().unwrap_or(0));
            return sign * (h * 3600 + m * 60);
        }
        0
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// The gate stopped a write: one line in the catches log, with the repo
/// the session works in (from its record) and what it was writing.
pub fn caught(stdin_data: &str, env: &Env) {
    let input = crate::harness::normalize(serde_json::from_str(stdin_data).unwrap_or(Value::Null));
    let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let session = field(&input, "session_id");
    let repo = session_file(&session, env)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|body| body.lines().next().map(str::to_string))
        .filter(|id| !id.is_empty())
        .or_else(|| {
            let cwd = field(&input, "cwd");
            let start = if cwd.is_empty() { std::env::current_dir().ok() } else { Some(PathBuf::from(cwd)) };
            config::find_repo_root(&[start]).map(|root| crate::machine::repo_id_for(&root))
        })
        .unwrap_or_default();
    if repo.is_empty() {
        return;
    }
    let tool_input = input.get("tool_input").cloned().unwrap_or(Value::Null);
    let what = ["file_path", "notebook_path", "path", "command"]
        .iter()
        .map(|k| field(&tool_input, k))
        .find(|v| !v.is_empty())
        .unwrap_or_default();
    let clean = |s: &str| s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }).take(200).collect::<String>();
    let line = format!("{}\t{}\t{}\t{}\t{}\n", now_s(), utc_offset(), clean(&session), clean(&repo), clean(&what));
    let path = dir(env).join("catches");
    let _ = std::fs::create_dir_all(dir(env));
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > CATCHES_MAX) {
        let keep: String = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.split('\t').next().and_then(|t| t.parse::<u64>().ok()).is_some_and(|t| now_s().saturating_sub(t) < 2 * 86_400))
            .map(|l| format!("{l}\n"))
            .collect();
        let _ = std::fs::write(&path, keep);
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn local_day(ts: u64, offset: i64) -> i64 {
    (ts as i64 + offset).div_euclid(86_400)
}

/// The catches in a repo on the local day `now` falls in, each file once
/// per session. The day is the one of the newest line's offset (the
/// statusline asks no clock but the system's).
pub fn count_catches(log: &str, repo: &str, now: u64) -> usize {
    let rows: Vec<Vec<&str>> = log.lines().map(|l| l.split('\t').collect::<Vec<_>>()).filter(|r| r.len() == 5).collect();
    let offset = rows.last().and_then(|r| r[1].parse::<i64>().ok()).unwrap_or(0);
    let today = local_day(now, offset);
    rows.iter()
        .filter(|r| r[3] == repo)
        .filter(|r| r[0].parse::<u64>().ok().zip(r[1].parse::<i64>().ok()).is_some_and(|(t, o)| local_day(t, o) == today))
        .map(|r| (r[2], r[4]))
        .collect::<BTreeSet<_>>()
        .len()
}

/// The status line: Cork's name, then only the parts that say something.
pub fn line(agents: usize, catches: usize, color: bool) -> String {
    let mut parts = vec![if color { "\x1b[1;38;2;255;198;26mcork\x1b[0m".to_string() } else { "cork".to_string() }];
    if agents > 1 {
        parts.push(format!("{agents} agents here"));
    }
    if catches > 0 {
        parts.push(format!("{catches} {} today", if catches == 1 { "catch" } else { "catches" }));
    }
    parts.join(" · ")
}

/// What Claude Code shows: None where Cork has nothing to say (not
/// installed, not a repo) or anything goes wrong.
fn render(env: &Env) -> Option<String> {
    use std::io::{IsTerminal, Read};
    if !crate::machine::installed(env) {
        return None;
    }
    let mut stdin = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().take(1 << 20).read_to_string(&mut stdin);
    }
    let input: Value = serde_json::from_str(&stdin).unwrap_or(Value::Null);
    let given = input.pointer("/workspace/current_dir").or_else(|| input.get("cwd")).and_then(Value::as_str).unwrap_or("");
    let cwd = if given.is_empty() { std::env::current_dir().ok()? } else { PathBuf::from(given) };
    let root = config::find_repo_root(&[Some(cwd)])?;
    let session = input.get("session_id").and_then(Value::as_str).unwrap_or("");
    // the repo as this session's hooks named it, so no git is asked
    let repo = session_file(session, env)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|body| body.lines().next().map(str::to_string))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| crate::machine::repo_id_for(&root));
    let mut here = sessions_here(&repo, &root, env);
    if let Some(own) = session_file(session, env).and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())) {
        here.insert(own);
    }
    let log = std::fs::read_to_string(dir(env).join("catches")).unwrap_or_default();
    Some(line(here.len(), count_catches(&log, &repo, now_s()), config::get(env, "NO_COLOR").is_empty()))
}

/// Whether a status line entry is Cork's: a command that runs this program
/// (under either name, by any path) as `statusline`.
fn is_ours(entry: &Value) -> bool {
    let command = entry.get("command").and_then(Value::as_str).unwrap_or("").trim();
    let Some(program) = command.strip_suffix(" statusline") else { return false };
    let program = program.trim();
    // `CORK_HOME=... ` in front when Cork's home is not the default one
    let program = match program.split_once(' ') {
        Some((first, rest)) if first.contains('=') => rest.trim(),
        _ => program,
    };
    let program = program.trim_matches('"');
    ["cork", "cork.exe", "collide", "collide.exe"]
        .iter()
        .any(|name| program == *name || program.ends_with(&format!("/{name}")) || program.ends_with(&format!("\\{name}")))
}

/// `~/.claude/settings.json` with Cork's status line in, everything else as
/// it was; or, as an Err, why it stays as it is: a status line of the
/// person's own is never replaced, and a file that is not JSON is never
/// rewritten.
pub fn with_statusline(existing: &str, command: &str) -> Result<String, String> {
    let mut current = if existing.trim().is_empty() {
        serde_json::Map::new()
    } else {
        match serde_json::from_str::<Value>(existing) {
            Ok(Value::Object(map)) => map,
            _ => return Err("~/.claude/settings.json is not valid JSON, so Cork left it as it is".to_string()),
        }
    };
    if let Some(theirs) = current.get("statusLine").filter(|entry| !is_ours(entry)) {
        let what = theirs.get("command").and_then(Value::as_str).map(|c| format!(" (`{c}`)")).unwrap_or_default();
        return Err(format!("you have a status line of your own{what}, so Cork left it as it is"));
    }
    current.insert("statusLine".into(), json!({"type": "command", "command": command}));
    Ok(format!("{}\n", serde_json::to_string_pretty(&Value::Object(current)).unwrap_or_default()))
}

/// The same file with Cork's status line taken out; None when it has none
/// of Cork's (the person's own stays).
pub fn without_statusline(existing: &str) -> Option<String> {
    let mut current = match serde_json::from_str::<Value>(existing) {
        Ok(Value::Object(map)) => map,
        _ => return None,
    };
    if !current.get("statusLine").is_some_and(is_ours) {
        return None;
    }
    current.remove("statusLine");
    Some(format!("{}\n", serde_json::to_string_pretty(&Value::Object(current)).unwrap_or_default()))
}

/// The command Claude Code runs: this machine's `cork` by its full path.
fn statusline_command(env: &Env) -> String {
    let program = crate::machine::command_path(env).display().to_string();
    let program = if program.contains(' ') { format!("\"{program}\"") } else { program };
    format!("{}{program} statusline", crate::machine::home_prefix(env))
}

/// Whether Claude Code is set up here with no status line of any kind:
/// what the install mentions `cork statusline on` for.
pub fn unset(env: &Env) -> bool {
    let path = crate::machine::claude_settings_path(env);
    path.parent().is_some_and(Path::is_dir)
        && config::load_json(&path).get("statusLine").is_none()
}

/// `cork statusline [on|off]`: the line itself (what Claude Code runs), or
/// Cork in and out of Claude Code's status line.
pub fn command(args: &[String], env: &Env) -> i32 {
    let paint = crate::machine::Paint::new(env);
    let path = crate::machine::claude_settings_path(env);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("on") => match with_statusline(&existing, &statusline_command(env)) {
            Ok(merged) if merged == existing => {
                println!("Cork is in Claude Code's status line already.");
                0
            }
            Ok(merged) => {
                let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
                if std::fs::write(&path, merged).is_err() {
                    println!("Cork statusline: could not write {}.", path.display());
                    return 1;
                }
                println!(
                    "Cork is in Claude Code's status line ({}). Restart open sessions if they do not show it; take it out with {}.",
                    line(3, 2, false),
                    paint.command("cork statusline off")
                );
                0
            }
            Err(why) => {
                println!("Cork statusline: {why}.");
                1
            }
        },
        Some("off") => match without_statusline(&existing) {
            Some(rest) => {
                if std::fs::write(&path, rest).is_err() {
                    println!("Cork statusline: could not write {}.", path.display());
                    return 1;
                }
                println!("Cork is out of Claude Code's status line.");
                0
            }
            None => {
                println!("Cork is not in Claude Code's status line; nothing changed.");
                0
            }
        },
        Some("-h" | "--help" | "help") => {
            println!(
                "usage: {}   Cork in Claude Code's status line: agents in this repo, writes it stopped today\n       {}  take it out (a status line of your own is never touched)",
                paint.command("cork statusline on"),
                paint.command("cork statusline off")
            );
            0
        }
        // what Claude Code runs: one line, or nothing at all
        _ => {
            if let Some(line) = render(env) {
                println!("{line}");
            }
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str = "/Users/ana/.cork/bin/cork statusline";

    #[test]
    fn statusline_on_never_replaces_a_status_line_of_the_persons_own() {
        // none set: Cork's goes in, everything else kept
        let merged = with_statusline(r#"{"model": "opus", "hooks": {}}"#, OURS).unwrap();
        let v: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["statusLine"], json!({"type": "command", "command": OURS}));
        assert_eq!(v["model"], json!("opus"));
        // no file yet
        assert!(with_statusline("", OURS).is_ok());
        // the person's own: refused, the file untouched
        let theirs = r#"{"statusLine": {"type": "command", "command": "~/bin/my-line.sh"}}"#;
        let why = with_statusline(theirs, OURS).unwrap_err();
        assert!(why.contains("~/bin/my-line.sh"), "{why}");
        // a file that is not JSON is never rewritten
        assert!(with_statusline("{ not json", OURS).is_err());
        // Cork's own, under an old path or name: brought up to date
        let old = r#"{"statusLine": {"type": "command", "command": "CORK_HOME=/tmp/h \"/tmp/h/.collide/bin/collide\" statusline"}}"#;
        let v: Value = serde_json::from_str(&with_statusline(old, OURS).unwrap()).unwrap();
        assert_eq!(v["statusLine"]["command"], json!(OURS));
    }

    #[test]
    fn statusline_off_takes_out_only_corks() {
        let merged = with_statusline(r#"{"model": "opus"}"#, OURS).unwrap();
        let v: Value = serde_json::from_str(&without_statusline(&merged).unwrap()).unwrap();
        assert!(v.get("statusLine").is_none());
        assert_eq!(v["model"], json!("opus"));
        let theirs = r#"{"statusLine": {"type": "command", "command": "npx ccstatusline"}}"#;
        assert!(without_statusline(theirs).is_none(), "the person's own stays");
        assert!(without_statusline(r#"{"statusLine": {"type": "command", "command": "corked statusline"}}"#).is_none());
        assert!(without_statusline("{}").is_none());
    }

    #[test]
    fn catches_count_once_per_file_and_session_over_the_local_day() {
        // 7pm on Nov 13 at UTC-8 (03:00 on Nov 14 in UTC): the afternoon's
        // catches are today's there, though UTC's day has turned
        let now = 1_794_625_200;
        let pst = -8 * 3600;
        let log = [
            format!("{}\t{pst}\ts1\tgithub.com/acme/api\tsrc/app.py", now - 5 * 3600),
            format!("{}\t{pst}\ts1\tgithub.com/acme/api\tsrc/app.py", now - 4 * 3600), // the same block again
            format!("{}\t{pst}\ts2\tgithub.com/acme/api\tsrc/app.py", now - 3 * 3600),
            format!("{}\t{pst}\ts1\tgithub.com/acme/web\tsrc/ui.ts", now - 3600), // another repo
            format!("{}\t{pst}\ts1\tgithub.com/acme/api\tsrc/db.py", now - 24 * 3600), // yesterday there
            "garbage".to_string(),
        ]
        .join("\n");
        assert_eq!(count_catches(&log, "github.com/acme/api", now), 2);
        assert_eq!(count_catches(&log, "github.com/acme/web", now), 1);
        assert_eq!(count_catches("", "github.com/acme/api", now), 0);
    }

    #[test]
    fn the_line_says_only_what_there_is() {
        assert_eq!(line(1, 0, false), "cork");
        assert_eq!(line(3, 2, false), "cork · 3 agents here · 2 catches today");
        assert_eq!(line(1, 1, false), "cork · 1 catch today");
        assert!(line(2, 0, true).starts_with("\x1b["));
    }

    #[test]
    fn a_session_is_here_until_it_ends() {
        let home = std::env::temp_dir().join(format!("cork-statusline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let env: Env = [("HOME".to_string(), home.to_string_lossy().to_string())].into_iter().collect();
        let root = Path::new("/work/api");
        note(root, "github.com/acme/api", "s1", "PostToolUse", &env);
        note(Path::new("/work/api-2"), "github.com/acme/api", "s2", "PostToolUse", &env);
        note(Path::new("/work/web"), "github.com/acme/web", "s3", "PostToolUse", &env);
        let here = sessions_here("github.com/acme/api", root, &env);
        assert_eq!(here.into_iter().collect::<Vec<_>>(), vec!["s1".to_string(), "s2".to_string()], "a worktree of the same repo too");
        note(root, "github.com/acme/api", "s1", "SessionEnd", &env);
        assert_eq!(sessions_here("github.com/acme/api", root, &env).len(), 1);
        let _ = std::fs::remove_dir_all(&home);
    }
}
