//! What a domain plugs in: which keys it owns, how they normalize, its
//! compatibility table, where rename-like ops move a key, and the dependency
//! graph subscriptions walk. Code is one domain (keys = symbols, graph =
//! calls); notes are another (keys = notes, graph = links).

use std::collections::{BTreeSet, HashMap};
use std::sync::RwLock;

use crate::frames::{DomainInfo, Op};
use crate::table::Table;

/// Dependency edges within one scope. `dependents(k)` are the keys that depend
/// on `k` (its callers, the notes linking to it): when `k` changes, their
/// subscribers hear about it.
pub trait Graph: Send + Sync {
    fn dependents(&self, scope: &str, key: &str) -> Vec<String>;
}

pub trait Domain: Send + Sync {
    /// The table; its `domain`, `prefixes` and `kinds` describe the domain.
    fn table(&self) -> &Table;

    fn name(&self) -> &str {
        &self.table().domain
    }

    /// Does this domain own the key? By default: the key starts with one of
    /// the table's prefixes.
    fn owns(&self, key: &str) -> bool {
        self.table().prefixes.iter().any(|p| key.starts_with(p.as_str()))
    }

    /// The one spelling of a key (`./src//a.py` → `src/a.py`). An error makes
    /// the op `invalid`.
    fn normalize_key(&self, key: &str) -> Result<String, String> {
        Ok(key.to_string())
    }

    /// For an op whose kind `moves_key`: the key its subject moves to. An
    /// error (missing or malformed public field) makes the op `invalid`.
    fn rekey_target(&self, key: &str, op: &Op) -> Result<Option<String>, String> {
        let _ = (key, op);
        Ok(None)
    }

    fn graph(&self) -> Option<&dyn Graph> {
        None
    }

    /// Called after an op is accepted, with the key it landed on; a domain
    /// that derives its graph from ops (notes: `link`/`unlink`) updates it here.
    fn observe(&self, scope: &str, key: &str, op: &Op) {
        let _ = (scope, key, op);
    }

    fn info(&self) -> DomainInfo {
        let t = self.table();
        DomainInfo { name: t.domain.clone(), prefixes: t.prefixes.clone(), kinds: t.kind_names(), coalesce_ms: t.coalesce_ms }
    }
}

/// An in-memory graph: `add_edge(scope, dependent, dependency)` records that
/// `dependent` depends on `dependency` (a caller and its callee).
#[derive(Default)]
pub struct MemGraph {
    // (scope, dependency) -> dependents
    rev: RwLock<HashMap<(String, String), BTreeSet<String>>>,
}

impl MemGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_edge(&self, scope: &str, dependent: &str, dependency: &str) {
        self.rev.write().unwrap().entry((scope.to_string(), dependency.to_string())).or_default().insert(dependent.to_string());
    }

    pub fn remove_edge(&self, scope: &str, dependent: &str, dependency: &str) {
        let mut g = self.rev.write().unwrap();
        let k = (scope.to_string(), dependency.to_string());
        if let Some(s) = g.get_mut(&k) {
            s.remove(dependent);
            if s.is_empty() {
                g.remove(&k);
            }
        }
    }

    /// Replace every outgoing edge of `dependent` with `dependencies`.
    pub fn set_dependencies(&self, scope: &str, dependent: &str, dependencies: &[String]) {
        let mut g = self.rev.write().unwrap();
        for ((s, _), set) in g.iter_mut() {
            if s == scope {
                set.remove(dependent);
            }
        }
        g.retain(|_, set| !set.is_empty());
        for d in dependencies {
            g.entry((scope.to_string(), d.clone())).or_default().insert(dependent.to_string());
        }
    }
}

impl Graph for MemGraph {
    fn dependents(&self, scope: &str, key: &str) -> Vec<String> {
        self.rev.read().unwrap().get(&(scope.to_string(), key.to_string())).map(|s| s.iter().cloned().collect()).unwrap_or_default()
    }
}

impl<G: Graph + ?Sized> Graph for std::sync::Arc<G> {
    fn dependents(&self, scope: &str, key: &str) -> Vec<String> {
        (**self).dependents(scope, key)
    }
}

/// A domain that is only its table: keys are taken as written, no kind moves
/// keys, no graph. Handy for tests and file-like domains.
pub struct TableDomain {
    pub table: Table,
}

impl Domain for TableDomain {
    fn table(&self) -> &Table {
        &self.table
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_graph_edges() {
        let g = MemGraph::new();
        g.add_edge("s", "caller", "callee");
        g.add_edge("s", "other", "callee");
        g.add_edge("t", "x", "callee");
        assert_eq!(g.dependents("s", "callee"), vec!["caller", "other"]);
        g.remove_edge("s", "caller", "callee");
        assert_eq!(g.dependents("s", "callee"), vec!["other"]);
        g.set_dependencies("s", "other", &["b".into()]);
        assert!(g.dependents("s", "callee").is_empty());
        assert_eq!(g.dependents("s", "b"), vec!["other"]);
        assert_eq!(g.dependents("t", "callee"), vec!["x"]);
    }
}
