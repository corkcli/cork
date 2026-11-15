//! The server as a sans-IO state machine. A transport feeds it frames with
//! the connection they came from and the time, and sends what it returns:
//!
//! ```text
//! engine.connected(conn, now)               -> Vec<Outgoing>
//! engine.handle(conn, frame, now)           -> Vec<Outgoing>
//! engine.disconnected(conn, now)            -> Vec<Outgoing>
//! engine.tick(now)                          -> Vec<Outgoing>   (grace expiry)
//! engine.next_deadline()                    -> Option<u64>     (when to tick)
//! ```
//!
//! No IO, no threads, no clocks: the same input gives the same output, which
//! is what the conformance vectors replay. The engine holds no lock across
//! IO; the only IO it causes is `Log::append`, synchronously, before an ack.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use serde_json::Value;

use crate::domain::Domain;
use crate::frames::*;
use crate::hash::{sha256_hex, ZERO_HASH};
use crate::hlc::{Clock, Hlc};
use crate::log::{entry_hash, Accepted, Log, LogEntry, LogError, LogRecord, MemLog, Snapshot};
use crate::store::{HistoryEntry, KeyState, MemStore, MovedTo, OpLoc, Store};
use crate::table::Relation;

/// A transport's id for one connection.
pub type ConnId = u64;

/// A frame for one connection. After an `error` with `fatal: true` the
/// transport closes that connection.
#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing {
    pub conn: ConnId,
    pub frame: ServerFrame,
}

/// Checks a hello's token and returns the identity the session acts as (the
/// returned `session` is ignored; the hello's is used).
pub type Authorizer = Box<dyn Fn(&Hello) -> Result<Peer, String> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct Config {
    /// How long a disconnected session keeps its leases and subscriptions.
    pub grace_ms: u64,
    /// The deepest dependency walk a subscription may ask for.
    pub max_depth: u32,
    /// Recent ops kept per key; a writer whose base is older is rejected.
    pub history_per_key: usize,
    pub max_payload_bytes: usize,
    pub max_batch_ops: usize,
    pub max_drift_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            grace_ms: 5_000,
            max_depth: 3,
            history_per_key: 64,
            max_payload_bytes: 1 << 20,
            max_batch_ops: 1_000,
            max_drift_ms: 60_000,
        }
    }
}

struct LeaseEntry {
    session: String,
    peer: Peer,
    kinds: Vec<String>,
    intent: Option<Intent>,
}

struct Session {
    peer: Peer,
    scope: String,
    conn: Option<ConnId>,
    detached_at: Option<u64>,
    leases: BTreeSet<String>,
    /// key (or `prefix*`) -> depth
    subs: BTreeMap<String, u32>,
}

#[derive(Default)]
struct ScopeRt {
    leases: HashMap<String, Vec<LeaseEntry>>,
    subs: HashMap<String, BTreeMap<String, u32>>,
    /// (prefix without `*`, session) -> depth
    prefix_subs: BTreeMap<(String, String), u32>,
    /// sessions denied a lease on the key: told when it is released
    waiters: HashMap<String, BTreeSet<String>>,
    deep_subs: usize,
}

/// What an op resolves to before anything is applied.
struct Resolved {
    final_key: String,
    rebased: Vec<TheirOp>,
}

enum Failure {
    Invalid(String),
    Conflict { key: String, theirs: Vec<TheirOp> },
}

struct Prepared {
    op: Op,
    named_key: String,
    resolved: Resolved,
    moves_to: Option<String>,
}

/// What an op comes to once it is checked: applied, or acked as what the
/// key already holds (nothing applied, nothing logged).
enum Decision {
    Apply(Prepared),
    Already(Ack),
}

/// An op that is history already landed on the shared branch: a commit's
/// ops as a hook's ingest sends them, `git.<sha>.<n>` (SPEC §10.1). It is
/// accepted over the drafts it meets and promoted as it lands.
pub fn is_landed(op_id: &str) -> bool {
    op_id.starts_with("git.")
}

pub struct Engine {
    cfg: Config,
    domains: Vec<Arc<dyn Domain>>,
    store: Box<dyn Store>,
    log: Box<dyn Log>,
    clock: Clock,
    head_seq: u64,
    head_hash: String,
    conns: HashMap<ConnId, Option<String>>,
    sessions: HashMap<String, Session>,
    scopes: HashMap<String, ScopeRt>,
    authorize: Option<Authorizer>,
}

pub struct EngineBuilder {
    cfg: Config,
    domains: Vec<Arc<dyn Domain>>,
    store: Option<Box<dyn Store>>,
    log: Option<Box<dyn Log>>,
    authorize: Option<Authorizer>,
    snapshot: Option<Snapshot>,
}

impl EngineBuilder {
    pub fn config(mut self, cfg: Config) -> Self {
        self.cfg = cfg;
        self
    }
    pub fn grace_ms(mut self, ms: u64) -> Self {
        self.cfg.grace_ms = ms;
        self
    }
    pub fn domain(mut self, d: impl Domain + 'static) -> Self {
        self.domains.push(Arc::new(d));
        self
    }
    pub fn domain_arc(mut self, d: Arc<dyn Domain>) -> Self {
        self.domains.push(d);
        self
    }
    pub fn store(mut self, s: impl Store + 'static) -> Self {
        self.store = Some(Box::new(s));
        self
    }
    pub fn log(mut self, l: impl Log + 'static) -> Self {
        self.log = Some(Box::new(l));
        self
    }
    pub fn authorize(mut self, f: impl Fn(&Hello) -> Result<Peer, String> + Send + Sync + 'static) -> Self {
        self.authorize = Some(Box::new(f));
        self
    }
    /// Start from a snapshot; then `Engine::replay` the log entries after it.
    pub fn snapshot(mut self, s: Snapshot) -> Self {
        self.snapshot = Some(s);
        self
    }
    pub fn build(self) -> Engine {
        let mut e = Engine {
            clock: Clock::new(self.cfg.max_drift_ms),
            cfg: self.cfg,
            domains: self.domains,
            store: self.store.unwrap_or_else(|| Box::new(MemStore::new())),
            log: self.log.unwrap_or_else(|| Box::new(MemLog::new())),
            head_seq: 0,
            head_hash: ZERO_HASH.to_string(),
            conns: HashMap::new(),
            sessions: HashMap::new(),
            scopes: HashMap::new(),
            authorize: self.authorize,
        };
        if let Some(s) = self.snapshot {
            e.head_seq = s.seq;
            e.head_hash = s.hash.clone();
            e.clock.floor(s.hlc);
            for (scope, key, st) in s.keys {
                *e.store.get_mut(&scope, &key) = st;
            }
            for (id, loc) in s.ops {
                e.store.put_op(&id, loc);
            }
        }
        e
    }
}

/// The hash a key moves to when `op` lands on it.
fn key_hash(prev: &str, op: &Op, from: Option<&str>) -> String {
    let payload = op.payload.as_deref().map(|p| sha256_hex(&[p.as_bytes()])).unwrap_or_default();
    sha256_hex(&[prev.as_bytes(), op.op_id.as_bytes(), op.kind.as_bytes(), payload.as_bytes(), from.unwrap_or("").as_bytes()])
}

fn valid_op_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'@' | b'-'))
}

struct Out(Vec<Outgoing>);

impl Out {
    fn conn(&mut self, conn: ConnId, frame: ServerFrame) {
        self.0.push(Outgoing { conn, frame });
    }
}

fn error(code: &str, message: impl Into<String>, fatal: bool, req: Option<String>) -> ServerFrame {
    ServerFrame::Error(ErrorFrame { code: code.into(), message: message.into(), fatal, req })
}

impl Engine {
    pub fn builder() -> EngineBuilder {
        EngineBuilder { cfg: Config::default(), domains: Vec::new(), store: None, log: None, authorize: None, snapshot: None }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The log head: (seq, hash) of the last appended entry.
    pub fn head(&self) -> (u64, &str) {
        (self.head_seq, &self.head_hash)
    }

    pub fn key_state(&self, scope: &str, key: &str) -> Option<&KeyState> {
        self.store.get(scope, key)
    }

    pub fn domains(&self) -> &[Arc<dyn Domain>] {
        &self.domains
    }

    /// Live (attached) sessions in a scope.
    pub fn sessions_in(&self, scope: &str) -> Vec<Peer> {
        let mut v: Vec<Peer> = self.sessions.values().filter(|s| s.scope == scope && s.conn.is_some()).map(|s| s.peer.clone()).collect();
        v.sort_by(|a, b| a.session.cmp(&b.session));
        v
    }

    /// The leases held in a scope: key, holder, kinds and intent, sorted by
    /// key. For dashboards and metrics.
    pub fn leases_in(&self, scope: &str) -> Vec<LeaseConflict> {
        let Some(rt) = self.scopes.get(scope) else { return Vec::new() };
        let mut v: Vec<LeaseConflict> = rt
            .leases
            .iter()
            .flat_map(|(k, ls)| {
                ls.iter().map(|l| LeaseConflict {
                    key: k.clone(),
                    holder: l.peer.clone(),
                    kinds: l.kinds.clone(),
                    intent: l.intent.clone(),
                })
            })
            .collect();
        v.sort_by(|a, b| (&a.key, &a.holder.session).cmp(&(&b.key, &b.holder.session)));
        v
    }

    /// Every key the store holds in a scope, with its head, sorted.
    pub fn keys_in(&self, scope: &str) -> Vec<(String, Head)> {
        let mut v = Vec::new();
        self.store.for_each_key(&mut |s, k, st| {
            if s == scope {
                v.push((k.to_string(), st.head()));
            }
        });
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// The scope and identity of a known session, and whether it is attached.
    pub fn session_info(&self, session: &str) -> Option<(String, Peer, bool)> {
        self.sessions.get(session).map(|s| (s.scope.clone(), s.peer.clone(), s.conn.is_some()))
    }

    /// A message from outside any session (an integrator's own sender: a
    /// person writing from a dashboard) to one attached session of `scope`.
    /// `None` when that session is not attached there, so the caller can fall
    /// back to its own mailbox. Not logged, like every message.
    pub fn deliver_message(&mut self, scope: &str, session: &str, from: Peer, text: String, now_ms: u64) -> Option<Vec<Outgoing>> {
        let s = self.sessions.get(session)?;
        if s.scope != scope {
            return None;
        }
        let conn = s.conn?;
        let hlc = self.clock.now(now_ms);
        let frame = ServerFrame::Message(Message { from, to: Some(session.to_string()), anchor: None, text, hlc });
        Some(vec![Outgoing { conn, frame }])
    }

    // ------------------------------------------------------------ transport events

    pub fn connected(&mut self, conn: ConnId, _now_ms: u64) -> Vec<Outgoing> {
        self.conns.insert(conn, None);
        Vec::new()
    }

    /// The connection is gone. Its session keeps leases and subscriptions for
    /// `grace_ms`; `tick` releases them after that.
    pub fn disconnected(&mut self, conn: ConnId, now_ms: u64) -> Vec<Outgoing> {
        if let Some(Some(sid)) = self.conns.remove(&conn) {
            if let Some(s) = self.sessions.get_mut(&sid) {
                if s.conn == Some(conn) {
                    s.conn = None;
                    s.detached_at = Some(now_ms);
                }
            }
        }
        Vec::new()
    }

    /// Release sessions whose grace ran out.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Outgoing> {
        let mut out = Out(Vec::new());
        let mut expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.detached_at.is_some_and(|t| t + self.cfg.grace_ms <= now_ms))
            .map(|(id, _)| id.clone())
            .collect();
        expired.sort();
        for sid in expired {
            self.drop_session(&sid, ReleaseReason::Expired, &mut out);
        }
        out.0
    }

    /// The earliest time `tick` has work to do.
    pub fn next_deadline(&self) -> Option<u64> {
        self.sessions.values().filter_map(|s| s.detached_at.map(|t| t + self.cfg.grace_ms)).min()
    }

    /// Decode a text frame and handle it; a frame that does not parse gets a
    /// non-fatal `error{code: "bad_frame"}`.
    pub fn handle_text(&mut self, conn: ConnId, text: &str, now_ms: u64) -> Vec<Outgoing> {
        match ClientFrame::decode(text) {
            Ok(f) => self.handle(conn, f, now_ms),
            Err(e) => vec![Outgoing { conn, frame: error("bad_frame", e.to_string(), false, None) }],
        }
    }

    pub fn handle(&mut self, conn: ConnId, frame: ClientFrame, now_ms: u64) -> Vec<Outgoing> {
        let mut out = Out(Vec::new());
        self.conns.entry(conn).or_insert(None);
        let frame = match frame {
            ClientFrame::Hello(h) => {
                self.on_hello(conn, h, now_ms, &mut out);
                return out.0;
            }
            f => f,
        };
        let Some(Some(sid)) = self.conns.get(&conn).cloned() else {
            out.conn(conn, error("hello_required", "send hello first", false, None));
            return out.0;
        };
        match frame {
            ClientFrame::Hello(_) => unreachable!(),
            ClientFrame::Op(op) => self.on_op(&sid, op, now_ms, &mut out),
            ClientFrame::Batch(b) => self.on_batch(&sid, b, now_ms, &mut out),
            ClientFrame::Lease(l) => self.on_lease(&sid, l, &mut out),
            ClientFrame::Release(r) => self.on_release(&sid, r, &mut out),
            ClientFrame::Subscribe(s) => self.on_subscribe(&sid, s, &mut out),
            ClientFrame::Unsubscribe(u) => self.on_unsubscribe(&sid, u),
            ClientFrame::Check(c) => self.on_check(&sid, c, &mut out),
            ClientFrame::Promote(p) => self.on_promote(&sid, p, now_ms, &mut out),
            ClientFrame::Message(m) => self.on_message(&sid, m, now_ms, &mut out),
        }
        out.0
    }

    // ------------------------------------------------------------ helpers

    fn send(&self, sid: &str, frame: ServerFrame, out: &mut Out) {
        if let Some(conn) = self.sessions.get(sid).and_then(|s| s.conn) {
            out.conn(conn, frame);
        }
    }

    fn domain_for(&self, key: &str) -> Option<Arc<dyn Domain>> {
        let key = key.strip_suffix('*').unwrap_or(key);
        self.domains.iter().find(|d| d.owns(key)).cloned()
    }

    fn normalize(&self, key: &str) -> Result<(String, Arc<dyn Domain>), String> {
        let d = self.domain_for(key).ok_or_else(|| format!("no domain owns key `{key}`"))?;
        if key.len() > 1024 {
            return Err("key longer than 1024 bytes".into());
        }
        Ok((d.normalize_key(key)?, d))
    }

    fn head_of(&self, scope: &str, key: &str) -> Head {
        self.store.get(scope, key).map(|s| s.head()).unwrap_or_default()
    }

    fn scope_of(&self, sid: &str) -> String {
        self.sessions.get(sid).map(|s| s.scope.clone()).unwrap_or_default()
    }

    fn peer_of(&self, sid: &str) -> Peer {
        self.sessions.get(sid).map(|s| s.peer.clone()).unwrap_or_default()
    }

    /// Another session's lease on `key` that `kind` would conflict with.
    fn lease_conflict(&self, scope: &str, sid: &str, key: &str, kind: Option<&str>, d: &dyn Domain) -> Option<&LeaseEntry> {
        let rt = self.scopes.get(scope)?;
        rt.leases.get(key)?.iter().find(|l| {
            l.session != sid
                && match kind {
                    None => true,
                    Some(k) => l.kinds.iter().any(|lk| !d.table().compatible(k, lk)),
                }
        })
    }

    fn lease_intent(&self, scope: &str, session: &str, key: &str) -> Option<Intent> {
        let rt = self.scopes.get(scope)?;
        if let Some(l) = rt.leases.get(key).and_then(|v| v.iter().find(|l| l.session == session)) {
            return l.intent.clone();
        }
        let s = self.sessions.get(session)?;
        s.leases.iter().find_map(|k| rt.leases.get(k)?.iter().find(|l| l.session == session)?.intent.clone())
    }

    // ------------------------------------------------------------ hello

    fn on_hello(&mut self, conn: ConnId, h: Hello, now: u64, out: &mut Out) {
        if h.proto != PROTO_VERSION {
            out.conn(conn, error("proto", format!("server speaks proto {PROTO_VERSION}, got {}", h.proto), true, None));
            return;
        }
        if h.session.is_empty() || h.scope.is_empty() || h.session.len() > 256 || h.scope.len() > 256 {
            out.conn(conn, error("bad_hello", "session and scope are required (at most 256 bytes)", true, None));
            return;
        }
        if matches!(self.conns.get(&conn), Some(Some(_))) {
            out.conn(conn, error("already_hello", "this connection already said hello", false, None));
            return;
        }
        let mut peer = match &self.authorize {
            Some(f) => match f(&h) {
                Ok(p) => p,
                Err(m) => {
                    out.conn(conn, error("unauthorized", m, true, None));
                    return;
                }
            },
            None => Peer { session: String::new(), agent_id: h.agent_id.clone(), person: h.person.clone() },
        };
        peer.session = h.session.clone();

        let mut resumed = false;
        if let Some(existing) = self.sessions.get(&h.session) {
            if h.resume && (existing.scope != h.scope || existing.peer.person != peer.person) {
                out.conn(conn, error("scope_mismatch", "that session belongs to another scope or person", true, None));
                return;
            }
            if let Some(old) = existing.conn {
                if old != conn {
                    out.conn(old, error("superseded", "another connection resumed this session", true, None));
                    self.conns.insert(old, None);
                }
            }
            if h.resume && existing.scope == h.scope && existing.peer.person == peer.person {
                resumed = true;
            } else {
                self.drop_session(&h.session, ReleaseReason::Superseded, out);
            }
        }
        if !resumed {
            self.sessions.insert(
                h.session.clone(),
                Session {
                    peer: peer.clone(),
                    scope: h.scope.clone(),
                    conn: None,
                    detached_at: None,
                    leases: BTreeSet::new(),
                    subs: BTreeMap::new(),
                },
            );
            self.scopes.entry(h.scope.clone()).or_default();
        }
        let s = self.sessions.get_mut(&h.session).unwrap();
        s.conn = Some(conn);
        s.detached_at = None;
        s.peer.agent_id = peer.agent_id.clone();
        self.conns.insert(conn, Some(h.session.clone()));

        let (resumed_leases, resumed_subscriptions) = if resumed {
            let s = &self.sessions[&h.session];
            let rt = self.scopes.get(&h.scope);
            let leases = s
                .leases
                .iter()
                .filter_map(|k| {
                    let l = rt?.leases.get(k)?.iter().find(|l| l.session == h.session)?;
                    Some(LeaseInfo { key: k.clone(), kinds: l.kinds.clone(), intent: l.intent.clone(), head: self.head_of(&h.scope, k) })
                })
                .collect();
            (leases, s.subs.keys().cloned().collect())
        } else {
            (Vec::new(), Vec::new())
        };
        let hlc = self.clock.now(now);
        out.conn(
            conn,
            ServerFrame::Welcome(Welcome {
                proto: PROTO_VERSION,
                session: h.session,
                resumed,
                hlc,
                grace_ms: self.cfg.grace_ms,
                resumed_leases,
                resumed_subscriptions,
                domains: self.domains.iter().map(|d| d.info()).collect(),
            }),
        );
    }

    /// Forget a session: release its leases (telling waiters and
    /// subscribers) and drop its subscriptions.
    fn drop_session(&mut self, sid: &str, reason: ReleaseReason, out: &mut Out) {
        let Some(s) = self.sessions.get(sid) else { return };
        let keys: Vec<String> = s.leases.iter().cloned().collect();
        let subs: Vec<String> = s.subs.keys().cloned().collect();
        let scope = s.scope.clone();
        if !keys.is_empty() {
            self.release_keys(sid, &scope, &keys, reason, None, out);
        }
        self.unsubscribe_keys(sid, &scope, &subs);
        if let Some(rt) = self.scopes.get_mut(&scope) {
            for w in rt.waiters.values_mut() {
                w.remove(sid);
            }
            rt.waiters.retain(|_, w| !w.is_empty());
        }
        self.sessions.remove(sid);
    }

    // ------------------------------------------------------------ ops

    fn validate(&self, op: &mut Op, sid: &str) -> Result<(Arc<dyn Domain>, Option<String>), String> {
        if !valid_op_id(&op.op_id) {
            return Err("op_id must be 1-128 of [A-Za-z0-9._:@-]".into());
        }
        let (key, d) = self.normalize(&op.key)?;
        op.key = key;
        let t = d.table();
        let Some(spec) = t.kind(&op.kind) else {
            return Err(format!("domain `{}` has no kind `{}`", t.domain, op.kind));
        };
        if !t.allows(&op.kind, &op.key) {
            return Err(format!("kind `{}` does not apply to `{}`", op.kind, op.key));
        }
        for f in &spec.public {
            if op.public.get(f).is_none_or(|v| v.is_null()) {
                return Err(format!("kind `{}` needs public field `{f}`", op.kind));
            }
        }
        if op.payload.as_ref().is_some_and(|p| p.len() > self.cfg.max_payload_bytes) {
            return Err(format!("payload over {} bytes", self.cfg.max_payload_bytes));
        }
        let moves_to = if spec.moves_key {
            match d.rekey_target(&op.key, op)? {
                Some(t) if t == op.key => return Err("the op moves the key onto itself".into()),
                Some(t) => Some(self.normalize(&t)?.0),
                None => return Err(format!("kind `{}` moves the key but named no target", op.kind)),
            }
        } else {
            None
        };
        op.author = self.peer_of(sid);
        Ok((d, moves_to))
    }

    /// For a base named by content or hash alone that the server has never
    /// seen on a key with history: the other sessions' unpromoted ops there
    /// (what the writer's copy may be missing). `None` for every other base.
    fn unknown_base(&self, scope: &str, sid: &str, key: &str, base: &Base) -> Option<Vec<TheirOp>> {
        KeyState::named_hash(base)?;
        let st = self.store.get(scope, key)?;
        match st.base_start(base) {
            Some(_) => None,
            None => Some(st.history.iter().filter(|e| e.author.session != sid && !e.promoted).map(|e| e.their_op(key)).collect()),
        }
    }

    /// The accept/rebase/reject walk (SPEC.md, "Resolving an op"). Pure.
    fn resolve(&self, scope: &str, sid: &str, kind: &str, key: &str, base: &Base, d: &dyn Domain) -> Result<Resolved, Failure> {
        let table = d.table();
        let mut key = key.to_string();
        // a base named by content or hash alone was resolved by `start_of`
        let named = KeyState::named_hash(base).is_some();
        let mut from = if named { self.store.get(scope, &key).and_then(|st| st.base_start(base)).unwrap_or(0) } else { base.version };
        let mut check_hash = !base.hash.is_empty() && !named;
        let mut rebased = Vec::new();
        for _hop in 0..32 {
            let empty = KeyState::default();
            let st = self.store.get(scope, &key).unwrap_or(&empty);
            if from > st.version {
                return Err(Failure::Invalid(format!("base version {from} is ahead of `{key}` (version {})", st.version)));
            }
            if check_hash {
                if let Some(h) = st.hash_at(from) {
                    if h != base.hash {
                        return Err(Failure::Invalid(format!("base hash does not match `{key}` at version {from}")));
                    }
                }
                check_hash = false;
            }
            if !st.covers(from) {
                let theirs = st.history.iter().filter(|e| e.author.session != sid).map(|e| e.their_op(&key)).collect();
                return Err(Failure::Conflict { key, theirs });
            }
            let mut follow: Option<MovedTo> = None;
            let mut conflicts = Vec::new();
            let mut walked_move = false;
            for e in st.history.iter().filter(|e| e.version > from) {
                if e.moved_to.is_some() {
                    walked_move = true;
                }
                if e.arrived_from.is_some() && e.author.session != sid {
                    // a subject moved onto this key since the base: whatever
                    // the kinds, the writer must look before writing here
                    conflicts.push(e.their_op(&key));
                    continue;
                }
                if e.author.session == sid {
                    // pipelined ops of the writer's own: never a conflict, and
                    // its own rename is always followed
                    if let Some(m) = &e.moved_to {
                        if conflicts.is_empty() {
                            follow = Some(m.clone());
                            break;
                        }
                    }
                    continue;
                }
                let rel = table.relation(kind, &e.kind).unwrap_or(Relation::Conflict);
                match (rel, &e.moved_to) {
                    (Relation::Commute, _) => rebased.push(e.their_op(&key)),
                    (Relation::Rekey, Some(m)) if conflicts.is_empty() => {
                        follow = Some(m.clone());
                        break;
                    }
                    _ => conflicts.push(e.their_op(&key)),
                }
            }
            if !conflicts.is_empty() {
                return Err(Failure::Conflict { key, theirs: conflicts });
            }
            if follow.is_none() && !walked_move {
                // the writer saw the move already but still names the old key
                if let (Some(m), Some(e)) = (&st.moved_to, st.history.iter().rev().find(|e| e.moved_to.is_some())) {
                    let rel =
                        if e.author.session == sid { Relation::Rekey } else { table.relation(kind, &e.kind).unwrap_or(Relation::Conflict) };
                    match rel {
                        Relation::Rekey => follow = Some(m.clone()),
                        Relation::Commute => {}
                        Relation::Conflict => return Err(Failure::Conflict { key: key.clone(), theirs: vec![e.their_op(&key)] }),
                    }
                }
            }
            match follow {
                Some(m) => {
                    key = m.key;
                    from = m.version;
                }
                None => return Ok(Resolved { final_key: key, rebased }),
            }
        }
        Err(Failure::Invalid("rekey chain longer than 32 hops".into()))
    }

    /// The ack for an op whose result the key already holds (SPEC §5.2): its
    /// content hash is the head's, or for a landed op any kept version's
    /// (the head descends from it). Never for an op that moves its key.
    fn already(&self, scope: &str, op: &Op, moves: bool) -> Option<Ack> {
        let content = op.content_hash.as_deref().filter(|c| !c.is_empty())?;
        let st = self.store.get(scope, &op.key)?;
        if moves || st.version == 0 || st.moved_to.is_some() {
            return None;
        }
        let head = st.history.back().filter(|e| e.version == st.version)?;
        let holds = |e: &HistoryEntry| e.content_hash.as_deref() == Some(content) && e.moved_to.is_none();
        let same = if holds(head) { head } else if is_landed(&op.op_id) { st.history.iter().rev().find(|e| holds(e))? } else { return None };
        Some(Ack {
            op_id: op.op_id.clone(),
            key: op.key.clone(),
            version: st.version,
            hash: st.hash.clone(),
            hlc: head.hlc,
            seq: self.store.op(&head.op_id).map(|l| l.ack.seq).unwrap_or(self.head_seq),
            batch_id: None,
            rebased_over: vec![],
            rekeyed_to: None,
            moved_to: None,
            same_as: Some(same.op_id.clone()),
        })
    }

    /// Validate and resolve one op without applying anything.
    fn prepare(&self, sid: &str, scope: &str, mut op: Op) -> Result<Decision, Box<Reject>> {
        let named_key = op.key.clone();
        let reject = |op: &Op, reason, current, msg: String| {
            Box::new(Reject {
                op_id: op.op_id.clone(),
                key: op.key.clone(),
                reason,
                current,
                theirs: vec![],
                their_intent: None,
                holder: None,
                batch_id: None,
                message: msg,
            })
        };
        let (d, moves_to) = match self.validate(&mut op, sid) {
            Ok(x) => x,
            Err(m) => return Err(reject(&op, RejectReason::Invalid, Head::default(), m)),
        };
        // the same result twice is not a conflict, and not news
        if let Some(ack) = self.already(scope, &op, moves_to.is_some()) {
            return Ok(Decision::Already(ack));
        }
        let landed = is_landed(&op.op_id);
        if op.leased && !landed {
            let holds = self.scopes.get(scope).and_then(|rt| rt.leases.get(&op.key)).is_some_and(|v| v.iter().any(|l| l.session == sid));
            if !holds {
                let cur = self.head_of(scope, &op.key);
                return Err(reject(&op, RejectReason::StaleLease, cur, "you no longer hold a lease on this key".into()));
            }
        }
        if let Some(theirs) = self.unknown_base(scope, sid, &op.key, &op.base).filter(|_| !landed) {
            let holder = theirs.first().map(|t| t.author.clone());
            return Err(Box::new(Reject {
                op_id: op.op_id.clone(),
                key: op.key.clone(),
                reason: RejectReason::UnknownBase,
                current: self.head_of(scope, &op.key),
                theirs,
                their_intent: None,
                holder,
                batch_id: None,
                message: format!("no version of `{}` the server keeps has the content your base names", op.key),
            }));
        }
        let resolved = match self.resolve(scope, sid, &op.kind, &op.key, &op.base, &*d) {
            Ok(r) => r,
            Err(Failure::Invalid(m)) => {
                let cur = self.head_of(scope, &op.key);
                return Err(reject(&op, RejectReason::Invalid, cur, m));
            }
            // history that landed is the truth: it goes over the drafts it
            // meets, whose authors hear it as a delta
            Err(Failure::Conflict { key, .. }) if landed => Resolved { final_key: key, rebased: vec![] },
            Err(Failure::Conflict { key, theirs }) => {
                let current = self.head_of(scope, &key);
                let first = theirs.first().map(|t| t.author.clone());
                let their_intent = theirs.first().and_then(|t| {
                    let text = self.lease_intent(scope, &t.author.session, &key).and_then(|i| i.text);
                    let i = Intent { id: t.intent_id.clone(), text };
                    (!i.is_empty()).then_some(i)
                });
                let n = theirs.len();
                return Err(Box::new(Reject {
                    op_id: op.op_id.clone(),
                    key,
                    reason: RejectReason::Conflict,
                    current,
                    theirs,
                    their_intent,
                    holder: first,
                    batch_id: None,
                    message: format!("{n} op(s) since your base conflict with `{}`", op.kind),
                }));
            }
        };
        let mut lease_keys = vec![resolved.final_key.clone()];
        if let Some(t) = &moves_to {
            lease_keys.push(t.clone());
        }
        for k in lease_keys.iter().filter(|_| !landed) {
            if let Some(l) = self.lease_conflict(scope, sid, k, Some(&op.kind), &*d) {
                return Err(Box::new(Reject {
                    op_id: op.op_id.clone(),
                    key: k.clone(),
                    reason: RejectReason::Leased,
                    current: self.head_of(scope, k),
                    theirs: vec![],
                    their_intent: l.intent.clone(),
                    holder: Some(l.peer.clone()),
                    batch_id: None,
                    message: format!("{} holds a lease on this key for {}", l.peer.agent_id, l.kinds.join(", ")),
                }));
            }
        }
        // a rename/move resolved onto another key moves from there
        let moves_to = match moves_to {
            Some(_) if resolved.final_key != op.key => {
                let mut moved = op.clone();
                moved.key = resolved.final_key.clone();
                d.rekey_target(&resolved.final_key, &moved).ok().flatten().and_then(|t| self.normalize(&t).ok().map(|x| x.0))
            }
            m => m,
        };
        Ok(Decision::Apply(Prepared { op, named_key, resolved, moves_to }))
    }

    fn on_op(&mut self, sid: &str, op: Op, now: u64, out: &mut Out) {
        if let Some(loc) = self.store.op(&op.op_id) {
            let ack = loc.ack.clone();
            self.send(sid, ServerFrame::Ack(ack), out);
            return;
        }
        let scope = self.scope_of(sid);
        self.clock.observe(op.hlc, now);
        match self.prepare(sid, &scope, op) {
            Err(r) => self.send(sid, ServerFrame::Reject(*r), out),
            Ok(Decision::Already(ack)) => self.send(sid, ServerFrame::Ack(ack), out),
            Ok(Decision::Apply(p)) => self.commit(sid, &scope, vec![p], None, now, out),
        }
    }

    fn on_batch(&mut self, sid: &str, b: Batch, now: u64, out: &mut Out) {
        let scope = self.scope_of(sid);
        if b.ops.is_empty() || b.ops.len() > self.cfg.max_batch_ops || !valid_op_id(&b.batch_id) {
            self.send(
                sid,
                error("invalid", format!("a batch needs a valid batch_id and 1-{} ops", self.cfg.max_batch_ops), false, None),
                out,
            );
            return;
        }
        if b.ops.iter().all(|o| self.store.op(&o.op_id).is_some()) {
            let acks: Vec<Ack> = b.ops.iter().map(|o| self.store.op(&o.op_id).unwrap().ack.clone()).collect();
            for a in acks {
                self.send(sid, ServerFrame::Ack(a), out);
            }
            return;
        }
        for o in &b.ops {
            self.clock.observe(o.hlc, now);
        }
        if !b.atomic {
            for mut op in b.ops {
                if self.store.op(&op.op_id).is_some() {
                    self.on_op(sid, op, now, out);
                    continue;
                }
                let batch_id = Some(b.batch_id.clone());
                match self.prepare(sid, &scope, std::mem::take(&mut op)) {
                    Err(mut r) => {
                        r.batch_id = batch_id;
                        self.send(sid, ServerFrame::Reject(*r), out)
                    }
                    Ok(Decision::Already(mut ack)) => {
                        ack.batch_id = batch_id;
                        self.send(sid, ServerFrame::Ack(ack), out)
                    }
                    Ok(Decision::Apply(p)) => self.commit(sid, &scope, vec![p], batch_id, now, out),
                }
            }
            return;
        }
        // phase 1: check every op against the current state, nothing applied
        let mut prepared = Vec::new();
        let mut already: Vec<Ack> = Vec::new();
        let mut failed: Vec<Reject> = Vec::new();
        let mut touched: BTreeSet<String> = BTreeSet::new();
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for op in &b.ops {
            let id = op.op_id.clone();
            let key = op.key.clone();
            match self.prepare(sid, &scope, op.clone()) {
                Err(r) => failed.push(*r),
                Ok(Decision::Already(ack)) if ids.insert(id.clone()) && touched.insert(ack.key.clone()) => already.push(ack),
                Ok(Decision::Already(ack)) => failed.push(Reject {
                    op_id: id,
                    key,
                    reason: RejectReason::Invalid,
                    current: self.head_of(&scope, &ack.key),
                    theirs: vec![],
                    their_intent: None,
                    holder: None,
                    batch_id: None,
                    message: "an atomic batch touches each key and op_id once".into(),
                }),
                Ok(Decision::Apply(p)) => {
                    let mut keys = vec![p.resolved.final_key.clone()];
                    keys.extend(p.moves_to.clone());
                    let dup = !ids.insert(id.clone()) || keys.iter().any(|k| !touched.insert(k.clone()));
                    if dup {
                        failed.push(Reject {
                            op_id: id,
                            key,
                            reason: RejectReason::Invalid,
                            current: self.head_of(&scope, &p.resolved.final_key),
                            theirs: vec![],
                            their_intent: None,
                            holder: None,
                            batch_id: None,
                            message: "an atomic batch touches each key and op_id once".into(),
                        });
                    } else {
                        prepared.push(p);
                    }
                }
            }
        }
        if !failed.is_empty() {
            let failed_ids: BTreeSet<String> = failed.iter().map(|r| r.op_id.clone()).collect();
            for mut r in failed {
                r.batch_id = Some(b.batch_id.clone());
                self.send(sid, ServerFrame::Reject(r), out);
            }
            let aborted = prepared
                .into_iter()
                .filter(|p| !failed_ids.contains(&p.op.op_id))
                .map(|p| (p.op.op_id.clone(), p.named_key.clone(), p.resolved.final_key.clone()))
                .chain(already.into_iter().filter(|a| !failed_ids.contains(&a.op_id)).map(|a| (a.op_id, a.key.clone(), a.key)));
            let aborted: Vec<(String, String, String)> = aborted.collect();
            for (op_id, named, landed_on) in aborted {
                let r = Reject {
                    op_id,
                    key: named,
                    reason: RejectReason::BatchAborted,
                    current: self.head_of(&scope, &landed_on),
                    theirs: vec![],
                    their_intent: None,
                    holder: None,
                    batch_id: Some(b.batch_id.clone()),
                    message: "another op in this atomic batch was rejected; nothing was applied".into(),
                };
                self.send(sid, ServerFrame::Reject(r), out);
            }
            return;
        }
        // phase 2: apply all, as one log entry; what the keys already held is acked as it is
        for mut ack in already {
            ack.batch_id = Some(b.batch_id.clone());
            self.send(sid, ServerFrame::Ack(ack), out);
        }
        if !prepared.is_empty() {
            self.commit(sid, &scope, prepared, Some(b.batch_id), now, out);
        }
    }

    /// Append, apply, ack, push deltas. Every prepared op lands or none does.
    fn commit(&mut self, sid: &str, scope: &str, items: Vec<Prepared>, batch_id: Option<String>, now: u64, out: &mut Out) {
        let mut accepted = Vec::with_capacity(items.len());
        let mut acks = Vec::with_capacity(items.len());
        let hlc_entry = self.clock.now(now);
        for (i, p) in items.into_iter().enumerate() {
            let mut op = p.op;
            op.hlc = if i == 0 { hlc_entry } else { self.clock.now(now) };
            let final_key = p.resolved.final_key.clone();
            let prev = self.head_of(scope, &final_key);
            let rekeyed_from = (final_key != op.key).then(|| op.key.clone());
            op.key = final_key.clone();
            let version = prev.version + 1;
            let hash = key_hash(&prev.hash, &op, None);
            let moved_to = p.moves_to.as_ref().map(|t| {
                let tst = self.head_of(scope, t);
                MovedTo { key: t.clone(), version: tst.version + 1, hash: key_hash(&tst.hash, &op, Some(&final_key)) }
            });
            acks.push(Ack {
                op_id: op.op_id.clone(),
                key: final_key.clone(),
                version,
                hash: hash.clone(),
                hlc: op.hlc,
                seq: 0,
                batch_id: batch_id.clone(),
                rebased_over: p.resolved.rebased.clone(),
                rekeyed_to: rekeyed_from.as_ref().map(|_| final_key.clone()),
                moved_to: moved_to.clone(),
                same_as: None,
            });
            accepted.push(Accepted {
                scope: scope.to_string(),
                op,
                version,
                hash,
                moved_to,
                rekeyed_from,
                rebased_over: p.resolved.rebased.iter().map(|t| t.op_id.clone()).collect(),
                batch_id: batch_id.clone(),
            });
        }
        let record = match (&batch_id, accepted.len()) {
            (Some(bid), n) if n > 1 => LogRecord::Batch { batch_id: bid.clone(), accepted: accepted.clone() },
            _ => LogRecord::Accept(Box::new(accepted[0].clone())),
        };
        let seq = match self.append(record, hlc_entry) {
            Ok(seq) => seq,
            Err(e) => {
                for a in &accepted {
                    self.send(
                        sid,
                        error("log", format!("not accepted, the log refused it: {}", e.0), false, Some(a.op.op_id.clone())),
                        out,
                    );
                }
                return;
            }
        };
        let person = self.peer_of(sid).person;
        for (a, mut ack) in accepted.iter().zip(acks) {
            ack.seq = seq;
            self.apply_accepted(a, &person, ack.clone());
            self.move_runtime(scope, a);
            self.send(sid, ServerFrame::Ack(ack), out);
        }
        self.maybe_snapshot();
        for a in &accepted {
            let mut touched = vec![a.op.key.clone()];
            if let Some(m) = &a.moved_to {
                touched.push(m.key.clone());
            }
            let delta = Delta {
                op: a.op.clone(),
                version: a.version,
                hash: a.hash.clone(),
                seq,
                layer: if is_landed(&a.op.op_id) { Layer::Promoted } else { Layer::Draft },
                via: None,
                moved_to: a.moved_to.clone(),
            };
            for (to, via) in self.recipients(scope, &touched, sid) {
                let mut d = delta.clone();
                d.via = via;
                self.send(&to, ServerFrame::Delta(d), out);
            }
        }
    }

    fn append(&mut self, record: LogRecord, hlc: Hlc) -> Result<u64, LogError> {
        let seq = self.head_seq + 1;
        let hash = entry_hash(&self.head_hash, seq, hlc, &record);
        let entry = LogEntry { seq, prev: self.head_hash.clone(), hash: hash.clone(), hlc, record };
        self.log.append(&entry)?;
        self.head_seq = seq;
        self.head_hash = hash;
        Ok(seq)
    }

    /// After an entry is applied: snapshot if the log asks for one now.
    fn maybe_snapshot(&mut self) {
        let seq = self.head_seq;
        if self.log.snapshot_every().is_some_and(|n| n > 0 && seq.is_multiple_of(n)) {
            let snap = self.snapshot();
            // a failed snapshot is not a failed write: the log already has it
            let _ = self.log.snapshot(&snap);
        }
    }

    /// Apply an accepted op to the store (live and in replay).
    fn apply_accepted(&mut self, a: &Accepted, person: &str, ack: Ack) {
        let limit = self.cfg.history_per_key.max(1);
        let entry = HistoryEntry {
            version: a.version,
            hash: a.hash.clone(),
            op_id: a.op.op_id.clone(),
            kind: a.op.kind.clone(),
            public: a.op.public.clone(),
            payload: a.op.payload.clone(),
            author: a.op.author.clone(),
            intent_id: a.op.intent_id.clone(),
            hlc: a.op.hlc,
            // what landed on the shared branch is proven already
            promoted: is_landed(&a.op.op_id),
            moved_to: a.moved_to.clone(),
            content_hash: a.op.content_hash.clone(),
            arrived_from: None,
        };
        let st = self.store.get_mut(&a.scope, &a.op.key);
        // the first op on a key, based on content: that content is version 0
        if st.version == 0 && a.rekeyed_from.is_none() && st.content0.is_none() {
            st.content0 = KeyState::named_hash(&a.op.base).map(str::to_string);
        }
        st.version = a.version;
        st.hash = a.hash.clone();
        st.moved_to = a.moved_to.clone();
        if entry.promoted {
            st.promoted_version = st.promoted_version.max(a.version);
        }
        st.history.push_back(entry.clone());
        while st.history.len() > limit {
            st.history.pop_front();
        }
        if let Some(m) = &a.moved_to {
            let tst = self.store.get_mut(&a.scope, &m.key);
            tst.version = m.version;
            tst.hash = m.hash.clone();
            tst.moved_to = None;
            tst.history.push_back(HistoryEntry {
                version: m.version,
                hash: m.hash.clone(),
                moved_to: None,
                arrived_from: Some(a.op.key.clone()),
                ..entry
            });
            while tst.history.len() > limit {
                tst.history.pop_front();
            }
        }
        self.store.put_op(
            &a.op.op_id,
            OpLoc { scope: a.scope.clone(), key: a.op.key.clone(), version: a.version, person: person.to_string(), ack },
        );
        if let Some(d) = self.domain_for(&a.op.key) {
            d.observe(&a.scope, &a.op.key, &a.op);
        }
    }

    /// After a rename/move: leases and subscriptions on the old key follow it.
    fn move_runtime(&mut self, scope: &str, a: &Accepted) {
        let Some(m) = &a.moved_to else { return };
        let from = &a.op.key;
        let Some(rt) = self.scopes.get_mut(scope) else { return };
        if let Some(ls) = rt.leases.remove(from) {
            for l in &ls {
                if let Some(s) = self.sessions.get_mut(&l.session) {
                    s.leases.remove(from);
                    s.leases.insert(m.key.clone());
                }
            }
            rt.leases.entry(m.key.clone()).or_default().extend(ls);
        }
        if let Some(subs) = rt.subs.get(from).cloned() {
            for (sid, depth) in subs {
                rt.subs.entry(m.key.clone()).or_default().entry(sid.clone()).or_insert(depth);
                if let Some(s) = self.sessions.get_mut(&sid) {
                    s.subs.entry(m.key.clone()).or_insert(depth);
                }
            }
        }
        if let Some(w) = rt.waiters.remove(from) {
            rt.waiters.entry(m.key.clone()).or_default().extend(w);
        }
    }

    /// Replay log entries after a snapshot (or from genesis) into the store.
    /// Entries must continue the chain from the engine's head.
    pub fn replay(&mut self, entries: &[LogEntry]) -> Result<(), crate::log::ChainError> {
        crate::log::verify_chain(entries, &self.head_hash.clone(), self.head_seq + 1)?;
        for e in entries {
            self.clock.floor(e.hlc);
            let accepted: Vec<&Accepted> = match &e.record {
                LogRecord::Accept(a) => vec![&**a],
                LogRecord::Batch { accepted, .. } => accepted.iter().collect(),
                LogRecord::Promote { scope, op_ids, keys, .. } => {
                    self.apply_promote(scope, op_ids, keys);
                    vec![]
                }
            };
            for a in accepted {
                self.clock.floor(a.op.hlc);
                let ack = Ack {
                    op_id: a.op.op_id.clone(),
                    key: a.op.key.clone(),
                    version: a.version,
                    hash: a.hash.clone(),
                    hlc: a.op.hlc,
                    seq: e.seq,
                    batch_id: a.batch_id.clone(),
                    rebased_over: vec![],
                    rekeyed_to: a.rekeyed_from.as_ref().map(|_| a.op.key.clone()),
                    moved_to: a.moved_to.clone(),
                    same_as: None,
                };
                let person = a.op.author.person.clone();
                self.apply_accepted(a, &person, ack);
            }
            self.head_seq = e.seq;
            self.head_hash = e.hash.clone();
        }
        Ok(())
    }

    /// The engine's durable state at the log head.
    pub fn snapshot(&self) -> Snapshot {
        let mut keys = Vec::new();
        self.store.for_each_key(&mut |s, k, st| keys.push((s.to_string(), k.to_string(), st.clone())));
        keys.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        let mut ops = Vec::new();
        self.store.for_each_op(&mut |id, loc| ops.push((id.to_string(), loc.clone())));
        ops.sort_by(|a, b| a.0.cmp(&b.0));
        Snapshot { seq: self.head_seq, hash: self.head_hash.clone(), hlc: self.clock.last(), keys, ops }
    }

    /// Sessions to tell about a change to `touched`, each with the dependency
    /// path it came by (`None` = directly). Sorted, excluding the writer.
    fn recipients(&self, scope: &str, touched: &[String], exclude: &str) -> BTreeMap<String, Option<Vec<String>>> {
        let mut to: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
        let Some(rt) = self.scopes.get(scope) else { return to };
        let prefix_hits = |key: &str, min_depth: u32| -> Vec<String> {
            rt.prefix_subs
                .iter()
                .filter(|((p, _), d)| key.starts_with(p.as_str()) && **d >= min_depth)
                .map(|((_, s), _)| s.clone())
                .collect()
        };
        for k in touched {
            let mut direct: Vec<String> = rt.subs.get(k).map(|m| m.keys().cloned().collect()).unwrap_or_default();
            direct.extend(rt.leases.get(k).into_iter().flatten().map(|l| l.session.clone()));
            direct.extend(prefix_hits(k, 0));
            for s in direct {
                to.insert(s, None);
            }
        }
        if rt.deep_subs == 0 {
            to.remove(exclude);
            return to;
        }
        for k in touched {
            let Some(dom) = self.domain_for(k) else { continue };
            let Some(graph) = dom.graph() else { continue };
            let mut seen: BTreeSet<String> = BTreeSet::from([k.clone()]);
            let mut frontier: Vec<Vec<String>> = vec![vec![k.clone()]];
            for dist in 1..=self.cfg.max_depth {
                let mut next = Vec::new();
                for path in &frontier {
                    let node = path.last().unwrap();
                    for dep in graph.dependents(scope, node) {
                        if !seen.insert(dep.clone()) {
                            continue;
                        }
                        let mut p = path.clone();
                        p.push(dep.clone());
                        let mut hits: Vec<String> = rt
                            .subs
                            .get(&dep)
                            .map(|m| m.iter().filter(|(_, d)| **d >= dist).map(|(s, _)| s.clone()).collect())
                            .unwrap_or_default();
                        hits.extend(prefix_hits(&dep, dist));
                        for s in hits {
                            to.entry(s).or_insert_with(|| Some(p.iter().rev().cloned().collect()));
                        }
                        next.push(p);
                    }
                }
                if next.is_empty() {
                    break;
                }
                frontier = next;
            }
        }
        to.remove(exclude);
        to
    }

    // ------------------------------------------------------------ leases

    fn on_lease(&mut self, sid: &str, l: Lease, out: &mut Out) {
        let scope = self.scope_of(sid);
        let mut keys = Vec::new();
        let mut doms = Vec::new();
        for k in &l.keys {
            match self.normalize(k) {
                Ok((k, d)) => {
                    let bad = l.kinds.iter().find(|kind| d.table().kind(kind).is_none());
                    if let Some(bad) = bad {
                        self.send(sid, error("invalid", format!("domain `{}` has no kind `{bad}`", d.name()), false, l.req.clone()), out);
                        return;
                    }
                    keys.push(k);
                    doms.push(d);
                }
                Err(m) => {
                    self.send(sid, error("invalid", m, false, l.req.clone()), out);
                    return;
                }
            }
        }
        if keys.is_empty() {
            self.send(sid, error("invalid", "lease names no keys", false, l.req.clone()), out);
            return;
        }
        let mut conflicts = Vec::new();
        let rt = self.scopes.entry(scope.clone()).or_default();
        for (k, d) in keys.iter().zip(&doms) {
            let kinds = if l.kinds.is_empty() { d.table().kind_names() } else { l.kinds.clone() };
            for e in rt.leases.get(k).into_iter().flatten().filter(|e| e.session != sid) {
                if kinds.iter().any(|a| e.kinds.iter().any(|b| !d.table().compatible(a, b))) {
                    conflicts.push(LeaseConflict {
                        key: k.clone(),
                        holder: e.peer.clone(),
                        kinds: e.kinds.clone(),
                        intent: e.intent.clone(),
                    });
                }
            }
        }
        if !conflicts.is_empty() {
            for c in &conflicts {
                rt.waiters.entry(c.key.clone()).or_default().insert(sid.to_string());
            }
            let f = LeaseDenied { req: l.req, keys, holder: conflicts[0].holder.clone(), intent: conflicts[0].intent.clone(), conflicts };
            self.send(sid, ServerFrame::LeaseDenied(f), out);
            return;
        }
        let peer = self.peer_of(sid);
        let rt = self.scopes.get_mut(&scope).unwrap();
        let mut granted_kinds = l.kinds.clone();
        for (k, d) in keys.iter().zip(&doms) {
            let kinds = if l.kinds.is_empty() { d.table().kind_names() } else { l.kinds.clone() };
            granted_kinds = kinds.clone();
            let v = rt.leases.entry(k.clone()).or_default();
            v.retain(|e| e.session != sid);
            v.push(LeaseEntry { session: sid.to_string(), peer: peer.clone(), kinds, intent: l.intent.clone() });
            if let Some(w) = rt.waiters.get_mut(k) {
                w.remove(sid);
            }
        }
        rt.waiters.retain(|_, w| !w.is_empty());
        let s = self.sessions.get_mut(sid).unwrap();
        s.leases.extend(keys.iter().cloned());
        let heads = keys.iter().map(|k| self.head_of(&scope, k)).collect();
        self.send(sid, ServerFrame::LeaseGranted(LeaseGranted { req: l.req, keys, kinds: granted_kinds, intent: l.intent, heads }), out);
    }

    fn on_release(&mut self, sid: &str, r: Release, out: &mut Out) {
        let scope = self.scope_of(sid);
        let keys: Vec<String> = r.keys.iter().map(|k| self.normalize(k).map(|x| x.0).unwrap_or_else(|_| k.clone())).collect();
        self.release_keys(sid, &scope, &keys, ReleaseReason::Released, r.req, out);
    }

    /// Drop `sid`'s leases on `keys`; tell the holder (if attached), the
    /// sessions waiting on those keys and their subscribers.
    fn release_keys(&mut self, sid: &str, scope: &str, keys: &[String], reason: ReleaseReason, req: Option<String>, out: &mut Out) {
        let peer = self.peer_of(sid);
        let mut released = Vec::new();
        let mut notify: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if let Some(rt) = self.scopes.get_mut(scope) {
            for k in keys {
                let Some(v) = rt.leases.get_mut(k) else { continue };
                let before = v.len();
                v.retain(|e| e.session != sid);
                if v.len() == before {
                    continue;
                }
                if v.is_empty() {
                    rt.leases.remove(k);
                }
                released.push(k.clone());
                let mut who: BTreeSet<String> = rt.waiters.remove(k).unwrap_or_default();
                who.extend(rt.subs.get(k).into_iter().flat_map(|m| m.keys().cloned()));
                who.extend(rt.leases.get(k).into_iter().flatten().map(|l| l.session.clone()));
                who.remove(sid);
                for w in who {
                    notify.entry(w).or_default().push(k.clone());
                }
            }
        }
        if let Some(s) = self.sessions.get_mut(sid) {
            for k in &released {
                s.leases.remove(k);
            }
        }
        if reason == ReleaseReason::Released || !released.is_empty() {
            self.send(sid, ServerFrame::LeaseReleased(LeaseReleased { req, keys: released.clone(), reason, holder: peer.clone() }), out);
        }
        for (w, ks) in notify {
            self.send(&w, ServerFrame::LeaseReleased(LeaseReleased { req: None, keys: ks, reason, holder: peer.clone() }), out);
        }
    }

    // ------------------------------------------------------------ subscriptions

    fn on_subscribe(&mut self, sid: &str, s: Subscribe, out: &mut Out) {
        let scope = self.scope_of(sid);
        let depth = s.depth.min(self.cfg.max_depth);
        let mut keys = Vec::new();
        for k in &s.keys {
            if let Some(p) = k.strip_suffix('*') {
                if self.domain_for(p).is_none() && !p.is_empty() {
                    self.send(sid, error("invalid", format!("no domain owns `{k}`"), false, s.req.clone()), out);
                    return;
                }
                keys.push(k.clone());
            } else {
                match self.normalize(k) {
                    Ok((k, _)) => keys.push(k),
                    Err(m) => {
                        self.send(sid, error("invalid", m, false, s.req.clone()), out);
                        return;
                    }
                }
            }
        }
        let rt = self.scopes.entry(scope).or_default();
        let sess = self.sessions.get_mut(sid).unwrap();
        for k in keys {
            if let Some(old) = sess.subs.insert(k.clone(), depth) {
                if old > 0 {
                    rt.deep_subs -= 1;
                }
            }
            if depth > 0 {
                rt.deep_subs += 1;
            }
            match k.strip_suffix('*') {
                Some(p) => {
                    rt.prefix_subs.insert((p.to_string(), sid.to_string()), depth);
                }
                None => {
                    rt.subs.entry(k).or_default().insert(sid.to_string(), depth);
                }
            }
        }
    }

    fn on_unsubscribe(&mut self, sid: &str, u: Unsubscribe) {
        let scope = self.scope_of(sid);
        let keys: Vec<String> = u
            .keys
            .iter()
            .map(|k| if k.ends_with('*') { k.clone() } else { self.normalize(k).map(|x| x.0).unwrap_or_else(|_| k.clone()) })
            .collect();
        self.unsubscribe_keys(sid, &scope, &keys);
    }

    fn unsubscribe_keys(&mut self, sid: &str, scope: &str, keys: &[String]) {
        let (Some(rt), Some(sess)) = (self.scopes.get_mut(scope), self.sessions.get_mut(sid)) else { return };
        for k in keys {
            let Some(depth) = sess.subs.remove(k) else { continue };
            if depth > 0 {
                rt.deep_subs -= 1;
            }
            match k.strip_suffix('*') {
                Some(p) => {
                    rt.prefix_subs.remove(&(p.to_string(), sid.to_string()));
                }
                None => {
                    if let Some(m) = rt.subs.get_mut(k) {
                        m.remove(sid);
                        if m.is_empty() {
                            rt.subs.remove(k);
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ check

    /// Other sessions' ops on `key` after version `from`, oldest first, and
    /// on the keys a rename or move took the subject to; with the key the
    /// subject now lives on. At most a few hops and the kept history.
    fn missed_since(&self, scope: &str, sid: &str, key: &str, from: u64) -> (Vec<TheirOp>, String) {
        let mut out = Vec::new();
        let (mut key, mut from) = (key.to_string(), from);
        for _hop in 0..8 {
            let Some(st) = self.store.get(scope, &key) else { break };
            let mut next: Option<(String, u64)> = None;
            for e in st.history.iter().filter(|e| e.version > from && e.arrived_from.is_none()) {
                if e.author.session != sid {
                    out.push(e.their_op(&key));
                }
                if let Some(m) = &e.moved_to {
                    next = Some((m.key.clone(), m.version));
                    break;
                }
            }
            match next {
                Some((k, v)) if k != key => {
                    key = k;
                    from = v;
                }
                _ => break,
            }
        }
        (out, key)
    }

    fn on_check(&mut self, sid: &str, c: Check, out: &mut Out) {
        let scope = self.scope_of(sid);
        let mut results = Vec::with_capacity(c.keys.len());
        for ck in c.keys {
            let (key, d) = match self.normalize(&ck.key) {
                Ok(x) => x,
                Err(m) => {
                    self.send(sid, error("invalid", m, false, c.req.clone()), out);
                    return;
                }
            };
            let current = self.head_of(&scope, &key);
            let mut e = CheckEntry {
                key: key.clone(),
                status: CheckStatus::Ok,
                current: current.clone(),
                would: None,
                rekeyed_to: None,
                theirs: vec![],
                holder: None,
                intent: None,
            };
            let base = ck.base.clone().unwrap_or(Base { version: current.version, ..Default::default() });
            let unknown = self.unknown_base(&scope, sid, &key, &base);
            let from = self.store.get(&scope, &key).and_then(|st| st.base_start(&base)).unwrap_or(base.version);
            if unknown.is_none() && from < current.version {
                e.status = CheckStatus::Stale;
            }
            let mut lease_key = key.clone();
            if let (Some(kind), None) = (&ck.kind, &unknown) {
                match self.resolve(&scope, sid, kind, &key, &base, &*d) {
                    Ok(r) => {
                        e.would = Some(if r.final_key != key {
                            Would::Rekey
                        } else if !r.rebased.is_empty() {
                            Would::Rebase
                        } else {
                            Would::Accept
                        });
                        if r.final_key != key {
                            e.rekeyed_to = Some(r.final_key.clone());
                            e.status = CheckStatus::Stale;
                            lease_key = r.final_key.clone();
                        }
                        e.theirs = r.rebased;
                    }
                    Err(Failure::Conflict { theirs, .. }) => {
                        e.would = Some(Would::Conflict);
                        e.status = CheckStatus::Stale;
                        e.theirs = theirs;
                    }
                    Err(Failure::Invalid(_)) => {
                        e.would = Some(Would::Conflict);
                        e.status = CheckStatus::Stale;
                    }
                }
            }
            // a check without a kind is a reader's: what its copy missed, by
            // others, following a rename or move to where the subject lives
            if ck.kind.is_none() && unknown.is_none() && from < current.version {
                let (theirs, landed) = self.missed_since(&scope, sid, &key, from);
                e.theirs = theirs;
                if landed != key {
                    e.rekeyed_to = Some(landed);
                }
            }
            if let Some(theirs) = unknown {
                e.status = CheckStatus::UnknownBase;
                e.theirs = theirs;
            }
            if let Some(l) = self.lease_conflict(&scope, sid, &lease_key, ck.kind.as_deref(), &*d) {
                e.status = CheckStatus::Leased;
                e.holder = Some(l.peer.clone());
                e.intent = l.intent.clone();
            }
            results.push(e);
        }
        self.send(sid, ServerFrame::CheckResult(CheckResult { req: c.req, results }), out);
    }

    // ------------------------------------------------------------ promotion

    fn apply_promote(&mut self, scope: &str, op_ids: &[String], keys: &[PromotedKey]) {
        let ids: BTreeSet<&String> = op_ids.iter().collect();
        for pk in keys {
            let st = self.store.get_mut(scope, &pk.key);
            st.promoted_version = st.promoted_version.max(pk.promoted_version);
            for e in st.history.iter_mut() {
                if ids.contains(&e.op_id) {
                    e.promoted = true;
                }
            }
        }
    }

    fn on_promote(&mut self, sid: &str, p: Promote, now: u64, out: &mut Out) {
        let scope = self.scope_of(sid);
        let peer = self.peer_of(sid);
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        let mut per_key: BTreeMap<String, u64> = BTreeMap::new();
        for id in &p.op_ids {
            match self.store.op(id) {
                Some(loc) if loc.scope == scope && loc.person == peer.person => {
                    known.push(id.clone());
                    let v = per_key.entry(loc.key.clone()).or_insert(0);
                    *v = (*v).max(loc.version);
                }
                _ => unknown.push(id.clone()),
            }
        }
        let keys: Vec<PromotedKey> = per_key
            .into_iter()
            .map(|(key, v)| {
                let cur = self.store.get(&scope, &key).map(|s| s.promoted_version).unwrap_or(0);
                PromotedKey { key, promoted_version: cur.max(v) }
            })
            .collect();
        if !known.is_empty() {
            let hlc = self.clock.now(now);
            let rec = LogRecord::Promote {
                scope: scope.clone(),
                op_ids: known.clone(),
                keys: keys.clone(),
                evidence: p.evidence.clone(),
                by: peer.clone(),
            };
            let seq = match self.append(rec, hlc) {
                Ok(s) => s,
                Err(e) => {
                    self.send(sid, error("log", format!("not promoted, the log refused it: {}", e.0), false, p.req), out);
                    return;
                }
            };
            self.apply_promote(&scope, &known, &keys);
            self.maybe_snapshot();
            // subscribers see each op again, now in the promoted layer
            let ids: BTreeSet<&String> = known.iter().collect();
            for pk in &keys {
                let Some(st) = self.store.get(&scope, &pk.key) else { continue };
                let entries: Vec<HistoryEntry> = st.history.iter().filter(|e| ids.contains(&e.op_id)).cloned().collect();
                let recips = self.recipients(&scope, std::slice::from_ref(&pk.key), sid);
                for e in entries {
                    let op = Op {
                        op_id: e.op_id.clone(),
                        key: pk.key.clone(),
                        base: Base::default(),
                        kind: e.kind.clone(),
                        public: e.public.clone(),
                        payload: e.payload.clone(),
                        author: e.author.clone(),
                        intent_id: e.intent_id.clone(),
                        hlc: e.hlc,
                        content_hash: e.content_hash.clone(),
                        leased: false,
                    };
                    let d = Delta {
                        op,
                        version: e.version,
                        hash: e.hash.clone(),
                        seq,
                        layer: Layer::Promoted,
                        via: None,
                        moved_to: e.moved_to.clone(),
                    };
                    for (to, via) in &recips {
                        let mut d = d.clone();
                        d.via = via.clone();
                        self.send(to, ServerFrame::Delta(d), out);
                    }
                }
            }
        }
        let evidence = if p.evidence.is_null() { Value::Null } else { p.evidence };
        self.send(sid, ServerFrame::Promoted(Promoted { req: p.req, op_ids: known, keys, unknown, evidence }), out);
    }

    // ------------------------------------------------------------ messages

    fn on_message(&mut self, sid: &str, m: SendMessage, now: u64, out: &mut Out) {
        let scope = self.scope_of(sid);
        let from = self.peer_of(sid);
        let mut to: BTreeSet<String> = BTreeSet::new();
        let mut anchor = None;
        if let Some(a) = &m.anchor {
            let key = match self.normalize(a) {
                Ok((k, _)) => k,
                Err(e) => {
                    self.send(sid, error("invalid", e, false, m.req), out);
                    return;
                }
            };
            if let Some(rt) = self.scopes.get(&scope) {
                to.extend(rt.subs.get(&key).into_iter().flat_map(|s| s.keys().cloned()));
                to.extend(rt.leases.get(&key).into_iter().flatten().map(|l| l.session.clone()));
            }
            if let Some(st) = self.store.get(&scope, &key) {
                to.extend(st.history.iter().filter(|e| !e.promoted).map(|e| e.author.session.clone()));
            }
            to.retain(|s| self.sessions.get(s).is_some_and(|x| x.scope == scope));
            anchor = Some(key);
        }
        if let Some(t) = &m.to {
            let named: BTreeSet<String> = self
                .sessions
                .iter()
                .filter(|(id, s)| s.scope == scope && (*id == t || &s.peer.agent_id == t || &s.peer.person == t))
                .map(|(id, _)| id.clone())
                .collect();
            to = if anchor.is_some() { to.intersection(&named).cloned().collect() } else { named };
        }
        if m.to.is_none() && m.anchor.is_none() {
            to = self.sessions.iter().filter(|(_, s)| s.scope == scope).map(|(id, _)| id.clone()).collect();
        }
        to.remove(sid);
        to.retain(|s| self.sessions.get(s).is_some_and(|x| x.conn.is_some()));
        if to.is_empty() {
            self.send(sid, error("undelivered", "no live session matches", false, m.req), out);
            return;
        }
        let hlc = self.clock.now(now);
        let frame = ServerFrame::Message(Message { from, to: m.to, anchor, text: m.text, hlc });
        for s in to {
            self.send(&s, frame.clone(), out);
        }
    }
}
