//! The session's protocol daemon: one WebSocket per agent session, held for
//! as long as the session lives, inside `cork listen` (listen.rs).
//!
//! Hooks are short-lived processes, and a connection per write would cost a
//! TLS handshake each time and lose every lease between writes (leases live
//! with the connection). So the session's listener also holds the socket,
//! and the hooks ask it over a unix socket (`proto::socket_path`), one JSON
//! line each way:
//!
//! - `{"cmd": "gate", "ops": [...]}` → `{"results": [...]}`: `check` the
//!   keys; then, unless a result blocks, lease them (after answering, so the
//!   gate never waits for the lease).
//! - `{"cmd": "ops", "ops": [...], "ingest"?: true}` → `{"acks", "rejects"}`:
//!   the write's ops as one non-atomic batch. Accepted ops become drafts and
//!   their keys are followed; an answer nobody waited for is spooled.
//! - `{"cmd": "subscribe", "keys": [...]}`: follow keys (depth 1).
//! - `{"cmd": "promote", "op_ids", "evidence"}` → `{"promoted": [...]}`, and
//!   the leases on those keys are released.
//! - `{"cmd": "ping"}`, `{"cmd": "bye"}` (the session ended).
//!
//! Teammates' deltas, refused ops and messages become lines in the
//! session's spool, which the next hook output carries; what lands on code
//! this session is editing also goes straight into a Claude Code session
//! through its messaging socket. The connection comes back by itself, with
//! backoff, resuming the same session (leases survive the server's grace).
//! While it is down, every request is answered `offline` at once.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use cork_protocol::ws_client::BlockingClient;
use cork_protocol::{ClientConfig, ClientFrame, ServerFrame, Welcome};
use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::proto::{self, Planned};

/// How long one wait on the socket lasts, which is how long a hook's request
/// may wait to be picked up: short while the session is working (a hook
/// asked within the last minute; reads ask too, so the write after them is
/// picked up at once), longer when it is idle, so an idle daemon costs
/// next to nothing.
const TICK_ACTIVE: Duration = Duration::from_millis(2);
const TICK_IDLE: Duration = Duration::from_millis(25);
const ACTIVE_FOR: Duration = Duration::from_secs(60);
/// With no session liveness to watch (not Claude Code), the daemon ends
/// after this long without a hook asking anything.
const IDLE: Duration = Duration::from_secs(3600);
/// A daemon that never got a connection ends sooner.
const IDLE_NEVER_CONNECTED: Duration = Duration::from_secs(300);
const ANSWER_WAIT: Duration = Duration::from_millis(1500);
/// How long a write refused over another session's lease waits to be
/// recorded (`Daemon::record_deferred`).
const DEFERRED_FOR: Duration = Duration::from_secs(1800);
/// A server without the protocol is not asked again for this long.
const NO_PROTOCOL_S: f64 = 600.0;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

pub struct Options {
    pub root: PathBuf,
    pub session: String,
    pub agent: String,
    /// The Claude Code session's inbox socket, when it has one: urgent lines
    /// go straight in, and the daemon lives as long as it answers.
    pub socket: String,
    pub socket_token: String,
    /// The harness process the session runs in (pid, program name), when
    /// known: the daemon ends with it. Codex has no socket to watch, and a
    /// killed Codex session sends no SessionEnd; its daemon used to live on
    /// for an hour, holding the session's leases (Study 9).
    pub owner: Option<(u32, String)>,
}

/// Whether the session the daemon serves is still there: never once its
/// harness process is gone; else while its Claude Code socket answers, or,
/// with no socket to watch, while a hook asked something within `idle`.
fn session_alive(owner_alive: Option<bool>, socket_open: Option<bool>, idle_for: Duration, idle: Duration) -> bool {
    if owner_alive == Some(false) {
        return false;
    }
    match socket_open {
        Some(open) => open,
        None => idle_for < idle,
    }
}

struct Daemon<'a> {
    opts: &'a Options,
    env: &'a Env,
    url: String,
    hello: ClientConfig,
    client: Option<BlockingClient>,
    connecting: Option<Receiver<Result<(BlockingClient, Welcome), String>>>,
    next_try: Instant,
    backoff: Duration,
    /// key → when this session last touched it
    leased: BTreeMap<String, Instant>,
    /// ops refused over another session's lease, and when (`record_deferred`)
    deferred: Vec<(Value, Instant)>,
    subscribed: BTreeSet<String>,
    /// A function in another file this session's files call: its key (and
    /// where a rename took it) → (the key as this copy names it, the
    /// callers `{path, called}`).
    callers: BTreeMap<String, (String, Vec<Value>)>,
    /// The session's stated intent, carried on its leases.
    intent: String,
    last_request: Instant,
    seq: u64,
    stop: bool,
    ever_connected: bool,
}

/// Run until the session ends. Returns when the session's socket stops
/// answering, after `bye`, after an hour unasked (no socket to watch), or
/// when another daemon took the session over.
pub fn run(opts: &Options, env: &Env) {
    let cfg = config::config(Some(&opts.root), env);
    if !proto::enabled(&opts.root, &cfg, env) {
        return;
    }
    #[cfg(unix)]
    {
        let path = proto::socket_path(env, &opts.session);
        let _ = std::fs::create_dir_all(proto::session_dir(env, &opts.session));
        // a live daemon answers: this one is not needed
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            return;
        }
        let _ = std::fs::remove_file(&path);
        let Ok(listener) = std::os::unix::net::UnixListener::bind(&path) else { return };
        let _ = listener.set_nonblocking(true);
        let mut d = Daemon {
            opts,
            env,
            url: proto::ws_url(&cfg.server, env),
            hello: proto::client_config(&opts.root, &cfg, &opts.session, &opts.agent),
            client: None,
            connecting: None,
            next_try: Instant::now(),
            backoff: Duration::from_secs(1),
            leased: BTreeMap::new(),
            deferred: Vec::new(),
            subscribed: BTreeSet::new(),
            callers: BTreeMap::new(),
            intent: String::new(),
            last_request: Instant::now(),
            seq: 0,
            stop: false,
            ever_connected: false,
        };
        eprintln!("{:.0} protocol: session {} on {}", now_s(), opts.session, d.url);
        let mut housekept = Instant::now();
        while !d.stop {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => d.serve(stream),
                    Err(_) => break,
                }
            }
            d.pump();
            if housekept.elapsed() >= Duration::from_secs(15) {
                housekept = Instant::now();
                d.housekeep();
                d.record_deferred();
            }
        }
        if let Some(c) = d.client.take() {
            c.close();
        }
        // the socket goes only if it is still this daemon's
        let _ = std::fs::remove_file(&path);
        eprintln!("{:.0} protocol: session {} ended", now_s(), opts.session);
    }
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

impl Daemon<'_> {
    // ------------------------------------------------------------ the connection

    /// Read what the server pushed, or (re)connect. Waits about one tick.
    fn pump(&mut self) {
        let tick = if self.last_request.elapsed() < ACTIVE_FOR { TICK_ACTIVE } else { TICK_IDLE };
        if let Some(c) = self.client.as_mut() {
            match c.recv(tick) {
                Ok(Some(f)) => self.on_push(f),
                Ok(None) => {}
                Err(cork_protocol::ws_client::WsError::Decode(_)) => {}
                Err(_) => self.lost(),
            }
            return;
        }
        if let Some(rx) = self.connecting.as_ref() {
            match rx.try_recv() {
                Ok(Ok((c, welcome))) => {
                    self.connecting = None;
                    self.backoff = Duration::from_secs(1);
                    self.client = Some(c);
                    self.ever_connected = true;
                    eprintln!("{:.0} protocol: connected (resumed: {})", now_s(), welcome.resumed);
                    // a session the server forgot follows its keys again
                    if !welcome.resumed && !self.subscribed.is_empty() {
                        let keys: Vec<String> = self.subscribed.iter().cloned().collect();
                        self.subscribed.clear();
                        self.leased.clear();
                        self.follow(&keys);
                    }
                }
                Ok(Err(why)) => {
                    eprintln!("{:.0} protocol: connect failed: {why}", now_s());
                    self.connecting = None;
                    if why.contains("HTTP error") {
                        // the server answers but does not speak the protocol:
                        // nobody asks it again for a while, and this daemon ends
                        proto::mark_down(self.env, &self.url, NO_PROTOCOL_S);
                        self.stop = true;
                    }
                    self.retry_later();
                }
                Err(TryRecvError::Disconnected) => {
                    self.connecting = None;
                    self.retry_later();
                }
                Err(TryRecvError::Empty) => {}
            }
        } else if Instant::now() >= self.next_try {
            let (tx, rx) = std::sync::mpsc::channel();
            let (url, hello) = (self.url.clone(), self.hello.clone());
            // connecting may hang on an unreachable host; requests meanwhile are answered `offline`
            std::thread::spawn(move || {
                let _ = tx.send(BlockingClient::connect(&url, hello, true, Duration::from_secs(10)).map_err(|e| e.to_string()));
            });
            self.connecting = Some(rx);
        }
        std::thread::sleep(tick);
    }

    fn lost(&mut self) {
        self.client = None;
        eprintln!("{:.0} protocol: connection lost", now_s());
        self.retry_later();
    }

    fn retry_later(&mut self) {
        self.next_try = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(Duration::from_secs(300));
    }

    fn send(&mut self, value: Value) -> bool {
        let Ok(frame) = ClientFrame::decode(&value.to_string()) else { return false };
        let Some(c) = self.client.as_mut() else { return false };
        if c.send(&frame).is_err() {
            self.lost();
            return false;
        }
        true
    }

    fn req(&mut self) -> String {
        self.seq += 1;
        format!("d{}", self.seq)
    }

    // ------------------------------------------------------------ requests

    #[cfg(unix)]
    fn serve(&mut self, stream: std::os::unix::net::UnixStream) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
        let mut line = String::new();
        let Ok(reader) = stream.try_clone() else { return };
        if BufReader::new(reader).read_line(&mut line).is_err() {
            return;
        }
        let Ok(request) = serde_json::from_str::<Value>(line.trim()) else { return };
        self.last_request = Instant::now();
        let mut stream = stream;
        let mut reply = |answer: &Value| stream.write_all(format!("{answer}\n").as_bytes()).is_ok();
        let online = self.client.is_some();
        match text(&request, "cmd").as_str() {
            "ping" => {
                reply(&json!({"ok": true, "online": online}));
            }
            "bye" => {
                reply(&json!({"ok": true}));
                self.release_all();
                self.stop = true;
            }
            _ if !online => {
                reply(&json!({"offline": true}));
            }
            "gate" => {
                let planned: Vec<Planned> =
                    request.get("ops").and_then(Value::as_array).into_iter().flatten().filter_map(Planned::from_json).collect();
                // what the write will call, checked as a reader in the same frame
                let deps: Vec<Value> = request.get("deps").and_then(Value::as_array).cloned().unwrap_or_default();
                let intent = text(&request, "intent");
                if !intent.is_empty() {
                    self.intent = intent;
                }
                let (results, dep_results) = self.check(&planned, &deps);
                reply(&json!({"results": results, "deps": dep_results}));
                if proto::gate_verdict(&results).is_none() {
                    self.lease(&planned);
                }
                self.note_callers(request.get("callers"));
            }
            "ops" => {
                let ingest = request.get("ingest").and_then(Value::as_bool) == Some(true);
                let ops: Vec<Value> = request.get("ops").and_then(Value::as_array).cloned().unwrap_or_default();
                let answer = self.write_ops(ops, ingest);
                if !reply(&answer) && !ingest {
                    // the report stopped waiting: what was refused waits for its next hook
                    for reject in answer.get("rejects").and_then(Value::as_array).into_iter().flatten() {
                        if let Some(notice) = proto::reject_notice(reject) {
                            self.tell(&notice, "notice", true);
                        }
                    }
                }
            }
            "subscribe" => {
                reply(&json!({"ok": true}));
                let keys: Vec<String> =
                    request.get("keys").and_then(Value::as_array).into_iter().flatten().filter_map(|k| k.as_str().map(str::to_string)).collect();
                self.follow(&keys);
                self.note_callers(request.get("callers"));
            }
            "promote" => {
                let ids: Vec<String> =
                    request.get("op_ids").and_then(Value::as_array).into_iter().flatten().filter_map(|k| k.as_str().map(str::to_string)).collect();
                let evidence = request.get("evidence").cloned().unwrap_or(Value::Null);
                let promoted = self.promote(ids, evidence);
                reply(&json!({"promoted": promoted}));
            }
            _ => {
                reply(&json!({"error": "unknown cmd"}));
            }
        }
    }

    /// Frames that arrive while waiting for an answer are handled as pushes.
    fn wait_answer<T>(&mut self, pick: impl FnMut(&ServerFrame) -> Option<T>) -> Option<T> {
        let mut pushed: Vec<ServerFrame> = Vec::new();
        let got = {
            let c = self.client.as_mut()?;
            proto::wait_for(c, ANSWER_WAIT, pick, |f| pushed.push(f))
        };
        for f in pushed {
            self.on_push(f);
        }
        got
    }

    /// The write's own keys (with their kinds) and the keys it calls (as a
    /// reader, no kind), in one frame; the answers split the same way.
    fn check(&mut self, planned: &[Planned], deps: &[Value]) -> (Vec<Value>, Vec<Value>) {
        if planned.is_empty() && deps.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let mut keys: Vec<Value> = planned.iter().map(|p| json!({"key": p.key, "base": p.base(), "kind": p.kind})).collect();
        keys.extend(deps.iter().map(|d| json!({"key": text(d, "key"), "base": d.get("base").cloned().unwrap_or(Value::Null)})));
        let mut pushed: Vec<ServerFrame> = Vec::new();
        let results = match self.client.as_mut() {
            Some(c) => proto::check_keys(c, &keys, ANSWER_WAIT, |f| pushed.push(f)),
            None => None,
        };
        for f in pushed {
            self.on_push(f);
        }
        let mut results = results.unwrap_or_default();
        let deps = results.split_off(planned.len().min(results.len()));
        (results, deps)
    }

    /// Who in this session calls which function elsewhere (`{key: [{path,
    /// called}]}`): the daemon follows those keys, and a teammate's breaking
    /// change to one is told with the lines that call it.
    fn note_callers(&mut self, callers: Option<&Value>) {
        let Some(map) = callers.and_then(Value::as_object) else { return };
        let mut keys: Vec<String> = Vec::new();
        for (key, rows) in map {
            let rows = rows.as_array().cloned().unwrap_or_default();
            let entry = self.callers.entry(key.clone()).or_insert_with(|| (key.clone(), Vec::new()));
            for row in rows {
                // one row per calling file: the newest says how it calls it
                entry.1.retain(|r| text(r, "path") != text(&row, "path"));
                entry.1.push(row);
            }
            keys.push(key.clone());
        }
        // bounded: the oldest functions go first
        while self.callers.len() > 400 {
            let Some(first) = self.callers.keys().next().cloned() else { break };
            self.callers.remove(&first);
        }
        self.follow(&keys);
    }

    /// Lease what the write will touch, one frame per kind; the answers
    /// arrive as pushes.
    fn lease(&mut self, planned: &[Planned]) {
        let mut by_kind: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for p in planned {
            by_kind.entry(p.kind).or_default().push(p.key.clone());
            self.leased.insert(p.key.clone(), Instant::now());
        }
        for (kind, keys) in by_kind {
            let req = self.req();
            let mut frame = json!({"t": "lease", "req": req, "keys": keys, "kinds": [kind]});
            // why, for whoever runs into the lease
            if !self.intent.is_empty() {
                frame["intent"] = json!({"text": self.intent});
            }
            self.send(frame);
        }
        let keys: Vec<String> = planned.iter().map(|p| p.key.clone()).collect();
        self.follow(&keys);
    }

    /// Ops the server refused only because another session held a lease on
    /// their key: the write is on disk, so it is recorded once that session
    /// lets go, sent again as it was. Accepted, it is a draft like any;
    /// refused for anything else (they changed the key since), it is
    /// dropped, as the gate and the deltas tell the agent of their change.
    fn record_deferred(&mut self) {
        self.deferred.retain(|(_, at)| at.elapsed() < DEFERRED_FOR);
        if self.deferred.is_empty() || self.client.is_none() {
            return;
        }
        let waiting = std::mem::take(&mut self.deferred);
        let ops: Vec<Value> = waiting
            .iter()
            .map(|(op, _)| {
                self.seq += 1;
                let mut op = op.clone();
                op["op_id"] = json!(format!("{}.d{}", text(&op, "op_id"), self.seq));
                op
            })
            .collect();
        let mut pushed: Vec<ServerFrame> = Vec::new();
        let answer = self.client.as_mut().and_then(|c| proto::send_ops(c, &ops, ANSWER_WAIT, |f| pushed.push(f)));
        for f in pushed {
            self.on_push(f);
        }
        let Some(answer) = answer else {
            self.deferred = waiting;
            return;
        };
        proto::settle(self.env, &self.opts.session, &answer);
        // still under their lease: waits on, as it was first sent
        let again: BTreeSet<String> = refused_over_lease(&ops, &answer).iter().map(|op| text(op, "op_id")).collect();
        for (sent, kept) in ops.iter().zip(waiting) {
            if again.contains(&text(sent, "op_id")) {
                self.deferred.push(kept);
            }
        }
    }

    fn write_ops(&mut self, ops: Vec<Value>, ingest: bool) -> Value {
        let prefix = if ingest { String::new() } else { format!("{}.{}", self.opts.session, std::process::id()) };
        let ops: Vec<Value> = ops
            .into_iter()
            .map(|mut op| {
                if text(&op, "op_id").is_empty() {
                    self.seq += 1;
                    op["op_id"] = json!(format!("{}.{}", safe_id(&prefix), self.seq));
                }
                op
            })
            .collect();
        let mut pushed: Vec<ServerFrame> = Vec::new();
        let answer = match self.client.as_mut() {
            Some(c) => proto::send_ops(c, &ops, ANSWER_WAIT, |f| pushed.push(f)),
            None => None,
        };
        for f in pushed {
            self.on_push(f);
        }
        let Some(answer) = answer else { return json!({"offline": true}) };
        if !ingest {
            proto::settle(self.env, &self.opts.session, &answer);
            // refused over another session's lease: recorded when it lets go
            for op in refused_over_lease(&ops, &answer) {
                self.deferred.push((op, Instant::now()));
            }
            let keys: Vec<String> = ops.iter().map(|o| text(o, "key")).collect();
            for key in &keys {
                if let Some(t) = self.leased.get_mut(key) {
                    *t = Instant::now();
                }
            }
            // a lease follows its subject when a rename moves it (SPEC.md §5)
            for ack in answer.get("acks").and_then(Value::as_array).into_iter().flatten() {
                if let Some(to) = ack.pointer("/moved_to/key").and_then(Value::as_str) {
                    if self.leased.remove(&text(ack, "key")).is_some() {
                        self.leased.insert(to.to_string(), Instant::now());
                    }
                }
            }
            // where an op landed (a rename's new key) is followed too
            let landed: Vec<String> = answer.get("acks").and_then(Value::as_array).into_iter().flatten()
                .flat_map(|a| [text(a, "key"), a.pointer("/moved_to/key").and_then(Value::as_str).unwrap_or("").to_string()])
                .filter(|k| !k.is_empty())
                .collect();
            self.follow(&keys.into_iter().chain(landed).collect::<Vec<_>>());
        }
        answer
    }

    fn follow(&mut self, keys: &[String]) {
        let new: Vec<String> = keys.iter().filter(|k| !k.is_empty() && !self.subscribed.contains(*k)).cloned().collect();
        if new.is_empty() {
            return;
        }
        let req = self.req();
        if self.send(json!({"t": "subscribe", "req": req, "keys": new, "depth": 1})) {
            self.subscribed.extend(new);
        }
    }

    fn promote(&mut self, ids: Vec<String>, evidence: Value) -> Vec<String> {
        if ids.is_empty() {
            return ids;
        }
        let req = self.req();
        if !self.send(json!({"t": "promote", "req": req, "op_ids": ids, "evidence": evidence})) {
            return Vec::new();
        }
        let promoted = self.wait_answer(|f| match f {
            ServerFrame::Promoted(p) if p.req.as_deref() == Some(req.as_str()) => Some(p.clone()),
            _ => None,
        });
        let Some(p) = promoted else { return Vec::new() };
        // proven and pushed: the keys are free for others again
        let keys: Vec<String> = p.keys.iter().map(|k| k.key.clone()).filter(|k| self.leased.contains_key(k)).collect();
        self.release(&keys);
        p.op_ids
    }

    fn release(&mut self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        for k in keys {
            self.leased.remove(k);
        }
        let req = self.req();
        self.send(json!({"t": "release", "req": req, "keys": keys}));
    }

    fn release_all(&mut self) {
        let keys: Vec<String> = self.leased.keys().cloned().collect();
        self.release(&keys);
    }

    // ------------------------------------------------------------ pushes

    fn editing(&self) -> BTreeSet<String> {
        let mut keys: BTreeSet<String> = self.leased.keys().cloned().collect();
        keys.extend(proto::drafts(self.env, &self.opts.session).iter().map(|d| text(d, "key")));
        keys
    }

    fn on_push(&mut self, frame: ServerFrame) {
        match frame {
            ServerFrame::Delta(d) => {
                let Ok(value) = serde_json::to_value(&d) else { return };
                // this session's own change, come back through a side session
                if proto::own_delta(&value, &self.opts.session) {
                    return;
                }
                if self.tell_callers(&value) {
                    return;
                }
                if let Some((line, urgent)) = proto::delta_line(&value, &self.editing()) {
                    // a breaking change is one fact, said once, whichever way it came
                    if let Some(fact) = self.breaking_fact(&value) {
                        let mut told = crate::deps::Told::load_for_push(self.env, &self.opts.session);
                        if told.knows(&fact) {
                            return;
                        }
                        told.note(&fact, "", false);
                        told.save();
                    }
                    self.tell(&line, "delta", urgent);
                }
            }
            ServerFrame::Message(m) => {
                let from = if m.from.person.is_empty() { m.from.agent_id.clone() } else { m.from.person.clone() };
                let about = m.anchor.as_deref().map(|a| format!(" about {}", proto::key_path(a))).unwrap_or_default();
                let line = format!("Cork: a message from {from}'s agent{about} (not your user): {}", m.text.trim());
                self.tell(&line, "notice", true);
            }
            ServerFrame::LeaseDenied(d) => {
                for key in &d.keys {
                    self.leased.remove(key);
                }
            }
            ServerFrame::Error(e) if e.code == "superseded" => {
                // another daemon resumed this session: it is the one now
                self.client = None;
                self.stop = true;
            }
            ServerFrame::Error(e) if e.fatal => self.lost(),
            _ => {}
        }
    }

    /// A teammate's breaking change to a function this session's files call
    /// (`callers`): told with the lines that still call it the old way, once,
    /// before the session writes against it. True when the delta was this
    /// kind (said, or nothing left to say), so no generic line follows.
    fn tell_callers(&mut self, delta: &Value) -> bool {
        let Some(op) = delta.get("op") else { return false };
        if text(delta, "layer") == "promoted" {
            return false;
        }
        let key = text(op, "key");
        let Some((key0, rows)) = self.callers.get(&key).cloned() else { return false };
        let author = op.pointer("/author/session").and_then(Value::as_str).unwrap_or("");
        let me = &self.opts.session;
        if author == me || author.starts_with(&format!("{me}.")) {
            return true;
        }
        // a rename takes the subject, and who calls it, to the new key
        if let Some(to) = delta.pointer("/moved_to/key").and_then(Value::as_str) {
            self.callers.insert(to.to_string(), (key0.clone(), rows.clone()));
        }
        let Some(change) = crate::deps::summarize(std::slice::from_ref(op), proto::who) else { return false };
        let findings = crate::deps::findings_on_disk(&self.opts.root, &key0, &rows, &change);
        let mut told = crate::deps::Told::load_for_push(self.env, &self.opts.session);
        let (_, lines) = crate::deps::tell(findings, &mut told);
        told.save();
        for line in lines {
            self.tell(&line, "delta", false);
        }
        true
    }

    /// The fact a breaking delta states, as the gate and the server name it.
    fn breaking_fact(&self, delta: &Value) -> Option<String> {
        let op = delta.get("op")?;
        let change = crate::deps::summarize(std::slice::from_ref(op), proto::who)?;
        Some(change.fact(&text(op, "key")))
    }

    /// A line for the session: into the spool for its next hook output, or,
    /// when urgent and Claude Code has a socket, straight into the session.
    fn tell(&self, line: &str, kind: &str, urgent: bool) {
        if urgent && !self.opts.socket.is_empty() && crate::listen::post_to_session(&self.opts.socket, &self.opts.socket_token, line) {
            return;
        }
        proto::spool_line(self.env, &self.opts.session, kind, line);
    }

    // ------------------------------------------------------------ upkeep

    fn housekeep(&mut self) {
        let idle: Vec<String> =
            self.leased.iter().filter(|(_, t)| t.elapsed() >= proto::LEASE_IDLE).map(|(k, _)| k.clone()).collect();
        self.release(&idle);
        let idle = if self.ever_connected { IDLE } else { IDLE_NEVER_CONNECTED };
        let socket_open = (!self.opts.socket.is_empty() && self.ever_connected)
            .then(|| crate::listen::session_open(&self.opts.socket));
        let owner_alive = self.opts.owner.as_ref().map(crate::listen::owner_alive);
        if !session_alive(owner_alive, socket_open, self.last_request.elapsed(), idle) {
            self.release_all();
            self.stop = true;
        }
    }
}

/// The ops of a batch the server refused only because another session held
/// a lease on their key: written, and waiting to be recorded.
fn refused_over_lease(ops: &[Value], answer: &Value) -> Vec<Value> {
    answer
        .get("rejects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| text(r, "reason") == "leased")
        .filter_map(|r| ops.iter().find(|op| text(op, "op_id") == text(r, "op_id")).cloned())
        .collect()
}

fn safe_id(prefix: &str) -> String {
    prefix.chars().map(|c| if c.is_ascii_alphanumeric() || "._:@-".contains(c) { c } else { '_' }).take(100).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write refused only over another session's lease is on disk, and is
    /// kept to be recorded when the lease goes; a conflict or an invalid op
    /// is not (v104's test: "the server did not take your change" about
    /// edits that had applied).
    #[test]
    fn only_a_write_refused_over_a_lease_waits_to_be_recorded() {
        let ops = vec![
            json!({"op_id": "s.1", "key": "code:src/db.rs::ISSUE_SELECT", "kind": "modify_body"}),
            json!({"op_id": "s.2", "key": "code:src/db.rs::open", "kind": "modify_body"}),
            json!({"op_id": "s.3", "key": "code:src/db.rs::Issue", "kind": "modify_body"}),
        ];
        let answer = json!({"acks": [{"op_id": "s.3"}], "rejects": [
            {"op_id": "s.1", "reason": "leased", "holder": {"session": "a"}},
            {"op_id": "s.2", "reason": "conflict", "theirs": []},
        ]});
        let waiting = refused_over_lease(&ops, &answer);
        assert_eq!(waiting, vec![ops[0].clone()]);
    }

    /// A Codex session's daemon (no socket to watch) used to live an hour
    /// after the session was killed; it ends as soon as the harness process
    /// is gone. A live harness still ends an idle daemon, and Claude Code's
    /// socket still decides for Claude Code.
    #[test]
    fn the_daemon_ends_with_its_session() {
        let (hour, minute) = (Duration::from_secs(3600), Duration::from_secs(60));
        assert!(!session_alive(Some(false), None, minute, hour), "Codex killed: gone at once");
        assert!(session_alive(Some(true), None, minute, hour), "Codex at work");
        assert!(!session_alive(Some(true), None, hour * 2, hour), "idle past the hour");
        assert!(session_alive(None, None, minute, hour), "no owner known: the old rule");
        assert!(!session_alive(Some(false), Some(true), minute, hour));
        assert!(!session_alive(None, Some(false), minute, hour), "Claude Code's socket closed");
    }
}
