//! Per-key state: version, hash, promoted version, where a rename moved the
//! key, and a bounded history of recent ops (what a stale writer is checked
//! against). The `Store` trait is the engine's working set; durability comes
//! from the `Log`, and a store is rebuilt from a snapshot plus the log's tail.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use crate::frames::MovedTo;
use crate::frames::{Ack, Head, Layer, Peer, TheirOp};
use crate::hlc::Hlc;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub version: u64,
    /// The key's hash after this op.
    pub hash: String,
    pub op_id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub public: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    pub author: Peer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
    pub hlc: Hlc,
    #[serde(default)]
    pub promoted: bool,
    /// Set on the entry of a rename/move: where the subject went.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<MovedTo>,
    /// The content hash after this op, if the writer sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// Set on the target key's entry for an op that moved a subject here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrived_from: Option<String>,
}

impl HistoryEntry {
    pub fn their_op(&self, key: &str) -> TheirOp {
        TheirOp {
            op_id: self.op_id.clone(),
            key: key.to_string(),
            version: self.version,
            kind: self.kind.clone(),
            public: self.public.clone(),
            payload: self.payload.clone(),
            author: self.author.clone(),
            intent_id: self.intent_id.clone(),
            hlc: self.hlc,
            layer: if self.promoted { Layer::Promoted } else { Layer::Draft },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct KeyState {
    /// The draft version: every accepted op bumps it.
    pub version: u64,
    pub hash: String,
    pub promoted_version: u64,
    /// Set while the key's subject lives elsewhere (after a rename/move);
    /// cleared when an op lands on the key again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<MovedTo>,
    /// The content hash at version 0, from the first op's `base.content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content0: Option<String>,
    /// The most recent ops, oldest first, at most `Config::history_per_key`.
    pub history: VecDeque<HistoryEntry>,
}

impl KeyState {
    pub fn head(&self) -> Head {
        Head { version: self.version, hash: self.hash.clone(), promoted_version: self.promoted_version }
    }

    /// The key's hash at `version`, if still known.
    pub fn hash_at(&self, version: u64) -> Option<&str> {
        if version == 0 {
            return Some("");
        }
        if version == self.version {
            return Some(&self.hash);
        }
        self.history.iter().find(|e| e.version == version).map(|e| e.hash.as_str())
    }

    /// The version a hash names on this key: the newest version whose key
    /// hash or content hash is `hash`, or 0 for the recorded version-0
    /// content. `Some(0)` for a key never written (any copy of a new key is
    /// a copy of nothing); `None` when nothing the server keeps has it.
    pub fn version_of_hash(&self, hash: &str) -> Option<u64> {
        if self.version == 0 {
            return Some(0);
        }
        if self.hash == hash {
            return Some(self.version);
        }
        if let Some(e) = self.history.iter().rev().find(|e| e.hash == hash || e.content_hash.as_deref() == Some(hash)) {
            return Some(e.version);
        }
        (self.content0.as_deref() == Some(hash)).then_some(0)
    }

    /// Where a base starts on this key (SPEC §5.1). A base by content
    /// (`content` with an empty `hash`) or by hash alone (`version: 0` and a
    /// non-empty `hash`) is looked up with `version_of_hash`; `None` when
    /// that finds nothing. Any other base starts at its version.
    pub fn base_start(&self, base: &crate::frames::Base) -> Option<u64> {
        let named = match (&base.content, base.hash.is_empty(), base.version) {
            (Some(c), true, _) => Some(c.as_str()),
            (_, false, 0) => Some(base.hash.as_str()),
            _ => None,
        };
        match named {
            Some(h) => self.version_of_hash(h).map(|v| v.max(base.version)),
            None => Some(base.version),
        }
    }

    /// The hash a base names by content or alone, if it does.
    pub fn named_hash(base: &crate::frames::Base) -> Option<&str> {
        match (&base.content, base.hash.is_empty(), base.version) {
            (Some(c), true, _) => Some(c),
            (_, false, 0) => Some(&base.hash),
            _ => None,
        }
    }

    /// Is every op after `version` still in the history?
    pub fn covers(&self, version: u64) -> bool {
        version >= self.version || self.history.front().is_some_and(|e| e.version <= version + 1)
    }
}

/// Where an accepted op landed, and the ack it got (resent on a duplicate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpLoc {
    pub scope: String,
    pub key: String,
    pub version: u64,
    pub person: String,
    pub ack: Ack,
}

pub trait Store: Send {
    fn get(&self, scope: &str, key: &str) -> Option<&KeyState>;
    /// The key's state, created empty if absent.
    fn get_mut(&mut self, scope: &str, key: &str) -> &mut KeyState;
    fn op(&self, op_id: &str) -> Option<&OpLoc>;
    fn put_op(&mut self, op_id: &str, loc: OpLoc);
    /// Every key, in no particular order (snapshots sort).
    fn for_each_key(&self, f: &mut dyn FnMut(&str, &str, &KeyState));
    fn for_each_op(&self, f: &mut dyn FnMut(&str, &OpLoc));
}

#[derive(Default)]
pub struct MemStore {
    keys: HashMap<String, HashMap<String, KeyState>>,
    ops: HashMap<String, OpLoc>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Store for MemStore {
    fn get(&self, scope: &str, key: &str) -> Option<&KeyState> {
        self.keys.get(scope)?.get(key)
    }

    fn get_mut(&mut self, scope: &str, key: &str) -> &mut KeyState {
        if !self.keys.contains_key(scope) {
            self.keys.insert(scope.to_string(), HashMap::new());
        }
        let s = self.keys.get_mut(scope).unwrap();
        if !s.contains_key(key) {
            s.insert(key.to_string(), KeyState::default());
        }
        s.get_mut(key).unwrap()
    }

    fn op(&self, op_id: &str) -> Option<&OpLoc> {
        self.ops.get(op_id)
    }

    fn put_op(&mut self, op_id: &str, loc: OpLoc) {
        self.ops.insert(op_id.to_string(), loc);
    }

    fn for_each_key(&self, f: &mut dyn FnMut(&str, &str, &KeyState)) {
        for (scope, keys) in &self.keys {
            for (k, st) in keys {
                f(scope, k, st);
            }
        }
    }

    fn for_each_op(&self, f: &mut dyn FnMut(&str, &OpLoc)) {
        for (id, loc) in &self.ops {
            f(id, loc);
        }
    }
}
