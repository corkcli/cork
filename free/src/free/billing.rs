//! The free version has no billing; the cloud's lives in Cork's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::store::Store;

pub const BILLING_BUCKET: &str = "billing";
pub const TRIAL_DAYS: i64 = 14;

pub enum Limit {
    Count(Option<i64>),
    Text(&'static str),
    Flag(bool),
}

/// A plan, as access reads it. On one machine there is one, and nothing in
/// it is limited: an empty list means every count is unmetered.
pub struct PlanDef {
    pub name: &'static str,
    pub label: &'static str,
    pub limits: &'static [(&'static str, Limit)],
}

pub const PLANS: &[PlanDef] = &[
    PlanDef { name: "free", label: "Cork", limits: &[] },
    PlanDef { name: "business", label: "Cork", limits: &[] },
];

pub fn count_call(_store: &Store, _workspace_id: &str) {}

/// The local workspace's plan record, as `local::bootstrap` wrote it.
pub fn plan_of(store: &Store, workspace_id: &str) -> Value {
    store.kv_get(BILLING_BUCKET, workspace_id).unwrap_or_else(|| json!({"plan": "business", "status": "active", "source": "local"}))
}

pub fn effective_access(_plan_record: &Value, _member: &Value) -> &'static str {
    "write"
}

pub fn is_active_status(status: Option<&Value>) -> bool {
    matches!(status.and_then(Value::as_str), Some("active") | Some("trialing") | Some("past_due"))
}

pub fn bound_repos(store: &Store, workspace_id: &str) -> BTreeSet<String> {
    let mut keys: BTreeSet<String> = bound_repo_ids(store, workspace_id).iter().map(|id| crate::repo::repo_key(id)).collect();
    keys.remove("");
    crate::repo::dedupe_spellings(keys)
}

/// [`bound_repos`] as the ids were spelled: on one machine, the repos with data.
pub fn bound_repo_ids(store: &Store, workspace_id: &str) -> Vec<String> {
    let prefix = format!("{workspace_id}:");
    store.list_scopes(&prefix).iter().map(|s| s[prefix.len()..].to_string()).filter(|id| !id.is_empty()).collect()
}

pub fn repo_cap_error(_store: &Store, _workspace_id: &str, _repo_id: &str) -> Option<Value> {
    None
}

pub fn seat_limit(_store: &Store, _workspace_id: &str) -> Option<i64> {
    None
}

pub fn start_trial(store: &Store, workspace_id: &str, _days: i64, _source: &str) -> Value {
    plan_of(store, workspace_id)
}

pub fn usage_notice(_store: &Store, _workspace_id: &str) -> Option<String> {
    None
}

pub fn workspace_limit_for(_store: &Store, _owned: &[Value]) -> Option<i64> {
    None
}

pub fn router() -> axum::Router<Arc<crate::App>> {
    axum::Router::new()
}

// agent hours (the cloud's meter): on one machine nothing is metered

pub const UNIT_CENTS: i64 = 2000;

pub fn hours_per_unit() -> i64 {
    100
}

pub fn session_gate(_store: &Store, _workspace_id: &str, _uid: &str, _session: &str, _dashboard_url: &str) -> Option<String> {
    None
}

pub fn meter_active(_store: &Store, _workspace_id: &str, _uid: &str, _secs: f64, _dashboard_url: &str) {}

pub fn hours_notice(_store: &Store, _workspace_id: &str, _uid: &str, _dashboard_url: &str) -> Option<String> {
    None
}

pub fn hours_state(_store: &Store, _workspace_id: &str, _owner_uid: &str) -> Option<(i64, f64, i64, &'static str, String, f64)> {
    None
}

pub fn move_units_pool(_store: &Store, _from_ws: &str, _to_ws: &str) -> bool {
    false
}

pub const SIGNUP_BONUS_HOURS: i64 = 30;
pub const INVITE_BONUS_HOURS: i64 = 10;

pub fn grant_bonus(_store: &Store, _workspace_id: &str, _uid: &str, _kind: &str, _hours: i64) -> bool {
    false
}

pub fn meter_mcp_call(_store: &Store, _workspace_id: &str, _uid: &str, _session: &str, _dashboard_url: &str) {}

pub fn seat_plan_sunset(_store: &Store, _workspace_id: &str) -> Option<Value> {
    None
}
