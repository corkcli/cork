//! The notes domain: shared general information, the second domain the
//! protocol carries. Keys are `note:<slug>`; the graph is links (a note that
//! links to another depends on it, so its subscribers hear when the other
//! changes). Fast, typing-like streams: the table suggests a 100 ms
//! coalescing window, and [`coalesce_merge`] collapses appends by
//! concatenation.

use std::sync::Arc;

use cork_protocol::hash::{base64_decode, base64_encode};
use cork_protocol::{Domain, Graph, MemGraph, Op, Table};

pub const TABLE_JSON: &str = include_str!("../table.json");

pub fn table() -> Table {
    Table::from_json(TABLE_JSON).expect("the bundled notes table is valid")
}

pub struct NotesDomain {
    table: Table,
    graph: Arc<MemGraph>,
}

impl Default for NotesDomain {
    fn default() -> Self {
        Self::new()
    }
}

impl NotesDomain {
    pub fn new() -> Self {
        NotesDomain { table: table(), graph: Arc::new(MemGraph::new()) }
    }

    /// The link graph the domain keeps from `link`/`unlink` ops.
    pub fn graph_handle(&self) -> Arc<MemGraph> {
        self.graph.clone()
    }
}

/// `My Note` → `my-note`: lowercase, spaces to dashes; `[a-z0-9-_/.]` only.
pub fn slug(s: &str) -> Result<String, String> {
    let s: String = s.trim().to_lowercase().chars().map(|c| if c.is_whitespace() { '-' } else { c }).collect();
    if s.is_empty() || s.len() > 200 || !s.chars().all(|c| c.is_ascii_alphanumeric() || "-_/.".contains(c)) {
        return Err(format!("`{s}` is not a note slug"));
    }
    Ok(s)
}

fn target(op: &Op) -> Option<String> {
    let t = op.public.get("target")?.as_str()?;
    Some(format!("note:{}", slug(t.strip_prefix("note:").unwrap_or(t)).ok()?))
}

impl Domain for NotesDomain {
    fn table(&self) -> &Table {
        &self.table
    }

    fn normalize_key(&self, key: &str) -> Result<String, String> {
        let rest = key.strip_prefix("note:").ok_or_else(|| format!("`{key}` is not note:<slug>"))?;
        Ok(format!("note:{}", slug(rest)?))
    }

    fn rekey_target(&self, _key: &str, op: &Op) -> Result<Option<String>, String> {
        if op.kind != "retitle" {
            return Ok(None);
        }
        let s = op.public.get("new_slug").and_then(|v| v.as_str()).ok_or("retitle needs a string public.new_slug")?;
        Ok(Some(format!("note:{}", slug(s)?)))
    }

    fn graph(&self) -> Option<&dyn Graph> {
        Some(&*self.graph)
    }

    fn observe(&self, scope: &str, key: &str, op: &Op) {
        match op.kind.as_str() {
            "link" => {
                if let Some(t) = target(op) {
                    self.graph.add_edge(scope, key, &t);
                }
            }
            "unlink" => {
                if let Some(t) = target(op) {
                    self.graph.remove_edge(scope, key, &t);
                }
            }
            _ => {}
        }
    }
}

/// The notes stream's coalescing merge: appends concatenate their payloads;
/// anything else, the later op wins. The earlier op's base is kept.
pub fn coalesce_merge(earlier: &mut Op, later: Op) {
    if earlier.kind == "append" && later.kind == "append" {
        let mut bytes = earlier.payload.as_deref().and_then(base64_decode).unwrap_or_default();
        bytes.extend(later.payload.as_deref().and_then(base64_decode).unwrap_or_default());
        let base = std::mem::take(&mut earlier.base);
        *earlier = later;
        earlier.base = base;
        earlier.payload = Some(base64_encode(&bytes));
    } else {
        cork_protocol::client::merge_last_wins(earlier, later);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cork_protocol::{Coalescer, Relation};
    use serde_json::json;

    #[test]
    fn table_has_commuting_appends() {
        let t = table();
        assert_eq!(t.relation("append", "append"), Some(Relation::Commute));
        assert_eq!(t.relation("edit_text", "retitle"), Some(Relation::Rekey));
        assert_eq!(t.coalesce_ms, 100);
    }

    #[test]
    fn keys_and_retitle() {
        let d = NotesDomain::new();
        assert_eq!(d.normalize_key("note:Release Plan").unwrap(), "note:release-plan");
        assert!(d.normalize_key("note:").is_err());
        let op =
            Op { kind: "retitle".into(), public: json!({"new_slug": "Launch Plan"}).as_object().unwrap().clone(), ..Default::default() };
        assert_eq!(d.rekey_target("note:release-plan", &op).unwrap().as_deref(), Some("note:launch-plan"));
    }

    #[test]
    fn links_build_the_graph() {
        let d = NotesDomain::new();
        let op = |kind: &str| Op {
            kind: kind.into(),
            public: json!({"target": "note:glossary"}).as_object().unwrap().clone(),
            ..Default::default()
        };
        d.observe("s", "note:plan", &op("link"));
        assert_eq!(d.graph_handle().dependents("s", "note:glossary"), vec!["note:plan"]);
        d.observe("s", "note:plan", &op("unlink"));
        assert!(d.graph_handle().dependents("s", "note:glossary").is_empty());
    }

    #[test]
    fn coalesced_appends_concatenate() {
        let mut c = Coalescer::with_merge(100, coalesce_merge);
        let mk = |id: &str, text: &[u8]| Op {
            op_id: id.into(),
            key: "note:log".into(),
            kind: "append".into(),
            payload: Some(base64_encode(text)),
            ..Default::default()
        };
        c.push(mk("1", b"hello "), 0);
        c.push(mk("2", b"world"), 10);
        let out = c.due(100);
        assert_eq!(out.len(), 1);
        assert_eq!(base64_decode(out[0].payload.as_deref().unwrap()).unwrap(), b"hello world");
    }
}
