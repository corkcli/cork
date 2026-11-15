//! The free version sends no email: the cloud's notification emails live in
//! Cork's private repo. These are their free answers, under the same
//! names, so the engine builds and runs on one machine without them. The
//! bell has nothing of the cloud's to list here; it shows only the repos'
//! unread update lines (unread.rs), which this server can count itself.

use std::sync::Arc;

use axum::routing::{get, post};

pub fn router() -> axum::Router<Arc<crate::App>> {
    axum::Router::new()
        .route("/api/notifications", get(crate::unread::api_updates_only))
        .route("/api/notifications/read", post(crate::unread::api_updates_read))
}

pub fn start(_app: Arc<crate::App>) {}

pub fn note_commit(_store: &crate::store::Store, _workspace: &str, _scope: &str, _email: &str, _sha: &str) {}
