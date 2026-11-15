//! A domain's compatibility table: for every pair of op kinds (mine, theirs),
//! what happens when mine arrives on a base that theirs has since moved past.
//! The table is data (JSON), checked complete and consistent on load.
//!
//! ```json
//! {
//!   "domain": "notes", "version": 1, "prefixes": ["note:"], "coalesce_ms": 100,
//!   "kinds": { "append": { "doc": "...", "public": [], "moves_key": false } },
//!   "relations": { "append": { "append": "commute" } }
//! }
//! ```
//! `relations[mine][theirs]`. Every kind must have a full row.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// Both apply; the later one is accepted on top and told what it rebased over.
    Commute,
    /// Theirs moved the key (rename/move); mine follows it to the new key.
    Rekey,
    /// The writer must merge locally; mine is rejected with theirs attached.
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KindSpec {
    #[serde(default)]
    pub doc: String,
    /// Public fields an op of this kind must carry.
    #[serde(default)]
    pub public: Vec<String>,
    /// The op moves its subject to another key (rename, move, retitle). The
    /// domain computes the target from the public fields.
    #[serde(default)]
    pub moves_key: bool,
    /// Key prefixes this kind applies to; empty = every prefix of the domain.
    #[serde(default)]
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub domain: String,
    pub version: u32,
    pub prefixes: Vec<String>,
    #[serde(default)]
    pub coalesce_ms: u32,
    pub kinds: BTreeMap<String, KindSpec>,
    pub relations: BTreeMap<String, BTreeMap<String, Relation>>,
    /// Free-form reasons per cell, `"mine/theirs": "why"`; documentation only.
    #[serde(default)]
    pub notes: BTreeMap<String, String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TableError {
    #[error("table JSON: {0}")]
    Json(String),
    #[error("relations name unknown kind `{0}`")]
    UnknownKind(String),
    #[error("missing relation {0} x {1}")]
    Missing(String, String),
    #[error("{0} x {1} is rekey but `{1}` does not move keys")]
    BadRekey(String, String),
    #[error("kind `{0}` names prefix `{1}` the domain does not own")]
    BadPrefix(String, String),
}

impl Table {
    pub fn from_json(text: &str) -> Result<Table, TableError> {
        let t: Table = serde_json::from_str(text).map_err(|e| TableError::Json(e.to_string()))?;
        t.validate()?;
        Ok(t)
    }

    pub fn validate(&self) -> Result<(), TableError> {
        for (mine, row) in &self.relations {
            if !self.kinds.contains_key(mine) {
                return Err(TableError::UnknownKind(mine.clone()));
            }
            for theirs in row.keys() {
                if !self.kinds.contains_key(theirs) {
                    return Err(TableError::UnknownKind(theirs.clone()));
                }
            }
        }
        for mine in self.kinds.keys() {
            for (theirs, spec) in &self.kinds {
                let rel = self
                    .relations
                    .get(mine)
                    .and_then(|r| r.get(theirs))
                    .ok_or_else(|| TableError::Missing(mine.clone(), theirs.clone()))?;
                if *rel == Relation::Rekey && !spec.moves_key {
                    return Err(TableError::BadRekey(mine.clone(), theirs.clone()));
                }
            }
        }
        for (name, spec) in &self.kinds {
            for p in &spec.keys {
                if !self.prefixes.iter().any(|q| q == p) {
                    return Err(TableError::BadPrefix(name.clone(), p.clone()));
                }
            }
        }
        Ok(())
    }

    /// `relations[mine][theirs]`; `None` when either kind is unknown.
    pub fn relation(&self, mine: &str, theirs: &str) -> Option<Relation> {
        self.relations.get(mine)?.get(theirs).copied()
    }

    pub fn kind(&self, name: &str) -> Option<&KindSpec> {
        self.kinds.get(name)
    }

    /// The kind exists and may be used on this key.
    pub fn allows(&self, kind: &str, key: &str) -> bool {
        match self.kinds.get(kind) {
            Some(spec) => spec.keys.is_empty() || spec.keys.iter().any(|p| key.starts_with(p.as_str())),
            None => false,
        }
    }

    /// Two leases (or a lease and a write) are compatible when neither order
    /// conflicts.
    pub fn compatible(&self, a: &str, b: &str) -> bool {
        self.relation(a, b) != Some(Relation::Conflict) && self.relation(b, a) != Some(Relation::Conflict)
    }

    pub fn kind_names(&self) -> Vec<String> {
        self.kinds.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = r#"{
      "domain": "t", "version": 1, "prefixes": ["t:"],
      "kinds": { "edit": {}, "mv": {"public": ["to"], "moves_key": true} },
      "relations": {
        "edit": {"edit": "conflict", "mv": "rekey"},
        "mv":   {"edit": "commute",  "mv": "conflict"}
      }
    }"#;

    #[test]
    fn loads_and_looks_up() {
        let t = Table::from_json(T).unwrap();
        assert_eq!(t.relation("edit", "mv"), Some(Relation::Rekey));
        assert_eq!(t.relation("mv", "edit"), Some(Relation::Commute));
        assert_eq!(t.relation("edit", "nope"), None);
        assert!(!t.compatible("edit", "edit"));
        assert!(t.compatible("edit", "mv"));
    }

    #[test]
    fn rejects_incomplete_tables() {
        let bad = T.replace(r#""mv": "rekey"}"#, "}").replace(r#""edit": "conflict", "#, r#""edit": "conflict""#);
        assert!(matches!(Table::from_json(&bad), Err(TableError::Missing(_, _))));
    }

    #[test]
    fn rejects_rekey_onto_a_kind_that_does_not_move() {
        let bad = T.replace(r#""edit": {"edit": "conflict""#, r#""edit": {"edit": "rekey""#);
        assert!(matches!(Table::from_json(&bad), Err(TableError::BadRekey(_, _))));
    }

    #[test]
    fn rejects_unknown_kinds() {
        let bad = T.replace(r#""mv":   {"edit": "commute""#, r#""mv":   {"zap": "commute", "edit": "commute""#);
        assert!(matches!(Table::from_json(&bad), Err(TableError::UnknownKind(_))));
    }
}
