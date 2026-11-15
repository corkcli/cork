//! What leaves this machine, decided at the one door every request goes
//! through (http.rs): applied to the body just before it is sent.
//!
//! - Absolute paths become repo-relative (the repo root, and any worktree
//!   root, is removed) and the home directory is written `~`, in every
//!   string of every body: commands, notes, closing messages, titles, error
//!   tails.
//! - A shell command is shortened to its action and targets
//!   (cork_core::privacy::shorten_command), unless the workspace turned
//!   `full_commands` on; then it goes whole, with secrets redacted.
//! - A file's text never goes: a stray `content` beside a `path` on /report
//!   or /observe is replaced by the structure parsed here.
//! - The workspace's switches (`closing_messages`, `session_titles`) drop
//!   what they turn off. The server sends them with its answers (the
//!   briefing, every report) and they are cached here, per server and repo.
//!
//! Fails open: anything that cannot be worked out leaves the body as the
//! caller built it, minus nothing it could not compute.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Map, Value};

/// The roots this process found (config.rs notes each one it resolves).
static ROOTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A repo root (or worktree) whose paths are made relative to it.
pub fn note_root(root: &Path) {
    let text = crate::config::absolute(root).to_string_lossy().to_string();
    if text.len() <= 1 {
        return;
    }
    if let Ok(mut roots) = ROOTS.lock() {
        if !roots.contains(&text) {
            roots.push(text);
        }
    }
}

fn roots() -> Vec<String> {
    let mut out = ROOTS.lock().map(|r| r.clone()).unwrap_or_default();
    // what a hook is told before it resolves a root of its own
    for key in ["CLAUDE_PROJECT_DIR", "CURSOR_PROJECT_DIR"] {
        if let Ok(dir) = std::env::var(key) {
            if dir.len() > 1 && !out.contains(&dir) {
                out.push(dir);
            }
        }
    }
    out
}

fn home() -> String {
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(home) = std::env::var(key) {
            if home.len() > 1 {
                return home;
            }
        }
    }
    String::new()
}

/// A text with this machine's paths taken out.
pub fn strip(text: &str) -> String {
    cork_core::privacy::strip_paths(text, &roots(), &home())
}

// ------------------------------------------------------------ the switches

/// A workspace's choices about what its hooks send. The defaults are what
/// the server assumes for a workspace that never chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switches {
    /// An agent's final message of a turn, kept as the note on what it changed.
    pub closing_messages: bool,
    /// Shell commands whole (secrets redacted, paths stripped), not shortened.
    pub full_commands: bool,
    /// What the harness calls the conversation, and a subagent's name.
    pub session_titles: bool,
}

impl Default for Switches {
    fn default() -> Self {
        Switches { closing_messages: true, full_commands: false, session_titles: true }
    }
}

impl Switches {
    pub fn from_json(value: &Value) -> Option<Switches> {
        let map = value.as_object()?;
        let d = Switches::default();
        let flag = |key: &str, default: bool| map.get(key).and_then(Value::as_bool).unwrap_or(default);
        Some(Switches {
            closing_messages: flag("closing_messages", d.closing_messages),
            full_commands: flag("full_commands", d.full_commands),
            session_titles: flag("session_titles", d.session_titles),
        })
    }

    pub fn to_json(self) -> Value {
        json!({"closing_messages": self.closing_messages, "full_commands": self.full_commands, "session_titles": self.session_titles})
    }
}

fn data_dir() -> PathBuf {
    crate::config::data_dir(&crate::config::env_map())
}

fn cache_path(server: &str, repo_id: &str) -> PathBuf {
    let digest = crate::report::sha256_hex(format!("{server}\n{repo_id}").as_bytes());
    data_dir().join("privacy").join(format!("{}.json", &digest[..16]))
}

/// The switches last heard from `server` for `repo_id`; the defaults when
/// nothing was heard yet.
pub fn switches(server: &str, repo_id: &str) -> Switches {
    if server.is_empty() {
        return Switches::default();
    }
    Switches::from_json(&crate::config::load_json(&cache_path(server, repo_id))).unwrap_or_default()
}

/// The server answered with the workspace's switches: kept for the next
/// request (written only when they changed).
pub fn learn(server: &str, sent: &Value, answer: &Value) {
    let Some(heard) = answer.get("privacy").and_then(Switches::from_json) else { return };
    let repo_id = sent.get("repo_id").and_then(Value::as_str).unwrap_or("");
    if server.is_empty() || repo_id.is_empty() || switches(server, repo_id) == heard && cache_path(server, repo_id).exists() {
        return;
    }
    crate::report::save_json(&cache_path(server, repo_id), &heard.to_json());
}

// ------------------------------------------------------------ the body

/// A command as it may leave: shortened (the action may change, a grep is
/// `searching` its pattern), or whole with secrets redacted.
pub fn command(raw: &str, switches: Switches) -> (Option<&'static str>, String) {
    let stripped = strip(raw.trim());
    if switches.full_commands {
        let redacted = crate::report::redact_command(&stripped);
        let capped: String = if redacted.chars().count() > 300 {
            format!("{}\u{2026}", redacted.chars().take(299).collect::<String>())
        } else {
            redacted
        };
        return (None, capped);
    }
    let short = cork_core::privacy::shorten_command(&stripped);
    // the regex redaction too: a shape the tokenizer missed still goes
    (Some(short.action), crate::report::redact_command(&short.text))
}

fn strip_all(value: &mut Value, roots: &[String], home: &str) {
    match value {
        Value::String(s) => {
            if s.contains('/') || s.contains('\\') {
                let out = cork_core::privacy::strip_paths(s, roots, home);
                if out != *s {
                    *s = out;
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| strip_all(v, roots, home)),
        Value::Object(map) => map.values_mut().for_each(|v| strip_all(v, roots, home)),
        _ => {}
    }
}

/// `{path, content}` becomes `{path, structure, line_hashes, names}`.
fn no_content(map: &mut Map<String, Value>) {
    let Some(Value::String(content)) = map.remove("content") else { return };
    let path = map.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    if !path.is_empty() {
        map.extend(cork_core::local::fields(&path, &content));
    }
}

fn shorten_commands(value: &mut Value, switches: Switches) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if key == "command" {
                    if let Value::String(s) = v {
                        *s = command(s, switches).1;
                        continue;
                    }
                }
                shorten_commands(v, switches);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| shorten_commands(v, switches)),
        _ => {}
    }
}

/// The body that is actually sent for `endpoint`.
pub fn outbound(server: &str, endpoint: &str, body: &Value) -> Value {
    let mut out = body.clone();
    let repo_id = body.get("repo_id").and_then(Value::as_str).unwrap_or("");
    let switches = switches(server, repo_id);
    let path_only = endpoint.split('?').next().unwrap_or(endpoint);
    if let Some(map) = out.as_object_mut() {
        if !switches.session_titles {
            map.remove("title");
            map.remove("worker_name");
        }
        let action = map.get("action").and_then(Value::as_str).unwrap_or("").to_string();
        if path_only == "/presence" && action == "settled" && !switches.closing_messages {
            map.insert("text".into(), json!(""));
        }
        // what an agent ran: the command is the presence's path
        if path_only == "/presence" && action == "running" {
            if let Some(raw) = map.get("path").and_then(Value::as_str).map(str::to_string) {
                let (verb, shown) = command(&raw, switches);
                let started = map.get("started").and_then(Value::as_bool) == Some(true);
                if verb == Some("searching") && !started && !map.contains_key("tests_ok") {
                    map.insert("action".into(), json!("searching"));
                }
                map.insert("path".into(), json!(shown));
            }
        }
        if matches!(path_only, "/report" | "/observe") {
            no_content(map);
            if let Some(Value::Array(files)) = map.get_mut("files") {
                for file in files.iter_mut() {
                    if let Some(file) = file.as_object_mut() {
                        no_content(file);
                    }
                }
            }
        }
        // a flaky test is its name and two counts: whatever else a caller
        // put in the body (output, a traceback) never leaves (flaky.rs)
        if path_only == "/flaky" {
            let tests: Vec<Value> = map
                .get("tests")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|t| {
                            let id = crate::flaky::clean_id(t.get("id").and_then(Value::as_str).unwrap_or(""));
                            let count = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
                            (!id.is_empty()).then(|| json!({"id": id, "runs": count("runs"), "fails": count("fails")}))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let repo = map.get("repo_id").cloned().unwrap_or(Value::Null);
            map.clear();
            map.insert("repo_id".into(), repo);
            map.insert("tests".into(), Value::Array(tests));
        }
        // a landing's laps name the test command
        if path_only == "/land" {
            if let Some(Value::Array(laps)) = map.get_mut("laps") {
                for lap in laps.iter_mut() {
                    if let Some(Value::String(test)) = lap.get_mut("test") {
                        *test = command(test, switches).1;
                    }
                }
            }
        }
    }
    shorten_commands(&mut out, switches);
    strip_all(&mut out, &roots(), &home());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_root(root: &str) {
        note_root(Path::new(root));
    }

    #[test]
    fn every_string_loses_the_repo_root_and_the_home() {
        let home = home();
        with_root("/work/acme-app");
        let body = json!({"repo_id": "r", "path": "cd /work/acme-app/server && pytest", "action": "running",
                          "text": format!("Edited /work/acme-app/src/a.py and {home}/notes.txt"),
                          "nested": [{"tail": "  File \"/work/acme-app/src/a.py\", line 3"}]});
        let out = outbound("", "/presence", &body);
        assert_eq!(out["path"], json!("pytest"), "cd is navigation; the command is shortened");
        assert_eq!(out["text"], json!("Edited src/a.py and ~/notes.txt"));
        assert_eq!(out["nested"][0]["tail"], json!("  File \"src/a.py\", line 3"));
    }

    #[test]
    fn a_grep_is_a_search_for_its_pattern() {
        let body = json!({"repo_id": "r", "path": "grep -rn compute_tax src/", "action": "running"});
        let out = outbound("", "/presence", &body);
        assert_eq!(out["action"], json!("searching"));
        assert_eq!(out["path"], json!("\"compute_tax\""));
    }

    #[test]
    fn raw_content_never_leaves_on_a_report_or_an_observation() {
        let body = json!({"repo_id": "r", "path": "a.py", "content": "def f(x):\n    return x\n"});
        let out = outbound("", "/report", &body);
        assert!(out.get("content").is_none());
        assert!(out.get("structure").is_some() && out.get("line_hashes").is_some() && out.get("names").is_some());
        let batch = json!({"repo_id": "r", "files": [{"path": "b.py", "content": "x = 1\n"}]});
        let out = outbound("", "/observe", &batch);
        assert!(out["files"][0].get("content").is_none());
        // an agent's message is not a file: left as it is
        let message = json!({"repo_id": "r", "content": "hello"});
        assert_eq!(outbound("", "/message", &message)["content"], json!("hello"));
    }

    #[test]
    fn the_switches_drop_what_they_turn_off() {
        let dir = std::env::temp_dir().join(format!("collide-privacy-switch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("CORK_HOME", &dir);
        let server = "https://switches.example";
        let sent = json!({"repo_id": "github.com/o/r"});
        // nothing heard yet: the defaults
        assert_eq!(switches(server, "github.com/o/r"), Switches::default());
        let settled = json!({"repo_id": "github.com/o/r", "action": "settled", "text": "Renamed compute_tax to tax_of in billing.",
                             "title": "Fix billing", "worker_name": "tests"});
        let out = outbound(server, "/presence", &settled);
        assert_eq!(out["text"], settled["text"]);
        assert_eq!(out["title"], json!("Fix billing"));
        // the server says: no closing messages, no titles, whole commands
        learn(server, &sent, &json!({"ok": true, "privacy": {"closing_messages": false, "session_titles": false, "full_commands": true}}));
        let out = outbound(server, "/presence", &settled);
        assert_eq!(out["text"], json!(""));
        assert!(out.get("title").is_none() && out.get("worker_name").is_none());
        let ran = json!({"repo_id": "github.com/o/r", "action": "running", "path": "STRIPE_KEY=sk_live_abcdef1234567890 npm run seed -- --big"});
        assert_eq!(outbound(server, "/presence", &ran)["path"], json!("STRIPE_KEY=[redacted] npm run seed -- --big"));
        // another repo on the same server keeps its own (default) switches
        assert_eq!(outbound(server, "/presence", &json!({"repo_id": "other", "action": "settled", "text": "x"}))["text"], json!("x"));
        std::env::remove_var("CORK_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_command_field_anywhere_is_shortened() {
        let body = json!({"repo_id": "r", "verified": {"command": "cd x && DATABASE_URL=postgres://u:p@h/db python -c 'import x' && pytest -q", "ok": true}});
        let out = outbound("", "/report", &body);
        assert_eq!(out["verified"]["command"], json!("python -c \u{2026} && pytest -q"));
    }

    #[test]
    fn a_flaky_report_is_names_and_counts_only() {
        let body = json!({"repo_id": "r", "output": "Traceback: secret", "tests": [
            {"id": "tests/a.py::test_x", "runs": 2, "fails": 1, "tail": "AssertionError: token=abc"},
            {"id": "check", "runs": 2, "fails": 1}]});
        let out = outbound("", "/flaky", &body);
        assert_eq!(out, json!({"repo_id": "r", "tests": [{"id": "tests/a.py::test_x", "runs": 2, "fails": 1}]}));
    }
}
