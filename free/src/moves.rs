//! Renames that keep their history: files and folders moved in git, and a
//! repo whose id changed.
//!
//! The ledger is one hash chain over every scope, so no row is ever edited
//! or re-keyed. A move is recorded instead, per scope, and every read
//! translates: a row written BEFORE a move that names the old path reads
//! back under the new one. Rows written after it are untouched, so a new
//! file that later appears at the old path stays itself. Undo is a state
//! change ("undone"): translation stops and there is nothing to restore.
//!
//! The hooks find moves in the checkout's own `git log` (renames detected
//! by git, collapsed to the folder that explains them) and send them at
//! session start; a repo rename is a checkout whose origin changed under
//! the same folder. Both are applied at once, because waiting for a click
//! would split the history: everything reported under the new names in the
//! meantime would start apart from the old. The dashboard offers the undo.
//!
//! Notes and decisions are anchored to paths. They are translated where the
//! store reads them (the `memory` bucket), by the note's `created` time, and
//! the anchor carries `moved_from`; a note written back is stored under its
//! original anchor again (`restore_note`), so an undo still undoes it.

use std::collections::HashSet;

use serde_json::{json, Value};

use crate::compat::truthy;
use crate::repo::{repo_key, Aliases};
use crate::store::{now, Store};

/// Per scope: `{"moves": [{from, to, files, commit, at, by, seen, state}]}`.
pub const MOVES_BUCKET: &str = "path_moves";
/// Per workspace and new repo: `{from, to, by, at, state, error?}`.
pub const REPO_RENAMES_BUCKET: &str = "repo_renames";
/// Per scope and person: `{until}`, when they last said a banner looked right.
const SEEN_BUCKET: &str = "renames_seen";
/// How long a move or rename is told about, in briefings and on the dashboard.
pub const TELL_FOR_S: f64 = 7.0 * 86_400.0;
/// The most moves a scope keeps; past it the oldest go (and stop translating).
const MAX_KEPT: usize = 500;
/// The most moves one report may add.
const MAX_PER_REPORT: usize = 200;
const MAX_PATH: usize = 512;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

// --------------------------------------------------------------- translation

/// One applied move, ready to match: a folder (`from` ends in '/') also
/// matches the folder named bare, and a file also matches `file::symbol`.
struct Step {
    from: String,
    to: String,
    from_bare: String,
    to_bare: String,
    folder: bool,
    at: f64,
}

impl Step {
    fn apply(&self, s: &str) -> Option<String> {
        if self.folder {
            if let Some(rest) = s.strip_prefix(self.from.as_str()) {
                return Some(format!("{}{rest}", self.to));
            }
            if s == self.from_bare {
                return Some(self.to_bare.clone());
            }
            return None;
        }
        if s == self.from {
            return Some(self.to.clone());
        }
        match s.strip_prefix(self.from.as_str()) {
            Some(rest) if rest.starts_with("::") => Some(format!("{}{rest}", self.to)),
            _ => None,
        }
    }
}

/// A path's first component, cut at a symbol: the cheap test that rules
/// out almost every string before any move is tried.
fn head_of(s: &str) -> &str {
    let first = s.split('/').next().unwrap_or(s);
    first.split("::").next().unwrap_or(first)
}

/// A scope's applied moves in time order, compiled for reads.
pub struct Moves {
    list: Vec<Step>,
    heads: HashSet<String>,
    /// The newest move's time: rows at or after it are never translated.
    pub latest: f64,
}

impl Moves {
    /// The applied moves of a `path_moves` record; `None` when there are none.
    pub fn from_record(record: &Value) -> Option<Moves> {
        let mut list: Vec<Step> = record
            .get("moves")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|m| text(m, "state") == "applied")
            .filter_map(|m| {
                let (from, to) = (text(m, "from"), text(m, "to"));
                let at = m.get("at").and_then(Value::as_f64)?;
                if from.is_empty() || to.is_empty() || from == to {
                    return None;
                }
                let folder = from.ends_with('/');
                Some(Step {
                    from_bare: from.trim_end_matches('/').to_string(),
                    to_bare: to.trim_end_matches('/').to_string(),
                    from,
                    to,
                    folder,
                    at,
                })
            })
            .collect();
        if list.is_empty() {
            return None;
        }
        list.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal));
        let heads = list.iter().map(|m| head_of(&m.from).to_string()).collect();
        let latest = list.iter().map(|m| m.at).fold(f64::MIN, f64::max);
        Some(Moves { list, heads, latest })
    }

    /// `s` as it reads after every move made after `ts`, in order, so a
    /// chain a -> b -> c lands on c. `None` when nothing applies.
    pub fn translate_str(&self, s: &str, ts: f64) -> Option<String> {
        // a path never spans lines, nor runs to pages
        if ts >= self.latest || s.is_empty() || s.len() > 2 * MAX_PATH || s.contains('\n') {
            return None;
        }
        if !self.heads.contains(head_of(s)) {
            return None;
        }
        let mut current: Option<String> = None;
        for step in &self.list {
            if step.at <= ts {
                continue;
            }
            if let Some(next) = step.apply(current.as_deref().unwrap_or(s)) {
                current = Some(next);
            }
        }
        current
    }

    /// Translate every string (and object key) in a row's payload written
    /// at `ts`. True when anything changed.
    pub fn translate(&self, value: &mut Value, ts: f64) -> bool {
        if ts >= self.latest {
            return false;
        }
        match value {
            Value::String(s) => match self.translate_str(s, ts) {
                Some(moved) => {
                    *s = moved;
                    true
                }
                None => false,
            },
            Value::Array(items) => {
                let mut changed = false;
                for item in items.iter_mut() {
                    changed |= self.translate(item, ts);
                }
                changed
            }
            Value::Object(map) => {
                let mut changed = false;
                for item in map.values_mut() {
                    changed |= self.translate(item, ts);
                }
                // a map keyed by path (a row's files) is rebuilt only when a
                // key moved, in its own order
                if map.keys().any(|k| self.translate_str(k, ts).is_some()) {
                    let old = std::mem::take(map);
                    for (key, item) in old {
                        let key = self.translate_str(&key, ts).unwrap_or(key);
                        map.insert(key, item);
                    }
                    changed = true;
                }
                changed
            }
            _ => false,
        }
    }

    /// The spellings a payload search for `needle` must also try: a row
    /// written before a move holds the old path. Newest move first, so a
    /// chain's every older name is found.
    pub fn old_spellings(&self, needle: &str) -> Vec<String> {
        let mut spellings: Vec<String> = vec![needle.to_string()];
        for step in self.list.iter().rev() {
            let found: Vec<String> = spellings
                .iter()
                .filter(|s| s.contains(step.to.as_str()))
                .map(|s| s.replace(step.to.as_str(), &step.from))
                .collect();
            for s in found {
                if !spellings.contains(&s) {
                    spellings.push(s);
                }
            }
        }
        spellings.remove(0);
        spellings
    }

    /// A note or decision as it reads after the moves made since it was
    /// written: each anchor's path moves, and keeps where it was as
    /// `moved_from`. The note's words are its author's and stay as written.
    pub fn translate_note(&self, note: &mut Value) -> bool {
        let created = note.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        if created >= self.latest {
            return false;
        }
        let mut changed = false;
        let mut move_anchor = |anchor: &mut Value| {
            let Some(map) = anchor.as_object_mut() else { return };
            let path = map.get("path").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(moved) = self.translate_str(&path, created) {
                if !map.contains_key("moved_from") {
                    map.insert("moved_from".into(), json!(path));
                }
                map.insert("path".into(), json!(moved));
                changed = true;
            }
        };
        if let Some(anchor) = note.get_mut("anchor") {
            move_anchor(anchor);
        }
        if let Some(Value::Array(anchors)) = note.get_mut("anchors") {
            for anchor in anchors.iter_mut() {
                move_anchor(anchor);
            }
        }
        changed
    }
}

/// A note about to be written back (superseded, withdrawn, scored) goes in
/// under its anchors as written, so translation stays a read-time view and
/// an undo still undoes it. `None` when there is nothing to restore.
pub fn restore_note(note: &Value) -> Option<Value> {
    let moved = |a: &Value| a.get("moved_from").is_some();
    let any = note.get("anchor").is_some_and(moved)
        || note.get("anchors").and_then(Value::as_array).is_some_and(|list| list.iter().any(moved));
    if !any {
        return None;
    }
    let mut out = note.clone();
    let restore = |anchor: &mut Value| {
        if let Some(map) = anchor.as_object_mut() {
            if let Some(Value::String(from)) = map.remove("moved_from") {
                map.insert("path".into(), json!(from));
            }
        }
    };
    if let Some(anchor) = out.get_mut("anchor") {
        restore(anchor);
    }
    if let Some(Value::Array(anchors)) = out.get_mut("anchors") {
        anchors.iter_mut().for_each(restore);
    }
    Some(out)
}

// ------------------------------------------------------------ path moves

/// A reported path as it is kept: repo-relative and forward-slashed, or
/// `None` when it is not one.
fn clean_path(raw: &str) -> Option<String> {
    let path = raw.trim().replace('\\', "/");
    let bare = path.trim_end_matches('/');
    if bare.is_empty() || path.len() > MAX_PATH || path.starts_with('/') || path.contains('\n')
        || path.contains("::") || bare.split('/').any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    Some(path)
}

/// Keep what a checkout's git said moved. Deduplicated by (from, to): a
/// move another checkout already sent, or one undone, stays as it is.
pub fn record_moves(store: &Store, scope: &str, by: &str, reported: &[Value]) -> Value {
    let stamp = now();
    let mut record = store.kv_get(MOVES_BUCKET, scope).unwrap_or_else(|| json!({"moves": []}));
    let mut list: Vec<Value> = record.get("moves").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut added: Vec<Value> = Vec::new();
    for reported in reported.iter().take(MAX_PER_REPORT) {
        let (Some(from), Some(to)) = (clean_path(&text(reported, "from")), clean_path(&text(reported, "to"))) else {
            continue;
        };
        // a folder moves to a folder, a file to a file
        if from == to || from.ends_with('/') != to.ends_with('/') {
            continue;
        }
        if list.iter().chain(added.iter()).any(|m| text(m, "from") == from && text(m, "to") == to) {
            continue;
        }
        // the commit's time, never later than now (a fast clock on the
        // machine would translate rows written after the move)
        let at = reported.get("at").and_then(Value::as_f64).filter(|t| *t > 0.0).unwrap_or(stamp).min(stamp);
        let commit: String = text(reported, "commit").chars().filter(char::is_ascii_hexdigit).take(40).collect();
        let files = reported.get("files").and_then(Value::as_u64).unwrap_or(1).clamp(1, 1_000_000);
        added.push(json!({
            "from": from, "to": to, "files": files, "commit": commit, "at": at,
            "by": by, "seen": stamp, "state": "applied",
        }));
    }
    if added.is_empty() {
        return json!({"ok": true, "added": 0});
    }
    let count = added.len();
    let summary: Vec<Value> = added
        .iter()
        .map(|m| json!({"from": m["from"], "to": m["to"], "files": m["files"], "commit": m["commit"]}))
        .collect();
    list.extend(added);
    list.sort_by(|a, b| {
        let at = |m: &Value| m.get("at").and_then(Value::as_f64).unwrap_or(0.0);
        at(a).partial_cmp(&at(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    if list.len() > MAX_KEPT {
        let over = list.len() - MAX_KEPT;
        list.drain(..over);
    }
    record["moves"] = json!(list);
    let _ = store.kv_put(MOVES_BUCKET, scope, &record, stamp);
    let _ = store.ledger_append(scope, "paths_moved", &json!({"user": by, "moves": summary}), stamp);
    json!({"ok": true, "added": count})
}

/// Undo a move, or apply it again: only its state changes.
pub fn set_move_state(store: &Store, scope: &str, from: &str, to: &str, state: &str, by: &str) -> Value {
    let stamp = now();
    let Some(mut record) = store.kv_get(MOVES_BUCKET, scope) else {
        return json!({"ok": false, "error": "no moves recorded for this repo"});
    };
    let mut found = false;
    if let Some(list) = record.get_mut("moves").and_then(Value::as_array_mut) {
        for m in list.iter_mut() {
            if text(m, "from") == from.trim() && text(m, "to") == to.trim() {
                m["state"] = json!(state);
                m["changed_by"] = json!(by);
                m["changed"] = json!(stamp);
                found = true;
            }
        }
    }
    if !found {
        return json!({"ok": false, "error": format!("no move from {from} to {to} here")});
    }
    let _ = store.kv_put(MOVES_BUCKET, scope, &record, stamp);
    let kind = if state == "undone" { "path_move_undone" } else { "path_move_applied" };
    let _ = store.ledger_append(scope, kind, &json!({"user": by, "from": from.trim(), "to": to.trim()}), stamp);
    json!({"ok": true, "state": state})
}

/// Every move recorded for a scope, newest first, whatever its state.
pub fn moves_of(store: &Store, scope: &str) -> Vec<Value> {
    let mut list: Vec<Value> = store
        .kv_get(MOVES_BUCKET, scope)
        .and_then(|r| r.get("moves").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    list.reverse();
    list
}

// ----------------------------------------------------------- repo renames

fn rename_key(workspace_id: &str, new_id: &str) -> String {
    format!("{workspace_id}:{}", repo_key(new_id))
}

fn put_rename(store: &Store, workspace_id: &str, old_id: &str, new_id: &str, by: &str, state: &str, error: &str) -> Value {
    let stamp = now();
    let key = rename_key(workspace_id, new_id);
    let at = store.kv_get(REPO_RENAMES_BUCKET, &key).and_then(|r| r.get("at").and_then(Value::as_f64)).unwrap_or(stamp);
    let mut record = json!({"from": old_id, "to": new_id, "by": by, "at": at, "seen": stamp, "state": state});
    if !error.is_empty() {
        record["error"] = json!(error);
    }
    let _ = store.kv_put(REPO_RENAMES_BUCKET, &key, &record, stamp);
    record
}

/// A checkout said its repo is now `new_id` and was `old_id` (its origin
/// changed under the same folder). The caller has already been checked: a
/// member who may record in `old_id`. Carried over at once when `new_id` has
/// no history of its own here; otherwise it waits for the owner to confirm,
/// since aliasing would hide that history.
pub fn repo_rename_reported(store: &Store, aliases: &Aliases, workspace_id: &str, old_id: &str, new_id: &str, by: &str) -> Value {
    let (old_id, new_id) = (old_id.trim(), new_id.trim());
    if old_id.is_empty() || new_id.is_empty() || repo_key(old_id) == repo_key(new_id) {
        return json!({"ok": false, "error": "need two different repo ids"});
    }
    if aliases.scope_for(workspace_id, old_id) == aliases.scope_for(workspace_id, new_id) {
        return json!({"ok": true, "state": "applied", "already": true});
    }
    // nothing recorded under the old name: nothing to carry, and no reason
    // to tie the new name to it
    if !crate::rename::scope_has_data(store, aliases, workspace_id, old_id) {
        return json!({"ok": false, "error": format!("nothing is recorded under {old_id} here")});
    }
    let key = rename_key(workspace_id, new_id);
    if let Some(prior) = store.kv_get(REPO_RENAMES_BUCKET, &key) {
        // a person's "no" or undo stands
        let state = text(&prior, "state");
        if text(&prior, "from") == old_id && matches!(state.as_str(), "dismissed" | "undone" | "pending") {
            return json!({"ok": true, "state": state});
        }
    }
    if crate::rename::scope_has_data(store, aliases, workspace_id, new_id) {
        put_rename(store, workspace_id, old_id, new_id, by, "pending", "the new name already has history of its own");
        return json!({"ok": true, "state": "pending"});
    }
    let relinked = crate::watch::relink(store, aliases, workspace_id, old_id, new_id, by, false);
    if !truthy(relinked.get("ok")) {
        let error = text(&relinked, "error");
        put_rename(store, workspace_id, old_id, new_id, by, "pending", &error);
        return json!({"ok": true, "state": "pending", "error": error});
    }
    put_rename(store, workspace_id, old_id, new_id, by, "applied", "");
    let _ = store.ledger_append(
        &aliases.scope_for(workspace_id, old_id),
        "repo_renamed",
        &json!({"user": by, "from": old_id, "to": new_id, "source": "checkout"}),
        now(),
    );
    json!({"ok": true, "state": "applied", "canonical": relinked["canonical"], "preferred": relinked["preferred"]})
}

/// The recorded rename into `new_id` from `old_id`, if there is one.
pub fn repo_rename(store: &Store, workspace_id: &str, old_id: &str, new_id: &str) -> Option<Value> {
    store.kv_get(REPO_RENAMES_BUCKET, &rename_key(workspace_id, new_id)).filter(|r| text(r, "from") == old_id.trim())
}

/// A pending rename confirmed: carried over, over the new name's own
/// history (which stays where it is, behind the alias, until an undo).
pub fn confirm_repo_rename(store: &Store, aliases: &Aliases, workspace_id: &str, old_id: &str, new_id: &str, by: &str) -> Value {
    let relinked = crate::watch::relink(store, aliases, workspace_id, old_id.trim(), new_id.trim(), by, true);
    if !truthy(relinked.get("ok")) {
        return relinked;
    }
    put_rename(store, workspace_id, old_id.trim(), new_id.trim(), by, "applied", "");
    let _ = store.ledger_append(
        &aliases.scope_for(workspace_id, old_id),
        "repo_renamed",
        &json!({"user": by, "from": old_id.trim(), "to": new_id.trim(), "source": "confirmed"}),
        now(),
    );
    json!({"ok": true, "state": "applied"})
}

/// Undo a carried-over rename: the new name resolves on its own again.
pub fn undo_repo_rename(store: &Store, aliases: &Aliases, workspace_id: &str, old_id: &str, new_id: &str, by: &str) -> Value {
    let unlinked = crate::watch::api_unalias_repo(store, aliases, workspace_id, &json!({"from": new_id.trim()}));
    if !truthy(unlinked.get("ok")) {
        return unlinked;
    }
    put_rename(store, workspace_id, old_id.trim(), new_id.trim(), by, "undone", "");
    json!({"ok": true, "state": "undone"})
}

/// A pending rename turned down: the two names stay two repos.
pub fn dismiss_repo_rename(store: &Store, workspace_id: &str, old_id: &str, new_id: &str, by: &str) -> Value {
    put_rename(store, workspace_id, old_id.trim(), new_id.trim(), by, "dismissed", "");
    json!({"ok": true, "state": "dismissed"})
}

/// The workspace's recorded repo renames that touch `scope`: into it, or
/// (pending) between it and another name.
pub fn repo_renames_for(store: &Store, aliases: &Aliases, workspace_id: &str, scope: &str) -> Vec<Value> {
    let mut found: Vec<Value> = store
        .kv_list(REPO_RENAMES_BUCKET, &format!("{workspace_id}:"))
        .into_iter()
        .map(|(_, r)| r)
        .filter(|r| {
            aliases.scope_for(workspace_id, &text(r, "to")) == scope
                || aliases.scope_for(workspace_id, &text(r, "from")) == scope
        })
        .collect();
    found.sort_by(|a, b| {
        let at = |m: &Value| m.get("seen").and_then(Value::as_f64).unwrap_or(0.0);
        at(b).partial_cmp(&at(a)).unwrap_or(std::cmp::Ordering::Equal)
    });
    found
}

// ------------------------------------------------------- telling people

fn seen_key(scope: &str, uid: &str) -> String {
    format!("{scope}:{uid}")
}

/// When this person last said the renames on a repo looked right.
pub fn seen_until(store: &Store, scope: &str, uid: &str) -> f64 {
    store.kv_get(SEEN_BUCKET, &seen_key(scope, uid)).and_then(|r| r.get("until").and_then(Value::as_f64)).unwrap_or(0.0)
}

pub fn mark_seen(store: &Store, scope: &str, uid: &str) {
    let stamp = now();
    let _ = store.kv_put(SEEN_BUCKET, &seen_key(scope, uid), &json!({"until": stamp}), stamp);
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} more {}", if n == 1 { one } else { many })
}

/// One sentence for a set of moves: the biggest folder named, the rest
/// counted, and the commit that made the named one.
pub fn moves_sentence(moves: &[Value]) -> Option<String> {
    let mut ordered: Vec<&Value> = moves.iter().collect();
    // folders first, the most files first
    ordered.sort_by_key(|m| (!text(m, "from").ends_with('/'), std::cmp::Reverse(m.get("files").and_then(Value::as_u64).unwrap_or(1))));
    let first = ordered.first()?;
    let folders = ordered.iter().filter(|m| text(m, "from").ends_with('/')).count();
    let files = ordered.len() - folders;
    let first_is_folder = text(first, "from").ends_with('/');
    let (more_folders, more_files) = if first_is_folder { (folders - 1, files) } else { (folders, files - 1) };
    let mut more: Vec<String> = Vec::new();
    if more_folders > 0 {
        more.push(plural(more_folders, "folder", "folders"));
    }
    if more_files > 0 {
        more.push(if more_folders > 0 { format!("{more_files} {}", if more_files == 1 { "file" } else { "files" }) } else { plural(more_files, "file", "files") });
    }
    let commit: String = text(first, "commit").chars().take(7).collect();
    let mut aside: Vec<String> = Vec::new();
    if !more.is_empty() {
        aside.push(format!("and {}", more.join(" and ")));
    }
    if !commit.is_empty() {
        aside.push(format!("commit {commit}"));
    }
    let aside = if aside.is_empty() { String::new() } else { format!(" ({})", aside.join(", ")) };
    let (whose, paths) = if ordered.len() == 1 { ("its", "path") } else { ("their", "paths") };
    Some(format!(
        "{} was renamed {}{aside}; {whose} history, notes and decisions follow the new {paths}.",
        text(first, "from"), text(first, "to"),
    ))
}

/// The session-start briefing's lines about renames: each person is told
/// once, for a week after Cork learned of it.
pub fn brief_lines(store: &Store, aliases: &Aliases, workspace_id: &str, scope: &str, user: &str) -> Vec<String> {
    let stamp = now();
    let mut lines: Vec<String> = Vec::new();
    for rename in repo_renames_for(store, aliases, workspace_id, scope) {
        let seen = rename.get("seen").and_then(Value::as_f64).unwrap_or(0.0);
        if stamp - seen > TELL_FOR_S {
            continue;
        }
        let (from, to) = (text(&rename, "from"), text(&rename, "to"));
        let said = format!("renamesaid:{scope}:{user}:{}:{}", repo_key(&to), text(&rename, "state"));
        let line = match text(&rename, "state").as_str() {
            "applied" => format!("Cork: this repo was renamed {from} to {to}; its history, notes and decisions carried over to the new name."),
            "pending" => format!(
                "Cork: this checkout's origin changed from {from} to {to}, which already has history of its own here, so Cork \
kept the two apart. The workspace owner can carry {from}'s history over from the repo's Overview on the dashboard."
            ),
            _ => continue,
        };
        if crate::access::tell_once(store, &said, TELL_FOR_S + 86_400.0) {
            lines.push(line);
        }
    }
    let told_key = format!("movesaid:{scope}:{user}");
    let told = store.eph_get(&told_key).and_then(|r| r.get("upto").and_then(Value::as_f64)).unwrap_or(0.0);
    let fresh: Vec<Value> = moves_of(store, scope)
        .into_iter()
        .filter(|m| text(m, "state") == "applied")
        .filter(|m| {
            let seen = m.get("seen").and_then(Value::as_f64).unwrap_or(0.0);
            seen > told && stamp - seen <= TELL_FOR_S
        })
        .collect();
    if let Some(sentence) = moves_sentence(&fresh) {
        let upto = fresh.iter().filter_map(|m| m.get("seen").and_then(Value::as_f64)).fold(told, f64::max);
        let _ = store.eph_set(&told_key, &json!({"upto": upto}), Some(TELL_FOR_S + 86_400.0));
        lines.push(format!("Cork: {sentence}"));
    }
    lines
}

/// What the dashboard shows for one repo: its recent moves and repo
/// renames, every state, and when this person last said they looked right.
pub fn listing(store: &Store, aliases: &Aliases, workspace_id: &str, repo_id: &str, uid: &str) -> Value {
    let scope = aliases.scope_for(workspace_id, repo_id);
    let stamp = now();
    let moves: Vec<Value> = moves_of(store, &scope)
        .into_iter()
        .filter(|m| stamp - m.get("seen").and_then(Value::as_f64).unwrap_or(0.0) <= 4.0 * TELL_FOR_S)
        .take(100)
        .collect();
    let mut renames = repo_renames_for(store, aliases, workspace_id, &scope);
    renames.retain(|r| text(r, "state") == "pending" || stamp - r.get("seen").and_then(Value::as_f64).unwrap_or(0.0) <= 4.0 * TELL_FOR_S);
    json!({
        "ok": true, "repo": repo_id, "moves": moves, "repo_renames": renames,
        "seen_until": seen_until(store, &scope, uid), "tell_for_s": TELL_FOR_S,
    })
}

/// The moves of a report body that is well formed enough to keep.
pub fn reported_moves(body: &Value) -> Vec<Value> {
    body.get("moves").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(Value::is_object).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("in-memory store")
    }

    fn moved(store: &Store, scope: &str, list: &[(&str, &str, f64)]) {
        let reported: Vec<Value> =
            list.iter().map(|(from, to, at)| json!({"from": from, "to": to, "files": 3, "commit": "921baf7aa", "at": at})).collect();
        let result = record_moves(store, scope, "alice", &reported);
        assert_eq!(result["ok"], true, "{result}");
    }

    const SCOPE: &str = "ws1:github.com/acme/app";

    #[test]
    fn a_row_written_before_a_folder_move_reads_back_under_the_new_path() {
        let store = store();
        let t = now() - 1000.0;
        store.ledger_append(SCOPE, "edit_reported", &json!({"user": "alice", "path": "server/collide-rs/src/a.rs",
            "symbols": ["server/collide-rs/src/a.rs::run"], "note": "server/collide-rs"}), t - 10.0).unwrap();
        store.ledger_append(SCOPE, "edit_reported", &json!({"user": "bob", "path": "server/collide-rs/src/new.rs"}), t + 10.0).unwrap();
        moved(&store, SCOPE, &[("server/collide-rs/", "server/cork-rs/", t)]);

        let rows = store.ledger_since(SCOPE, 0.0);
        let edits: Vec<&crate::store::LedgerRow> = rows.iter().filter(|r| r.kind == "edit_reported").collect();
        assert_eq!(edits[0].payload["path"], "server/cork-rs/src/a.rs");
        assert_eq!(edits[0].payload["symbols"][0], "server/cork-rs/src/a.rs::run");
        assert_eq!(edits[0].payload["note"], "server/cork-rs", "the folder named bare moves too");
        // written after the move: a new file at the old path stays itself
        assert_eq!(edits[1].payload["path"], "server/collide-rs/src/new.rs");

        let kinds = store.ledger_since_kinds(SCOPE, 0.0, &["edit_reported"]);
        assert_eq!(kinds[0].payload["path"], "server/cork-rs/src/a.rs");
        let tail = store.ledger_tail(SCOPE, 10, None);
        assert_eq!(tail.iter().find(|r| r.kind == "edit_reported").unwrap().payload["path"], "server/cork-rs/src/a.rs");
        let page = store.ledger_page(SCOPE, 10, None);
        assert_eq!(page.iter().find(|r| r["kind"] == "edit_reported").unwrap()["payload"]["path"], "server/cork-rs/src/a.rs");
        let fields = store.ledger_fields_since(SCOPE, 0.0, &["edit_reported"], &["user", "path"], 10);
        let alice = fields.iter().find(|(_, _, f)| f[0] == "alice").unwrap();
        assert_eq!(alice.2[1], "server/cork-rs/src/a.rs");
        let bob = fields.iter().find(|(_, _, f)| f[0] == "bob").unwrap();
        assert_eq!(bob.2[1], "server/collide-rs/src/new.rs");

        // a search for the new path finds the row that holds the old one,
        // and a row after the move under the old path is not passed off as it
        let found = store.ledger_since_containing(SCOPE, 0.0, &["server/cork-rs/src/"]);
        assert_eq!(found.len(), 1, "{:?}", found.iter().map(|r| r.payload.clone()).collect::<Vec<_>>());
        assert_eq!(found[0].payload["path"], "server/cork-rs/src/a.rs");
        let old = store.ledger_since_containing(SCOPE, 0.0, &["server/collide-rs/src/new.rs"]);
        assert_eq!(old.len(), 1);
    }

    #[test]
    fn the_in_memory_tail_translates_as_the_disk_does() {
        let store = store();
        let t = now() - 60.0;
        // a recent read first, so the tail exists before the row and the move
        let _ = store.ledger_since(SCOPE, t - 100.0);
        store.ledger_append(SCOPE, "edit_reported", &json!({"path": "lib/x.py"}), t - 5.0).unwrap();
        moved(&store, SCOPE, &[("lib/x.py", "src/x.py", t)]);
        let rows = store.ledger_since(SCOPE, t - 100.0);
        let edit = rows.iter().find(|r| r.kind == "edit_reported").unwrap();
        assert_eq!(edit.payload["path"], "src/x.py");
    }

    #[test]
    fn moves_chain_in_time_order_and_undo_stops_translation() {
        let store = store();
        let t = now() - 1000.0;
        store.ledger_append(SCOPE, "edit_reported", &json!({"path": "a/m.rs"}), t - 50.0).unwrap();
        store.ledger_append(SCOPE, "edit_reported", &json!({"path": "b/m.rs"}), t + 50.0).unwrap();
        moved(&store, SCOPE, &[("a/", "b/", t), ("b/", "c/", t + 100.0)]);
        let rows = store.ledger_since_kinds(SCOPE, 0.0, &["edit_reported"]);
        assert_eq!(rows[0].payload["path"], "c/m.rs", "a -> b -> c");
        assert_eq!(rows[1].payload["path"], "c/m.rs", "written at b, moved on to c");

        let undone = set_move_state(&store, SCOPE, "b/", "c/", "undone", "alice");
        assert_eq!(undone["ok"], true, "{undone}");
        let rows = store.ledger_since_kinds(SCOPE, 0.0, &["edit_reported"]);
        assert_eq!(rows[0].payload["path"], "b/m.rs");
        assert_eq!(rows[1].payload["path"], "b/m.rs");
        set_move_state(&store, SCOPE, "a/", "b/", "undone", "alice");
        let rows = store.ledger_since_kinds(SCOPE, 0.0, &["edit_reported"]);
        assert_eq!(rows[0].payload["path"], "a/m.rs", "nothing to restore: the row was never changed");

        // the same move sent again stays undone
        moved(&store, SCOPE, &[("a/", "b/", t)]);
        let rows = store.ledger_since_kinds(SCOPE, 0.0, &["edit_reported"]);
        assert_eq!(rows[0].payload["path"], "a/m.rs");
    }

    #[test]
    fn a_scope_without_moves_reads_rows_as_written() {
        let store = store();
        store.ledger_append(SCOPE, "edit_reported", &json!({"path": "a/m.rs"}), 1.0).unwrap();
        moved(&store, "ws1:github.com/acme/other", &[("a/", "b/", 100.0)]);
        assert_eq!(store.ledger_since(SCOPE, 0.0)[0].payload["path"], "a/m.rs");
    }

    #[test]
    fn bad_paths_are_not_kept() {
        let store = store();
        let result = record_moves(&store, SCOPE, "alice", &[
            json!({"from": "/etc/", "to": "x/"}),
            json!({"from": "../a", "to": "b"}),
            json!({"from": "a/", "to": "b.rs"}),
            json!({"from": "a", "to": "a"}),
            json!({"from": "", "to": "b/"}),
        ]);
        assert_eq!(result["added"], 0, "{result}");
    }

    #[test]
    fn a_note_anchored_at_the_old_path_is_found_and_judged_at_the_new_one() {
        let store = store();
        let t = now() - 1000.0;
        let note = json!({"id": "n1", "fact": "server/collide-rs/src/x.rs holds the parser", "created": t - 100.0, "tags": [],
            "anchor": {"kind": "symbol", "path": "server/collide-rs/src/x.rs", "symbol": "parse"}});
        store.kv_put("memory", &format!("{SCOPE}:n1"), &note, t - 100.0).unwrap();
        let decision = json!({"id": "d1", "kind": "decision", "fact": "keep it", "created": t - 100.0, "tags": [],
            "anchor": {"kind": "dir", "path": "server/collide-rs"},
            "anchors": [{"kind": "dir", "path": "server/collide-rs"}, {"kind": "file", "path": "README.md"}]});
        store.kv_put("memory", &format!("{SCOPE}:d1"), &decision, t - 100.0).unwrap();
        // written after the move: its anchor is its own
        let later = json!({"id": "n2", "fact": "new", "created": t + 10.0, "tags": [],
            "anchor": {"kind": "file", "path": "server/collide-rs/src/x.rs"}});
        store.kv_put("memory", &format!("{SCOPE}:n2"), &later, t + 10.0).unwrap();
        moved(&store, SCOPE, &[("server/collide-rs/", "server/cork-rs/", t)]);

        let got = store.kv_get("memory", &format!("{SCOPE}:n1")).unwrap();
        assert_eq!(got["anchor"]["path"], "server/cork-rs/src/x.rs");
        assert_eq!(got["anchor"]["moved_from"], "server/collide-rs/src/x.rs");
        assert_eq!(got["fact"], "server/collide-rs/src/x.rs holds the parser", "the words stay as written");
        assert_eq!(store.kv_get("memory", &format!("{SCOPE}:n2")).unwrap()["anchor"]["path"], "server/collide-rs/src/x.rs");

        // the readers find it at the new path: locality, decisions, the honesty layer
        let found = crate::memory::locality_memories(&store, SCOPE, &["server/cork-rs/src/x.rs".to_string()], &[], 10);
        assert!(found.iter().any(|m| m["id"] == "n1"), "{found:?}");
        let covering = crate::decisions::covering(&store, SCOPE, "server/cork-rs/src/lib.rs");
        assert_eq!(covering.len(), 1, "{covering:?}");
        assert_eq!(covering[0]["anchors"][1]["path"], "README.md");
        let annotated = crate::memory::annotate(&store, SCOPE, &[got.clone()]);
        assert_eq!(annotated[0]["anchor"]["path"], "server/cork-rs/src/x.rs");

        // written back, it is stored as written; read again, moved again
        store.kv_put("memory", &format!("{SCOPE}:n1"), &got, t + 20.0).unwrap();
        let workspace_wide = store.kv_list("memory", "ws1:");
        assert!(workspace_wide.iter().any(|(_, m)| m["id"] == "n1" && m["anchor"]["path"] == "server/cork-rs/src/x.rs"));
        set_move_state(&store, SCOPE, "server/collide-rs/", "server/cork-rs/", "undone", "alice");
        let back = store.kv_get("memory", &format!("{SCOPE}:n1")).unwrap();
        assert_eq!(back["anchor"]["path"], "server/collide-rs/src/x.rs");
        assert!(back["anchor"].get("moved_from").is_none());
    }

    fn repo_store(scopes: &[&str]) -> (Store, Aliases) {
        let store = store();
        for scope in scopes {
            store.put_tree(scope, "alice", &json!({"root": "r", "dirs": {}, "files": {}}), 1.0).unwrap();
        }
        let aliases = Aliases::new();
        aliases.seed(&scopes.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        (store, aliases)
    }

    const OLD: &str = "github.com/lithometric/collide-cloud";
    const NEW: &str = "github.com/lithometric/cork-cloud";

    #[test]
    fn a_reported_rename_carries_the_repo_over_when_the_new_name_is_clean() {
        let (store, aliases) = repo_store(&["ws1:github.com/lithometric/collide-cloud"]);
        let result = repo_rename_reported(&store, &aliases, "ws1", OLD, NEW, "alice");
        assert_eq!(result["state"], "applied", "{result}");
        assert_eq!(aliases.scope_for("ws1", NEW), aliases.scope_for("ws1", OLD));
        assert_eq!(crate::envelope::preferred_repo_id(&store, &aliases, "ws1", OLD), NEW);
        assert_eq!(repo_rename(&store, "ws1", OLD, NEW).unwrap()["state"], "applied");

        // told once in the session-start briefing, then not again
        let scope = aliases.scope_for("ws1", NEW);
        let first = brief_lines(&store, &aliases, "ws1", &scope, "alice");
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].contains("renamed github.com/lithometric/collide-cloud to github.com/lithometric/cork-cloud"), "{first:?}");
        assert!(brief_lines(&store, &aliases, "ws1", &scope, "alice").is_empty());
        assert_eq!(brief_lines(&store, &aliases, "ws1", &scope, "bob").len(), 1, "once per person");

        // undo: the new name resolves on its own again
        let undone = undo_repo_rename(&store, &aliases, "ws1", OLD, NEW, "alice");
        assert_eq!(undone["ok"], true, "{undone}");
        assert_ne!(aliases.scope_for("ws1", NEW), aliases.scope_for("ws1", OLD));
        // and the same report does not redo it
        let again = repo_rename_reported(&store, &aliases, "ws1", OLD, NEW, "alice");
        assert_eq!(again["state"], "undone", "{again}");
        assert_ne!(aliases.scope_for("ws1", NEW), aliases.scope_for("ws1", OLD));
    }

    #[test]
    fn a_reported_rename_waits_when_the_new_name_has_history() {
        let (store, aliases) = repo_store(&["ws1:github.com/lithometric/collide-cloud", "ws1:github.com/lithometric/cork-cloud"]);
        let result = repo_rename_reported(&store, &aliases, "ws1", OLD, NEW, "alice");
        assert_eq!(result["state"], "pending", "{result}");
        assert_ne!(aliases.scope_for("ws1", NEW), aliases.scope_for("ws1", OLD), "nothing aliased yet");
        let listed = listing(&store, &aliases, "ws1", NEW, "uid1");
        assert_eq!(listed["repo_renames"][0]["state"], "pending", "{listed}");
        let line = brief_lines(&store, &aliases, "ws1", &aliases.scope_for("ws1", NEW), "alice");
        assert!(line[0].contains("kept the two apart"), "{line:?}");

        let confirmed = confirm_repo_rename(&store, &aliases, "ws1", OLD, NEW, "owner");
        assert_eq!(confirmed["state"], "applied", "{confirmed}");
        assert_eq!(aliases.scope_for("ws1", NEW), aliases.scope_for("ws1", OLD));
    }

    #[test]
    fn a_rename_of_a_repo_with_no_history_is_not_recorded() {
        let (store, aliases) = repo_store(&[]);
        let result = repo_rename_reported(&store, &aliases, "ws1", OLD, NEW, "alice");
        assert_eq!(result["ok"], false, "{result}");
        assert!(repo_rename(&store, "ws1", OLD, NEW).is_none());
    }

    #[test]
    fn the_briefing_tells_each_person_about_moves_once() {
        let store = store();
        let t = now() - 1000.0;
        let reported: Vec<Value> = [
            ("server/collide-rs/", "server/cork-rs/", 131), ("server/collide-hooks/", "server/cork-hooks/", 58),
            ("collide-npm/", "cork-npm/", 8), ("nextjs-app/content/blog/collide-vs-zed.md", "nextjs-app/content/blog/cork-vs-zed.md", 1),
        ]
        .iter()
        .map(|(from, to, files)| json!({"from": from, "to": to, "files": files, "commit": "921baf7aa", "at": t}))
        .collect();
        record_moves(&store, SCOPE, "alice", &reported);
        let aliases = Aliases::new();
        let lines = brief_lines(&store, &aliases, "ws1", SCOPE, "alice");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Cork: server/collide-rs/ was renamed server/cork-rs/ (and 2 more folders and 1 file, commit 921baf7)"), "{lines:?}");
        assert!(lines[0].ends_with("their history, notes and decisions follow the new paths."), "{lines:?}");
        assert!(brief_lines(&store, &aliases, "ws1", SCOPE, "alice").is_empty(), "told once");
        assert_eq!(brief_lines(&store, &aliases, "ws1", SCOPE, "bob").len(), 1);
        // a move learned later is told on its own
        moved(&store, SCOPE, &[("docs/a.md", "docs/b.md", t)]);
        let next = brief_lines(&store, &aliases, "ws1", SCOPE, "alice");
        assert_eq!(next, vec!["Cork: docs/a.md was renamed docs/b.md (commit 921baf7); its history, notes and decisions follow the new path.".to_string()]);
    }
}
