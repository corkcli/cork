//! Cork's parse engine and code graph, in Rust: the tree-sitter parse
//! that runs on every reported edit, cross-file edge resolution, Louvain
//! communities for the repo map, and the local symbol view and structured
//! edits the hooks use. Shared by the server (cork-rs) and the hooks
//! (cork-hooks).

pub mod community;
pub mod edit;
pub mod engine;
pub mod graph;
pub mod langs;
pub mod local;
pub mod privacy;
pub mod tokens;
