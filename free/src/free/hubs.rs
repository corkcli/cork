//! The free version has no hubs: they connect people across accounts, and a
//! local server has one person and no accounts. The cloud's lives in
//! Cork's private repo. These are its free answers, under the same names,
//! so the engine builds and runs on one machine without it.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::store::Store;

pub const NEED_ACCOUNT: &str = "hubs need a Cork account";

pub fn router() -> axum::Router<Arc<crate::App>> {
    axum::Router::new()
}

pub fn is_hub_send(args: &Value) -> bool {
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string();
    !text("hub").is_empty() || text("to").starts_with('@')
}

pub fn note_session(_store: &Store, _uid: &str, _session: &str, _scope: &str, _label: &str) -> i64 {
    0
}

pub fn hook_note(_store: &Store, _uid: &str, _session: &str) -> Option<String> {
    None
}

pub fn brief_line(_store: &Store, _uid: &str, _session: &str) -> Option<String> {
    None
}

pub fn take_for_session(_store: &Store, _uid: &str, _session: &str) -> Vec<Value> {
    Vec::new()
}

pub fn agent_send_live(_app: &crate::App, _uid: &str, _session: &str, _agent: &str, _args: &Value) -> Value {
    json!({"ok": false, "error": NEED_ACCOUNT})
}
