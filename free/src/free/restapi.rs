//! The free version's local server is reached by this machine's own agents
//! only, and has no public REST API: no versioned aliases, rate limits,
//! idempotency records or async jobs (the cloud's contract layer lives in
//! Cork's private repo). These are its free answers, under the same names.

use std::sync::Arc;

use axum::Router;

use crate::App;

pub fn wrap(_app: Arc<App>, inner: Router) -> Router {
    inner.layer(crate::cors_layer())
}

pub fn router() -> Router<Arc<App>> {
    Router::new()
}
