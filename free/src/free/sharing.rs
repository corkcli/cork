//! The free version counts no machines per login; the cloud's count lives in Cork's
//! private repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::store::Store;

pub fn logins(_store: &Store, _workspace: &str, _users: &BTreeSet<String>, _now: f64) -> Value {
    json!({})
}

pub fn note(_store: &Store, _workspace: &str, _user: &str, _machine: &str, _now: f64) {}
