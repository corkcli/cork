//! What leaves the machine, end to end: the real hook program, run the way
//! an agent's harness runs it, against a stand-in server that keeps every
//! body it is sent. The audit log must hold exactly those bodies, and none
//! of them may carry a file's text, an absolute local path, a secret, or
//! what the workspace's switches turned off. The protocol's WebSocket
//! frames are logged too.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// A server that answers every POST with `answer` and keeps (path, body).
struct Stub {
    port: u16,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    answer: Arc<Mutex<Value>>,
}

fn stub() -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let answer = Arc::new(Mutex::new(json!({"ok": true})));
    let (s, a) = (seen.clone(), answer.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (s, a) = (s.clone(), a.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    return;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                    s.lock().unwrap().push((path, v));
                }
                let text = a.lock().unwrap().to_string();
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            });
        }
    });
    Stub { port, seen, answer }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
    assert!(ok.success(), "git {args:?}");
}

struct Machine {
    home: PathBuf,
    root: PathBuf,
    audit: PathBuf,
    port: u16,
}

impl Machine {
    fn hook(&self, input: Value) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cork-hook"))
            .arg("report")
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("CORK_AUDIT_DIR", &self.audit)
            .env("CORK_SERVER_URL", format!("http://127.0.0.1:{}", self.port))
            .env("CORK_TOKEN", "cat_test_token_0123456789abcdef")
            .env("CORK_REPO_ID", "github.com/acme/app")
            .env("CORK_PROTOCOL", "0")
            .env("CORK_LISTEN", "0")
            .env("CORK_NATIVE_INSTALL", "0")
            .env("CORK_NO_BROWSER", "1")
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the hook runs");
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        let _ = child.wait();
    }

    fn records(&self) -> Vec<Value> {
        cork_hooks::audit::records(&self.audit)
    }
}

fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
        Value::Object(m) => m.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

#[test]
fn nothing_private_leaves_and_the_audit_log_holds_exactly_what_did() {
    let server = stub();
    let base = std::env::temp_dir().join(format!("collide-trust-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let home = base.join("home");
    let root = home.join("code").join("app");
    std::fs::create_dir_all(root.join(".collide")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // an old per-repo config that still asks for file content: ignored
    std::fs::write(root.join(".collide/config.json"), r#"{"local_parse": false}"#).unwrap();
    let source = "\"\"\"Billing: what each order owes.\"\"\"\n\n\ndef compute_tax(amount, rate):\n    \"\"\"The tax on one order.\"\"\"\n    secret_formula = amount * rate\n    return round(secret_formula, 2)\n";
    std::fs::write(root.join("src/billing.py"), source).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "kim@x.com"]);
    git(&root, &["config", "user.name", "kim"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "seed"]);
    let machine = Machine { home: home.clone(), root: root.clone(), audit: base.join("audit"), port: server.port };
    let file = root.join("src/billing.py").to_string_lossy().to_string();
    let at = |event: &str| json!({"hook_event_name": event, "session_id": "s-trust", "cwd": root.to_string_lossy()});

    // the session starts: the tracked tree is indexed, the briefing asked for
    machine.hook(at("SessionStart"));
    // an edit, a read, a command with a secret and absolute paths, a search
    let mut edit = at("PostToolUse");
    edit["tool_name"] = json!("Edit");
    edit["tool_input"] = json!({"file_path": file, "old_string": "2)", "new_string": "2)"});
    machine.hook(edit);
    let mut read = at("PostToolUse");
    read["tool_name"] = json!("Read");
    read["tool_input"] = json!({"file_path": file});
    machine.hook(read);
    let mut ran = at("PostToolUse");
    ran["tool_name"] = json!("Bash");
    ran["tool_input"] = json!({"command": format!(
        "cd {} && STRIPE_KEY=sk_live_abcdef1234567890 python -c 'import os; print(1)' && pytest -q {}/tests 2>&1 | tail -5",
        root.display(), root.display())});
    machine.hook(ran);
    let mut searched = at("PostToolUse");
    searched["tool_name"] = json!("Bash");
    searched["tool_input"] = json!({"command": "grep -rn compute_tax src/"});
    machine.hook(searched);
    // the turn ends with a closing message naming a local path
    let mut stop = at("Stop");
    stop["last_assistant_message"] = json!(format!("Changed compute_tax in {}/src/billing.py; notes in {}/notes.txt.", root.display(), home.display()));
    machine.hook(stop.clone());

    // the workspace turns closing messages off: the next turn's goes empty
    *server.answer.lock().unwrap() = json!({"ok": true, "privacy": {"closing_messages": false, "full_commands": false, "session_titles": false}});
    let mut prompt = at("UserPromptSubmit");
    prompt["prompt"] = json!("fix compute_tax rounding");
    machine.hook(prompt);
    machine.hook(stop);

    // the detached git baseline posts from its own process: logged too
    let started = Instant::now();
    let landed = || {
        machine.records().iter().any(|r| r["endpoint"] == json!("/land"))
            && server.seen.lock().unwrap().iter().any(|(p, _)| p == "/land")
    };
    while !landed() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(100));
    }

    let records = machine.records();
    let seen = server.seen.lock().unwrap().clone();
    let http: Vec<&Value> = records.iter().filter(|r| r["channel"] == json!("http") && r["method"] == json!("POST")).collect();
    assert!(!http.is_empty(), "the audit log has the requests");
    // 1. the log holds exactly what the server received, request by request
    let logged: Vec<(String, Value)> = http.iter().map(|r| (r["endpoint"].as_str().unwrap().to_string(), r["body"].clone())).collect();
    let mut received: Vec<(String, Value)> = seen.iter().map(|(p, b)| {
        let mut b = b.clone();
        // the log masks credentials; the server got them
        if let Some(m) = b.as_object_mut() {
            for k in ["token", "code"] {
                if m.contains_key(k) {
                    m.insert(k.into(), json!("[masked]"));
                }
            }
        }
        (p.split('?').next().unwrap().to_string(), b)
    }).collect();
    let mut logged_sorted = logged.clone();
    logged_sorted.sort_by_key(|(p, b)| format!("{p}{b}"));
    received.sort_by_key(|(p, b)| format!("{p}{b}"));
    assert_eq!(logged_sorted, received, "every request is logged as it was sent");
    for endpoint in ["/observe", "/report", "/presence", "/brief", "/land"] {
        assert!(logged.iter().any(|(p, _)| p == endpoint), "{endpoint} was logged: {:?}", logged.iter().map(|l| &l.0).collect::<Vec<_>>());
    }
    // 2. no file text, no absolute path, no home, no secret, anywhere
    let home_text = home.to_string_lossy().to_string();
    let root_text = root.to_string_lossy().to_string();
    for (endpoint, body) in &logged {
        let text = body.to_string();
        assert!(!text.contains("secret_formula = amount"), "{endpoint} carried file text: {text}");
        assert!(body.get("content").is_none(), "{endpoint} carried content");
        if let Some(files) = body.get("files").and_then(Value::as_array) {
            assert!(files.iter().all(|f| f.get("content").is_none()), "{endpoint} carried content in a batch");
        }
        let mut all = Vec::new();
        strings(body, &mut all);
        for s in &all {
            assert!(!s.contains(&home_text) && !s.contains(&root_text), "{endpoint} carried a local path: {s}");
        }
        assert!(!text.contains("sk_live_abcdef1234567890") && !text.contains("cat_test_token"), "{endpoint} carried a secret");
    }
    // 3. the structure goes instead: symbols, docstrings, the module doc, line hashes
    let report = logged.iter().find(|(p, _)| p == "/report").unwrap();
    assert_eq!(report.1["path"], json!("src/billing.py"));
    let symbols = report.1["structure"]["symbols"].as_array().unwrap();
    assert!(symbols.iter().any(|s| s["name"] == json!("compute_tax") && s["doc"] == json!("The tax on one order.")));
    assert_eq!(report.1["structure"]["doc"], json!("Billing: what each order owes."));
    assert!(report.1["line_hashes"].as_array().is_some_and(|h| !h.is_empty()));
    // 4. commands shortened; a grep is a search for its pattern
    let presences: Vec<&Value> = logged.iter().filter(|(p, _)| p == "/presence").map(|(_, b)| b).collect();
    assert!(presences.iter().any(|b| b["action"] == json!("running") && b["path"] == json!("python -c \u{2026} && pytest -q tests | tail")), "{presences:?}");
    assert!(presences.iter().any(|b| b["action"] == json!("searching") && b["path"] == json!("\"compute_tax\"")), "{presences:?}");
    // 5. the closing message: relative while on, empty once switched off; titles gone
    let settled: Vec<&&Value> = presences.iter().filter(|b| b["action"] == json!("settled")).collect();
    assert_eq!(settled.len(), 2, "{settled:?}");
    assert_eq!(settled[0]["text"], json!("Changed compute_tax in src/billing.py; notes in ~/notes.txt."));
    assert_eq!(settled[1]["text"], json!(""));
    // 6. the prompt's words go to the briefing as they were typed
    assert!(logged.iter().any(|(p, b)| p == "/brief" && b["prompt"] == json!("fix compute_tax rounding")));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn the_protocol_frames_are_logged_too() {
    let protocol = Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol");
    let built = Command::new("cargo")
        .args(["build", "--release", "-q", "-p", "cork-protocol-server"])
        .current_dir(&protocol)
        .status()
        .expect("cargo runs");
    assert!(built.success());
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    // wherever cargo builds: CARGO_TARGET_DIR or a config's build.target-dir moves it
    let metadata = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(&protocol)
        .output()
        .expect("cargo runs");
    let metadata: Value = serde_json::from_slice(&metadata.stdout).unwrap();
    let target = PathBuf::from(metadata["target_directory"].as_str().unwrap());
    let mut server = Command::new(target.join("release/cork-protocol-server"))
        .args(["--addr", &format!("127.0.0.1:{port}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    }
    let audit = std::env::temp_dir().join(format!("collide-trust-ws-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&audit);
    std::env::set_var("CORK_AUDIT_DIR", &audit);
    cork_protocol::ws_client::set_tap(cork_hooks::audit::ws_tap);
    let cfg = cork_protocol::ClientConfig {
        session: "s-ws".into(), agent_id: "claude".into(), person: "kim@x.com".into(), scope: "acme/app".into(),
        token: Some("cat_ws_token_0123456789abcdef".into()),
    };
    let (mut client, _) = cork_protocol::ws_client::BlockingClient::connect(&format!("ws://127.0.0.1:{port}"), cfg, false, Duration::from_secs(5)).unwrap();
    let keys = vec![json!({"key": "code:src/billing.py::compute_tax", "base": {"version": 0, "hash": ""}, "kind": "modify_body"})];
    let _ = cork_hooks::proto::check_keys(&mut client, &keys, Duration::from_secs(2), |_| {});
    client.close();
    let _ = server.kill();
    let rows = cork_hooks::audit::records(&audit);
    let kinds: Vec<&str> = rows.iter().filter(|r| r["channel"] == json!("ws")).filter_map(|r| r["method"].as_str()).collect();
    assert!(kinds.contains(&"hello") && kinds.contains(&"check"), "{kinds:?}");
    let hello = rows.iter().find(|r| r["method"] == json!("hello")).unwrap();
    assert_eq!(hello["body"]["token"], json!("[masked]"), "the credential is never written to the log");
    assert_eq!(hello["endpoint"], json!("/"));
    let _ = std::fs::remove_dir_all(&audit);
}
