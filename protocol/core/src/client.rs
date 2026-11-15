//! The client side, sans-IO: builds frames, tracks what each key's base is,
//! and optionally coalesces fast streams. A transport sends what it returns
//! and feeds it what arrives.
//!
//! Two views of every key are kept apart on purpose:
//! - `base(key)`: what this agent's content is built on. It advances on this
//!   agent's own acks, and on `adopt` when the agent has applied someone
//!   else's op locally. Ops are based on it.
//! - `head(key)`: the newest version the server has told us about (deltas,
//!   check results). Seeing a change is not having it.
//!
//! A stream whose client applies every remote op as it arrives (a shared
//! notes document) sets `auto_adopt` for its prefix, and deltas advance the
//! base too.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::frames::*;
use crate::hash::base64_encode;
use crate::hlc::Clock;

#[derive(Debug, Clone, Default)]
pub struct ClientConfig {
    pub session: String,
    pub agent_id: String,
    pub person: String,
    pub scope: String,
    pub token: Option<String>,
}

/// Collapses a later op into an earlier one on the same key. The default
/// keeps the earlier op's base and takes everything else from the later one.
pub type MergeFn = fn(earlier: &mut Op, later: Op);

pub fn merge_last_wins(earlier: &mut Op, later: Op) {
    let base = std::mem::take(&mut earlier.base);
    *earlier = later;
    earlier.base = base;
}

/// Buffers ops for `window_ms` and collapses same-key, same-kind ops into one.
/// An op of another kind on a buffered key flushes the buffered one first, so
/// order per key is kept.
#[derive(Debug, Clone)]
pub struct Coalescer {
    window_ms: u64,
    merge: MergeFn,
    buf: Vec<(u64, Op)>,
}

impl Coalescer {
    pub fn new(window_ms: u64) -> Self {
        Coalescer { window_ms, merge: merge_last_wins, buf: Vec::new() }
    }

    pub fn with_merge(window_ms: u64, merge: MergeFn) -> Self {
        Coalescer { window_ms, merge, buf: Vec::new() }
    }

    /// Buffer `op`; returns ops that must go out now.
    pub fn push(&mut self, op: Op, now_ms: u64) -> Vec<Op> {
        if let Some(i) = self.buf.iter().position(|(_, o)| o.key == op.key) {
            if self.buf[i].1.kind == op.kind {
                (self.merge)(&mut self.buf[i].1, op);
                return Vec::new();
            }
            let (_, earlier) = self.buf.remove(i);
            self.buf.push((now_ms + self.window_ms, op));
            return vec![earlier];
        }
        self.buf.push((now_ms + self.window_ms, op));
        Vec::new()
    }

    /// Ops whose window has passed, in the order they were buffered.
    pub fn due(&mut self, now_ms: u64) -> Vec<Op> {
        let (due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.buf).into_iter().partition(|(t, _)| *t <= now_ms);
        self.buf = keep;
        due.into_iter().map(|(_, o)| o).collect()
    }

    pub fn flush(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.buf).into_iter().map(|(_, o)| o).collect()
    }

    pub fn next_due(&self) -> Option<u64> {
        self.buf.iter().map(|(t, _)| *t).min()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

struct Stream {
    prefix: String,
    coalescer: Option<Coalescer>,
    auto_adopt: bool,
}

pub struct Client {
    cfg: ClientConfig,
    clock: Clock,
    n: u64,
    bases: HashMap<String, Base>,
    heads: HashMap<String, Head>,
    pending: HashMap<String, String>,
    streams: Vec<Stream>,
    welcome: Option<Welcome>,
}

impl Client {
    pub fn new(cfg: ClientConfig) -> Self {
        Client {
            cfg,
            clock: Clock::default(),
            n: 0,
            bases: HashMap::new(),
            heads: HashMap::new(),
            pending: HashMap::new(),
            streams: Vec::new(),
            welcome: None,
        }
    }

    pub fn config(&self) -> &ClientConfig {
        &self.cfg
    }

    pub fn welcome(&self) -> Option<&Welcome> {
        self.welcome.as_ref()
    }

    pub fn hello(&self, resume: bool) -> ClientFrame {
        ClientFrame::Hello(Hello {
            proto: PROTO_VERSION,
            session: self.cfg.session.clone(),
            agent_id: self.cfg.agent_id.clone(),
            person: self.cfg.person.clone(),
            scope: self.cfg.scope.clone(),
            token: self.cfg.token.clone(),
            resume,
        })
    }

    /// Per-prefix stream settings: a coalescing window (0 = off, the
    /// default) and whether deltas advance the base.
    pub fn stream(&mut self, prefix: &str, coalesce_ms: u64, auto_adopt: bool) {
        self.stream_with(prefix, (coalesce_ms > 0).then(|| Coalescer::new(coalesce_ms)), auto_adopt);
    }

    pub fn stream_with(&mut self, prefix: &str, coalescer: Option<Coalescer>, auto_adopt: bool) {
        self.streams.retain(|s| s.prefix != prefix);
        self.streams.push(Stream { prefix: prefix.to_string(), coalescer, auto_adopt });
        // longest prefix first
        self.streams.sort_by_key(|s| std::cmp::Reverse(s.prefix.len()));
    }

    fn stream_for(&mut self, key: &str) -> Option<&mut Stream> {
        self.streams.iter_mut().find(|s| key.starts_with(s.prefix.as_str()))
    }

    /// A fresh op id: `<session>.<n>` with characters outside the op-id
    /// alphabet replaced by `_`.
    pub fn next_op_id(&mut self) -> String {
        self.n += 1;
        let s: String =
            self.cfg.session.chars().map(|c| if c.is_ascii_alphanumeric() || "._:@-".contains(c) { c } else { '_' }).take(100).collect();
        format!("{s}.{}", self.n)
    }

    fn next_req(&mut self) -> String {
        self.n += 1;
        format!("r{}", self.n)
    }

    pub fn base(&self, key: &str) -> Base {
        self.bases.get(key).cloned().unwrap_or_default()
    }

    pub fn head(&self, key: &str) -> Option<&Head> {
        self.heads.get(key)
    }

    /// The agent has applied everything up to this version locally.
    pub fn adopt(&mut self, key: &str, base: Base) {
        self.bases.insert(key.to_string(), base);
    }

    /// Ops sent and not yet acked or rejected.
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// An op on `key`, based on what this agent has.
    pub fn op(&mut self, key: &str, kind: &str, public: Map<String, Value>, payload: Option<&[u8]>, now_ms: u64) -> Op {
        Op {
            op_id: self.next_op_id(),
            key: key.to_string(),
            base: self.base(key),
            kind: kind.to_string(),
            public,
            payload: payload.map(base64_encode),
            author: Peer::default(),
            intent_id: None,
            hlc: self.clock.now(now_ms),
            content_hash: None,
            leased: false,
        }
    }

    /// Send an op: straight away, or through its stream's coalescer.
    pub fn submit(&mut self, op: Op, now_ms: u64) -> Vec<ClientFrame> {
        let ops = match self.stream_for(&op.key).and_then(|s| s.coalescer.as_mut()) {
            Some(c) => c.push(op, now_ms),
            None => vec![op],
        };
        self.track(ops)
    }

    fn track(&mut self, ops: Vec<Op>) -> Vec<ClientFrame> {
        ops.into_iter()
            .map(|o| {
                self.pending.insert(o.op_id.clone(), o.key.clone());
                ClientFrame::Op(o)
            })
            .collect()
    }

    /// Coalesced ops whose window has passed.
    pub fn poll(&mut self, now_ms: u64) -> Vec<ClientFrame> {
        let mut ops = Vec::new();
        for s in &mut self.streams {
            if let Some(c) = s.coalescer.as_mut() {
                ops.extend(c.due(now_ms));
            }
        }
        self.track(ops)
    }

    /// Everything still buffered, now (before a disconnect or a push).
    pub fn flush(&mut self) -> Vec<ClientFrame> {
        let mut ops = Vec::new();
        for s in &mut self.streams {
            if let Some(c) = s.coalescer.as_mut() {
                ops.extend(c.flush());
            }
        }
        self.track(ops)
    }

    /// When `poll` next has something.
    pub fn next_poll(&self) -> Option<u64> {
        self.streams.iter().filter_map(|s| s.coalescer.as_ref()?.next_due()).min()
    }

    /// An atomic batch of ops (not coalesced).
    pub fn batch(&mut self, ops: Vec<Op>) -> ClientFrame {
        for o in &ops {
            self.pending.insert(o.op_id.clone(), o.key.clone());
        }
        self.n += 1;
        ClientFrame::Batch(Batch { batch_id: format!("{}.b{}", self.next_op_id(), self.n), atomic: true, ops })
    }

    pub fn lease(&mut self, keys: &[&str], kinds: &[&str], intent: Option<Intent>) -> ClientFrame {
        ClientFrame::Lease(Lease {
            req: Some(self.next_req()),
            keys: keys.iter().map(|s| s.to_string()).collect(),
            kinds: kinds.iter().map(|s| s.to_string()).collect(),
            intent,
        })
    }

    pub fn release(&mut self, keys: &[&str]) -> ClientFrame {
        ClientFrame::Release(Release { req: Some(self.next_req()), keys: keys.iter().map(|s| s.to_string()).collect() })
    }

    pub fn subscribe(&mut self, keys: &[&str], depth: u32) -> ClientFrame {
        ClientFrame::Subscribe(Subscribe { req: Some(self.next_req()), keys: keys.iter().map(|s| s.to_string()).collect(), depth })
    }

    /// The pre-write gate: would writes of `kind` on these keys, from this
    /// agent's bases, be accepted?
    pub fn check(&mut self, keys: &[&str], kind: Option<&str>) -> ClientFrame {
        let keys = keys.iter().map(|k| CheckKey { key: k.to_string(), base: Some(self.base(k)), kind: kind.map(str::to_string) }).collect();
        ClientFrame::Check(Check { req: Some(self.next_req()), keys })
    }

    pub fn promote(&mut self, op_ids: Vec<String>, evidence: Value) -> ClientFrame {
        ClientFrame::Promote(Promote { req: Some(self.next_req()), op_ids, evidence })
    }

    pub fn message(&mut self, to: Option<&str>, anchor: Option<&str>, text: &str) -> ClientFrame {
        ClientFrame::Message(SendMessage {
            req: None,
            to: to.map(str::to_string),
            anchor: anchor.map(str::to_string),
            text: text.to_string(),
        })
    }

    fn see(&mut self, key: &str, version: u64, hash: &str) {
        let h = self.heads.entry(key.to_string()).or_default();
        if version >= h.version {
            h.version = version;
            h.hash = hash.to_string();
        }
    }

    /// Feed a frame from the server; updates bases and heads.
    pub fn receive(&mut self, f: &ServerFrame, now_ms: u64) {
        match f {
            ServerFrame::Welcome(w) => {
                self.clock.observe(w.hlc, now_ms);
                for l in &w.resumed_leases {
                    self.see(&l.key, l.head.version, &l.head.hash);
                }
                self.welcome = Some(w.clone());
            }
            ServerFrame::Ack(a) => {
                self.clock.observe(a.hlc, now_ms);
                if let Some(named) = self.pending.remove(&a.op_id) {
                    if a.rekeyed_to.is_some() {
                        self.bases.remove(&named);
                    }
                }
                self.see(&a.key, a.version, &a.hash);
                self.bases.insert(a.key.clone(), Base::at(a.version, a.hash.clone()));
                if let Some(m) = &a.moved_to {
                    self.see(&m.key, m.version, &m.hash);
                    self.bases.insert(m.key.clone(), Base::at(m.version, m.hash.clone()));
                }
            }
            ServerFrame::Reject(r) => {
                self.pending.remove(&r.op_id);
                self.see(&r.key, r.current.version, &r.current.hash);
            }
            ServerFrame::Delta(d) => {
                self.clock.observe(d.op.hlc, now_ms);
                self.see(&d.op.key, d.version, &d.hash);
                if let Some(m) = &d.moved_to {
                    self.see(&m.key, m.version, &m.hash);
                }
                let adopt = self.stream_for(&d.op.key).is_some_and(|s| s.auto_adopt);
                if adopt && d.via.is_none() && d.layer == Layer::Draft {
                    self.bases.insert(d.op.key.clone(), Base::at(d.version, d.hash.clone()));
                    if let Some(m) = &d.moved_to {
                        self.bases.insert(m.key.clone(), Base::at(m.version, m.hash.clone()));
                    }
                }
            }
            ServerFrame::LeaseGranted(g) => {
                for (k, h) in g.keys.iter().zip(&g.heads) {
                    self.see(k, h.version, &h.hash);
                }
            }
            ServerFrame::CheckResult(c) => {
                for e in &c.results {
                    self.see(&e.key, e.current.version, &e.current.hash);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(key: &str, kind: &str, id: &str) -> Op {
        Op { op_id: id.into(), key: key.into(), kind: kind.into(), ..Default::default() }
    }

    #[test]
    fn coalescer_collapses_same_key_same_kind() {
        let mut c = Coalescer::new(100);
        let mut a = op("note:x", "edit_text", "1");
        a.base = Base::at(4, "h4");
        assert!(c.push(a, 0).is_empty());
        let mut b = op("note:x", "edit_text", "2");
        b.payload = Some("bGF0ZXI=".into());
        assert!(c.push(b, 50).is_empty());
        assert!(c.push(op("note:y", "edit_text", "3"), 60).is_empty());
        assert!(c.due(99).is_empty());
        let due = c.due(100);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].op_id, "2");
        assert_eq!(due[0].base.version, 4, "keeps the first base");
        assert_eq!(due[0].payload.as_deref(), Some("bGF0ZXI="));
        assert_eq!(c.due(160).len(), 1);
        assert!(c.is_empty());
    }

    #[test]
    fn coalescer_flushes_on_kind_change_to_keep_order() {
        let mut c = Coalescer::new(100);
        c.push(op("note:x", "append", "1"), 0);
        let out = c.push(op("note:x", "retitle", "2"), 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].op_id, "1");
        assert_eq!(c.flush()[0].op_id, "2");
    }

    #[test]
    fn coalescer_custom_merge() {
        fn concat(e: &mut Op, l: Op) {
            let a = e.payload.take().unwrap_or_default();
            e.payload = Some(a + &l.payload.unwrap_or_default());
            e.op_id = l.op_id;
        }
        let mut c = Coalescer::with_merge(10, concat);
        let mut a = op("k", "append", "1");
        a.payload = Some("a".into());
        let mut b = op("k", "append", "2");
        b.payload = Some("b".into());
        c.push(a, 0);
        c.push(b, 1);
        assert_eq!(c.flush()[0].payload.as_deref(), Some("ab"));
    }

    #[test]
    fn client_tracks_bases_from_acks_not_deltas() {
        let mut cl = Client::new(ClientConfig { session: "s 1".into(), ..Default::default() });
        let o = cl.op("code:a.py::f", "modify_body", Map::new(), Some(b"x"), 1);
        assert_eq!(o.op_id, "s_1.1");
        assert_eq!(o.payload.as_deref(), Some("eA=="));
        let frames = cl.submit(o.clone(), 1);
        assert_eq!(frames.len(), 1);
        assert_eq!(cl.in_flight(), 1);
        cl.receive(
            &ServerFrame::Ack(Ack {
                op_id: o.op_id.clone(),
                key: o.key.clone(),
                version: 1,
                hash: "h1".into(),
                hlc: Default::default(),
                seq: 1,
                batch_id: None,
                rebased_over: vec![],
                rekeyed_to: None,
                moved_to: None,
                same_as: None,
            }),
            2,
        );
        assert_eq!(cl.in_flight(), 0);
        assert_eq!(cl.base("code:a.py::f"), Base::at(1, "h1"));
        let mut d = o.clone();
        d.op_id = "other.1".into();
        cl.receive(
            &ServerFrame::Delta(Delta { op: d, version: 2, hash: "h2".into(), seq: 2, layer: Layer::Draft, via: None, moved_to: None }),
            3,
        );
        assert_eq!(cl.base("code:a.py::f").version, 1, "seeing is not having");
        assert_eq!(cl.head("code:a.py::f").unwrap().version, 2);
    }

    #[test]
    fn auto_adopt_streams_follow_deltas_and_coalesce() {
        let mut cl = Client::new(ClientConfig { session: "n".into(), ..Default::default() });
        cl.stream("note:", 100, true);
        let a = cl.op("note:x", "append", Map::new(), Some(b"a"), 0);
        assert!(cl.submit(a, 0).is_empty());
        let b = cl.op("note:x", "append", Map::new(), Some(b"b"), 10);
        assert!(cl.submit(b, 10).is_empty());
        assert_eq!(cl.next_poll(), Some(100));
        assert_eq!(cl.poll(100).len(), 1);
        let d = Op { op_id: "o.1".into(), key: "note:x".into(), kind: "append".into(), ..Default::default() };
        cl.receive(
            &ServerFrame::Delta(Delta { op: d, version: 7, hash: "h7".into(), seq: 1, layer: Layer::Draft, via: None, moved_to: None }),
            0,
        );
        assert_eq!(cl.base("note:x").version, 7);
    }
}
