//! The free version has no channels: they are a workspace's rooms for its
//! people, and a local server has one person and no accounts. The cloud's
//! lives in Cork's private repo. This is its free answer, under the same
//! name, so the engine builds and runs on one machine without it.

use std::sync::Arc;

pub fn router() -> axum::Router<Arc<crate::App>> {
    axum::Router::new()
}
