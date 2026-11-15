//! `cork mcp`: Cork's MCP tools for the agents on this machine, over
//! stdio. The local server already serves MCP at `/mcp`; agent tools start
//! MCP servers as programs, so this relays each line the agent writes to
//! that endpoint and writes back what it answers. The server is started when
//! it is not running, and a server that restarted (an update replaces it)
//! gets a fresh session, opened the way the agent opened the first.
//!
//! In a repo that works with a team the same relay talks to Cork's cloud
//! instead, with the credential the machine already holds: one sign-in
//! (`cork login`, or the code `setup` hands out) covers hooks and tools.
//!
//! A relay, not a request/response loop: every message the agent sends is
//! posted at once on its own thread, and every event the server streams
//! back is written the moment it arrives. A server may ask the client
//! something in the middle of a call and wait for the answer; holding its
//! stream until it closed left that question unread and the call waiting.

use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::Env;
use crate::machine::{ensure_local, local_endpoint};

/// The longest one call may take before the agent is told it failed.
const ANSWER_WITHIN: Duration = Duration::from_secs(120);
/// The server closes an MCP session left idle for five minutes; a call sent
/// as it closes is never answered, so a session idle this long is reopened.
const REOPEN_AFTER: Duration = Duration::from_secs(240);

enum Sent {
    /// the server took it (and, for a request, answered it)
    Done,
    /// no server, or it no longer knows this session
    Gone,
    Failed(String),
}

struct Relay {
    env: Env,
    /// Cork's cloud, with this machine's own credential, when the repo
    /// (or the machine) works with a team; None: the local server
    cloud: Option<(String, String)>,
    out: Mutex<std::io::Stdout>,
    session: Mutex<String>,
    /// how the agent opened its session, replayed when it is reopened
    opening: Mutex<Vec<String>>,
    last_sent: Mutex<Instant>,
    /// one reopen at a time
    reopening: Mutex<()>,
}

/// Each server-sent event's `data`, handed to `emit` as the event ends;
/// stops early when `emit` says the answer is in (the stream may stay open).
fn read_events(reader: impl BufRead, mut emit: impl FnMut(&str) -> bool) {
    let mut data: Vec<String> = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            let done = emit(&data.join("\n"));
            data.clear();
            if done {
                return;
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    emit(&data.join("\n"));
}

impl Relay {
    fn write(&self, line: &str) {
        if let Ok(mut out) = self.out.lock() {
            let _ = writeln!(out, "{line}").and_then(|_| out.flush());
        }
    }

    fn session(&self) -> String {
        self.session.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Post one message. Each event the server streams back is written as
    /// it arrives (unless `quiet`); `id` is the answer to wait for.
    fn send(&self, body: &str, id: Option<&Value>, quiet: bool) -> Sent {
        let (base, token) = self.cloud.clone().unwrap_or_else(|| local_endpoint(&self.env));
        let session = self.session();
        let mut request = ureq::AgentBuilder::new()
            .timeout(ANSWER_WITHIN)
            .build()
            .post(&format!("{base}/mcp"))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("Authorization", &format!("Bearer {token}"));
        if !session.is_empty() {
            request = request.set("Mcp-Session-Id", &session);
        }
        crate::http::audit_raw("POST", &format!("{base}/mcp"), body);
        let response = match request.send_string(body) {
            Ok(response) => response,
            Err(ureq::Error::Status(404, _)) | Err(ureq::Error::Transport(_)) => return Sent::Gone,
            Err(ureq::Error::Status(code, response)) => {
                return Sent::Failed(format!("{code} {}", response.into_string().unwrap_or_default().trim()))
            }
        };
        if let Some(new) = response.header("mcp-session-id") {
            if let Ok(mut s) = self.session.lock() {
                *s = new.to_string();
            }
        }
        let streamed = response.content_type().contains("event-stream");
        let mut answered = false;
        let emit = |payload: &str, answered: &mut bool| {
            let payload = payload.trim();
            if payload.is_empty() {
                return;
            }
            if let (Some(id), Ok(message)) = (id, serde_json::from_str::<Value>(payload)) {
                if message.get("id") == Some(id) && (message.get("result").is_some() || message.get("error").is_some()) {
                    *answered = true;
                }
            }
            if !quiet {
                self.write(payload);
            }
        };
        let reader = BufReader::new(response.into_reader());
        if streamed {
            read_events(reader, |payload| {
                emit(payload, &mut answered);
                answered
            });
        } else {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                emit(&line, &mut answered);
            }
        }
        if id.is_some() && !answered {
            return Sent::Failed("no answer in time".into());
        }
        Sent::Done
    }

    /// Open a new session the way the agent opened its first.
    fn reopen(&self, stale: &str) {
        let Ok(_one) = self.reopening.lock() else { return };
        if self.session() != stale {
            return; // another thread already reopened it
        }
        if let Ok(mut s) = self.session.lock() {
            s.clear();
        }
        if self.cloud.is_none() {
            ensure_local(&self.env, Duration::from_secs(8));
        }
        let opening = self.opening.lock().map(|o| o.clone()).unwrap_or_default();
        for earlier in opening {
            let id = serde_json::from_str::<Value>(&earlier).ok().and_then(|m| m.get("id").cloned());
            let _ = self.send(&earlier, id.as_ref(), true);
        }
    }

    fn handle(&self, line: &str, message: &Value) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        // a reply from the agent to the server's own question has an id and no method
        let wanted = if method.is_empty() { None } else { message.get("id") };
        if !matches!(method, "initialize" | "notifications/initialized") {
            let idle = self.last_sent.lock().map(|t| t.elapsed() > REOPEN_AFTER).unwrap_or(false);
            let stale = self.session();
            if idle && !stale.is_empty() {
                self.reopen(&stale);
            }
        }
        if let Ok(mut t) = self.last_sent.lock() {
            *t = Instant::now();
        }
        let mut sent = self.send(line, wanted, false);
        if matches!(sent, Sent::Gone) && method != "notifications/initialized" {
            if method == "initialize" {
                if self.cloud.is_none() {
                    ensure_local(&self.env, Duration::from_secs(8));
                }
            } else {
                let stale = self.session();
                self.reopen(&stale);
            }
            sent = self.send(line, wanted, false);
        }
        let why = match sent {
            Sent::Done => return,
            Sent::Gone => "Cork's local server is not running".to_string(),
            Sent::Failed(why) => format!("Cork's local server: {why}"),
        };
        if wanted.is_some() {
            self.write(&failure(message, &why));
        }
    }
}

/// Where this session's MCP tools live: a repo (or a machine) that works
/// with a team talks to Cork's cloud with the credential the machine
/// already holds, so connecting the tools needs no sign-in of its own;
/// everything else talks to the local server.
fn cloud_target(env: &Env) -> Option<(String, String)> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let project = crate::config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project.is_empty()).then(|| std::path::PathBuf::from(project));
    if let Some(root) = crate::config::find_repo_root(&[Some(cwd), project]) {
        let cfg = crate::config::config(Some(&root), env);
        if cfg.usable() && !cfg.machine_local {
            return Some((cfg.server.trim_end_matches('/').to_string(), cfg.token));
        }
        return None;
    }
    // outside any repo: the machine's own mode
    crate::machine::cloud_account(env)
}

/// The repo id of the checkout these tools were started in ("" outside one).
fn checkout_repo_id(env: &Env) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let project = crate::config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project.is_empty()).then(|| std::path::PathBuf::from(project));
    crate::config::find_repo_root(&[Some(cwd), project])
        .map(|root| crate::config::config(Some(&root), env).repo_id)
        .unwrap_or_default()
}

/// A tool call that names no repo runs in this session's checkout: every
/// Cork tool takes `repo_id`, and agents went hunting for it (v76).
/// The line as it was when there is nothing to fill in.
fn with_repo_id(message: &Value, repo_id: &str) -> Option<String> {
    if repo_id.is_empty() || message.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    let given = message.pointer("/params/arguments/repo_id").and_then(Value::as_str).unwrap_or("");
    if !given.trim().is_empty() {
        return None;
    }
    let mut filled = message.clone();
    let params = filled.get_mut("params")?.as_object_mut()?;
    let args = params.entry("arguments").or_insert_with(|| json!({}));
    args.as_object_mut()?.insert("repo_id".into(), json!(repo_id));
    Some(filled.to_string())
}

pub fn run(env: &Env) -> i32 {
    let cloud = cloud_target(env);
    let repo_id = checkout_repo_id(env);
    if cloud.is_none() {
        ensure_local(env, Duration::from_secs(8));
    }
    // open as long as the agent session: brings Cork into agent tools installed since
    crate::harnesses::watch(env.clone());
    let relay = Arc::new(Relay {
        env: env.clone(),
        cloud,
        out: Mutex::new(std::io::stdout()),
        session: Mutex::new(String::new()),
        opening: Mutex::new(Vec::new()),
        last_sent: Mutex::new(Instant::now()),
        reopening: Mutex::new(()),
    });
    let mut running: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut mapped = false;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim().to_string();
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        if matches!(method.as_str(), "initialize" | "notifications/initialized") {
            if let Ok(mut opening) = relay.opening.lock() {
                if method == "initialize" {
                    opening.clear();
                }
                opening.push(line.clone());
            }
            if method == "initialize" {
                if let Ok(mut s) = relay.session.lock() {
                    s.clear();
                }
            }
            // the session must exist before anything else is sent on it
            relay.handle(&line, &message);
            continue;
        }
        // the first tool call maps the repo it is made from, as the first
        // hook call does: an agent that reaches Cork only through these
        // tools still gets a graph, once per repo and server on this machine
        if !mapped && method == "tools/call" {
            mapped = true;
            crate::machine::index_here(env);
        }
        let (line, message) = match with_repo_id(&message, &repo_id) {
            Some(filled) => {
                let parsed = serde_json::from_str::<Value>(&filled).unwrap_or(message);
                (filled, parsed)
            }
            None => (line, message),
        };
        running.retain(|t| !t.is_finished());
        let relay = Arc::clone(&relay);
        running.push(std::thread::spawn(move || relay.handle(&line, &message)));
    }
    for thread in running {
        let _ = thread.join();
    }
    0
}

fn failure(message: &Value, why: &str) -> String {
    json!({"jsonrpc": "2.0", "id": message.get("id").cloned().unwrap_or(Value::Null),
           "error": {"code": -32603, "message": why}})
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_call_without_a_repo_runs_in_the_sessions_checkout() {
        let call = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "get_symbol", "arguments": {"path": "a.py"}}});
        let filled = with_repo_id(&call, "github.com/acme/app").expect("filled");
        let filled: Value = serde_json::from_str(&filled).unwrap();
        assert_eq!(filled["params"]["arguments"]["repo_id"], "github.com/acme/app");
        assert_eq!(filled["params"]["arguments"]["path"], "a.py");
        // a call that names its repo, another method, or no checkout: untouched
        let named = json!({"method": "tools/call", "params": {"name": "recap", "arguments": {"repo_id": "x/y"}}});
        assert!(with_repo_id(&named, "github.com/acme/app").is_none());
        assert!(with_repo_id(&json!({"method": "tools/list"}), "github.com/acme/app").is_none());
        assert!(with_repo_id(&call, "").is_none());
        // no arguments at all
        let bare = json!({"method": "tools/call", "params": {"name": "recap"}});
        let filled: Value = serde_json::from_str(&with_repo_id(&bare, "r/s").unwrap()).unwrap();
        assert_eq!(filled["params"]["arguments"]["repo_id"], "r/s");
    }

    #[test]
    fn events_are_handed_on_as_they_end_and_reading_stops_at_the_answer() {
        let stream = "id: 0/0\nretry: 3000\ndata:\n\ndata: {\"method\":\"elicitation/create\",\"id\":7}\n\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\ndata: {\"never\":\"read\"}\n\n";
        let mut seen: Vec<String> = Vec::new();
        read_events(stream.as_bytes(), |payload| {
            if !payload.trim().is_empty() {
                seen.push(payload.to_string());
            }
            payload.contains("\"result\"")
        });
        assert_eq!(seen, vec![r#"{"method":"elicitation/create","id":7}"#.to_string(), r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string()]);
    }
}
