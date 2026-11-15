//! The code domain: keys are symbols and files, the graph is calls.
//!
//! - `code:<path>::<symbol>`: a symbol, e.g. `code:src/cli.py::Config.load`.
//!   `<symbol>` is dot-qualified (`Class.method`).
//! - `file:<path>`: a whole file, the fallback for anything without symbols
//!   (configs, lockfiles, binaries). Same rules, one key per file.
//!
//! Paths are repo-relative with `/` separators. The kinds and their
//! compatibility table are in `table.json`; SPEC.md renders it.
//!
//! The graph comes from whoever parses the code (Cork's server keeps it):
//! hand an `Arc<dyn Graph>` to [`CodeDomain::with_graph`], or use the
//! built-in [`MemGraph`] through [`CodeDomain::graph_handle`].

use std::sync::Arc;

use cork_protocol::{Domain, Graph, MemGraph, Op, Table};

pub const TABLE_JSON: &str = include_str!("../table.json");

pub fn table() -> Table {
    Table::from_json(TABLE_JSON).expect("the bundled code table is valid")
}

pub struct CodeDomain {
    table: Table,
    graph: Arc<dyn Graph>,
    mem: Option<Arc<MemGraph>>,
}

impl Default for CodeDomain {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeDomain {
    /// With an empty in-memory graph (see `graph_handle`).
    pub fn new() -> Self {
        let mem = Arc::new(MemGraph::new());
        CodeDomain { table: table(), graph: mem.clone(), mem: Some(mem) }
    }

    /// With the caller's graph (e.g. Cork's call index).
    pub fn with_graph(graph: Arc<dyn Graph>) -> Self {
        CodeDomain { table: table(), graph, mem: None }
    }

    /// The built-in graph, to add call edges to; `None` with `with_graph`.
    pub fn graph_handle(&self) -> Option<Arc<MemGraph>> {
        self.mem.clone()
    }
}

/// `./src//a.py` → `src/a.py`; `..`, absolute paths and empty paths are refused.
pub fn normalize_path(p: &str) -> Result<String, String> {
    let p = p.replace('\\', "/");
    if p.starts_with('/') || p.contains(':') && p.as_bytes().get(1) == Some(&b':') {
        return Err(format!("`{p}` is absolute; keys use repo-relative paths"));
    }
    let mut parts = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => return Err(format!("`{p}` leaves the repo")),
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        return Err("empty path".into());
    }
    Ok(parts.join("/"))
}

/// Split `code:<path>::<symbol>`.
pub fn split_symbol_key(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix("code:")?.rsplit_once("::")
}

pub fn symbol_key(path: &str, symbol: &str) -> String {
    format!("code:{path}::{symbol}")
}

pub fn file_key(path: &str) -> String {
    format!("file:{path}")
}

fn public_str<'a>(op: &'a Op, field: &str) -> Result<&'a str, String> {
    op.public
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("`{}` needs a string public.{field}", op.kind))
}

impl Domain for CodeDomain {
    fn table(&self) -> &Table {
        &self.table
    }

    fn normalize_key(&self, key: &str) -> Result<String, String> {
        if let Some(path) = key.strip_prefix("file:") {
            return Ok(file_key(&normalize_path(path)?));
        }
        let (path, symbol) = split_symbol_key(key).ok_or_else(|| format!("`{key}` is not code:<path>::<symbol>"))?;
        let symbol = symbol.trim();
        if symbol.is_empty() || symbol.chars().any(char::is_whitespace) || symbol.starts_with('.') || symbol.ends_with('.') {
            return Err(format!("`{symbol}` is not a symbol name"));
        }
        Ok(symbol_key(&normalize_path(path)?, symbol))
    }

    fn rekey_target(&self, key: &str, op: &Op) -> Result<Option<String>, String> {
        match op.kind.as_str() {
            "rename" => {
                let (path, symbol) = split_symbol_key(key).ok_or("rename applies to code: keys")?;
                let new = public_str(op, "new_name")?;
                let sym = if new.contains('.') {
                    new.to_string()
                } else {
                    match symbol.rsplit_once('.') {
                        Some((owner, _)) => format!("{owner}.{new}"),
                        None => new.to_string(),
                    }
                };
                Ok(Some(symbol_key(path, &sym)))
            }
            "move" => {
                let new_path = normalize_path(public_str(op, "new_path")?)?;
                if key.starts_with("file:") {
                    return Ok(Some(file_key(&new_path)));
                }
                let (_, symbol) = split_symbol_key(key).ok_or("move applies to code: and file: keys")?;
                Ok(Some(symbol_key(&new_path, symbol)))
            }
            _ => Ok(None),
        }
    }

    fn graph(&self) -> Option<&dyn Graph> {
        Some(&*self.graph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cork_protocol::Relation;
    use serde_json::json;

    fn op(kind: &str, public: serde_json::Value) -> Op {
        Op { op_id: "x.1".into(), key: String::new(), kind: kind.into(), public: public.as_object().unwrap().clone(), ..Default::default() }
    }

    #[test]
    fn table_loads_and_is_complete() {
        let t = table();
        assert_eq!(t.kinds.len(), 7);
        assert_eq!(t.relation("modify_body", "rename"), Some(Relation::Rekey));
        assert_eq!(t.relation("modify_body", "modify_body"), Some(Relation::Conflict));
        assert_eq!(t.relation("signature_change", "modify_body"), Some(Relation::Conflict));
        assert_eq!(t.relation("modify_body", "move"), Some(Relation::Rekey));
        assert_eq!(t.relation("rename", "modify_body"), Some(Relation::Commute));
        for theirs in t.kinds.keys() {
            if theirs != "delete" {
                assert_eq!(t.relation("delete", theirs), Some(Relation::Conflict), "delete x {theirs}");
            }
        }
        // every documented note names a real cell
        for k in t.notes.keys() {
            let (a, b) = k.split_once('/').unwrap();
            assert!(t.relation(a, b).is_some(), "{k}");
        }
    }

    #[test]
    fn keys_normalize() {
        let d = CodeDomain::new();
        assert_eq!(d.normalize_key("code:./src//cli.py::main").unwrap(), "code:src/cli.py::main");
        assert_eq!(d.normalize_key("file:.\\config\\app.toml").unwrap(), "file:config/app.toml");
        assert!(d.normalize_key("code:../x.py::f").is_err());
        assert!(d.normalize_key("code:/abs.py::f").is_err());
        assert!(d.normalize_key("code:a.py::").is_err());
        assert!(d.normalize_key("code:a.py").is_err());
        assert!(d.normalize_key("code:a.py::has space").is_err());
    }

    #[test]
    fn rename_and_move_targets() {
        let d = CodeDomain::new();
        let r = d.rekey_target("code:a.py::Config.load", &op("rename", json!({"new_name": "read"}))).unwrap();
        assert_eq!(r.as_deref(), Some("code:a.py::Config.read"));
        let r = d.rekey_target("code:a.py::Config.load", &op("rename", json!({"new_name": "Settings.load"}))).unwrap();
        assert_eq!(r.as_deref(), Some("code:a.py::Settings.load"));
        let r = d.rekey_target("code:a.py::f", &op("move", json!({"new_path": "./b/c.py"}))).unwrap();
        assert_eq!(r.as_deref(), Some("code:b/c.py::f"));
        let r = d.rekey_target("file:a.toml", &op("move", json!({"new_path": "conf/a.toml"}))).unwrap();
        assert_eq!(r.as_deref(), Some("file:conf/a.toml"));
        assert!(d.rekey_target("code:a.py::f", &op("rename", json!({}))).is_err());
        assert_eq!(d.rekey_target("code:a.py::f", &op("modify_body", json!({}))).unwrap(), None);
    }

    #[test]
    fn kinds_apply_to_their_prefixes() {
        let t = table();
        assert!(t.allows("file_replace", "file:a.toml"));
        assert!(!t.allows("file_replace", "code:a.py::f"));
        assert!(!t.allows("modify_body", "file:a.toml"));
        assert!(t.allows("delete", "file:a.toml") && t.allows("delete", "code:a.py::f"));
    }
}
