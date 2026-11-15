//! First touch: the briefing for a repo a session reaches after it started.
//!
//! A chat that starts in a project folder (Codex's projects, a scratch
//! folder) and works in a repo elsewhere on the machine gets no session-start
//! briefing: the folder it started in is not a repo. Its writes are still
//! gated and reported against the right repo, because those look at the
//! file's own folder, but nothing told it who else is working there.
//!
//! So the first time a session reads or writes a file in a repo it has not
//! been briefed on, the server's small first-touch briefing (brief::touch)
//! rides in on that tool call's result, once: who is live in the repo, the
//! owner's decisions and the notes on the files touched, and for a write who
//! calls them and what a teammate changed there lately. Nothing when there is
//! nothing to say, which is most of the time for one person's agents.
//!
//! Which repos a session has been briefed on lives on this machine
//! (~/.collide/touch/<session>.json): checking it is a file read, so a call
//! in an already-briefed repo never reaches the network. The session-start
//! briefing marks its own repo; a compaction clears every mark, because the
//! briefing left the context with it.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::config::{self, Env};

/// Tries per repo when the server does not answer, so a dead server cannot
/// add a request to every tool call.
const MAX_TRIES: i64 = 3;
/// The most a first touch may spend waiting on the server.
pub const TIMEOUT: Duration = Duration::from_millis(400);

fn state_path(session: &str, env: &Env) -> PathBuf {
    let tag: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(64).collect();
    crate::config::data_dir(env).join("touch").join(format!("{tag}.json"))
}

fn load(session: &str, env: &Env) -> Map<String, Value> {
    config::load_json(&state_path(session, env)).as_object().cloned().unwrap_or_default()
}

fn save(session: &str, env: &Env, state: &Map<String, Value>) {
    crate::report::save_json(&state_path(session, env), &Value::Object(state.clone()));
}

fn briefed(state: &Map<String, Value>, repo_id: &str) -> bool {
    state.get("briefed").and_then(Value::as_array).is_some_and(|l| l.iter().any(|r| r.as_str() == Some(repo_id)))
}

/// This session has its briefing on `repo_id` (the session start gave it).
pub fn mark(session: &str, repo_id: &str, env: &Env) {
    if session.is_empty() || repo_id.is_empty() {
        return;
    }
    let mut state = load(session, env);
    if briefed(&state, repo_id) {
        return;
    }
    let mut list = state.get("briefed").and_then(Value::as_array).cloned().unwrap_or_default();
    list.push(json!(repo_id));
    state.insert("briefed".into(), Value::Array(list));
    save(session, env, &state);
}

/// The context was compacted or cleared: every briefing left it.
pub fn clear(session: &str, env: &Env) {
    if !session.is_empty() {
        let _ = std::fs::remove_file(state_path(session, env));
    }
}

/// The first-touch briefing for `repo_id`, or "" when this session already
/// has it, the server had nothing to say, or it could not be asked. `paths`
/// are repo-relative; `write` asks for callers and teammates' changes too.
pub fn note(
    cfg: &config::Config, session: &str, paths: &[String], write: bool, budget: Duration, env: &Env,
) -> String {
    if session.is_empty() || cfg.repo_id.is_empty() {
        return String::new();
    }
    let mut state = load(session, env);
    if briefed(&state, &cfg.repo_id) {
        return String::new();
    }
    let tries_key = format!("tries:{}", cfg.repo_id);
    let tries = state.get(&tries_key).and_then(Value::as_i64).unwrap_or(0);
    if tries >= MAX_TRIES {
        return String::new();
    }
    let payload = json!({"repo_id": cfg.repo_id, "session": session, "paths": paths, "write": write});
    let wait = budget.min(TIMEOUT);
    match crate::http::post(&cfg.server, "/brief/touch", &cfg.token, &crate::report::user_agent(), &payload, wait) {
        Ok(answer) if answer.get("ok").and_then(Value::as_bool) == Some(true) => {
            state.remove(&tries_key);
            let mut list = state.get("briefed").and_then(Value::as_array).cloned().unwrap_or_default();
            list.push(json!(cfg.repo_id));
            state.insert("briefed".into(), Value::Array(list));
            save(session, env, &state);
            answer.get("text").and_then(Value::as_str).unwrap_or("").to_string()
        }
        _ => {
            state.insert(tries_key, json!(tries + 1));
            save(session, env, &state);
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_in(dir: &std::path::Path) -> Env {
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().to_string());
        env
    }

    #[test]
    fn a_marked_repo_is_never_asked_about_and_a_compaction_forgets_it() {
        let dir = std::env::temp_dir().join(format!("collide-touch-{}", std::process::id()));
        let env = env_in(&dir);
        mark("s1", "github.com/acme/api", &env);
        assert!(briefed(&load("s1", &env), "github.com/acme/api"));
        assert!(!briefed(&load("s1", &env), "github.com/acme/web"));
        // briefed: answered locally, with no server to ask
        let cfg = config::Config {
            server: "http://127.0.0.1:9".into(), token: "t".into(), repo_id: "github.com/acme/api".into(),
            verify: Default::default(), local_parse: false, machine_local: false,
        };
        assert_eq!(note(&cfg, "s1", &[], false, Duration::from_millis(50), &env), "");
        clear("s1", &env);
        assert!(!briefed(&load("s1", &env), "github.com/acme/api"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_server_that_does_not_answer_is_tried_three_times_then_left_alone() {
        let dir = std::env::temp_dir().join(format!("collide-touch-tries-{}", std::process::id()));
        let env = env_in(&dir);
        let cfg = config::Config {
            server: "http://127.0.0.1:9".into(), token: "t".into(), repo_id: "github.com/acme/web".into(),
            verify: Default::default(), local_parse: false, machine_local: false,
        };
        for _ in 0..5 {
            assert_eq!(note(&cfg, "s2", &["a.py".into()], true, Duration::from_millis(50), &env), "");
        }
        assert_eq!(load("s2", &env).get("tries:github.com/acme/web").and_then(Value::as_i64), Some(MAX_TRIES));
        assert!(!briefed(&load("s2", &env), "github.com/acme/web"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
