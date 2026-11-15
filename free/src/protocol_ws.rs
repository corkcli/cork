//! Cork's real-time protocol (server/protocol, SPEC.md) served here:
//! `GET /protocol/v1/ws`, one WebSocket per agent session carrying versioned
//! ops, gate checks, leases, subscriptions over the call graph, and messages.
//!
//! The protocol crate is a sans-IO `Engine`; this module is its transport and
//! its plumbing into the rest of Cork:
//!
//! - **Identity** is the hooks' own credential: `hello.token` (or the
//!   upgrade's bearer header) goes through `authorize_full`, exactly as a
//!   `/report` does, and the engine's scope is the canonical workspace scope
//!   that `/report` writes under. A viewer seat connects read-only; a
//!   dashboard's Firebase token connects read-only too.
//! - **Durability**: the op log is `app_kv` rows (`protocol_log`), so it
//!   rides the store's changeset shipping (engine.rs) like every other write,
//!   with a snapshot every [`SNAPSHOT_EVERY`] entries. The engine is rebuilt
//!   from snapshot + replay at boot, and caught up before a frame when
//!   another machine appended (a deploy's handover). A frame that appended
//!   is answered once the store's log has the write.
//! - **Graph**: `dependents(code:<path>::<sym>)` are its callers in the live
//!   code graph (graphview), so a subscription at depth n hears about
//!   changes n calls away.
//! - **Product**: a conflict or lease refusal is a ledger row (`op_conflict`)
//!   and a `collision` on the dashboard's feed, whether a write was refused
//!   or a gate's `check` (one naming a kind) said a write would be: the hooks
//!   stop most collisions at the check, before any op is sent, so recording
//!   only refused ops left the dashboard blind to them; accepted ops touch presence;
//!   promotions are ledger rows (`op_promoted`); messages go to Cork's
//!   inboxes and thread, and a dashboard message to one session arrives on
//!   its socket when it is connected.
//!
//! One engine for every scope, behind one mutex. `Log::append` is one
//! insert-if-absent in the store's group commit; nothing else in a frame's
//! handling does IO, and the product side effects run after, off the lock.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use cork_protocol::{
    CheckStatus, ClientFrame, ConnId, Engine, EngineBuilder, ErrorFrame, Graph, Log, LogEntry, LogError, Outgoing, Peer, RejectReason,
    ServerFrame, Would,
};
use cork_protocol_code::{file_key, split_symbol_key, symbol_key, CodeDomain};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::store::{now, Store};
use crate::App;

/// One row per log entry, keyed by the zero-padded seq.
pub const LOG_BUCKET: &str = "protocol_log";
/// `latest`: the newest snapshot (gzipped); `head`: its seq alone, small
/// enough to stay in the kv cache for the per-frame catch-up check.
pub const SNAP_BUCKET: &str = "protocol_snapshot";
pub const SNAPSHOT_EVERY: u64 = 1000;
/// Recent ops and conflicts kept per scope for the dashboard's read.
const RECENT_KEEP: usize = 50;
/// The most keys one dashboard read lists (the counts cover them all).
const VIEW_KEYS: usize = 200;

fn seq_key(seq: u64) -> String {
    format!("{seq:020}")
}

fn now_ms() -> u64 {
    (now() * 1000.0) as u64
}

fn env_u64(name: &str, default: u64) -> u64 {
    crate::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

// ------------------------------------------------------------------ the log

/// The engine's log on the store: one insert-if-absent per entry, so two
/// machines can never both write one position (the loser's writer gets
/// `error{code: "log"}` and resends after the catch-up).
struct KvLog {
    store: Arc<Store>,
    every: u64,
}

impl Log for KvLog {
    fn append(&mut self, entry: &LogEntry) -> Result<(), LogError> {
        let value = serde_json::to_value(entry).map_err(|e| LogError(e.to_string()))?;
        match self.store.kv_put_if_absent(LOG_BUCKET, &seq_key(entry.seq), &value, now()) {
            Ok(true) => Ok(()),
            Ok(false) => Err(LogError("another server took this log position; resend".into())),
            Err(e) => Err(LogError(e.to_string())),
        }
    }

    fn snapshot_every(&self) -> Option<u64> {
        Some(self.every)
    }

    /// Written off the engine's lock: gzipped, then the entries it covers
    /// are pruned, so the log holds at most a snapshot interval or so.
    fn snapshot(&mut self, snap: &cork_protocol::Snapshot) -> Result<(), LogError> {
        let store = Arc::clone(&self.store);
        let snap = snap.clone();
        std::thread::Builder::new()
            .name("protocol-snapshot".into())
            .spawn(move || write_snapshot(&store, &snap))
            .map(|_| ())
            .map_err(|e| LogError(e.to_string()))
    }
}

static SNAPSHOT_WRITER: Mutex<()> = Mutex::new(());

fn write_snapshot(store: &Store, snap: &cork_protocol::Snapshot) {
    use base64::Engine as _;
    use std::io::Write;
    let _one_at_a_time = SNAPSHOT_WRITER.lock().unwrap_or_else(|e| e.into_inner());
    let stored = store.kv_get(SNAP_BUCKET, "head").and_then(|h| h.get("seq").and_then(Value::as_u64)).unwrap_or(0);
    if snap.seq <= stored {
        return;
    }
    let Ok(raw) = serde_json::to_vec(snap) else { return };
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    if gz.write_all(&raw).is_err() {
        return;
    }
    let Ok(bytes) = gz.finish() else { return };
    let body = json!({"seq": snap.seq, "hash": snap.hash, "gz": base64::engine::general_purpose::STANDARD.encode(bytes)});
    let at = now();
    if store.kv_put(SNAP_BUCKET, "latest", &body, at).is_err() || store.kv_put(SNAP_BUCKET, "head", &json!({"seq": snap.seq}), at).is_err() {
        return;
    }
    let mut covered = Vec::new();
    store.kv_for_each(LOG_BUCKET, "", |key, _| {
        if key.parse::<u64>().is_ok_and(|seq| seq <= snap.seq) {
            covered.push(key);
        }
    });
    for key in covered {
        let _ = store.kv_delete(LOG_BUCKET, &key);
    }
}

fn read_snapshot(store: &Store) -> Option<cork_protocol::Snapshot> {
    use base64::Engine as _;
    use std::io::Read;
    let body = store.kv_get(SNAP_BUCKET, "latest")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(body.get("gz")?.as_str()?).ok()?;
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut raw).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn snapshot_seq(store: &Store) -> u64 {
    store.kv_get(SNAP_BUCKET, "head").and_then(|h| h.get("seq").and_then(Value::as_u64)).unwrap_or(0)
}

/// The entries after `seq`, in order.
fn entries_after(store: &Store, seq: u64) -> Vec<LogEntry> {
    let mut out: Vec<LogEntry> = Vec::new();
    store.kv_for_each(LOG_BUCKET, "", |key, value| {
        if key.parse::<u64>().is_ok_and(|s| s > seq) {
            if let Ok(e) = serde_json::from_value(value) {
                out.push(e);
            }
        }
    });
    out.sort_by_key(|e| e.seq);
    out
}

/// Replay what continues the engine's chain; stop at the first break (an
/// edited or missing entry) rather than refuse the whole tail.
fn replay(engine: &mut Engine, entries: &[LogEntry]) {
    if let Err(error) = engine.replay(entries) {
        let bad = match error {
            cork_protocol::ChainError::BrokenLink { seq }
            | cork_protocol::ChainError::BadHash { seq }
            | cork_protocol::ChainError::Gap { seq, .. } => seq,
        };
        tracing::error!("protocol: the op log breaks at entry {bad} ({error}); replaying up to it");
        let good: Vec<LogEntry> = entries.iter().take_while(|e| e.seq < bad).cloned().collect();
        let _ = engine.replay(&good);
    }
}

// ------------------------------------------------------------------ the graph

/// Callers from the live code graph: `dependents(callee) = callers`.
struct CallGraph {
    store: Weak<Store>,
}

impl Graph for CallGraph {
    fn dependents(&self, scope: &str, key: &str) -> Vec<String> {
        let node = if let Some((path, symbol)) = split_symbol_key(key) {
            crate::graphview::node_id(path, symbol)
        } else if let Some(path) = key.strip_prefix("file:") {
            crate::graphview::node_id(path, "")
        } else {
            return Vec::new();
        };
        let Some(store) = self.store.upgrade() else { return Vec::new() };
        let graph = crate::graphview::snapshot(&store, scope);
        let mut out: Vec<String> = graph
            .predecessors(&node)
            .filter_map(|n| {
                let (path, symbol) = crate::graphview::split_node(n);
                match (path.is_empty(), symbol.is_empty()) {
                    (true, _) => None,
                    (false, true) => Some(file_key(path)),
                    (false, false) => Some(symbol_key(path, symbol)),
                }
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

// ------------------------------------------------------------------ the hub

/// Who a connection is, once its hello was authorized.
#[derive(Clone, Debug)]
struct Identity {
    scope: String,
    workspace: String,
    /// The display identity records carry (`Caller.user_id`: the email;
    /// on a local server, one per agent session).
    label: String,
    /// The protocol's `person`: the credential's account (email, else uid),
    /// the same for every session it opens, so promotion by "the same
    /// person" spans a hook's fallback and ingest sessions.
    person: String,
    uid: String,
    session: String,
    agent_id: String,
    read_only: bool,
}

struct Conn {
    tx: UnboundedSender<(String, bool)>,
    identity: Option<Identity>,
}

struct Shared {
    engine: Engine,
    conns: HashMap<ConnId, Conn>,
}

#[derive(Default)]
struct Stats {
    connections_total: AtomicU64,
    ops_accepted: AtomicU64,
    rejects: AtomicU64,
    conflicts: AtomicU64,
    messages: AtomicU64,
    log_errors: AtomicU64,
}

/// What a frame did that the rest of Cork records, run after the frame
/// is answered and off the engine's lock.
enum Effect {
    /// `stage`: "write" for a refused op, "gate" for a check that said a
    /// write of `kind` would be refused (the writer was stopped before it).
    /// `identical`: the refused op wrote what the key already held (its
    /// content hash is the head's); None where a side named no hash.
    Conflict { who: Identity, reject: cork_protocol::Reject, kind: String, intent: Option<String>, stage: &'static str, identical: Option<bool> },
    Accepted { who: Identity },
    Promoted { who: Identity, promoted: cork_protocol::Promoted },
    /// `heard`: whoever got it live on a socket (their records' labels):
    /// not queued again in their inbox.
    Message { who: Identity, to: Option<String>, anchor: Option<String>, text: String, heard: Vec<String> },
}

pub struct Hub {
    store: Arc<Store>,
    dashboard_url: String,
    state: Mutex<Shared>,
    /// hello nonce -> the identity the authorizer hands the engine
    pending: Arc<Mutex<HashMap<String, Result<Peer, String>>>>,
    next_conn: AtomicU64,
    stats: Stats,
    /// per scope: recent accepted ops and conflicts, newest last
    recent_ops: Mutex<HashMap<String, VecDeque<Value>>>,
    recent_conflicts: Mutex<HashMap<String, VecDeque<Value>>>,
    /// gate clashes already recorded: (scope, writer session, key, their
    /// version) -> when. A gate asks again on every retry; one clash is one
    /// record for [`GATE_REPEAT_S`].
    gate_seen: Mutex<HashMap<String, f64>>,
}

/// How long a gate clash, asked about again, is the same clash.
const GATE_REPEAT_S: f64 = 600.0;

fn engine_builder(store: &Arc<Store>, pending: &Arc<Mutex<HashMap<String, Result<Peer, String>>>>) -> EngineBuilder {
    let graph: Arc<dyn Graph> = Arc::new(CallGraph { store: Arc::downgrade(store) });
    let pending = Arc::clone(pending);
    Engine::builder()
        .config(cork_protocol::Config {
            grace_ms: env_u64("CORK_PROTO_GRACE_MS", 5_000),
            history_per_key: env_u64("CORK_PROTO_HISTORY", 64) as usize,
            ..Default::default()
        })
        .domain(CodeDomain::with_graph(graph))
        .log(KvLog { store: Arc::clone(store), every: env_u64("CORK_PROTO_SNAPSHOT_EVERY", SNAPSHOT_EVERY).max(1) })
        // identity comes only from here: the transport authorized the hello
        // with Cork's credential and left the answer under a nonce
        .authorize(move |hello| {
            let nonce = hello.token.clone().unwrap_or_default();
            pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&nonce).unwrap_or_else(|| Err("unauthorized".into()))
        })
}

/// The engine as the store has it: the latest snapshot, then every entry
/// after it.
fn load_engine(store: &Arc<Store>, pending: &Arc<Mutex<HashMap<String, Result<Peer, String>>>>) -> Engine {
    let mut builder = engine_builder(store, pending);
    let snap = read_snapshot(store);
    if let Some(s) = &snap {
        builder = builder.snapshot(s.clone());
    }
    let mut engine = builder.build();
    let tail = entries_after(store, snap.as_ref().map(|s| s.seq).unwrap_or(0));
    replay(&mut engine, &tail);
    engine
}

fn is_write(frame: &ClientFrame) -> bool {
    matches!(
        frame,
        ClientFrame::Op(_) | ClientFrame::Batch(_) | ClientFrame::Lease(_) | ClientFrame::Promote(_) | ClientFrame::Message(_)
    )
}

fn error_frame(code: &str, message: &str, fatal: bool) -> String {
    ServerFrame::Error(ErrorFrame { code: code.into(), message: message.into(), fatal, req: None }).encode()
}

impl Hub {
    /// The hub over `store`, its engine rebuilt from the store's log. Starts
    /// the grace timer.
    pub fn open(store: Arc<Store>, dashboard_url: &str) -> Arc<Hub> {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let started = std::time::Instant::now();
        let engine = load_engine(&store, &pending);
        let seq = engine.head().0;
        if seq > 0 {
            tracing::info!("protocol: op log at seq {seq}, rebuilt in {:?}", started.elapsed());
        }
        let hub = Arc::new(Hub {
            store,
            dashboard_url: dashboard_url.to_string(),
            state: Mutex::new(Shared { engine, conns: HashMap::new() }),
            pending,
            next_conn: AtomicU64::new(1),
            stats: Stats::default(),
            recent_ops: Mutex::new(HashMap::new()),
            recent_conflicts: Mutex::new(HashMap::new()),
            gate_seen: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&hub);
        std::thread::Builder::new()
            .name("protocol-grace".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(200));
                let Some(hub) = weak.upgrade() else { return };
                hub.tick();
            })
            .ok();
        hub
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn tick(&self) {
        let mut st = self.lock();
        let now = now_ms();
        if st.engine.next_deadline().is_some_and(|d| d <= now) {
            let outs = st.engine.tick(now);
            send_all(&st, outs);
        }
    }

    /// Another machine appended (a deploy's handover): replay what it wrote,
    /// or, past a snapshot this engine never saw, rebuild and close every
    /// socket so its session resumes against the new state.
    fn catch_up(&self, st: &mut Shared) {
        let head = st.engine.head().0;
        if snapshot_seq(&self.store) > head {
            tracing::warn!("protocol: another server snapshotted past seq {head}; rebuilding");
            st.engine = load_engine(&self.store, &self.pending);
            for conn in st.conns.values_mut() {
                let _ = conn.tx.send((error_frame("reload", "the server reloaded its state: reconnect with resume", true), true));
                conn.identity = None;
            }
            return;
        }
        if self.store.kv_get(LOG_BUCKET, &seq_key(head + 1)).is_some() {
            let tail = entries_after(&self.store, head);
            replay(&mut st.engine, &tail);
        }
    }

    fn connect(&self, tx: UnboundedSender<(String, bool)>) -> ConnId {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.stats.connections_total.fetch_add(1, Ordering::Relaxed);
        let mut st = self.lock();
        st.conns.insert(id, Conn { tx, identity: None });
        let outs = st.engine.connected(id, now_ms());
        send_all(&st, outs);
        id
    }

    fn disconnect(&self, conn: ConnId) {
        let mut st = self.lock();
        st.conns.remove(&conn);
        let outs = st.engine.disconnected(conn, now_ms());
        send_all(&st, outs);
    }

    /// Is this gate clash new, or one the same session already asked about?
    fn first_gate_clash(&self, who: &Identity, key: &str, version: u64) -> bool {
        let at = now();
        let mut seen = self.gate_seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, ts| at - *ts < GATE_REPEAT_S);
        let id = format!("{}\u{0}{}\u{0}{key}\u{0}{version}", who.scope, who.session);
        if seen.contains_key(&id) {
            return false;
        }
        seen.insert(id, at);
        true
    }

    fn reply(&self, conn: ConnId, text: String, fatal: bool) {
        if let Some(c) = self.lock().conns.get(&conn) {
            let _ = c.tx.send((text, fatal));
        }
    }

    /// One text frame from a connection. `identify` authorizes a hello.
    async fn receive(&self, conn: ConnId, text: &str, identify: impl FnOnce(&cork_protocol::Hello, &Value) -> Result<Identity, String>) {
        let frame = match ClientFrame::decode(text) {
            Ok(f) => f,
            Err(e) => return self.reply(conn, error_frame("bad_frame", &e.to_string(), false), false),
        };
        let identified = self.lock().conns.get(&conn).and_then(|c| c.identity.clone());
        let frame = match frame {
            ClientFrame::Hello(mut hello) => {
                let nonce = format!("hello-{}", crate::compat::new_id());
                if identified.is_none() {
                    let raw: Value = serde_json::from_str(text).unwrap_or(Value::Null);
                    let answer = identify(&hello, &raw);
                    let answer = answer.map(|who| {
                        hello.scope = who.scope.clone();
                        let peer = Peer { session: hello.session.clone(), agent_id: who.agent_id.clone(), person: who.person.clone() };
                        if let Some(c) = self.lock().conns.get_mut(&conn) {
                            c.identity = Some(who);
                        }
                        peer
                    });
                    self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(nonce.clone(), answer);
                }
                // a second hello on one connection is the engine's to refuse
                hello.token = Some(nonce);
                ClientFrame::Hello(hello)
            }
            f => f,
        };
        if identified.as_ref().is_some_and(|who| who.read_only) && is_write(&frame) {
            return self.reply(conn, error_frame("read_only", "a viewer can look but not write: ask the workspace owner to make you a player", false), false);
        }
        let (wait, sends, effects) = self.handle(conn, frame);
        if wait > 0 {
            if let Some(engine) = self.store.engine() {
                engine.wait_acked_async(wait).await;
            }
            for (tx, text, fatal) in sends {
                let _ = tx.send((text, fatal));
            }
        }
        if !effects.is_empty() {
            let store = Arc::clone(&self.store);
            let dashboard_url = self.dashboard_url.clone();
            tokio::task::spawn_blocking(move || {
                for effect in effects {
                    record(&store, &dashboard_url, effect);
                }
            });
        }
    }

    /// The engine's half, under the lock. Answers go out at once, or, when
    /// the frame wrote to a log that ships elsewhere, come back to be sent
    /// once it is durable (the seq to wait for is the first value).
    #[allow(clippy::type_complexity)]
    fn handle(&self, conn: ConnId, frame: ClientFrame) -> (i64, Vec<(UnboundedSender<(String, bool)>, String, bool)>, Vec<Effect>) {
        let mut st = self.lock();
        self.catch_up(&mut st);
        // what the effects need from the frame, before the engine takes it
        let mut ops: HashMap<String, (String, Option<String>)> = HashMap::new();
        // each op's content hash: a refused one is compared with the head
        let mut hashes: HashMap<String, String> = HashMap::new();
        let mut message = None;
        // a check that names a kind is a gate asking before a write
        let mut gate_kinds: HashMap<String, String> = HashMap::new();
        match &frame {
            ClientFrame::Check(c) => {
                for k in &c.keys {
                    if let Some(kind) = &k.kind {
                        gate_kinds.insert(k.key.clone(), kind.clone());
                    }
                }
            }
            ClientFrame::Op(op) => {
                ops.insert(op.op_id.clone(), (op.kind.clone(), op.intent_id.clone()));
                hashes.extend(op.content_hash.clone().map(|h| (op.op_id.clone(), h)));
            }
            ClientFrame::Batch(b) => {
                for op in &b.ops {
                    ops.insert(op.op_id.clone(), (op.kind.clone(), op.intent_id.clone()));
                    hashes.extend(op.content_hash.clone().map(|h| (op.op_id.clone(), h)));
                }
            }
            ClientFrame::Message(m) => message = Some(m.clone()),
            _ => {}
        }
        let commits = ingested_commits(&frame);
        let at = now_ms();
        let (outs, wait) = crate::store::DURABLE.sync_scope(std::cell::Cell::new(0), || {
            let outs = st.engine.handle(conn, frame, at);
            (outs, crate::store::DURABLE.with(|mark| mark.get()))
        });
        let who = st.conns.get(&conn).and_then(|c| c.identity.clone());
        let mut effects = Vec::new();
        if let Some(who) = &who {
            for (sha, email) in &commits {
                crate::notify::note_commit(&self.store, &who.workspace, &who.scope, email, sha);
            }
            let mut accepted = false;
            for o in outs.iter().filter(|o| o.conn == conn) {
                match &o.frame {
                    ServerFrame::Ack(a) => {
                        accepted = true;
                        self.stats.ops_accepted.fetch_add(1, Ordering::Relaxed);
                        let kind = ops.get(&a.op_id).map(|k| k.0.clone()).unwrap_or_default();
                        self.remember(&self.recent_ops, &who.scope, json!({
                            "op_id": a.op_id, "key": a.key, "kind": kind, "version": a.version, "seq": a.seq,
                            "author": who.label, "agent": who.agent_id, "ts": now(),
                        }));
                    }
                    ServerFrame::Reject(r) => {
                        self.stats.rejects.fetch_add(1, Ordering::Relaxed);
                        if matches!(r.reason, RejectReason::Conflict | RejectReason::Leased) {
                            self.stats.conflicts.fetch_add(1, Ordering::Relaxed);
                            let (kind, intent) = ops.get(&r.op_id).cloned().unwrap_or_default();
                            let identical = same_as_head(&st.engine, &who.scope, &r.key, hashes.get(&r.op_id));
                            self.remember(&self.recent_conflicts, &who.scope, conflict_row(who, r, &kind, intent.as_deref()));
                            effects.push(Effect::Conflict { who: who.clone(), reject: r.clone(), kind, intent, stage: "write", identical });
                        } else if matches!(r.reason, RejectReason::UnknownBase | RejectReason::StaleLease) {
                            // a write from a copy the server never saw, or on a
                            // lease it no longer held: stale writes too, on the
                            // ledger for the waste report (not the live
                            // collision feed, which is for clashes between agents)
                            let (kind, intent) = ops.get(&r.op_id).cloned().unwrap_or_default();
                            let identical = same_as_head(&st.engine, &who.scope, &r.key, hashes.get(&r.op_id));
                            effects.push(Effect::Conflict { who: who.clone(), reject: r.clone(), kind, intent, stage: "write", identical });
                        }
                    }
                    ServerFrame::CheckResult(cr) if !gate_kinds.is_empty() => {
                        for entry in &cr.results {
                            let reason = match (entry.status, entry.would) {
                                (CheckStatus::Leased, _) => RejectReason::Leased,
                                (_, Some(Would::Conflict)) => RejectReason::Conflict,
                                _ => continue,
                            };
                            let Some(kind) = gate_kinds.get(&entry.key) else { continue };
                            if !self.first_gate_clash(who, &entry.key, entry.current.version) {
                                continue;
                            }
                            self.stats.conflicts.fetch_add(1, Ordering::Relaxed);
                            let reject = cork_protocol::Reject {
                                op_id: String::new(),
                                key: entry.key.clone(),
                                reason,
                                current: entry.current.clone(),
                                theirs: entry.theirs.clone(),
                                their_intent: entry.intent.clone(),
                                holder: entry.holder.clone(),
                                batch_id: None,
                                message: String::new(),
                            };
                            let mut row = conflict_row(who, &reject, kind, None);
                            row["stage"] = json!("gate");
                            self.remember(&self.recent_conflicts, &who.scope, row);
                            effects.push(Effect::Conflict { who: who.clone(), reject, kind: kind.clone(), intent: None, stage: "gate", identical: None });
                        }
                    }
                    ServerFrame::Promoted(p) if !p.op_ids.is_empty() => {
                        effects.push(Effect::Promoted { who: who.clone(), promoted: p.clone() });
                    }
                    ServerFrame::Error(e) if e.code == "log" => {
                        self.stats.log_errors.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
            if accepted {
                effects.push(Effect::Accepted { who: who.clone() });
            }
            if let Some(m) = message {
                self.stats.messages.fetch_add(1, Ordering::Relaxed);
                // a session or agent id names its person; anything else is one
                // a session or agent id names whose inbox; anything else is one
                let named = |t: &str| {
                    st.conns
                        .values()
                        .filter_map(|c| c.identity.as_ref())
                        .find(|x| x.scope == who.scope && (x.session == t || x.agent_id == t || x.person == t))
                        .map(|x| x.label.clone())
                };
                let to = m.to.as_ref().map(|t| named(t).unwrap_or_else(|| t.split('#').next().unwrap_or(t).to_string()));
                let mut heard: Vec<String> = outs
                    .iter()
                    .filter(|o| o.conn != conn && matches!(o.frame, ServerFrame::Message(_)))
                    .filter_map(|o| st.conns.get(&o.conn)?.identity.as_ref().map(|x| x.label.clone()))
                    .collect();
                heard.sort();
                heard.dedup();
                effects.push(Effect::Message { who: who.clone(), to, anchor: m.anchor, text: m.text, heard });
            }
        }
        if wait > 0 && self.store.engine().is_some() {
            let sends = outs
                .into_iter()
                .filter_map(|o| {
                    let fatal = matches!(&o.frame, ServerFrame::Error(e) if e.fatal);
                    st.conns.get(&o.conn).map(|c| (c.tx.clone(), o.frame.encode(), fatal))
                })
                .collect();
            return (wait, sends, effects);
        }
        send_all(&st, outs);
        (0, Vec::new(), effects)
    }

    fn remember(&self, ring: &Mutex<HashMap<String, VecDeque<Value>>>, scope: &str, row: Value) {
        let mut rings = ring.lock().unwrap_or_else(|e| e.into_inner());
        let ring = rings.entry(scope.to_string()).or_default();
        ring.push_back(row);
        while ring.len() > RECENT_KEEP {
            ring.pop_front();
        }
    }

    /// A message from a person (the dashboard) to one agent session: on its
    /// socket when it is connected in `scope` and is `to`'s, else `false` so
    /// the caller files it in the inbox.
    pub fn deliver_to_session(&self, scope: &str, session: &str, to: &str, from: &str, text: &str) -> bool {
        let mut st = self.lock();
        let owner = st.conns.values().filter_map(|c| c.identity.as_ref()).find(|who| who.session == session && who.scope == scope);
        let Some(owner) = owner else { return false };
        if owner.label != to && owner.uid != to && owner.person != to {
            return false;
        }
        let peer = Peer { session: String::new(), agent_id: "dashboard".into(), person: from.to_string() };
        let Some(outs) = st.engine.deliver_message(scope, session, peer, text.to_string(), now_ms()) else { return false };
        self.stats.messages.fetch_add(1, Ordering::Relaxed);
        send_all(&st, outs);
        true
    }

    /// A hub message (`hubs.rs`) to one person's agent sessions, in whatever
    /// repo each is connected: every session of account `uid`, or only
    /// `only` when it is set, never `skip` (the sender's own). Returns the
    /// sessions it reached; the rest pick it up from the hub inbox.
    pub fn deliver_to_person(&self, uid: &str, only: &str, skip: &str, from: &str, text: &str) -> Vec<String> {
        if uid.is_empty() {
            return Vec::new();
        }
        let mut st = self.lock();
        let targets: Vec<(String, String)> = st
            .conns
            .values()
            .filter_map(|c| c.identity.as_ref())
            .filter(|who| who.uid == uid && !who.session.is_empty())
            .filter(|who| (only.is_empty() || who.session == only) && who.session != skip)
            .map(|who| (who.scope.clone(), who.session.clone()))
            .collect();
        let mut reached: Vec<String> = Vec::new();
        for (scope, session) in targets {
            if reached.contains(&session) {
                continue;
            }
            let peer = Peer { session: String::new(), agent_id: "hub".into(), person: from.to_string() };
            if let Some(outs) = st.engine.deliver_message(&scope, &session, peer, text.to_string(), now_ms()) {
                self.stats.messages.fetch_add(1, Ordering::Relaxed);
                send_all(&st, outs);
                reached.push(session);
            }
        }
        reached
    }

    /// The dashboard's read of one repo: who is connected, the leases, the
    /// recent ops and conflicts, and each key's draft and promoted versions.
    pub fn view(&self, scope: &str) -> Value {
        let st = self.lock();
        let sessions: Vec<Value> = st
            .engine
            .sessions_in(scope)
            .into_iter()
            .map(|p| {
                let read_only = st
                    .conns
                    .values()
                    .filter_map(|c| c.identity.as_ref())
                    .any(|who| who.session == p.session && who.scope == scope && who.read_only);
                json!({"session": p.session, "agent_id": p.agent_id, "person": p.person, "read_only": read_only})
            })
            .collect();
        let leases: Vec<Value> = st
            .engine
            .leases_in(scope)
            .into_iter()
            .map(|l| json!({"key": l.key, "holder": l.holder, "kinds": l.kinds, "intent": l.intent}))
            .collect();
        let keys = st.engine.keys_in(scope);
        drop(st);
        let drafts = keys.iter().filter(|(_, h)| h.version > h.promoted_version).count();
        let promoted = keys.iter().filter(|(_, h)| h.promoted_version > 0).count();
        let mut listed: Vec<&(String, cork_protocol::Head)> = keys.iter().collect();
        // keys with unpromoted drafts first, then the most written
        listed.sort_by_key(|(k, h)| (std::cmp::Reverse(h.version - h.promoted_version.min(h.version)), std::cmp::Reverse(h.version), k.clone()));
        let listed: Vec<Value> = listed
            .into_iter()
            .take(VIEW_KEYS)
            .map(|(k, h)| json!({"key": k, "version": h.version, "promoted_version": h.promoted_version, "drafts": h.version.saturating_sub(h.promoted_version)}))
            .collect();
        let ring = |r: &Mutex<HashMap<String, VecDeque<Value>>>| -> Vec<Value> {
            r.lock().unwrap_or_else(|e| e.into_inner()).get(scope).map(|v| v.iter().rev().cloned().collect()).unwrap_or_default()
        };
        json!({
            "sessions": sessions,
            "leases": leases,
            "recent_ops": ring(&self.recent_ops),
            "recent_conflicts": ring(&self.recent_conflicts),
            "keys": {"total": keys.len(), "with_drafts": drafts, "promoted": promoted, "list": listed},
        })
    }

    /// What `/health` shows: enough to see the protocol working on staging.
    pub fn health(&self) -> Value {
        let st = self.lock();
        let (seq, _) = st.engine.head();
        let live = st.conns.values().filter(|c| c.identity.is_some()).count();
        let open = st.conns.len();
        drop(st);
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
        json!({
            "connections": open,
            "sessions": live,
            "connections_total": n(&self.stats.connections_total),
            "ops_accepted": n(&self.stats.ops_accepted),
            "rejects": n(&self.stats.rejects),
            "conflicts": n(&self.stats.conflicts),
            "messages": n(&self.stats.messages),
            "log_errors": n(&self.stats.log_errors),
            "log_seq": seq,
            "snapshot_seq": snapshot_seq(&self.store),
        })
    }

    #[cfg(test)]
    fn head_seq(&self) -> u64 {
        self.lock().engine.head().0
    }
}

/// The commits a hook ingested from the upstream in this frame: ops named
/// `git.<sha>.<n>` carrying the commit's author email. Work that reached
/// the repo without an agent running Cork (a commit an agent made with
/// Cork carries a `Collide-Ops:` trailer and is never ingested), which
/// is what tells notify.rs a member's machine went dark.
fn ingested_commits(frame: &ClientFrame) -> Vec<(String, String)> {
    let ops: Vec<_> = match frame {
        ClientFrame::Op(op) => vec![op],
        ClientFrame::Batch(b) => b.ops.iter().collect(),
        _ => return Vec::new(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for op in ops {
        let Some(sha) = op.op_id.strip_prefix("git.").and_then(|rest| rest.split('.').next()) else { continue };
        let email = op.public.get("author_email").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
        if !sha.is_empty() && !email.is_empty() && !out.iter().any(|(s, _)| s == sha) {
            out.push((sha.to_string(), email));
        }
    }
    out
}

fn send_all(st: &Shared, outs: Vec<Outgoing>) {
    for o in outs {
        let fatal = matches!(&o.frame, ServerFrame::Error(e) if e.fatal);
        if let Some(c) = st.conns.get(&o.conn) {
            let _ = c.tx.send((o.frame.encode(), fatal));
        }
    }
}

// ------------------------------------------------------------------ what it records

/// `code:<path>::<sym>` → (path, symbol); `file:<path>` → (path, "").
fn path_symbol(key: &str) -> (String, String) {
    if let Some((path, symbol)) = split_symbol_key(key) {
        return (path.to_string(), symbol.to_string());
    }
    (key.strip_prefix("file:").unwrap_or(key).to_string(), String::new())
}

fn conflict_row(who: &Identity, r: &cork_protocol::Reject, kind: &str, intent: Option<&str>) -> Value {
    let (path, symbol) = path_symbol(&r.key);
    let theirs: Vec<Value> = r
        .theirs
        .iter()
        .map(|t| json!({
            "op_id": t.op_id, "kind": t.kind, "version": t.version, "author": t.author.person,
            "agent": t.author.agent_id, "session": t.author.session, "intent_id": t.intent_id,
            // when they made it, and whether it had reached origin (explain.rs)
            "at": t.hlc.w as f64 / 1000.0, "pushed": t.layer == cork_protocol::Layer::Promoted,
        }))
        .collect();
    let holder = r.holder.clone().unwrap_or_default();
    json!({
        "key": r.key, "path": path, "symbol": symbol,
        "reason": r.reason, "op_id": r.op_id, "kind": kind, "intent_id": intent,
        "writer": who.label, "writer_agent": who.agent_id, "writer_session": who.session,
        "holder": holder.person, "holder_agent": holder.agent_id, "holder_session": holder.session,
        "their_intent": r.their_intent, "their_kinds": r.theirs.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        "theirs": theirs, "current_version": r.current.version, "message": r.message, "ts": now(),
    })
}

/// Whether a refused op wrote what its key already held: its content hash
/// against the head op's. The ingest of a commit an agent already reported
/// meets that agent's own op this way, and the collision is no collision.
fn same_as_head(engine: &Engine, scope: &str, key: &str, hash: Option<&String>) -> Option<bool> {
    let head = engine.key_state(scope, key)?.history.back()?.content_hash.clone()?;
    Some(hash? == &head)
}

/// A protocol session a hook opens on an agent session's behalf: the commit
/// ingest (`<session>.ingest`) or the fallback writer when the listener is
/// down (`<session>.d<pid>`).
fn hook_side_session(session: &str) -> bool {
    if session.ends_with(".ingest") {
        return true;
    }
    session
        .rsplit_once(".d")
        .is_some_and(|(base, pid)| !base.is_empty() && !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()))
}

/// The agent session behind a hook's side session: what its reports carry.
pub(crate) fn agent_session(session: &str) -> &str {
    if hook_side_session(session) {
        if let Some(base) = session.strip_suffix(".ingest") {
            return base;
        }
        if let Some((base, _)) = session.rsplit_once(".d") {
            return base;
        }
    }
    session
}

/// An `op_conflict` row that was never two agents' clash: a landed op a
/// server refused before it took landed history as the truth (SPEC §10.1),
/// or an op that wrote what its key already held (`identical`). No one's
/// catch, not on the Collisions list (explain.rs), no news, and the landed
/// sweep takes its collision off the feed.
pub(crate) fn no_clash(row: &Value) -> bool {
    cork_protocol::is_landed(&text(row, "op_id")) || row.get("identical").and_then(Value::as_bool) == Some(true)
}

/// A reject only history that already landed caused (SPEC §10.1): the
/// writer's copy is older than origin, which is a stale write, not two
/// agents changing the same code.
fn stale_on_landed(reject: &cork_protocol::Reject) -> bool {
    !reject.theirs.is_empty() && reject.theirs.iter().all(|t| cork_protocol::is_landed(&t.op_id))
}

fn record(store: &Store, dashboard_url: &str, effect: Effect) {
    match effect {
        // landed history is never refused a conflict (the engine accepts it
        // over drafts); one an older server refused is not anyone's catch
        Effect::Conflict { reject, .. } if cork_protocol::is_landed(&reject.op_id) => {}
        Effect::Conflict { who, reject, kind, intent, stage, identical } => {
            let mut row = conflict_row(&who, &reject, &kind, intent.as_deref());
            row["stage"] = json!(stage);
            if let Some(identical) = identical {
                row["identical"] = json!(identical);
            }
            let _ = store.ledger_append(&who.scope, "op_conflict", &row, now());
            // what the writer does next settles it (outcomes.rs)
            let part = outcome_part(stage, row["reason"].as_str().unwrap_or(""));
            let path = row["path"].as_str().unwrap_or("").to_string();
            if let (Some(part), false) = (part, path.is_empty()) {
                crate::outcomes::open(store, &who.scope, part, &who.label, agent_session(&who.session), &[path], json!({
                    "holder": row["holder"],
                    "holder_session": agent_session(row["holder_session"].as_str().unwrap_or("")),
                    "reason": row["reason"],
                }), now());
            }
            // the Inbox reads `collision` events from the feed: clashes
            // between agents, not a write from a copy the server never saw,
            // nor one from a copy older than what landed upstream, nor the
            // same code twice (the engine acks that; a key that moved away
            // can still meet it)
            if !matches!(reject.reason, RejectReason::Conflict | RejectReason::Leased) || stale_on_landed(&reject) || identical == Some(true) {
                return;
            }
            let theirs_kind = reject.theirs.first().map(|t| t.kind.clone()).unwrap_or_else(|| "lease".into());
            let item = json!({
                "symbol": if row["symbol"].as_str().unwrap_or("").is_empty() { row["path"].clone() } else { row["symbol"].clone() },
                "path": row["path"], "kind": format!("{kind} × {theirs_kind}"),
                "detail": reject.message, "confidence": "certain", "author": row["holder"],
            });
            crate::events::publish(store, &who.scope, json!({
                "kind": "collision", "checker": who.label, "checker_agent": who.agent_id, "mode": "blocked",
                "source": "protocol", "op_id": reject.op_id, "items": [item],
            }), "");
        }
        Effect::Accepted { who } => {
            // a hook's own side sessions (`<session>.ingest`, `<session>.d<pid>`)
            // are not agents: the session they serve keeps its presence through
            // its hooks, and touching theirs put a nameless "protocol" agent on
            // the roster beside it
            if !hook_side_session(&who.session) {
                crate::presence::touch_presence(store, &who.scope, &who.label, &who.session, "protocol", "");
            }
        }
        Effect::Promoted { who, promoted } => {
            let _ = store.ledger_append(&who.scope, "op_promoted", &json!({
                "by": who.label, "agent": who.agent_id, "session": who.session,
                "op_ids": promoted.op_ids, "keys": promoted.keys, "evidence": promoted.evidence,
            }), now());
        }
        Effect::Message { who, to, anchor, text, heard } => {
            let to = to.filter(|t| !t.is_empty());
            // one copy per recipient: a message heard live on a socket is
            // not queued in that person's inbox as well; the thread keeps it
            let thread_only = match (&to, &anchor) {
                (Some(t), _) => heard.contains(t),
                (None, Some(_)) => !heard.is_empty(),
                (None, None) => true,
            };
            if thread_only {
                let record = json!({
                    "id": crate::compat::new_id(), "to": to.clone().unwrap_or_default(), "from": who.label,
                    "message": text.chars().take(4000).collect::<String>(), "agent": "protocol", "ts": now(),
                    "session": who.session, "live": !heard.is_empty(), "anchor": anchor, "broadcast": to.is_none() && anchor.is_none(),
                });
                crate::agenttools::file_thread(store, &who.scope, &record);
                return;
            }
            match (to.as_deref(), anchor) {
                (_, Some(anchor)) => {
                    let (path, symbol) = path_symbol(&anchor);
                    let label = if symbol.is_empty() { path } else { format!("{path}::{symbol}") };
                    crate::agenttools::send_anchored_message(
                        store, &who.scope, &who.workspace, &who.label, to.as_deref().unwrap_or(""), &text, &label, "protocol", dashboard_url,
                    );
                }
                (Some(to), None) => {
                    crate::agenttools::send_agent_message(store, &who.scope, &who.label, to, &text, "protocol");
                }
                (None, None) => {}
            }
        }
    }
}

// ------------------------------------------------------------------ the landed sweep

/// Where the sweep notes that it ran, once per database.
const SWEEP_BUCKET: &str = "protocol_sweeps";
const LANDED_SWEEP: &str = "landed_v1";
/// How far back it looks: the Inbox's feed keeps a day, outcomes settle in
/// half an hour.
const SWEEP_LOOKBACK_S: f64 = 3.0 * 86_400.0;

/// What the landed sweep took away.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    pub scopes: usize,
    pub collisions: usize,
    pub outcomes: usize,
}

/// The waste report's part a protocol reject opened an outcome as (`record`).
fn outcome_part(stage: &str, reason: &str) -> Option<&'static str> {
    match (stage, reason) {
        ("write", "conflict") => Some("protocol_stale"),
        ("write", "unknown_base") => Some("protocol_unknown_base"),
        ("write", "stale_lease") => Some("protocol_stale_lease"),
        ("write", "leased") | ("gate", "leased") => Some("protocol_lease"),
        ("gate", _) => Some("protocol_check"),
        _ => None,
    }
}

/// Once per database: the collisions made before the engine took landed
/// history as the truth and acked the same result (SPEC §5.2, §10.1) leave
/// the Inbox's feed, and the outcomes they opened are dropped. A commit's
/// ops, ingested after the agent that wrote that code had reported it, were
/// refused (`insert × insert` on the same function) and shown as "blocked
/// from changing" (46 on one dashboard in one evening). Their `op_conflict`
/// rows (`no_clash`) stay on the ledger, which is append-only and chained;
/// no reader counts them (`waste::catch_of`, `unread`). A collision goes
/// only when such a row names the same agent and symbol within seconds of
/// it, so nothing else on the feed is touched. `None` when it ran before.
pub fn sweep_landed(store: &Store, now: f64) -> Option<Swept> {
    if store.kv_get(SWEEP_BUCKET, LANDED_SWEEP).is_some() {
        return None;
    }
    let mut swept = Swept::default();
    for scope in store.ledger_scopes_since(now - SWEEP_LOOKBACK_S) {
        let never_clashed: Vec<(f64, Value)> = store
            .ledger_since_kinds(&scope, now - SWEEP_LOOKBACK_S, &["op_conflict"])
            .into_iter()
            .filter(|r| no_clash(&r.payload))
            .map(|r| (r.ts, r.payload))
            .collect();
        if never_clashed.is_empty() {
            continue;
        }
        let key = format!("recent:{scope}");
        if let Some(mut ring) = store.eph_get(&key) {
            let made_by_them = |e: &Value| {
                if text(e, "kind") != "collision" || text(e, "source") != "protocol" {
                    return false;
                }
                // published since with the op that was refused
                let op_id = text(e, "op_id");
                if !op_id.is_empty() {
                    return cork_protocol::is_landed(&op_id) || never_clashed.iter().any(|(_, p)| text(p, "op_id") == op_id);
                }
                let at = e.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
                let item = e.get("items").and_then(|i| i.get(0)).cloned().unwrap_or_default();
                never_clashed.iter().any(|(ts, p)| {
                    let symbol = if text(p, "symbol").is_empty() { text(p, "path") } else { text(p, "symbol") };
                    text(p, "writer_agent") == text(e, "checker_agent")
                        && text(p, "path") == text(&item, "path")
                        && symbol == text(&item, "symbol")
                        && (at - ts).abs() <= 30.0
                })
            };
            if let Some(events) = ring.get_mut("events").and_then(Value::as_array_mut) {
                let before = events.len();
                events.retain(|e| !made_by_them(e));
                let gone = before - events.len();
                if gone > 0 && store.eph_replace(&key, &ring) {
                    swept.scopes += 1;
                    swept.collisions += gone;
                }
            }
        }
        // nobody was blocked, so nobody adapts, waits or gives up
        for (_, p) in &never_clashed {
            let Some(part) = outcome_part(&text(p, "stage"), &text(p, "reason")) else { continue };
            let session = agent_session(&text(p, "writer_session")).to_string();
            if crate::outcomes::drop_pending(store, &scope, part, &text(p, "writer"), &session, &[text(p, "path")]) {
                swept.outcomes += 1;
            }
        }
    }
    let _ = store.kv_put(SWEEP_BUCKET, LANDED_SWEEP, &json!({
        "ts": now, "scopes": swept.scopes, "collisions": swept.collisions, "outcomes": swept.outcomes,
    }), now);
    Some(swept)
}

/// At boot, detached: the landed sweep, once the store has caught up with
/// the durable log (a standby's writes wait for the lease by themselves).
pub fn start_landed_sweep(store: Arc<Store>) {
    let _ = std::thread::Builder::new().name("cork-landed-sweep".into()).spawn(move || {
        let waited = std::time::Instant::now();
        while store.engine().is_some_and(|e| !e.ready()) && waited.elapsed() < std::time::Duration::from_secs(3600) {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        if let Some(swept) = sweep_landed(&store, now()) {
            tracing::info!(
                scopes = swept.scopes, collisions = swept.collisions, outcomes = swept.outcomes,
                "landed sweep: took landed history's collisions off the feed and dropped the outcomes they opened"
            );
        }
    });
}

// ------------------------------------------------------------------ routes

/// `GET /protocol/v1/ws`: the protocol's WebSocket. The hello's `token` (or
/// this upgrade's bearer header) is a hook credential, or a dashboard token
/// with `?workspace=` (or `workspace` in the hello) for a read-only peer.
pub async fn ws_endpoint(
    ws: WebSocketUpgrade, State(app): State<Arc<App>>, headers: HeaderMap, Query(query): Query<Value>,
) -> axum::response::Response {
    let header_token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.split_once(' ').filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer")).map(|(_, t)| t.trim().to_string()))
        .unwrap_or_default();
    let workspace_hint = text(&query, "workspace");
    ws.max_message_size(8 << 20).on_upgrade(move |socket| connection(app, socket, header_token, workspace_hint))
}

async fn connection(app: Arc<App>, socket: WebSocket, header_token: String, workspace_hint: String) {
    let hub = Arc::clone(&app.protocol);
    let (mut sink, mut source) = socket.split();
    let (tx, mut rx) = unbounded_channel::<(String, bool)>();
    let conn = hub.connect(tx);
    let writer = tokio::spawn(async move {
        while let Some((text, fatal)) = rx.recv().await {
            if sink.send(WsMessage::Text(text)).await.is_err() {
                break;
            }
            if fatal {
                let _ = sink.send(WsMessage::Close(None)).await;
                break;
            }
        }
    });
    while let Some(msg) = source.next().await {
        match msg {
            Ok(WsMessage::Text(text)) => {
                hub.receive(conn, &text, |hello, raw| identify(&app, hello, raw, &header_token, &workspace_hint)).await;
            }
            Ok(WsMessage::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
        if writer.is_finished() {
            break;
        }
    }
    hub.disconnect(conn);
}

/// A hello's identity: the hook credential the way `/report` checks it,
/// else a dashboard sign-in as a read-only peer.
fn identify(app: &App, hello: &cork_protocol::Hello, raw: &Value, header_token: &str, workspace_hint: &str) -> Result<Identity, String> {
    let token = hello.token.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| header_token.to_string());
    let token = token.trim();
    if token.is_empty() {
        return Err("hello.token (or a bearer header) is required".into());
    }
    let mut headers = HeaderMap::new();
    if let Ok(value) = format!("Bearer {token}").parse() {
        headers.insert("authorization", value);
    }
    let body = json!({"repo_id": hello.scope, "session": hello.session});
    let hook = crate::authorize_full(app, &headers, &body, json!({}));
    if let Ok(caller) = &hook {
        // on one machine (the free version) one credential serves every
        // checkout: the person is the checkout's own git identity, so two
        // clones worked by two people (or two agents) read as who they are
        let person = if crate::local::active() && hello.person.contains('@') {
            hello.person.clone()
        } else if caller.email.is_empty() {
            caller.uid.clone()
        } else {
            caller.email.clone()
        };
        return Ok(Identity {
            scope: caller.scope.clone(),
            workspace: caller.workspace.clone(),
            label: caller.user_id.clone(),
            person,
            uid: caller.uid.clone(),
            session: hello.session.clone(),
            agent_id: crate::presence::agent_id(&caller.user_id, &hello.session),
            read_only: caller.read_only,
        });
    }
    let refused = match hook {
        Err(Json(body)) => text(&body, "reason"),
        Ok(_) => String::new(),
    };
    // a dashboard: the signed-in person, read-only, in a repo they may see
    if !token.starts_with(crate::auth::ACCESS_TOKEN_PREFIX) {
        if let Some(principal) = app.dashboard_auth.principal_for(&app.store, token, now()) {
            let workspace = Some(text(raw, "workspace")).filter(|w| !w.is_empty()).unwrap_or_else(|| workspace_hint.to_string());
            let auth = crate::auth::repo_auth(&app.store, Some(principal), &workspace, &hello.scope)
                .map_err(|(_, body)| text(&body, "error"))?;
            return Ok(Identity {
                scope: app.aliases.scope_for(&auth.workspace, &hello.scope),
                workspace: auth.workspace,
                label: auth.user_id.clone(),
                person: auth.user_id.clone(),
                uid: auth.uid,
                session: hello.session.clone(),
                agent_id: format!("dashboard#{}", hello.session),
                read_only: true,
            });
        }
    }
    Err(if refused.is_empty() { "unauthorized".into() } else { refused })
}

/// `GET /api/workspaces/:id/protocol?repo=`: the protocol's live state for
/// one repo (`Hub::view`), under the dashboard's auth.
pub async fn api_protocol(
    State(app): State<Arc<App>>, Path(workspace_id): Path<String>, headers: HeaderMap, Query(query): Query<Value>,
) -> (StatusCode, Json<Value>) {
    let principal = app.dashboard_auth.principal_of(&app.store, &headers, &text(&query, "token"), now());
    let repo = text(&query, "repo");
    if repo.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "repo is required"})));
    }
    let auth = match crate::auth::repo_auth(&app.store, principal, &workspace_id, &repo) {
        Ok(auth) => auth,
        Err((status, body)) => return (StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN), Json(body)),
    };
    let scope = app.aliases.scope_for(&auth.workspace, &repo);
    let mut view = app.protocol.view(&scope);
    view["repo"] = json!(repo);
    (StatusCode::OK, Json(view))
}

pub fn router() -> axum::Router<Arc<App>> {
    axum::Router::new()
        .route("/protocol/v1/ws", axum::routing::get(ws_endpoint))
        .route("/api/workspaces/:workspace_id/protocol", axum::routing::get(api_protocol))
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_hooks_side_sessions_are_not_agents() {
        assert!(super::hook_side_session("59d150c8-272d.ingest"));
        assert!(super::hook_side_session("59d150c8-272d.d4182"));
        assert!(!super::hook_side_session("59d150c8-272d"));
        assert!(!super::hook_side_session("alice-1"));
        assert!(!super::hook_side_session("my.design"));
    }

    #[test]
    fn an_ingested_commit_names_its_author_and_an_agents_op_does_not() {
        let frame = |id: &str, public: Value| ClientFrame::Op(op(id, "code:a.py::f", "modify_body", 0, public));
        assert_eq!(
            super::ingested_commits(&frame("git.abc123.0", json!({"author_email": "Bob@X.dev"}))),
            vec![("abc123".to_string(), "bob@x.dev".to_string())]
        );
        assert!(super::ingested_commits(&frame("s1.4", json!({"author_email": "bob@x.dev"}))).is_empty());
        assert!(super::ingested_commits(&frame("git.abc123.1", json!({}))).is_empty());
    }

    use std::time::Duration;

    use cork_protocol::ws_client::{BlockingClient, WsError};
    use cork_protocol::{Base, ClientConfig, Lease, Op, Subscribe};

    use super::*;

    const WS: &str = "ws1";
    const REPO: &str = "github.com/acme/api";
    const K: &str = "code:src/cli.py::parse_config";
    const T: Duration = Duration::from_secs(5);

    struct Server {
        app: Arc<App>,
        url: String,
        _rt: tokio::runtime::Runtime,
    }

    impl Server {
        fn scope(&self) -> String {
            self.app.aliases.scope_for(WS, REPO)
        }

        fn client(&self, token: &str, session: &str) -> Result<(BlockingClient, cork_protocol::Welcome), WsError> {
            let cfg = ClientConfig {
                session: session.into(),
                agent_id: "ignored".into(),
                person: "ignored".into(),
                scope: REPO.into(),
                token: Some(token.into()),
            };
            BlockingClient::connect(&self.url, cfg, false, T)
        }
    }

    fn app_on(store: Arc<Store>) -> Arc<App> {
        // short grace so the expiry test does not wait five seconds
        crate::env::set("CORK_PROTO_GRACE_MS", "300");
        let protocol = Hub::open(Arc::clone(&store), "");
        Arc::new(App {
            store,
            aliases: crate::repo::Aliases::new(),
            started: std::time::Instant::now(),
            dashboard_url: String::new(),
            public_url: String::new(),
            mcp_url: String::new(),
            dashboard_auth: crate::auth::DashboardAuth { test_mode: true, firebase_project_id: String::new(), firebase_emulator: false },
            hot_ttl_s: 900.0,
            idle_after_s: 600.0,
            intent_ttl_s: 900.0,
            protocol,
        })
    }

    /// A team workspace where alice owns, bob plays and vic views, each with
    /// a hook credential `cat_<name>`.
    fn seeded() -> Arc<Store> {
        let store = Arc::new(Store::open(std::path::Path::new(":memory:")).unwrap());
        store.kv_put(crate::billing::BILLING_BUCKET, WS, &json!({"plan": "team", "seats": 5, "status": "active"}), 1.0).unwrap();
        for (name, role, access) in [("alice", "owner", "write"), ("bob", "member", "write"), ("vic", "member", "read")] {
            let email = format!("{name}@x.dev");
            store
                .kv_put("member", &format!("{WS}/{name}"), &json!({
                    "uid": name, "email": email, "role": role, "access": access, "repos": "*", "added": 0.0,
                }), 0.0)
                .unwrap();
            store
                .kv_put(crate::auth::TOKEN_BUCKET, &crate::auth::token_key(&format!("cat_{name}")), &json!({
                    "kind": "access", "expires": 1e12, "uid": name, "email": email, "workspace": WS,
                }), 0.0)
                .unwrap();
        }
        store
    }

    fn serve(app: Arc<App>) -> Server {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let router = router().with_state(Arc::clone(&app));
        rt.spawn(async move { axum::serve(listener, router).await });
        Server { app, url: format!("ws://{addr}/protocol/v1/ws"), _rt: rt }
    }

    fn op(id: &str, key: &str, kind: &str, base: u64, public: Value) -> Op {
        Op {
            op_id: id.into(),
            key: key.into(),
            base: Base { version: base, ..Default::default() },
            kind: kind.into(),
            public: public.as_object().cloned().unwrap_or_default(),
            ..Default::default()
        }
    }

    /// Frames until `pick` matches, within `T`.
    fn wait_for<R>(c: &mut BlockingClient, mut pick: impl FnMut(&ServerFrame) -> Option<R>) -> R {
        let deadline = std::time::Instant::now() + T;
        while std::time::Instant::now() < deadline {
            if let Ok(Some(f)) = c.recv(Duration::from_millis(100)) {
                if let Some(r) = pick(&f) {
                    return r;
                }
            }
        }
        panic!("the frame never came");
    }

    fn eventually(mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + T;
        while !done() {
            assert!(std::time::Instant::now() < deadline, "never happened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_stale_write_is_rejected_with_the_op_it_missed_and_lands_in_the_ledger() {
        let s = serve(app_on(seeded()));
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        let (mut b, _) = s.client("cat_bob", "sb").unwrap();
        let mut first = op("sa.1", K, "modify_body", 0, json!({}));
        first.intent_id = Some("tighten".into());
        let acked = a.write(first, T).unwrap().unwrap();
        assert_eq!(acked.version, 1);
        let r = b.write(op("sb.1", K, "modify_body", 0, json!({})), T).unwrap().unwrap_err();
        assert_eq!(r.reason, RejectReason::Conflict);
        assert_eq!(r.theirs[0].op_id, "sa.1");
        // the server's identity, not the client's
        assert_eq!(r.theirs[0].author.person, "alice@x.dev");
        assert_eq!(r.theirs[0].author.agent_id, "alice@x.dev#sa");
        let scope = s.scope();
        eventually(|| !s.app.store.ledger_since_kinds(&scope, 0.0, &["op_conflict"]).is_empty());
        let row = &s.app.store.ledger_since_kinds(&scope, 0.0, &["op_conflict"])[0].payload;
        assert_eq!((row["writer"].as_str(), row["holder"].as_str()), (Some("bob@x.dev"), Some("alice@x.dev")));
        assert_eq!(row["key"], json!(K));
        assert_eq!(row["their_kinds"], json!(["modify_body"]));
        // the Inbox's feed has it as a blocked collision
        eventually(|| s.app.store.eph_get(&format!("recent:{scope}")).is_some());
        let ring = s.app.store.eph_get(&format!("recent:{scope}")).unwrap();
        let hit = ring["events"].as_array().unwrap().iter().find(|e| e["kind"] == json!("collision")).unwrap().clone();
        assert_eq!(hit["mode"], json!("blocked"));
        assert_eq!(hit["items"][0]["symbol"], json!("parse_config"));
        // accepted ops leave presence, not ledger rows
        assert!(s.app.store.ledger_since_kinds(&scope, 0.0, &["op_accepted"]).is_empty());
        let health = s.app.protocol.health();
        assert_eq!((health["ops_accepted"].as_u64(), health["conflicts"].as_u64(), health["log_seq"].as_u64()), (Some(1), Some(1), Some(1)));
    }

    #[test]
    fn a_write_on_a_base_the_server_never_saw_is_a_stale_write_on_the_ledger_not_a_collision() {
        let s = serve(app_on(seeded()));
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        let (mut b, _) = s.client("cat_bob", "sb.ingest").unwrap();
        let mut first = op("sa.1", K, "modify_body", 0, json!({}));
        first.content_hash = Some("body-1".into());
        a.write(first, T).unwrap().unwrap();
        let mut blind = op("sb.1", K, "modify_body", 0, json!({}));
        blind.base = Base::of_content("a-copy-nobody-sent");
        let r = b.write(blind, T).unwrap().unwrap_err();
        assert_eq!(r.reason, RejectReason::UnknownBase);
        let scope = s.scope();
        eventually(|| !s.app.store.ledger_since_kinds(&scope, 0.0, &["op_conflict"]).is_empty());
        let row = &s.app.store.ledger_since_kinds(&scope, 0.0, &["op_conflict"])[0].payload;
        assert_eq!((row["stage"].as_str(), row["reason"].as_str()), (Some("write"), Some("unknown_base")));
        // its outcome is pending for bob's agent session, not the ingest side session
        let prefix = format!("{scope}|{}|", crate::presence::agent_id("bob@x.dev", "sb"));
        eventually(|| !s.app.store.kv_list(crate::outcomes::BUCKET, &prefix).is_empty());
        let pending = s.app.store.kv_list(crate::outcomes::BUCKET, &prefix);
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0].1["kind"], "protocol_unknown_base");
        // not a clash between agents: the live collision feed stays quiet
        let ring = s.app.store.eph_get(&format!("recent:{scope}")).unwrap_or(json!({"events": []}));
        assert!(!ring["events"].as_array().unwrap().iter().any(|e| e["kind"] == json!("collision")));
    }

    fn collisions(s: &Server) -> usize {
        let ring = s.app.store.eph_get(&format!("recent:{}", s.scope())).unwrap_or(json!({"events": []}));
        ring["events"].as_array().unwrap().iter().filter(|e| e["kind"] == json!("collision")).count()
    }

    /// The channels agent reported its function as it wrote it; the commit
    /// that carried it upstream came back through the ingest. Same code
    /// twice: acked, never "blocked from changing".
    #[test]
    fn the_same_function_from_two_sessions_is_acked_and_never_a_collision() {
        let s = serve(app_on(seeded()));
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        let (mut b, _) = s.client("cat_bob", "sb.ingest").unwrap();
        let (mut c, _) = s.client("cat_bob", "sc").unwrap();
        let mut inserted = op("sa.1", K, "insert", 0, json!({}));
        inserted.content_hash = Some("fn-1".into());
        a.write(inserted.clone(), T).unwrap().unwrap();
        for (client, id) in [(&mut b, "git.b266fed3d7a5721e.0"), (&mut c, "sc.1")] {
            let mut again = inserted.clone();
            again.op_id = id.into();
            let acked = client.write(again, T).unwrap().unwrap();
            assert_eq!((acked.version, acked.same_as.as_deref()), (1, Some("sa.1")), "{id}");
        }
        let scope = s.scope();
        assert!(s.app.store.ledger_since_kinds(&scope, 0.0, &["op_conflict"]).is_empty());
        assert_eq!(collisions(&s), 0);
        assert_eq!(s.app.protocol.health()["conflicts"].as_u64(), Some(0));
    }

    /// A commit that landed upstream with other code than a draft: the
    /// truth now, promoted, with no collision row. The draft's author
    /// writing on its older copy is a stale write, not a clash.
    #[test]
    fn a_landed_commit_goes_over_a_draft_promoted_and_records_no_collision() {
        let s = serve(app_on(seeded()));
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        let (mut b, _) = s.client("cat_bob", "sb.ingest").unwrap();
        let mut draft = op("sa.1", K, "modify_body", 0, json!({}));
        draft.content_hash = Some("body-1".into());
        a.write(draft, T).unwrap().unwrap();
        let mut landed = op("git.bc5a7671885737d5.0", K, "modify_body", 0, json!({"author_email": "bob@x.dev"}));
        landed.content_hash = Some("body-9".into());
        let acked = b.write(landed, T).unwrap().unwrap();
        assert_eq!((acked.version, acked.same_as), (2, None));
        let r = a.write(op("sa.2", K, "modify_body", 1, json!({})), T).unwrap().unwrap_err();
        assert_eq!((r.reason, r.theirs[0].op_id.as_str()), (RejectReason::Conflict, "git.bc5a7671885737d5.0"));
        let scope = s.scope();
        eventually(|| !s.app.store.ledger_since_kinds(&scope, 0.0, &["op_conflict"]).is_empty());
        let rows = s.app.store.ledger_since_kinds(&scope, 0.0, &["op_conflict"]);
        assert_eq!(rows.len(), 1, "only alice's stale write");
        assert_eq!(rows[0].payload["writer"], json!("alice@x.dev"));
        assert_eq!(collisions(&s), 0, "a copy older than what landed is not a clash");
        let st = s.app.protocol.lock();
        let key = st.engine.key_state(&scope, K).unwrap();
        assert_eq!((key.version, key.promoted_version), (2, 2));
    }

    /// What the landed sweep takes away: the collisions a landed op's
    /// refusal put on the feed and the outcomes it opened. A real collision,
    /// its outcome and everything else stay; a second run does nothing.
    #[test]
    fn the_landed_sweep_takes_only_landed_historys_collisions_off_the_feed() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "ws1:github.com/acme/web";
        let t = now() - 3600.0;
        let conflict = |op_id: &str, writer: &str, session: &str, path: &str, symbol: &str| json!({
            "op_id": op_id, "writer": writer, "writer_agent": crate::presence::agent_id(writer, session), "writer_session": session,
            "path": path, "symbol": symbol, "stage": "write", "reason": "conflict", "holder": "ana@x.dev",
        });
        store.ledger_append(scope, "op_conflict", &conflict("git.b266fed3d7a5721e.7", "kevin@x.dev", "s1.ingest", "channels/page.tsx", "ChannelsView"), t).unwrap();
        store.ledger_append(scope, "op_conflict", &conflict("git.b266fed3d7a5721e.9", "kevin@x.dev", "s1", "channels.css", ""), t).unwrap();
        store.ledger_append(scope, "op_conflict", &conflict("sb.4", "bob@x.dev", "sb", "api.py", "charge"), t + 5.0).unwrap();
        let mut same = conflict("sc.2", "carol@x.dev", "sc", "billing.py", "refund");
        same["identical"] = json!(true);
        store.ledger_append(scope, "op_conflict", &same, t + 6.0).unwrap();
        let hit = |agent: &str, path: &str, symbol: &str, ts: f64, op_id: Option<&str>| {
            let mut e = json!({"kind": "collision", "source": "protocol", "mode": "blocked", "checker_agent": agent, "ts": ts,
                               "items": [{"path": path, "symbol": symbol}]});
            if let Some(id) = op_id {
                e["op_id"] = json!(id);
            }
            e
        };
        let kevin = crate::presence::agent_id("kevin@x.dev", "s1.ingest");
        let events = json!({"events": [
            hit(&kevin, "channels/page.tsx", "ChannelsView", t + 0.2, None),
            hit(&crate::presence::agent_id("kevin@x.dev", "s1"), "channels.css", "channels.css", t + 0.1, None),
            hit(&crate::presence::agent_id("bob@x.dev", "sb"), "api.py", "charge", t + 5.1, None),
            hit(&kevin, "route-modal.tsx", "WINDOWS", t + 9.0, Some("git.bc5a7671885737d5.3")),
            hit(&crate::presence::agent_id("carol@x.dev", "sc"), "billing.py", "refund", t + 6.1, None),
            // the same agent and symbol a day later is another matter
            hit(&kevin, "channels/page.tsx", "ChannelsView", t + 3000.0, None),
            {"kind": "edit", "user": "ana@x.dev", "path": "api.py", "ts": t},
        ]});
        store.eph_set(&format!("recent:{scope}"), &events, Some(86_400.0)).unwrap();
        crate::outcomes::open(&store, scope, "protocol_stale", "kevin@x.dev", "s1", &["channels/page.tsx".into()], json!({}), t);
        crate::outcomes::open(&store, scope, "protocol_stale", "bob@x.dev", "sb", &["api.py".into()], json!({}), t);

        let swept = sweep_landed(&store, now()).expect("first run");
        assert_eq!(swept, Swept { scopes: 1, collisions: 4, outcomes: 1 });
        let left = store.eph_get(&format!("recent:{scope}")).unwrap();
        let left: Vec<(String, String)> = left["events"].as_array().unwrap().iter()
            .map(|e| (text(e, "kind"), e["items"][0]["path"].as_str().or(e["path"].as_str()).unwrap_or("").to_string())).collect();
        assert_eq!(left, vec![
            ("collision".to_string(), "api.py".to_string()),
            ("collision".to_string(), "channels/page.tsx".to_string()),
            ("edit".to_string(), "api.py".to_string()),
        ]);
        let pending: Vec<String> = store.kv_list(crate::outcomes::BUCKET, "").into_iter().map(|(_, v)| text(&v, "user")).collect();
        assert_eq!(pending, vec!["bob@x.dev"]);
        assert!(sweep_landed(&store, now()).is_none(), "once per database");
        // and the ledger rows it cannot change are no one's catch (the
        // cloud's waste report reads them)
        #[cfg(feature = "cloud")]
        {
            let rows: Vec<(String, crate::store::LedgerRow)> =
                store.ledger_since_kinds(scope, 0.0, &["op_conflict"]).into_iter().map(|r| (scope.to_string(), r)).collect();
            let caught: Vec<String> = crate::waste::incidents_by_person(&rows).into_iter().map(|(who, _)| who).collect();
            assert_eq!(caught, vec!["bob@x.dev"]);
        }
    }



    #[test]
    fn a_rename_rekeys_a_stale_body_edit() {
        let s = serve(app_on(seeded()));
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        let (mut b, _) = s.client("cat_bob", "sb").unwrap();
        let renamed = a.write(op("sa.1", K, "rename", 0, json!({"new_name": "load_config"})), T).unwrap().unwrap();
        assert_eq!(renamed.moved_to.unwrap().key, "code:src/cli.py::load_config");
        let edited = b.write(op("sb.1", K, "modify_body", 0, json!({})), T).unwrap().unwrap();
        assert_eq!(edited.rekeyed_to.as_deref(), Some("code:src/cli.py::load_config"));
        assert_eq!(edited.key, "code:src/cli.py::load_config");
    }

    #[test]
    fn a_lease_outlives_its_connection_by_the_grace_and_no_more() {
        let s = serve(app_on(seeded()));
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        let (mut b, _) = s.client("cat_bob", "sb").unwrap();
        let lease = |kinds: &[&str]| ClientFrame::Lease(Lease {
            req: None,
            keys: vec![K.into()],
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
            intent: None,
        });
        a.send(&lease(&["modify_body"])).unwrap();
        wait_for(&mut a, |f| matches!(f, ServerFrame::LeaseGranted(_)).then_some(()));
        b.send(&lease(&["modify_body"])).unwrap();
        wait_for(&mut b, |f| matches!(f, ServerFrame::LeaseDenied(_)).then_some(()));
        let r = b.write(op("sb.1", K, "modify_body", 0, json!({})), T).unwrap().unwrap_err();
        assert_eq!(r.reason, RejectReason::Leased);
        assert_eq!(r.holder.unwrap().session, "sa");
        a.close();
        // still held inside the grace
        assert_eq!(s.app.protocol.view(&s.scope())["leases"].as_array().unwrap().len(), 1);
        let released = wait_for(&mut b, |f| match f {
            ServerFrame::LeaseReleased(r) => Some(r.clone()),
            _ => None,
        });
        assert_eq!(released.reason, cork_protocol::ReleaseReason::Expired);
        assert!(s.app.protocol.view(&s.scope())["leases"].as_array().unwrap().is_empty());
        b.send(&lease(&["modify_body"])).unwrap();
        wait_for(&mut b, |f| matches!(f, ServerFrame::LeaseGranted(_)).then_some(()));
    }

    /// The hooks stop most collisions at the gate's `check`, before an op is
    /// sent: a check naming a kind that a write would lose is recorded like a
    /// refused write — once, however often the gate asks — and a plain check
    /// (no kind: a look, not a gate) records nothing.
    #[test]
    fn a_gate_check_that_would_be_refused_is_a_collision_on_the_dashboard() {
        let s = serve(app_on(seeded()));
        let scope = s.scope();
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        let (mut b, _) = s.client("cat_bob", "sb").unwrap();
        a.lease(&[K], &["modify_body"], None, T).unwrap().unwrap();
        b.check(&[K], None, T).unwrap();
        let checked = b.check(&[K], Some("modify_body"), T).unwrap();
        assert_eq!(checked.results[0].status, CheckStatus::Leased);
        b.check(&[K], Some("modify_body"), T).unwrap();
        b.check(&[K], Some("modify_body"), T).unwrap();
        eventually(|| !s.app.store.ledger_since_kinds(&scope, 0.0, &["op_conflict"]).is_empty());
        std::thread::sleep(Duration::from_millis(100));
        let rows = s.app.store.ledger_since_kinds(&scope, 0.0, &["op_conflict"]);
        assert_eq!(rows.len(), 1, "one clash, asked about three times");
        let row = &rows[0].payload;
        assert_eq!((row["stage"].as_str(), row["reason"].as_str()), (Some("gate"), Some("leased")));
        assert_eq!((row["writer"].as_str(), row["holder"].as_str()), (Some("bob@x.dev"), Some("alice@x.dev")));
        assert_eq!(row["symbol"], json!("parse_config"));
        eventually(|| s.app.store.eph_get(&format!("recent:{scope}")).is_some());
        let ring = s.app.store.eph_get(&format!("recent:{scope}")).unwrap();
        assert!(ring["events"].as_array().unwrap().iter().any(|e| e["kind"] == json!("collision")), "the Inbox has it");
        assert_eq!(s.app.protocol.view(&scope)["recent_conflicts"].as_array().map(Vec::len), Some(1));

        // a refused write still records as a write
        let r = b.write(op("sb.1", K, "modify_body", 0, json!({})), T).unwrap().unwrap_err();
        assert_eq!(r.reason, RejectReason::Leased);
        eventually(|| s.app.store.ledger_since_kinds(&scope, 0.0, &["op_conflict"]).len() == 2);
        assert_eq!(s.app.store.ledger_since_kinds(&scope, 0.0, &["op_conflict"])[1].payload["stage"], json!("write"));
    }

    #[test]
    fn a_change_reaches_whoever_subscribes_to_its_caller() {
        let s = serve(app_on(seeded()));
        let scope = s.scope();
        let record = json!({"path": "src/a.py", "language": "python", "imports": [], "ts": 1.0, "symbols": {
            "callee": {"kind": "function"},
            "caller": {"kind": "function", "edges": [["callee", "", "calls"]]},
        }});
        crate::codegraph::update_file(&s.app.store, &scope, "src/a.py", record, 1.0);
        let graph = CallGraph { store: Arc::downgrade(&s.app.store) };
        assert_eq!(graph.dependents(&scope, "code:src/a.py::callee"), vec!["code:src/a.py::caller"]);
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        let (mut b, _) = s.client("cat_bob", "sb").unwrap();
        b.send(&ClientFrame::Subscribe(Subscribe { req: None, keys: vec!["code:src/a.py::caller".into()], depth: 1 })).unwrap();
        // the subscribe is processed before the write: a check round-trips
        b.check(&["code:src/a.py::caller"], None, T).unwrap();
        a.write(op("sa.1", "code:src/a.py::callee", "signature_change", 0, json!({"new_signature": "callee(x, y)"})), T).unwrap().unwrap();
        let d = wait_for(&mut b, |f| match f {
            ServerFrame::Delta(d) => Some(d.clone()),
            _ => None,
        });
        assert_eq!(d.op.key, "code:src/a.py::callee");
        assert_eq!(d.via, Some(vec!["code:src/a.py::caller".to_string(), "code:src/a.py::callee".to_string()]));
    }

    #[test]
    fn a_restart_rebuilds_versions_from_the_log_and_its_snapshot() {
        let store = seeded();
        {
            let s = serve(app_on(Arc::clone(&store)));
            let (mut a, _) = s.client("cat_alice", "sa").unwrap();
            a.write(op("sa.1", K, "modify_body", 0, json!({})), T).unwrap().unwrap();
            // a snapshot mid-way: the restart must use it and the tail after
            let snap = s.app.protocol.lock().engine.snapshot();
            write_snapshot(&store, &snap);
            assert!(store.kv_get(LOG_BUCKET, &seq_key(1)).is_none(), "the snapshot prunes what it covers");
            let mut second = op("sa.2", K, "modify_body", 1, json!({}));
            second.content_hash = Some("body-2".into());
            a.write(second, T).unwrap().unwrap();
            assert_eq!(s.app.protocol.head_seq(), 2);
        }
        let s = serve(app_on(Arc::clone(&store)));
        assert_eq!(s.app.protocol.head_seq(), 2);
        let (mut b, _) = s.client("cat_bob", "sb").unwrap();
        // a stale base still conflicts; a base by content (SPEC §5.1) lands on v3
        let r = b.write(op("sb.1", K, "modify_body", 1, json!({})), T).unwrap().unwrap_err();
        assert_eq!((r.reason, r.current.version), (RejectReason::Conflict, 2));
        let mut by_hash = op("sb.2", K, "modify_body", 0, json!({}));
        by_hash.base = Base::of_content("body-2");
        assert_eq!(b.write(by_hash, T).unwrap().unwrap().version, 3);
        assert_eq!(s.app.protocol.head_seq(), 3);
    }

    #[test]
    fn a_viewer_may_watch_but_not_write() {
        let s = serve(app_on(seeded()));
        // viewers are the cloud's (a plan's members): on one machine every
        // member writes
        #[cfg(feature = "cloud")]
        {
            let (mut v, _) = s.client("cat_vic", "sv").unwrap();
            v.send(&ClientFrame::Op(op("sv.1", K, "modify_body", 0, json!({})))).unwrap();
            let e = wait_for(&mut v, |f| match f {
                ServerFrame::Error(e) => Some(e.clone()),
                _ => None,
            });
            assert_eq!(e.code, "read_only");
            assert_eq!(s.app.protocol.head_seq(), 0);
            // but it subscribes and hears others' writes
            v.send(&ClientFrame::Subscribe(Subscribe { req: None, keys: vec!["code:*".into()], depth: 0 })).unwrap();
            v.check(&[K], None, T).unwrap();
            let (mut a, _) = s.client("cat_alice", "sa").unwrap();
            a.write(op("sa.1", K, "modify_body", 0, json!({})), T).unwrap().unwrap();
            wait_for(&mut v, |f| matches!(f, ServerFrame::Delta(_)).then_some(()));
            assert_eq!(s.app.protocol.view(&s.scope())["sessions"].as_array().unwrap().iter().filter(|x| x["read_only"] == json!(true)).count(), 1);
        }
        // a bad credential never gets in
        assert!(matches!(s.client("cat_nobody", "sx"), Err(WsError::Server(e)) if e.code == "unauthorized"));
    }

    #[test]
    fn a_hub_message_reaches_a_persons_connected_sessions_in_any_repo() {
        let s = serve(app_on(seeded()));
        let (mut b, _) = s.client("cat_bob", "sb").unwrap();
        let hub = Arc::clone(&s.app.protocol);
        assert!(hub.deliver_to_person("bob", "", "sb", "hub", "x").is_empty(), "never the skipped session");
        assert!(hub.deliver_to_person("bob", "other", "", "hub", "x").is_empty(), "only the one named");
        assert!(hub.deliver_to_person("alice", "", "", "hub", "x").is_empty(), "alice has no session open");
        assert_eq!(hub.deliver_to_person("bob", "", "", "hub", "[hub: ops · @kim] hi"), vec!["sb".to_string()]);
        let m = wait_for(&mut b, |f| match f {
            ServerFrame::Message(m) => Some(m.clone()),
            _ => None,
        });
        assert_eq!(m.text, "[hub: ops · @kim] hi");
    }

    /// Hubs are the cloud's: the free version has none.
    #[cfg(feature = "cloud")]
    #[test]
    fn a_hub_carries_only_what_was_sent_never_the_repos_work() {
        use crate::hubs;
        let s = serve(app_on(seeded()));
        let store = &s.app.store;
        for (uid, name) in [("alice", "alice"), ("bob", "bob")] {
            store.kv_put("profile", uid, &json!({"username": name}), 1.0).unwrap();
            store.kv_put("usernames", name, &json!({"uid": uid}), 1.0).unwrap();
        }
        hubs::request(store, "alice", "bob").unwrap();
        hubs::accept(store, "bob", "alice").unwrap();
        let (hub, _) = hubs::create(store, "alice", "ops", &["bob".into()]).unwrap();
        let none = |_: &str, _: &str, _: &str, _: &str| Vec::new();
        let sender = hubs::Sender { uid: "alice", session: "sa", agent: "claude-code", via: "mcp" };
        hubs::post_message(store, &hub, &sender, "lunch at noon", "", &none).unwrap();
        // the sender's repo keeps working: a protocol op, an edit report, presence
        let scope = s.scope();
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        a.write(op("sa.1", K, "modify_body", 0, json!({})), T).unwrap().unwrap();
        crate::report::report_edit(store, &crate::report::ReportInput {
            scope: &scope, user_id: "alice@x.dev", path: "pkg/secret.py", content: "def token():\n    return 1\n",
            local: None, agent: "claude-code", model: "", effort: Default::default(), branch: "main", session: "sa", tokens: 0,
            input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0, turn_id: "",
            context: 0, why: "rotate the token", hot_ttl_s: 600.0, via: "", workspace: WS, draft: false, auto: true,
            verified: None,
            worker: "",
            resent: false,
        });
        hubs::note_session(store, "alice", "sa", &scope, "alice@x.dev");
        // the hub holds the one message, as sent, and nothing of the repo
        let rows = hubs::messages(store, &hub, "bob", 0.0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["text"], json!("lunch at noon"));
        assert_eq!(store.kv_list(hubs::HUB_MESSAGES, "").len(), 1);
        let copies = store.kv_list(hubs::HUB_INBOX, "bob:");
        assert_eq!(copies.len(), 1);
        let everything = serde_json::to_string(&(rows, copies, hubs::list_hubs(store, "bob"))).unwrap();
        for leaked in ["secret.py", "token", "rotate", "modify_body", K, "main"] {
            assert!(!everything.contains(leaked), "{leaked} reached the hub: {everything}");
        }
    }

    #[test]
    fn a_dashboard_message_to_one_session_arrives_on_its_socket() {
        let s = serve(app_on(seeded()));
        let (mut b, _) = s.client("cat_bob", "sb").unwrap();
        let scope = s.scope();
        let hub = Arc::clone(&s.app.protocol);
        // the dashboard's sender (dashmsg) is the cloud's
        #[cfg(feature = "cloud")]
        {
            let live = |to: &str, session: &str, text: &str| hub.deliver_to_session(&scope, session, to, "alice@x.dev", text);
            let sent = crate::dashmsg::send_with(&s.app.store, &scope, WS, REPO, "alice@x.dev",
                &json!({"text": "rebase first", "to": "bob@x.dev", "session": "sb"}), "", &live);
            assert_eq!(sent["live"], json!(true), "{sent}");
            let m = wait_for(&mut b, |f| match f {
                ServerFrame::Message(m) => Some(m.clone()),
                _ => None,
            });
            assert_eq!((m.text.as_str(), m.from.person.as_str()), ("rebase first", "alice@x.dev"));
            assert!(s.app.store.kv_list("inbox", &format!("{scope}:bob@x.dev:")).is_empty());
        }
        // another person's session is not this recipient's: the inbox
        assert!(!hub.deliver_to_session(&scope, "sb", "alice@x.dev", "carol", "x"));
        // and a session's own message frame reaches Cork's inboxes too
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        #[cfg(feature = "cloud")]
        let threaded = |s: &Server| crate::dashmsg::thread(&s.app.store, &scope, WS, "alice@x.dev", 0.0)["messages"].as_array().unwrap().len();
        #[cfg(feature = "cloud")]
        let before = threaded(&s);
        a.send(&ClientFrame::Message(cork_protocol::SendMessage { req: None, to: Some("sb".into()), anchor: None, text: "ping".into() })).unwrap();
        wait_for(&mut b, |f| matches!(f, ServerFrame::Message(m) if m.text == "ping").then_some(()));
        // heard live: in the thread, not queued again in bob's inbox
        #[cfg(feature = "cloud")]
        eventually(|| threaded(&s) == before + 1);
        assert!(s.app.store.kv_list("inbox", &format!("{scope}:bob@x.dev:")).is_empty());
        // nobody of bob's connected: the inbox carries it
        b.close();
        eventually(|| s.app.protocol.view(&scope)["sessions"].as_array().unwrap().iter().all(|x| x["session"] != json!("sb")));
        a.send(&ClientFrame::Message(cork_protocol::SendMessage { req: None, to: Some("bob@x.dev".into()), anchor: None, text: "later".into() })).unwrap();
        eventually(|| s.app.store.kv_list("inbox", &format!("{scope}:bob@x.dev:")).len() == 1);
    }

    #[test]
    fn a_dashboard_sign_in_connects_as_a_read_only_peer() {
        let s = serve(app_on(seeded()));
        let cfg = ClientConfig { session: "dash-1".into(), agent_id: String::new(), person: String::new(), scope: REPO.into(), token: Some("test:bob".into()) };
        let (mut d, _) = BlockingClient::connect(&format!("{}?workspace={WS}", s.url), cfg, false, T).unwrap();
        d.send(&ClientFrame::Subscribe(Subscribe { req: None, keys: vec!["code:*".into()], depth: 0 })).unwrap();
        d.check(&[K], None, T).unwrap();
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        a.write(op("sa.1", K, "modify_body", 0, json!({})), T).unwrap().unwrap();
        wait_for(&mut d, |f| matches!(f, ServerFrame::Delta(_)).then_some(()));
        d.send(&ClientFrame::Op(op("dash-1.1", K, "modify_body", 1, json!({})))).unwrap();
        wait_for(&mut d, |f| matches!(f, ServerFrame::Error(e) if e.code == "read_only").then_some(()));
        // a workspace the person is not in is refused
        let cfg = ClientConfig { session: "dash-2".into(), agent_id: String::new(), person: String::new(), scope: REPO.into(), token: Some("test:bob".into()) };
        assert!(BlockingClient::connect(&format!("{}?workspace=elsewhere", s.url), cfg, false, T).is_err());
    }

    #[test]
    fn the_dashboard_read_lists_sessions_leases_and_key_layers() {
        let s = serve(app_on(seeded()));
        let (mut a, _) = s.client("cat_alice", "sa").unwrap();
        let acked = a.write(op("sa.1", K, "modify_body", 0, json!({})), T).unwrap().unwrap();
        a.write(op("sa.2", "code:src/cli.py::other", "modify_body", 0, json!({})), T).unwrap().unwrap();
        // the hook that pushes is another session of the same credential
        let (mut ingest, _) = s.client("cat_alice", "sa.ingest").unwrap();
        let promoted = ingest.promote(vec![acked.op_id], json!({"tests": "passed"}), T).unwrap();
        assert_eq!(promoted.op_ids.len(), 1, "{promoted:?}");
        let view = s.app.protocol.view(&s.scope());
        assert_eq!(view["sessions"][0]["agent_id"], json!("alice@x.dev#sa"));
        assert_eq!(view["keys"]["total"], json!(2));
        assert_eq!(view["keys"]["promoted"], json!(1));
        assert_eq!(view["keys"]["with_drafts"], json!(1));
        assert_eq!(view["recent_ops"].as_array().unwrap().len(), 2);
        let scope = s.scope();
        eventually(|| !s.app.store.ledger_since_kinds(&scope, 0.0, &["op_promoted"]).is_empty());
    }
}
