//! The tasks domain: a shared work queue, the third domain the protocol
//! carries. Keys are `task:<id>`; the graph is blockers (a task that depends
//! on another hears when the other changes, so closing a blocker reaches
//! everyone waiting on it).
//!
//! The point of the example: nothing here is about code. Two agents that
//! `claim` the same task conflict in one round trip, a `claim` lease keeps a
//! task taken exactly as long as its claimer is connected, and `merge` moves
//! the key so stale edits to a duplicate follow it to the canonical task.

use std::sync::Arc;

use cork_protocol::{Domain, Graph, MemGraph, Op, Table};

pub const TABLE_JSON: &str = include_str!("../table.json");

pub fn table() -> Table {
    Table::from_json(TABLE_JSON).expect("the bundled tasks table is valid")
}

pub struct TasksDomain {
    table: Table,
    graph: Arc<MemGraph>,
}

impl Default for TasksDomain {
    fn default() -> Self {
        Self::new()
    }
}

impl TasksDomain {
    pub fn new() -> Self {
        TasksDomain { table: table(), graph: Arc::new(MemGraph::new()) }
    }

    /// The blocker graph the domain keeps from `depend`/`undepend` ops.
    pub fn graph_handle(&self) -> Arc<MemGraph> {
        self.graph.clone()
    }
}

/// `ENG-42` → `eng-42`: lowercase; `[a-z0-9-_./#]` only.
pub fn task_id(s: &str) -> Result<String, String> {
    let s = s.trim().to_lowercase();
    if s.is_empty() || s.len() > 200 || !s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./#".contains(c)) {
        return Err(format!("`{s}` is not a task id"));
    }
    Ok(s)
}

fn task_key(s: &str) -> Option<String> {
    Some(format!("task:{}", task_id(s.strip_prefix("task:").unwrap_or(s)).ok()?))
}

fn target(op: &Op) -> Option<String> {
    task_key(op.public.get("target")?.as_str()?)
}

impl Domain for TasksDomain {
    fn table(&self) -> &Table {
        &self.table
    }

    fn normalize_key(&self, key: &str) -> Result<String, String> {
        let rest = key.strip_prefix("task:").ok_or_else(|| format!("`{key}` is not task:<id>"))?;
        Ok(format!("task:{}", task_id(rest)?))
    }

    fn rekey_target(&self, _key: &str, op: &Op) -> Result<Option<String>, String> {
        if op.kind != "merge" {
            return Ok(None);
        }
        let into = op.public.get("into").and_then(|v| v.as_str()).ok_or("merge needs a string public.into")?;
        task_key(into).map(Some).ok_or_else(|| format!("`{into}` is not a task id"))
    }

    fn graph(&self) -> Option<&dyn Graph> {
        Some(&*self.graph)
    }

    fn observe(&self, scope: &str, key: &str, op: &Op) {
        match op.kind.as_str() {
            "depend" => {
                if let Some(t) = target(op) {
                    self.graph.add_edge(scope, key, &t);
                }
            }
            "undepend" => {
                if let Some(t) = target(op) {
                    self.graph.remove_edge(scope, key, &t);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cork_protocol::Relation;
    use serde_json::json;

    #[test]
    fn table_settles_a_double_claim() {
        let t = table();
        assert_eq!(t.relation("claim", "claim"), Some(Relation::Conflict));
        assert_eq!(t.relation("comment", "claim"), Some(Relation::Commute));
        assert_eq!(t.relation("edit", "merge"), Some(Relation::Rekey));
        assert!(!t.compatible("claim", "claim"), "two claim leases on one task");
        assert!(t.compatible("comment", "claim"));
    }

    #[test]
    fn keys_and_merge() {
        let d = TasksDomain::new();
        assert_eq!(d.normalize_key("task:ENG-42").unwrap(), "task:eng-42");
        assert!(d.normalize_key("task:").is_err());
        assert!(d.normalize_key("note:x").is_err());
        let op = Op { kind: "merge".into(), public: json!({"into": "ENG-7"}).as_object().unwrap().clone(), ..Default::default() };
        assert_eq!(d.rekey_target("task:eng-42", &op).unwrap().as_deref(), Some("task:eng-7"));
    }

    #[test]
    fn blockers_build_the_graph() {
        let d = TasksDomain::new();
        let op = |kind: &str| Op {
            kind: kind.into(),
            public: json!({"target": "task:eng-1"}).as_object().unwrap().clone(),
            ..Default::default()
        };
        d.observe("s", "task:eng-2", &op("depend"));
        assert_eq!(d.graph_handle().dependents("s", "task:eng-1"), vec!["task:eng-2"]);
        d.observe("s", "task:eng-2", &op("undepend"));
        assert!(d.graph_handle().dependents("s", "task:eng-1").is_empty());
    }
}
