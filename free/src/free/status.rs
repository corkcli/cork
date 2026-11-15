//! The free version has no status page: it runs on one machine, and the
//! cloud's self-checks, history and incidents live in Cork's private
//! repo. These are their free answers, under the same names, so the engine
//! builds and runs without them.

use std::sync::Arc;

/// The header the status probes send; nothing reads it here.
pub const PROBE_HEADER: &str = "x-collide-status-probe";

pub fn router() -> axum::Router<Arc<crate::App>> {
    axum::Router::new()
}

pub fn start(_app: Arc<crate::App>) {}

pub fn note_loop(_name: &str) {}

pub fn page_url(_app: &crate::App) -> Option<String> {
    None
}
