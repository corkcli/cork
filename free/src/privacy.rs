//! A workspace's choices about what its machines send, and the server's own
//! half of keeping them.
//!
//! Three switches, owner-set on the dashboard's Settings page:
//!
//! - `closing_messages` (on): an agent's last message of a turn becomes the
//!   note on the code it changed, which teammates' briefings carry.
//! - `full_commands` (off): shell commands go whole (paths stripped, secrets
//!   redacted) instead of shortened to the program and its targets.
//! - `session_titles` (on): what the harness calls a conversation, and a
//!   subagent's name, for the roster.
//!
//! The hooks learn them from the answers to /brief and /report and enforce
//! them before anything is sent (cork-hooks privacy.rs). The server
//! enforces them again on what arrives, so an older hook, or one that has
//! not heard yet, is held to the same line: what a switch turns off is
//! dropped before it is stored, and a command is shortened here too.

use serde_json::{json, Value};

use crate::store::Store;

pub const BUCKET: &str = "privacy_settings";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switches {
    pub closing_messages: bool,
    pub full_commands: bool,
    pub session_titles: bool,
}

impl Default for Switches {
    fn default() -> Self {
        Switches { closing_messages: true, full_commands: false, session_titles: true }
    }
}

impl Switches {
    pub fn to_json(self) -> Value {
        json!({"closing_messages": self.closing_messages, "full_commands": self.full_commands, "session_titles": self.session_titles})
    }
}

/// The workspace's switches; the defaults where it never chose.
pub fn of(store: &Store, workspace: &str) -> Switches {
    let d = Switches::default();
    if workspace.is_empty() {
        return d;
    }
    let Some(saved) = store.kv_get(BUCKET, workspace) else { return d };
    let flag = |key: &str, default: bool| saved.get(key).and_then(Value::as_bool).unwrap_or(default);
    Switches {
        closing_messages: flag("closing_messages", d.closing_messages),
        full_commands: flag("full_commands", d.full_commands),
        session_titles: flag("session_titles", d.session_titles),
    }
}

/// Merge the switches named in `body` into the workspace's; the result.
pub fn set(store: &Store, workspace: &str, body: &Value) -> Result<Switches, String> {
    let mut now = of(store, workspace);
    let mut any = false;
    for (key, slot) in [
        ("closing_messages", &mut now.closing_messages),
        ("full_commands", &mut now.full_commands),
        ("session_titles", &mut now.session_titles),
    ] {
        match body.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::Bool(b)) => {
                *slot = *b;
                any = true;
            }
            Some(_) => return Err(format!("{key} is true or false")),
        }
    }
    if !any {
        return Err("name at least one of closing_messages, full_commands, session_titles".into());
    }
    store.kv_put(BUCKET, workspace, &now.to_json(), crate::store::now()).map_err(|e| e.to_string())?;
    Ok(now)
}

fn shorten(raw: &str, full: bool) -> (Option<&'static str>, String) {
    let homeless = cork_core::privacy::strip_home_like(raw);
    if full {
        return (None, homeless);
    }
    let short = cork_core::privacy::shorten_command(&homeless);
    (Some(short.action), short.text)
}

fn shorten_commands(value: &mut Value, full: bool) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                match (key.as_str(), &mut *v) {
                    ("command", Value::String(s)) => *s = shorten(s, full).1,
                    _ => shorten_commands(v, full),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| shorten_commands(v, full)),
        _ => {}
    }
}

/// What arrived at `endpoint`, held to the workspace's switches before
/// anything of it is stored. Idempotent: a current hook's body, already
/// shaped, passes unchanged.
pub fn enforce(store: &Store, workspace: &str, endpoint: &str, body: &mut Value) {
    let switches = of(store, workspace);
    let Some(map) = body.as_object_mut() else { return };
    if !switches.session_titles {
        map.remove("title");
        map.remove("worker_name");
        // what Claude Code calls the session is a name for it too, and its
        // task is its prompt's words
        map.remove("cc_name");
        map.remove("task");
        // a branch's purpose is its session's title, or its first commit's
        if endpoint == "/trunk" {
            map.remove("purpose");
        }
    }
    let action = map.get("action").and_then(Value::as_str).unwrap_or("").to_string();
    if endpoint == "/presence" {
        if action == "settled" && !switches.closing_messages {
            map.insert("text".into(), json!(""));
        }
        // a home directory an older hook left in: `~` instead
        for key in ["text", "title"] {
            if let Some(Value::String(s)) = map.get_mut(key) {
                *s = cork_core::privacy::strip_home_like(s);
            }
        }
        if action == "running" {
            if let Some(raw) = map.get("path").and_then(Value::as_str).map(str::to_string) {
                let (verb, shown) = shorten(&raw, switches.full_commands);
                let started = map.get("started").and_then(Value::as_bool) == Some(true);
                if verb == Some("searching") && !started && !map.contains_key("tests_ok") {
                    map.insert("action".into(), json!("searching"));
                }
                map.insert("path".into(), json!(shown));
            }
        }
    }
    if endpoint == "/land" {
        if let Some(Value::Array(laps)) = map.get_mut("laps") {
            for lap in laps.iter_mut() {
                if let Some(Value::String(test)) = lap.get_mut("test") {
                    *test = shorten(test, switches.full_commands).1;
                }
            }
        }
    }
    shorten_commands(body, switches.full_commands);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("memory store")
    }

    #[test]
    fn the_defaults_until_the_owner_chooses() {
        let store = store();
        assert_eq!(of(&store, "w1"), Switches::default());
        assert!(set(&store, "w1", &json!({})).is_err());
        assert!(set(&store, "w1", &json!({"full_commands": "yes"})).is_err());
        let now = set(&store, "w1", &json!({"closing_messages": false})).unwrap();
        assert_eq!(now, Switches { closing_messages: false, full_commands: false, session_titles: true });
        assert_eq!(of(&store, "w2"), Switches::default(), "one workspace's choice is its own");
    }

    #[test]
    fn what_a_switch_turns_off_is_dropped_on_arrival() {
        let store = store();
        set(&store, "w1", &json!({"closing_messages": false, "session_titles": false})).unwrap();
        let mut settled = json!({"repo_id": "r", "action": "settled", "text": "Renamed compute_tax.", "title": "Fix tax", "worker_name": "w"});
        enforce(&store, "w1", "/presence", &mut settled);
        assert_eq!(settled["text"], json!(""));
        assert!(settled.get("title").is_none() && settled.get("worker_name").is_none());
        // another workspace keeps its notes
        let mut kept = json!({"action": "settled", "text": "Renamed compute_tax.", "title": "Fix tax"});
        enforce(&store, "w2", "/presence", &mut kept);
        assert_eq!(kept["text"], json!("Renamed compute_tax."));
        assert_eq!(kept["title"], json!("Fix tax"));
    }

    #[test]
    fn an_older_hooks_command_is_shortened_here() {
        let store = store();
        let mut ran = json!({"action": "running", "path": "cd /Users/kim/code/app && OPENAI_API_KEY=sk-abcdefghijklmnopqrstuv python -m pytest -q tests/a.py 2>&1 | tail -5"});
        enforce(&store, "w1", "/presence", &mut ran);
        assert_eq!(ran["path"], json!("python -m pytest -q tests/a.py | tail"));
        let mut grep = json!({"action": "running", "path": "grep -rn compute_tax src/"});
        enforce(&store, "w1", "/presence", &mut grep);
        assert_eq!((grep["action"].clone(), grep["path"].clone()), (json!("searching"), json!("\"compute_tax\"")));
        let mut verified = json!({"paths": ["a.py"], "command": "cd /home/dev/app && pytest -q", "ok": true});
        enforce(&store, "w1", "/verified", &mut verified);
        assert_eq!(verified["command"], json!("pytest -q"));
        // a workspace that keeps commands whole: only the home goes
        set(&store, "w3", &json!({"full_commands": true})).unwrap();
        let mut whole = json!({"action": "running", "path": "cd /Users/kim/app && make test"});
        enforce(&store, "w3", "/presence", &mut whole);
        assert_eq!(whole["path"], json!("cd ~/app && make test"));
        // a current hook's body passes unchanged
        let mut shaped = json!({"action": "running", "path": "python -m pytest -q tests/a.py | tail"});
        let before = shaped.clone();
        enforce(&store, "w1", "/presence", &mut shaped);
        assert_eq!(shaped, before);
    }
}
