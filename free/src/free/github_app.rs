//! The free version has no GitHub App; the cloud's lives in Cork's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use std::sync::Arc;

use crate::store::Store;

pub fn router() -> axum::Router<Arc<crate::App>> {
    axum::Router::new()
}

pub fn workspace_for_repo(_store: &Store, _repo_id: &str) -> Option<String> {
    None
}

pub fn covers(_store: &Store, _workspace_id: &str, _repo_id: &str) -> bool {
    false
}
