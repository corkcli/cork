//! Is a teammate's change on origin? Checked against git, on this machine.
//!
//! The server and the protocol know what a teammate's session recorded:
//! its renames, its new signatures, whether its ops were promoted. They
//! cannot know whether that change reached the origin THIS checkout pulls
//! from. Study 8's phantom block was exactly that gap: a session's rename was
//! recorded (and promoted, onto another clone's origin), the gate told a new
//! agent four times to pull it, there was nothing to pull, and the agent
//! bypassed the gate with `sed`.
//!
//! So before the hooks tell an agent to pull, they look: the file as origin
//! has it (`@{u}`, as this checkout last fetched it) against the file as
//! this copy has it. A block that says "pull" stands only when origin has
//! the change and this copy does not, which is also exactly what a pull and
//! a re-read clear. Anything else is a line saying what to write against.
//! Two local git calls, only when a teammate's change is in question.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

const GIT_WAIT: Duration = Duration::from_millis(400);

/// A file's top-level declarations: name → (signature, content hash).
pub type Symbols = BTreeMap<String, (String, String)>;

fn symbols_of(rel: &str, content: &str) -> Option<Symbols> {
    let parsed = cork_core::local::symbols(rel, content)?;
    if !parsed.clean {
        return None;
    }
    Some(parsed.symbols.into_iter().map(|s| (s.name, (s.signature, s.hash))).collect())
}

/// The file as origin has it, parsed: `Some(empty)` when origin has no such
/// file, `None` when this checkout has no upstream (or git cannot say).
pub fn upstream_symbols(root: &Path, rel: &str) -> Option<Symbols> {
    crate::git::git(root, &["rev-parse", "--verify", "-q", "@{u}"], GIT_WAIT)?;
    match crate::git::git(root, &["show", &format!("@{{u}}:{rel}")], GIT_WAIT) {
        Some(content) => symbols_of(rel, &content),
        None => Some(Symbols::new()),
    }
}

/// The file as this copy has it on disk, parsed.
pub fn local_symbols(root: &Path, rel: &str) -> Option<Symbols> {
    let content = std::fs::read_to_string(root.join(rel)).unwrap_or_default();
    symbols_of(rel, &content)
}

/// A change to one declaration, in whichever words the channel used.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Change {
    pub symbol: String,
    /// `renamed`, `removed`, `signature`, or anything else (a body edit, an
    /// insert): then only "origin's copy of it differs from this one" counts
    pub kind: String,
    pub new_name: String,
    pub signature_after: String,
}

impl Change {
    /// From a server event (`hot` markers, gate collisions).
    pub fn from_event(event: &serde_json::Value) -> Change {
        let text = |k: &str| event.get(k).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
        let mut kind = text("kind");
        if kind == "modified" && event.get("signature_change").and_then(serde_json::Value::as_bool) == Some(true) {
            kind = "signature".into();
        }
        Change { symbol: text("symbol"), kind, new_name: text("new_name"), signature_after: text("signature_after") }
    }

    /// From a protocol op (`rename`, `signature_change`, `delete`, ...).
    pub fn from_op(op: &serde_json::Value) -> Change {
        let text = |v: &serde_json::Value, k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
        let public = op.get("public").cloned().unwrap_or(serde_json::Value::Null);
        let key = text(op, "key");
        let symbol = key.rsplit("::").next().unwrap_or("").to_string();
        let kind = match text(op, "kind").as_str() {
            "rename" | "move" => "renamed",
            "delete" => "removed",
            "signature_change" => "signature",
            other => other,
        }
        .to_string();
        Change { symbol, kind, new_name: text(&public, "new_name"), signature_after: text(&public, "new_signature") }
    }

    fn reflected_in(&self, symbols: &Symbols) -> bool {
        match self.kind.as_str() {
            "renamed" => !self.new_name.is_empty() && symbols.contains_key(&self.new_name),
            "removed" => !symbols.contains_key(&self.symbol),
            "signature" => {
                let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
                !self.signature_after.is_empty()
                    && symbols.get(&self.symbol).is_some_and(|(sig, _)| squash(sig) == squash(&self.signature_after))
            }
            _ => false,
        }
    }

    /// Whether a pull would bring this change into the copy: origin has it
    /// and the copy does not. `None` when origin cannot be read (no
    /// upstream): the caller then keeps what it already knew.
    pub fn on_origin(&self, root: &Path, rel: &str) -> Option<bool> {
        let upstream = upstream_symbols(root, rel)?;
        let local = local_symbols(root, rel).unwrap_or_default();
        Some(match self.kind.as_str() {
            "renamed" | "removed" | "signature" => self.reflected_in(&upstream) && !self.reflected_in(&local),
            // a body edit or an insert: origin's version of it is not this one
            _ => upstream.get(&self.symbol).map(|(_, h)| h) != local.get(&self.symbol).map(|(_, h)| h)
                && upstream.contains_key(&self.symbol),
        })
    }
}

/// Whether origin's copy of `rel` differs from this checkout's committed
/// one: something a pull would bring in. `None` without an upstream.
pub fn file_behind(root: &Path, rel: &str) -> Option<bool> {
    crate::git::git(root, &["rev-parse", "--verify", "-q", "@{u}"], GIT_WAIT)?;
    let blob = |rev: &str| crate::git::git(root, &["rev-parse", "-q", "--verify", &format!("{rev}:{rel}")], GIT_WAIT).map(|s| s.trim().to_string());
    Some(blob("@{u}") != blob("HEAD"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        assert!(ok.success(), "git {args:?}");
    }

    /// The phantom block, reproduced: a rename recorded somewhere (here, as a
    /// server would report it) that never reached this checkout's origin is
    /// not on origin; once a teammate pushes it, it is, until this copy pulls.
    #[test]
    fn a_rename_is_on_origin_only_when_origin_has_it_and_this_copy_does_not() {
        let base = std::env::temp_dir().join(format!("collide-origin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let origin = base.join("origin.git");
        sh(&base, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        let seed = base.join("seed");
        sh(&base, &["clone", "-q", origin.to_str().unwrap(), seed.to_str().unwrap()]);
        sh(&seed, &["config", "user.email", "s@x.com"]);
        sh(&seed, &["config", "user.name", "s"]);
        std::fs::write(seed.join("total.py"), "def amount_due(lines):\n    return sum(lines)\n").unwrap();
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-qm", "seed"]);
        sh(&seed, &["push", "-q", "origin", "HEAD"]);
        let mine = base.join("mine");
        sh(&base, &["clone", "-q", origin.to_str().unwrap(), mine.to_str().unwrap()]);

        let rename = Change { symbol: "amount_due".into(), kind: "renamed".into(), new_name: "checkout_total".into(), signature_after: String::new() };
        assert_eq!(rename.on_origin(&mine, "total.py"), Some(false), "recorded, never pushed: nothing to pull");
        assert_eq!(file_behind(&mine, "total.py"), Some(false));

        // a teammate pushes it, and this checkout fetches
        std::fs::write(seed.join("total.py"), "def checkout_total(lines):\n    return sum(lines)\n").unwrap();
        sh(&seed, &["commit", "-qam", "rename"]);
        sh(&seed, &["push", "-q", "origin", "HEAD"]);
        sh(&mine, &["fetch", "-q"]);
        assert_eq!(rename.on_origin(&mine, "total.py"), Some(true));
        assert_eq!(file_behind(&mine, "total.py"), Some(true));
        // pulled and re-read: cleared
        sh(&mine, &["pull", "-q", "--ff-only"]);
        assert_eq!(rename.on_origin(&mine, "total.py"), Some(false));
        // a copy with no upstream cannot say
        let alone = base.join("alone");
        std::fs::create_dir_all(&alone).unwrap();
        sh(&alone, &["init", "-q"]);
        assert_eq!(rename.on_origin(&alone, "total.py"), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
