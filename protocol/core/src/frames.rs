//! Every frame on the wire. One WebSocket per agent session carries JSON text
//! frames, each an object tagged by `"t"`. SPEC.md documents every field; the
//! JSON Schema in `schema/frames.schema.json` is generated from the same shapes.
//!
//! Unknown fields are ignored on decode (forward compatibility); optional
//! fields are omitted on encode when empty.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::hlc::Hlc;

/// The protocol version a client sends in `hello.proto` and the server echoes
/// in `welcome.proto`. A different major version is refused with a fatal error.
pub const PROTO_VERSION: u32 = 1;

fn is_false(b: &bool) -> bool {
    !*b
}

fn default_true() -> bool {
    true
}

/// What the writer last saw of a key: its version and that version's hash.
/// A key nobody has written is `{version: 0, hash: ""}`. An empty `hash` skips
/// the hash check (the version alone is compared).
///
/// **Bases by content**, for a writer that knows what its copy holds but not
/// the server's version (a hook that sees a file on disk): either
/// `{version: 0, hash: "", content: C}` or, equivalently, `{version: 0,
/// hash: C}` (a non-empty hash at version 0 can never be a chain hash). The
/// server finds the version whose key hash or `content_hash` is `C` (SPEC
/// §5.1); an unknown `C` on a written key is `unknown_base`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Base {
    pub version: u64,
    #[serde(default)]
    pub hash: String,
    /// The hash of the content the writer has (e.g. sha256 of a symbol's
    /// normalized source), for writers that know content but not versions.
    /// Used only when `hash` is empty: the server maps it to the version
    /// whose op produced that content (SPEC §5.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl Base {
    /// A base by version and chain hash (from an ack, delta or reject).
    pub fn at(version: u64, hash: impl Into<String>) -> Self {
        Base { version, hash: hash.into(), content: None }
    }

    /// A base by content alone: "I have the content that hashes to this".
    pub fn of_content(content: impl Into<String>) -> Self {
        Base { version: 0, hash: String::new(), content: Some(content.into()) }
    }
}

/// A key's head as the server sees it: the draft version (every accepted op),
/// its hash, and the highest promoted version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Head {
    pub version: u64,
    pub hash: String,
    #[serde(default)]
    pub promoted_version: u64,
}

/// Where a rename/move sent a key's subject: the new key and its head there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovedTo {
    pub key: String,
    pub version: u64,
    pub hash: String,
}

/// Who did something: the session (one connection's identity), the agent and
/// the person behind it. The server fills `author` on every op from the
/// session's hello; a client's own value is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Peer {
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    pub person: String,
}

/// Why someone is doing something: an id (e.g. Cork's intent id) and/or a
/// line of text ("rename parse_config to load_config across the CLI").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Intent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Intent {
    pub fn is_empty(&self) -> bool {
        self.id.is_none() && self.text.is_none()
    }
}

/// The unit of change: one typed op on one key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Op {
    /// Client-chosen, globally unique, `[A-Za-z0-9._:@-]{1,128}`. Resending an
    /// accepted op_id returns the original ack (idempotent).
    pub op_id: String,
    /// The key written, e.g. `code:src/cli.py::parse_config`. On a delta, the
    /// key the op landed on (after any rekey).
    pub key: String,
    /// What the writer based the op on.
    #[serde(default)]
    pub base: Base,
    /// A kind the key's domain declares (`modify_body`, `rename`, `append`...).
    pub kind: String,
    /// Kind-declared public fields the server may read (`new_name`,
    /// `new_signature`, `new_path`...). Everything else goes in `payload`.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub public: Map<String, Value>,
    /// Opaque bytes, base64. The server stores and forwards them, never reads
    /// them; encrypting them is the client's business.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    /// Filled by the server from the session; ignored on the way in.
    #[serde(default)]
    pub author: Peer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
    /// The writer's hybrid logical clock when it made the op.
    #[serde(default)]
    pub hlc: Hlc,
    /// The hash of the key's content after this op, so later writers can
    /// name their base by content (`base.content`). Opaque to the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// "I am writing under my lease on this key." If the session no longer
    /// holds one, the op is rejected `stale_lease`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub leased: bool,
}

// ---------------------------------------------------------------- client → server

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u32,
    /// Stable for the life of the agent session; the key leases hang on.
    pub session: String,
    pub agent_id: String,
    pub person: String,
    /// The partition keys live in (a repo, a workspace's notes).
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Reattach to `session` and keep its leases and subscriptions if it is
    /// still inside its grace period.
    #[serde(default, skip_serializing_if = "is_false")]
    pub resume: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Batch {
    pub batch_id: String,
    /// All-or-nothing (two-phase) when true, which is the default.
    #[serde(default = "default_true")]
    pub atomic: bool,
    pub ops: Vec<Op>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Lease {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    pub keys: Vec<String>,
    /// The op kinds the holder intends; the domain table decides which writes
    /// by others conflict with them. Empty means every kind of the domain.
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<Intent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Release {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Subscribe {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    /// Exact keys, or prefixes ending in `*` (`code:src/cli.py::*`).
    pub keys: Vec<String>,
    /// 0 = only these keys; n = also any key they depend on within n hops.
    #[serde(default)]
    pub depth: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Unsubscribe {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CheckKey {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Base>,
    /// With a kind, the result also says what a write would do (`would`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Check {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    pub keys: Vec<CheckKey>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Promote {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    pub op_ids: Vec<String>,
    /// What proves the ops good: `{"tests": "passed", "commit": "<sha>"}`.
    #[serde(default)]
    pub evidence: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SendMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    /// A session id, agent id or person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// A key: delivered to whoever holds a lease on it, subscribes to it, or
    /// has unpromoted ops on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello(Hello),
    Op(Op),
    Batch(Batch),
    Lease(Lease),
    Release(Release),
    Subscribe(Subscribe),
    Unsubscribe(Unsubscribe),
    Check(Check),
    Promote(Promote),
    Message(SendMessage),
}

// ---------------------------------------------------------------- server → client

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainInfo {
    pub name: String,
    pub prefixes: Vec<String>,
    pub kinds: Vec<String>,
    /// The domain's suggested client coalescing window; 0 = off.
    pub coalesce_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseInfo {
    pub key: String,
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<Intent>,
    pub head: Head,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub proto: u32,
    pub session: String,
    /// True when `resume` found the session inside its grace period.
    pub resumed: bool,
    pub hlc: Hlc,
    pub grace_ms: u64,
    #[serde(default)]
    pub resumed_leases: Vec<LeaseInfo>,
    #[serde(default)]
    pub resumed_subscriptions: Vec<String>,
    pub domains: Vec<DomainInfo>,
}

/// Someone else's accepted op, as shown to a writer it affects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TheirOp {
    pub op_id: String,
    pub key: String,
    pub version: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub public: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    pub author: Peer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
    pub hlc: Hlc,
    pub layer: Layer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    pub op_id: String,
    /// Where the op landed (after any rekey).
    pub key: String,
    pub version: u64,
    /// The key's hash at `version`: the next op's `base.hash`.
    pub hash: String,
    pub hlc: Hlc,
    /// The op's position in the hash-chained log; the ack is sent only after
    /// this append.
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    /// Ops since the writer's base that commute with it; apply them locally.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rebased_over: Vec<TheirOp>,
    /// Set when someone's rename/move moved the key and the op followed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rekeyed_to: Option<String>,
    /// For the writer's own rename/move: the new key and its head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<MovedTo>,
    /// The key already held what the op makes it (SPEC §5.2): the op whose
    /// version has that content. Nothing was applied and the op id is not
    /// kept, so it is no draft of the writer's to promote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_as: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// An op since the base conflicts per the domain table.
    Conflict,
    /// Another session holds a lease whose kinds conflict with this op.
    Leased,
    /// Malformed: unknown kind or key, missing public field, bad base...
    Invalid,
    /// The op claimed `leased` but the session no longer holds the lease.
    StaleLease,
    /// Part of an atomic batch in which another op was rejected.
    BatchAborted,
    /// The base was named by content (or by hash alone) and no version of
    /// the key the server keeps has it: the writer's copy holds something the
    /// server never saw (a human edit, a local merge). `theirs` lists the
    /// other sessions' unpromoted ops on the key.
    UnknownBase,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reject {
    pub op_id: String,
    pub key: String,
    pub reason: RejectReason,
    pub current: Head,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub theirs: Vec<TheirOp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub their_intent: Option<Intent>,
    /// The lease holder (`leased`) or the first conflicting author (`conflict`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<Peer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// Accepted, in flight in its author's layer, visible to all.
    #[default]
    Draft,
    /// Proven by its author (tests passed, pushed).
    Promoted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    /// The op as accepted: `key` is where it landed, `author` is filled.
    pub op: Op,
    pub version: u64,
    pub hash: String,
    pub seq: u64,
    pub layer: Layer,
    /// Present when delivered through the graph: the path from the subscribed
    /// key to the changed key, e.g. `[caller, callee]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Vec<String>>,
    /// For a rename/move: the key the op moved its subject to, and its head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<MovedTo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseGranted {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    pub keys: Vec<String>,
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<Intent>,
    /// Each key's head when granted, in `keys` order.
    pub heads: Vec<Head>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseConflict {
    pub key: String,
    pub holder: Peer,
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<Intent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseDenied {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    pub keys: Vec<String>,
    /// The first conflicting holder; every one is in `conflicts`.
    pub holder: Peer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<Intent>,
    pub conflicts: Vec<LeaseConflict>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    /// The holder sent `release`.
    Released,
    /// The holder disconnected and its grace period ran out.
    Expired,
    /// The holder's session started over (hello without resume).
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseReleased {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    pub keys: Vec<String>,
    pub reason: ReleaseReason,
    pub holder: Peer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The base is current and no conflicting lease is held.
    Ok,
    /// Others wrote since the base; `would` says what a write would do.
    Stale,
    /// Another session holds a lease that conflicts with `kind`.
    Leased,
    /// The base was named by content (or by hash alone) and the server has
    /// seen no version of the key with it; `theirs` lists others' unpromoted
    /// ops. No `would`.
    UnknownBase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Would {
    Accept,
    Rebase,
    Rekey,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckEntry {
    pub key: String,
    pub status: CheckStatus,
    pub current: Head,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub would: Option<Would>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rekeyed_to: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub theirs: Vec<TheirOp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<Peer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<Intent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    pub results: Vec<CheckEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotedKey {
    pub key: String,
    pub promoted_version: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Promoted {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    pub op_ids: Vec<String>,
    pub keys: Vec<PromotedKey>,
    /// Op ids the server does not know, or that another person wrote.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<String>,
    #[serde(default)]
    pub evidence: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub from: Peer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    pub text: String,
    pub hlc: Hlc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorFrame {
    pub code: String,
    pub message: String,
    /// The server closes the connection after a fatal error.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fatal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerFrame {
    Welcome(Welcome),
    Ack(Ack),
    Reject(Reject),
    Delta(Delta),
    LeaseGranted(LeaseGranted),
    LeaseDenied(LeaseDenied),
    LeaseReleased(LeaseReleased),
    CheckResult(CheckResult),
    Promoted(Promoted),
    Message(Message),
    Error(ErrorFrame),
}

impl ClientFrame {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("frames always serialize")
    }
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

impl ServerFrame {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("frames always serialize")
    }
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
    /// The frame's `t`.
    pub fn tag(&self) -> &'static str {
        match self {
            ServerFrame::Welcome(_) => "welcome",
            ServerFrame::Ack(_) => "ack",
            ServerFrame::Reject(_) => "reject",
            ServerFrame::Delta(_) => "delta",
            ServerFrame::LeaseGranted(_) => "lease_granted",
            ServerFrame::LeaseDenied(_) => "lease_denied",
            ServerFrame::LeaseReleased(_) => "lease_released",
            ServerFrame::CheckResult(_) => "check_result",
            ServerFrame::Promoted(_) => "promoted",
            ServerFrame::Message(_) => "message",
            ServerFrame::Error(_) => "error",
        }
    }
}
