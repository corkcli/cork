//! The op log: hash-chained like a ledger. Each entry carries the previous
//! entry's hash, and its own hash covers `prev`, `seq` and the canonical JSON
//! of its record, so any edit or gap breaks verification. The engine computes
//! seq and hashes; a `Log` only stores entries. An op is acked only after its
//! entry is appended.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::frames::{Op, Peer, PromotedKey};
use crate::hash::{canonical_json, sha256_hex, ZERO_HASH};
use crate::hlc::Hlc;
use crate::store::{KeyState, MovedTo, OpLoc};

/// One accepted op, as the log records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Accepted {
    pub scope: String,
    /// The op as accepted: `key` is where it landed, `author` is filled and
    /// `hlc` is the server's stamp.
    pub op: Op,
    pub version: u64,
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<MovedTo>,
    /// The key the writer named, when the op followed a rename elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rekeyed_from: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rebased_over: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "r", rename_all = "snake_case")]
pub enum LogRecord {
    Accept(Box<Accepted>),
    /// An atomic batch: one entry, so it is appended (and replayed) whole.
    Batch {
        batch_id: String,
        accepted: Vec<Accepted>,
    },
    Promote {
        scope: String,
        op_ids: Vec<String>,
        keys: Vec<PromotedKey>,
        evidence: Value,
        by: Peer,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: u64,
    pub prev: String,
    pub hash: String,
    pub hlc: Hlc,
    pub record: LogRecord,
}

pub fn entry_hash(prev: &str, seq: u64, hlc: Hlc, record: &LogRecord) -> String {
    let body = canonical_json(&serde_json::to_value(record).expect("records serialize"));
    let hlc = format!("{}.{}", hlc.w, hlc.c);
    sha256_hex(&[prev.as_bytes(), seq.to_string().as_bytes(), hlc.as_bytes(), body.as_bytes()])
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChainError {
    #[error("entry {seq}: prev does not match the previous entry's hash")]
    BrokenLink { seq: u64 },
    #[error("entry {seq}: hash does not match its contents")]
    BadHash { seq: u64 },
    #[error("entry {seq}: expected seq {expected}")]
    Gap { seq: u64, expected: u64 },
}

/// Verify a run of entries. `prev` is the hash before the first entry
/// (`ZERO_HASH` from genesis, or a snapshot's head hash).
pub fn verify_chain(entries: &[LogEntry], prev: &str, first_seq: u64) -> Result<(), ChainError> {
    let mut prev = prev.to_string();
    for (i, e) in entries.iter().enumerate() {
        let expected = first_seq + i as u64;
        if e.seq != expected {
            return Err(ChainError::Gap { seq: e.seq, expected });
        }
        if e.prev != prev {
            return Err(ChainError::BrokenLink { seq: e.seq });
        }
        if entry_hash(&e.prev, e.seq, e.hlc, &e.record) != e.hash {
            return Err(ChainError::BadHash { seq: e.seq });
        }
        prev = e.hash.clone();
    }
    Ok(())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("log: {0}")]
pub struct LogError(pub String);

/// The engine's state at a log position: restore it, then replay the entries
/// after `seq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub seq: u64,
    pub hash: String,
    pub hlc: Hlc,
    /// (scope, key, state), sorted.
    pub keys: Vec<(String, String, KeyState)>,
    /// (op_id, where it landed), sorted.
    pub ops: Vec<(String, OpLoc)>,
}

pub trait Log: Send {
    /// Make the entry durable. The engine acks only after this returns Ok; on
    /// Err nothing is applied and the writer gets an `error` frame.
    fn append(&mut self, entry: &LogEntry) -> Result<(), LogError>;

    /// Ask for a snapshot every n entries; `None` = never.
    fn snapshot_every(&self) -> Option<u64> {
        None
    }

    /// Store a snapshot (object storage, a table...).
    fn snapshot(&mut self, snap: &Snapshot) -> Result<(), LogError> {
        let _ = snap;
        Ok(())
    }
}

/// Keeps everything in memory; for tests, the reference server and replay.
#[derive(Default)]
pub struct MemLog {
    pub entries: Vec<LogEntry>,
    pub snapshots: Vec<Snapshot>,
    pub every: Option<u64>,
    /// Fail the next n appends (tests).
    pub fail_next: u32,
}

impl MemLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn verify(&self) -> Result<(), ChainError> {
        verify_chain(&self.entries, ZERO_HASH, 1)
    }
}

impl Log for MemLog {
    fn append(&mut self, entry: &LogEntry) -> Result<(), LogError> {
        if self.fail_next > 0 {
            self.fail_next -= 1;
            return Err(LogError("injected failure".into()));
        }
        self.entries.push(entry.clone());
        Ok(())
    }

    fn snapshot_every(&self) -> Option<u64> {
        self.every
    }

    fn snapshot(&mut self, snap: &Snapshot) -> Result<(), LogError> {
        self.snapshots.push(snap.clone());
        Ok(())
    }
}

/// A shared handle, so a test (or the reference server) can read the log the
/// engine owns.
impl<L: Log + ?Sized> Log for std::sync::Arc<std::sync::Mutex<L>> {
    fn append(&mut self, entry: &LogEntry) -> Result<(), LogError> {
        self.lock().unwrap().append(entry)
    }
    fn snapshot_every(&self) -> Option<u64> {
        self.lock().unwrap().snapshot_every()
    }
    fn snapshot(&mut self, snap: &Snapshot) -> Result<(), LogError> {
        self.lock().unwrap().snapshot(snap)
    }
}
