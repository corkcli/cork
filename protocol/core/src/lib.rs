//! # cork-protocol
//!
//! Cork's real-time protocol for agents, as a sans-IO core: versioned keys,
//! typed ops, leases tied to the connection, and instant subscriptions over a
//! dependency graph. Domain-agnostic: a [`Domain`] supplies key rules, a
//! compatibility [`Table`] (data, from JSON) and a [`Graph`].
//!
//! - [`frames`]: every frame on the wire (`ClientFrame`, `ServerFrame`).
//! - [`Engine`]: the server state machine. `handle(conn, frame, now)` returns
//!   the frames to send; no IO, no threads.
//! - [`Client`]: the client state machine: tracks bases from acks, builds ops,
//!   optional [`Coalescer`].
//! - [`Log`], [`Store`], [`Graph`], [`Domain`]: what an integrator plugs in;
//!   [`MemLog`], [`MemStore`], [`MemGraph`] are provided.
//! - [`trailer`]: the `Cork-Ops:` commit trailer (`Collide-Ops:` still read).
//!
//! Features: `ws-server` (tokio reference server, [`ws_server`]),
//! `ws-client-blocking` (sync client, [`ws_client`]),
//! `ws-client-blocking-tls` (adds `wss://`).
//!
//! The normative description is SPEC.md next to this crate.

pub mod client;
pub mod domain;
pub mod engine;
pub mod frames;
pub mod hash;
pub mod hlc;
pub mod log;
pub mod store;
pub mod table;
pub mod trailer;

#[cfg(feature = "ws-server")]
pub mod ws_server;

#[cfg(feature = "ws-client-blocking")]
pub mod ws_client;

pub use client::{Client, ClientConfig, Coalescer, MergeFn};
pub use domain::{Domain, Graph, MemGraph, TableDomain};
pub use engine::{is_landed, Authorizer, Config, ConnId, Engine, EngineBuilder, Outgoing};
pub use frames::*;
pub use hlc::{Clock, Hlc};
pub use log::{verify_chain, Accepted, ChainError, Log, LogEntry, LogError, LogRecord, MemLog, Snapshot};
pub use store::{HistoryEntry, KeyState, MemStore, OpLoc, Store};
pub use table::{KindSpec, Relation, Table, TableError};
