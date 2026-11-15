//! The linter whose rules are your teammates' changes.
//!
//! A rename, removal or signature change made by one agent becomes a
//! short-lived rule for everyone else's next report. The scan happens at the
//! only moment source exists — in memory, at report time — and the content is
//! discarded immediately afterwards, the same promise parsing makes.
//!
//! Limits keep it from becoming noise an agent learns to ignore:
//!
//! - a rule names its file, and a word is a finding only when it refers to
//!   that file's symbol: the file imports it (or its module), or is that
//!   file. Matching the bare word flagged axum's `post` over a teammate's
//!   `post` rename, `messages` in page text, and every `<SiteNav />`;
//! - a rule reaches a file only from its branch, or once landed: one
//!   person's worktrees and side sessions on other bases no longer lint
//!   each other, and a rule the graph's latest report of its file
//!   contradicts (a copy on another base reported it back) is dropped;
//! - rules expire after seven days once landed, after one day if not, and
//!   names shorter than four characters are skipped because `id` or `run`
//!   matches everything. Your own changes never lint your own file.

use std::collections::BTreeMap;

use regex::Regex;
use serde_json::{json, Map, Value};

use cork_core::engine::ParseOutput;

use crate::store::{now, Store};

const RULE_TTL_S: f64 = 7.0 * 86400.0;
/// A change nobody landed lints for a day: a branch that never merges stops
/// being anyone's rule. A landed change keeps the week.
const UNLANDED_TTL_S: f64 = 86400.0;
const MAX_RULES_PER_BUCKET: usize = 200;
const MAX_FINDINGS: usize = 10;
const MAX_HITS_PER_SYMBOL: usize = 10;
const MIN_LINTABLE_NAME: usize = 4;
const BUCKETS: [&str; 3] = ["renames", "removed", "signatures"];

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn clip(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// Workspace setting: off (compute nothing), findings (report only, the
/// default), propose (findings plus fix instructions), auto (mechanical fixes
/// applied by the hook's applier).
pub fn autofix_mode(store: &Store, workspace_id: &str) -> String {
    let mode = store
        .kv_get("autofix", workspace_id)
        .map(|record| text(&record, "mode"))
        .unwrap_or_default();
    match mode.as_str() {
        "off" | "findings" | "propose" | "auto" => mode,
        _ => "findings".to_string(),
    }
}

/// Line numbers where a name appears as a whole word. Short names are skipped:
/// matching `id` or `run` across a file produces findings nobody can act on.
fn occurrences(lines: &[&str], name: &str) -> Vec<usize> {
    if name.chars().count() < MIN_LINTABLE_NAME {
        return Vec::new();
    }
    let Ok(pattern) = Regex::new(&format!(r"\b{}\b", regex::escape(name))) else {
        return Vec::new();
    };
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| pattern.is_match(line))
        .map(|(index, _)| index + 1)
        .take(MAX_HITS_PER_SYMBOL)
        .collect()
}

/// The file being linted: where it is and what its parse says it imports
/// and defines. A word is a finding only when it names the changed symbol.
pub struct Reader<'a> {
    pub path: &'a str,
    pub parsed: Option<&'a ParseOutput>,
}

/// Flag references to symbols OTHER people renamed, removed or re-signatured
/// recently. Advisory and capped.
pub fn lint_report(store: &Store, scope: &str, user_id: &str, session: &str, reader: &Reader, content: &str) -> Vec<Value> {
    let lines: Vec<&str> = content.lines().collect();
    lint_with(store, scope, user_id, session, reader, &|name| occurrences(&lines, name))
}

/// The same lint over a hook's local parse: `{name: [lines]}` instead of the
/// file.
pub fn lint_names(store: &Store, scope: &str, user_id: &str, session: &str, reader: &Reader, names: &Value) -> Vec<Value> {
    let lookup = |name: &str| -> Vec<usize> {
        if name.chars().count() < MIN_LINTABLE_NAME {
            return Vec::new();
        }
        names
            .get(name)
            .and_then(Value::as_array)
            .map(|lines| lines.iter().filter_map(Value::as_u64).map(|n| n as usize).take(MAX_HITS_PER_SYMBOL).collect())
            .unwrap_or_default()
    };
    lint_with(store, scope, user_id, session, reader, &lookup)
}

/// `Config.load` → `load`, `crate::x::y` → `y`.
fn last_segment(name: &str) -> &str {
    name.rsplit(['.', ':']).next().unwrap_or(name)
}

/// `Config.load` → `Config`.
fn first_segment(name: &str) -> &str {
    name.split(['.', ':']).next().unwrap_or(name)
}

/// A signature as compared: whitespace and line breaks are formatting, not
/// interface.
fn squashed(signature: &str) -> String {
    signature.chars().filter(|c| !c.is_whitespace()).collect()
}

/// A signature's parameters as a call sees them: how many, and how many of
/// them may be left out (a default, `?`, a rest parameter). None when there
/// is no parameter list to read.
fn param_shape(signature: &str) -> Option<(usize, usize)> {
    let open = signature.find('(')?;
    let mut depth = 0i32;
    let mut params: Vec<String> = vec![String::new()];
    for c in signature[open..].chars() {
        match c {
            '(' | '[' | '{' | '<' => {
                depth += 1;
                if depth == 1 {
                    continue;
                }
            }
            ')' | ']' | '}' | '>' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            ',' if depth == 1 => {
                params.push(String::new());
                continue;
            }
            _ => {}
        }
        if let Some(last) = params.last_mut() {
            last.push(c);
        }
    }
    let params: Vec<String> = params
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty() && !matches!(p.as_str(), "self" | "&self" | "&mut self" | "cls" | "this" | "/" | "*"))
        .collect();
    let optional = params
        .iter()
        .filter(|p| p.contains('=') || p.starts_with('*') || p.starts_with("...") || p.split(':').next().is_some_and(|n| n.trim_end().ends_with('?')))
        .count();
    Some((params.len(), optional))
}

/// Two signatures every call of one fits the other: the same number of
/// parameters, as many optional. Unknown when either is unknown.
fn same_shape(before: &str, after: &str) -> bool {
    match (param_shape(before), param_shape(after)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// How this file reaches `symbol` of `rule_path`, if it does: the name to
/// look for in it, and how it got there (for the finding). A bare word is
/// not a reference: an axum `post` import is not the `post` a teammate
/// renamed in another module, and `messages` in page text is no symbol at
/// all. The file must be the symbol's own file (without redefining it), or
/// import the symbol, or import its module.
fn binding(
    reader: &Reader, rule_path: &str, symbol: &str,
    resolves_to: &mut dyn FnMut(&str, &str) -> bool,
) -> Option<(String, String)> {
    let parsed = reader.parsed?;
    let bare = last_segment(symbol);
    let head = first_segment(symbol);
    if rule_path == reader.path {
        // the same file: a copy that still defines it has its own version,
        // and the gate speaks to two copies of one file
        let defines = parsed.symbols.iter().any(|s| s.name == symbol || last_segment(&s.name) == bare);
        return (!defines).then(|| (bare.to_string(), "defined in this file".to_string()));
    }
    let glob_binds = matches!(parsed.language, "python" | "rust");
    let join = |module: &str, name: &str| -> Option<String> {
        match parsed.language {
            "rust" => Some(if module.is_empty() { name.to_string() } else { format!("{module}::{name}") }),
            "python" => Some(if module.ends_with('.') { format!("{module}{name}") } else { format!("{module}.{name}") }),
            _ => None,
        }
    };
    for import in &parsed.imports {
        if import.module.is_empty() {
            continue;
        }
        let module_is_it = resolves_to(&import.module, rule_path);
        for (local, original) in &import.names {
            if module_is_it {
                match original.as_str() {
                    "*" if local == "*" => {
                        if glob_binds {
                            return Some((bare.to_string(), format!("glob import of {rule_path}")));
                        }
                    }
                    "*" => return Some((bare.to_string(), format!("`{local}` is {rule_path}"))),
                    "default" if local == bare => {
                        return Some((local.clone(), format!("`{local}` imported from {rule_path}")));
                    }
                    o if o == bare && head == bare => {
                        return Some((local.clone(), format!("`{local}` imported from {rule_path}")));
                    }
                    o if o == head && head != bare => {
                        return Some((bare.to_string(), format!("`{local}` imported from {rule_path}")));
                    }
                    _ => {}
                }
            } else if !matches!(original.as_str(), "*" | "default") {
                // `from pkg import core`, `use crate::lint;`: the name is the module
                if let Some(joined) = join(&import.module, original) {
                    if resolves_to(&joined, rule_path) {
                        return Some((bare.to_string(), format!("`{local}` is {rule_path}")));
                    }
                }
            }
        }
    }
    None
}

/// Whether the change is still what the repo's graph says the file holds.
/// One person's copies on different bases report one file in turn, and each
/// turn reads as a rename, removal or new signature (and back): a rule the
/// latest report of its own file contradicts is that echo, not news.
fn still_true(store: &Store, scope: &str, bucket: &str, info: &Value) -> bool {
    let symbols = crate::codegraph::symbols_of(store, scope, &text(info, "path"));
    if symbols.is_empty() {
        return true; // nothing known: the rule stands
    }
    let symbol = text(info, "symbol");
    match bucket {
        "renames" => symbols.contains_key(&text(info, "new")),
        "removed" => !symbols.contains_key(&symbol),
        _ => {
            let signature = text(info, "signature");
            signature.is_empty()
                || symbols
                    .get(&symbol)
                    .map(|s| squashed(&text(s, "signature")) == squashed(&signature))
                    .unwrap_or(false)
        }
    }
}

fn lint_with(
    store: &Store, scope: &str, user_id: &str, session: &str, reader: &Reader,
    occurrences_of: &dyn Fn(&str) -> Vec<usize>,
) -> Vec<Value> {
    let Some(registry) = store.kv_get("semlint", scope) else { return Vec::new() };
    let stamp = now();
    let cutoff = stamp - RULE_TTL_S;

    let bucket = |name: &str| -> Map<String, Value> {
        registry.get(name).and_then(Value::as_object).cloned().unwrap_or_default()
    };
    // a change reaches this file only when it can reach its branch: made on
    // the same branch, or landed. This agent's own change never lints its own
    // file; another agent's does, the same person's included (one person runs
    // many agents, and each one's rename breaks the others' code the same
    // way), but only from this branch. The same person's other copies
    // (worktrees, side sessions, helper agents) whose branch is not known
    // count once they land: until then they are on a base this file is not.
    let mine = crate::branches::of(store, scope, user_id, session);
    let landings = store.ledger_since_kinds(scope, cutoff, &["landed"]);
    let landed_after = |by: &str, rule_session: &str, ts: f64| -> bool {
        landings.iter().any(|row| {
            let landed_by = text(&row.payload, "session");
            row.ts >= ts
                && text(&row.payload, "user") == by
                && (rule_session.is_empty() || landed_by.is_empty() || landed_by == rule_session)
        })
    };
    let reaches = |info: &Value| -> bool {
        let ts = info.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        if ts < cutoff {
            return false;
        }
        let (by, rule_session) = (text(info, "by"), text(info, "session"));
        let own = by == user_id && (rule_session.is_empty() || session.is_empty() || rule_session == session);
        if own {
            return false;
        }
        let landed = landed_after(&by, &rule_session, ts);
        // a branch that never merges stops linting anyone after a day
        if !landed && ts < stamp - UNLANDED_TTL_S {
            return false;
        }
        let mut theirs = text(info, "branch");
        if theirs.is_empty() {
            theirs = crate::branches::of(store, scope, &by, &rule_session);
        }
        if !mine.is_empty() && !theirs.is_empty() {
            // another branch meets this code when the branches merge: the
            // gate said so once (branches.rs), and the lint does not say it again
            return theirs == mine;
        }
        if by == user_id {
            return landed;
        }
        true
    };

    // module resolution against the repo's files, each module once; a rule's
    // own file always counts as known (a file the graph has not seen yet)
    let mut resolved: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut known: Option<std::collections::BTreeSet<String>> = None;
    let language = reader.parsed.map(|p| p.language).unwrap_or("");
    let mut resolves_to = |module: &str, rule_path: &str| -> bool {
        let target = resolved.entry(module.to_string()).or_insert_with(|| {
            let known = known.get_or_insert_with(|| crate::codegraph::with_known(store, scope, |k| k.clone()));
            cork_core::graph::resolve_module(module, reader.path, language, known)
        });
        match target {
            Some(path) => path == rule_path,
            None => {
                let only = std::collections::BTreeSet::from([rule_path.to_string()]);
                cork_core::graph::resolve_module(module, reader.path, language, &only).as_deref() == Some(rule_path)
            }
        }
    };

    let mut findings: Vec<Value> = Vec::new();
    for name in BUCKETS {
        for (_, info) in bucket(name) {
            let symbol = text(&info, "symbol");
            let rule_path = text(&info, "path");
            // a rule that does not say which file's symbol it is cannot be
            // told from any other word spelled the same (pre-v80 rules)
            if symbol.is_empty() || rule_path.is_empty() {
                continue;
            }
            // cheapest first: the word is not in the file at all
            if occurrences_of(last_segment(&symbol)).is_empty() {
                continue;
            }
            if !reaches(&info) || !still_true(store, scope, name, &info) {
                continue;
            }
            // parameters renamed, none added, removed or given a default:
            // every call written against the old one still fits
            if name == "signatures" && same_shape(&text(&info, "before"), &text(&info, "signature")) {
                continue;
            }
            let Some((local, via)) = binding(reader, &rule_path, &symbol, &mut resolves_to) else { continue };
            let hits = occurrences_of(&local);
            if hits.is_empty() {
                continue;
            }
            let by = text(&info, "by");
            let change_ts = info.get("ts").cloned().unwrap_or(json!(0));
            let finding = match name {
                "renames" => {
                    let new_name = text(&info, "new");
                    // the mechanical fix rewrites the name this file wrote:
                    // through an alias or a module path, the import is what changes
                    let fix = if local == symbol {
                        json!({"class": "mechanical", "kind": "rename", "from": symbol, "to": new_name})
                    } else {
                        json!({"class": "judgment", "prompt": format!(
                            "`{symbol}` in {rule_path} was renamed to `{new_name}` by {by}; this file reaches it \
as `{local}` ({via}; lines {hits:?}): update those uses.")})
                    };
                    json!({
                        "rule": "stale_name", "change_ts": change_ts,
                        "symbol": symbol, "in": rule_path, "via": via, "renamed_to": new_name,
                        "by": by, "lines": hits, "fix": fix,
                    })
                }
                "removed" => json!({
                    "rule": "removed_symbol_use", "change_ts": change_ts,
                    "symbol": symbol, "in": rule_path, "via": via, "by": by, "lines": hits,
                    "fix": {"class": "judgment", "prompt": format!(
                        "`{symbol}` was removed from {rule_path} by {by}, and this file still uses it ({via}; \
lines {hits:?}). Find its replacement (get_symbol on {rule_path}) and adapt each use.")},
                }),
                _ => {
                    let signature = text(&info, "signature");
                    let to = if signature.is_empty() { String::new() } else { format!(" to `{signature}`") };
                    let mut finding = json!({
                        "rule": "signature_drift", "change_ts": change_ts,
                        "symbol": symbol, "in": rule_path, "via": via, "by": by, "lines": hits,
                        "fix": {"class": "judgment", "prompt": format!(
                            "`{symbol}` in {rule_path} changed its signature{to} ({by}); this file uses it ({via}): \
check the uses on lines {hits:?} match.")},
                    });
                    if !signature.is_empty() {
                        finding["signature"] = json!(signature);
                    }
                    finding
                }
            };
            // which of their agents made the change: the step it broke
            // keeps a link to theirs (clash.rs)
            let mut finding = finding;
            let by_session = text(&info, "session");
            if !by_session.is_empty() {
                finding["by_session"] = json!(by_session);
            }
            findings.push(finding);
        }
    }

    findings.truncate(MAX_FINDINGS);
    findings
}

/// The registry key of one file's symbol: two files' `post` are two rules.
fn rule_key(path: &str, symbol: &str) -> String {
    format!("{path}::{symbol}")
}

/// Certain renames, removals and signature changes become lint rules for
/// everyone else's future reports. Each rule names its file and the branch
/// it was made on, so a reader can tell whether it is about the code it
/// uses and whether the change can reach it.
pub fn update_registry(store: &Store, scope: &str, user_id: &str, session: &str, path: &str, hot: &[Value], stamp: f64) {
    if hot.is_empty() {
        return;
    }
    let mut registry = store.kv_get("semlint", scope).unwrap_or_else(|| json!({}));
    let Some(root) = registry.as_object_mut() else { return };
    let branch = crate::branches::of(store, scope, user_id, session);

    for event in hot {
        let symbol = text(event, "symbol");
        if symbol.is_empty() {
            continue;
        }
        let detail = event.get("detail").cloned().unwrap_or(Value::Null);
        let kind = text(event, "kind");
        let key = rule_key(path, &symbol);
        let mut base = json!({"by": user_id, "ts": stamp, "symbol": symbol, "path": path});
        if !session.is_empty() {
            base["session"] = json!(session);
        }
        if !branch.is_empty() {
            base["branch"] = json!(branch);
        }

        if kind == "renamed" && !text(&detail, "new_name").is_empty() {
            let mut entry = base.clone();
            entry["new"] = json!(text(&detail, "new_name"));
            root.entry("renames").or_insert_with(|| json!({}));
            if let Some(map) = root.get_mut("renames").and_then(Value::as_object_mut) {
                map.insert(key.clone(), entry);
            }
            // a rename is not a removal; clear any stale removal rule
            if let Some(map) = root.get_mut("removed").and_then(Value::as_object_mut) {
                map.remove(&key);
            }
        } else if kind == "removed" {
            root.entry("removed").or_insert_with(|| json!({}));
            if let Some(map) = root.get_mut("removed").and_then(Value::as_object_mut) {
                map.insert(key, base);
            }
        } else {
            // the event's own before and after (a modified event's detail is
            // prose, not the signature); one that only moved whitespace
            // changed nothing a caller writes
            let after = {
                let after = text(event, "signature_after");
                if after.is_empty() { text(&detail, "signature") } else { after }
            };
            let before = text(event, "signature_before");
            if !before.is_empty() && squashed(&before) == squashed(&after) {
                continue;
            }
            let mut entry = base.clone();
            entry["signature"] = json!(clip(&after, 200));
            if !before.is_empty() {
                entry["before"] = json!(clip(&before, 200));
            }
            root.entry("signatures").or_insert_with(|| json!({}));
            if let Some(map) = root.get_mut("signatures").and_then(Value::as_object_mut) {
                map.insert(key, entry);
            }
        }
    }

    // expire old rules (and pre-v80 rules, which name no file) and cap each
    // bucket, newest first
    let cutoff = stamp - RULE_TTL_S;
    for name in BUCKETS {
        let Some(entries) = root.get(name).and_then(Value::as_object).cloned() else { continue };
        let mut kept: Vec<(String, Value)> = entries
            .into_iter()
            .filter(|(_, value)| value.get("ts").and_then(Value::as_f64).unwrap_or(0.0) >= cutoff)
            .filter(|(_, value)| !text(value, "path").is_empty())
            .collect();
        kept.sort_by(|a, b| {
            let ts = |entry: &Value| entry.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            ts(&b.1).partial_cmp(&ts(&a.1)).unwrap_or(std::cmp::Ordering::Equal)
        });
        kept.truncate(MAX_RULES_PER_BUCKET);
        let bucket: BTreeMap<String, Value> = kept.into_iter().collect();
        root.insert(name.to_string(), json!(bucket));
    }
    let _ = store.kv_put("semlint", scope, &registry, stamp);
}

pub(crate) fn reconciliation_id(path: &str, symbol: &str, rule: &str, b_user: &str) -> String {
    use sha1::{Digest, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(format!("{path}|{symbol}|{rule}|{b_user}").as_bytes());
    format!("{:x}", hasher.finalize())[..12].to_string()
}

const RECONCILE_TTL_S: f64 = 6.0 * 3600.0;
const MAX_RECONCILIATIONS: usize = 100;

/// Judgment-class findings ARE active collisions: one agent's interface change
/// against another agent's code. Each becomes a claimable record, redelivered
/// until resolved.
///
/// Resolution is evidence-based and differs per rule. For a stale name or a
/// removed symbol, a fresh report of the path that no longer flags it proves
/// the reference is gone. For signature drift the NAME legitimately survives
/// an adapted call, so occurrence proves nothing — instead a re-report after
/// the record was claimed counts as action.
///
/// A resolved record also SUPPRESSES redelivery of the same finding while the
/// underlying change is unchanged. A file that already survived a collision
/// must not be nagged about it again.
pub fn track_reconciliations(
    store: &Store, scope: &str, user_id: &str, path: &str, findings: Vec<Value>,
) -> Vec<Value> {
    let key = format!("reconcile:{scope}");
    let mut records = store
        .eph_get(&key)
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let stamp = now();
    let mut delivered: Vec<Value> = Vec::new();
    let mut open_symbols: Vec<String> = Vec::new();
    let mut changed = false;

    for finding in findings {
        let fix_class = finding.get("fix").map(|fix| text(fix, "class")).unwrap_or_default();
        if fix_class != "judgment" {
            delivered.push(finding);
            continue;
        }
        let symbol = text(&finding, "symbol");
        let rule = text(&finding, "rule");
        let rid = reconciliation_id(path, &symbol, &rule, user_id);
        let change_ts = finding.get("change_ts").and_then(Value::as_f64).unwrap_or(0.0);
        let existing = records.get(&rid).cloned();

        if let Some(record) = &existing {
            let resolved = text(record, "status") == "resolved";
            let same_change =
                record.get("change_ts").and_then(Value::as_f64).unwrap_or(-1.0) == change_ts;
            if resolved && same_change {
                continue; // already reconciled against this exact change: quiet
            }
        }
        open_symbols.push(symbol.clone());
        delivered.push(finding.clone());

        let fresh = existing
            .as_ref()
            .map(|record| text(record, "status") == "resolved")
            .unwrap_or(true);
        if fresh {
            let mut record = json!({
                "id": rid, "rule": rule, "symbol": symbol,
                "a_user": text(&finding, "by"), "b_user": user_id,
                "path": path,
                "lines": finding.get("lines").cloned().unwrap_or(json!([])),
                "change_ts": change_ts,
                "opened": stamp, "status": "open", "claimed_by": "", "claim_ts": 0.0,
            });
            if let Some(map) = record.as_object_mut() {
                for carried in ["signature", "renamed_to"] {
                    if let Some(value) = finding.get(carried) {
                        if !value.as_str().unwrap_or("").is_empty() {
                            map.insert(carried.into(), value.clone());
                        }
                    }
                }
            }
            records.insert(rid, record);
        } else if let Some(record) = records.get_mut(&rid) {
            if let Some(map) = record.as_object_mut() {
                map.insert("lines".into(), finding.get("lines").cloned().unwrap_or(json!([])));
            }
        }
        changed = true;
    }

    // anything still open for this user on this path that no longer appears
    let ids: Vec<String> = records.keys().cloned().collect();
    for rid in ids {
        let Some(record) = records.get(&rid).cloned() else { continue };
        if text(&record, "status") != "open"
            || text(&record, "path") != path
            || text(&record, "b_user") != user_id
        {
            continue;
        }
        let symbol = text(&record, "symbol");
        let gone = !open_symbols.contains(&symbol);
        let acted = text(&record, "rule") == "signature_drift"
            && !text(&record, "claimed_by").is_empty()
            && stamp > record.get("claim_ts").and_then(Value::as_f64).unwrap_or(0.0);
        if !(gone || acted) {
            continue;
        }
        if let Some(entry) = records.get_mut(&rid).and_then(Value::as_object_mut) {
            entry.insert("status".into(), json!("resolved"));
            entry.insert("resolved_by".into(), json!(user_id));
            entry.insert("resolved_ts".into(), json!(stamp));
        }
        changed = true;
        let _ = store.ledger_append(
            scope,
            "reconciliation_resolved",
            &json!({"id": rid, "symbol": symbol, "user": user_id,
                    "rule": text(&record, "rule")}),
            stamp,
        );
    }

    if changed {
        let kept: Map<String, Value> = records
            .into_iter()
            .filter(|(_, record)| {
                text(record, "status") == "open"
                    || stamp - record.get("resolved_ts").and_then(Value::as_f64).unwrap_or(stamp)
                        < RECONCILE_TTL_S
            })
            .collect();
        let capped: Map<String, Value> = if kept.len() > MAX_RECONCILIATIONS {
            kept.into_iter().take(MAX_RECONCILIATIONS).collect()
        } else {
            kept
        };
        let _ = store.eph_set(&key, &Value::Object(capped), Some(RECONCILE_TTL_S));
    }
    delivered
}

/// Every reconciliation this scope still keeps, by id: "open" or
/// "resolved" (the dashboard's collision outcome, clash.rs).
pub fn reconciliation_states(store: &Store, scope: &str) -> BTreeMap<String, String> {
    store
        .eph_get(&format!("reconcile:{scope}"))
        .and_then(|value| value.as_object().cloned())
        .map(|records| records.into_iter().map(|(id, record)| (id, text(&record, "status"))).collect())
        .unwrap_or_default()
}

/// Open collisions in this scope, for the `reconciliations_pending` notice.
pub fn open_reconciliations(store: &Store, scope: &str) -> Vec<Value> {
    store
        .eph_get(&format!("reconcile:{scope}"))
        .and_then(|value| value.as_object().cloned())
        .map(|records| {
            records
                .into_values()
                .filter(|record| text(record, "status") == "open")
                .collect()
        })
        .unwrap_or_default()
}

/// An edit landing on a path this AGENT was just blocked on is an override:
/// it saw the collision and wrote anyway. Recorded, never prevented. Keyed by
/// agent, like the block itself: another session of the same person writing
/// the path never saw the block, so its write is not an override. A block
/// that named no session belongs to the person, though — a gate reached
/// without one cannot say which agent it stopped — so any of the person's
/// agents writing that path overrides it.
pub fn note_override(store: &Store, scope: &str, user_id: &str, session: &str, path: &str, path_key: &str) -> bool {
    let own = format!("blocked:{scope}:{}:{path_key}", crate::presence::agent_id(user_id, session));
    let person = format!("blocked:{scope}:{user_id}:{path_key}");
    let key = if store.eph_get(&own).is_some() {
        own
    } else if !session.is_empty() && store.eph_get(&person).is_some() {
        person
    } else {
        return false;
    };
    store.eph_delete(&key);
    let stamp = now();
    let salience_key = format!("{scope}:{path_key}");
    let mut record = store.kv_get("salience", &salience_key).unwrap_or_else(|| {
        json!({"path": path, "collisions": 0, "overrides": 0, "blocks": 0})
    });
    if let Some(map) = record.as_object_mut() {
        let read = |map: &Map<String, Value>, name: &str| {
            map.get(name).and_then(Value::as_i64).unwrap_or(0)
        };
        let overrides = read(map, "overrides") + 1;
        map.insert("overrides".into(), json!(overrides));
        let score = 3 * read(map, "collisions") + 4 * overrides + 2 * read(map, "blocks");
        map.insert("score".into(), json!(score));
        map.insert("updated".into(), json!(stamp));
    }
    let _ = store.kv_put("salience", &salience_key, &record, stamp);
    let _ = store.ledger_append(
        scope,
        "override",
        &json!({"user": user_id, "session": session, "path": path}),
        stamp,
    );
    true
}

#[cfg(test)]
mod tests {
    //! The false alarms the v79 lint raised in this repo, each against a
    //! real use that must still fire.
    use super::*;

    const SCOPE: &str = "w:r";

    fn db() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn rule(db: &Store, bucket: &str, path: &str, symbol: &str, extra: Value) {
        let mut registry = db.kv_get("semlint", SCOPE).unwrap_or_else(|| json!({}));
        let mut entry = json!({"symbol": symbol, "path": path, "by": "bob@x.com", "session": "b1", "ts": now()});
        if let (Some(map), Some(more)) = (entry.as_object_mut(), extra.as_object()) {
            map.extend(more.clone());
        }
        if registry.get(bucket).is_none() {
            registry[bucket] = json!({});
        }
        registry[bucket][rule_key(path, symbol)] = entry;
        db.kv_put("semlint", SCOPE, &registry, now()).unwrap();
    }

    type Found = Vec<(String, String, Vec<usize>)>;

    fn found(rule: &str, symbol: &str, lines: &[usize]) -> Found {
        vec![(rule.to_string(), symbol.to_string(), lines.to_vec())]
    }

    /// Lint `content` at `path` as alice's session a1, both through the
    /// server's own parse of the file and through a hook's local parse.
    fn lint(db: &Store, path: &str, content: &str) -> Found {
        let parsed = cork_core::engine::parse(path, content);
        let reader = Reader { path, parsed: parsed.as_ref() };
        let from_text = lint_report(db, SCOPE, "alice@x.com", "a1", &reader, content);
        let names = json!(cork_core::local::name_lines(content));
        let from_names = lint_names(db, SCOPE, "alice@x.com", "a1", &reader, &names);
        let shape = |found: &[Value]| -> Found {
            found
                .iter()
                .map(|f| {
                    let lines = f["lines"].as_array().unwrap().iter().filter_map(Value::as_u64).map(|n| n as usize).collect();
                    (text(f, "rule"), text(f, "symbol"), lines)
                })
                .collect()
        };
        assert_eq!(shape(&from_text), shape(&from_names), "the local parse lints the same as the file");
        shape(&from_text)
    }

    /// v79: "`post` was renamed to `post_message`" on every axum router.
    #[test]
    fn an_axum_post_import_is_not_a_teammates_post() {
        let db = db();
        rule(&db, "renames", "server/cork-rs/src/chat.rs", "post", json!({"new": "post_message"}));
        let router = "use axum::routing::{get, post};\n\npub fn routes() -> Router {\n    Router::new().route(\"/m\", post(send))\n}\n";
        assert_eq!(lint(&db, "server/cork-rs/src/main.rs", router), vec![]);
        // the real use: the function imported from the module it was renamed in
        let caller = "use crate::chat::post;\n\npub fn send(m: &str) {\n    post(m);\n}\n";
        assert_eq!(lint(&db, "server/cork-rs/src/inbox.rs", caller), found("stale_name", "post", &[1, 4]));
        // and through the module: `use crate::chat;` then `chat::post(..)`
        let through = "use crate::chat;\n\npub fn send(m: &str) {\n    chat::post(m);\n}\n";
        assert_eq!(lint(&db, "server/cork-rs/src/inbox.rs", through), found("stale_name", "post", &[4]));
    }

    /// v79: "`messages` was removed" on a page whose text says "messages"
    /// and whose local variable is called that.
    #[test]
    fn a_word_in_page_text_is_not_a_removed_symbol() {
        let db = db();
        rule(&db, "removed", "website/lib/inbox.ts", "messages", json!({}));
        let page = "export default function Inbox() {\n  const messages = useThread();\n  return <p>No messages yet ({messages.length})</p>;\n}\n";
        assert_eq!(lint(&db, "website/app/inbox/page.tsx", page), vec![]);
        let user = "import { messages } from '../../lib/inbox';\n\nexport default function Inbox() {\n  return <ul>{messages().map(m => <li>{m}</li>)}</ul>;\n}\n";
        assert_eq!(lint(&db, "website/app/inbox/page.tsx", user), found("removed_symbol_use", "messages", &[1, 4]));
    }

    /// Lattice test, round 2 (v104): `pickLabels(anchor, {cur, onChange})`
    /// became `pickLabels(anchor, opts)`, a rename of parameters, and every
    /// caller was told its signature changed. A call written against the old
    /// signature fits the new one, so there is nothing to check.
    #[test]
    fn renamed_parameters_are_no_finding_and_a_new_parameter_is() {
        let db = db();
        let menu = "static/js/menu.js";
        let view = "import { pickLabels } from '../menu.js';\n\nexport function row(anchor) {\n  pickLabels(anchor, { current: [] });\n}\n";
        rule(&db, "signatures", menu, "pickLabels",
            json!({"signature": "export function pickLabels(anchor, opts)", "before": "export function pickLabels(anchor, { cur, onChange })"}));
        assert_eq!(lint(&db, "static/js/views/issue.js", view), vec![]);
        rule(&db, "signatures", menu, "pickLabels",
            json!({"signature": "export function pickLabels(anchor, opts, team)", "before": "export function pickLabels(anchor, opts)"}));
        assert_eq!(lint(&db, "static/js/views/issue.js", view), found("signature_drift", "pickLabels", &[1, 4]));
        assert!(same_shape("def f(self, a, b=1)", "def f(self, x, y=2)") && !same_shape("def f(a, b)", "def f(a, b=1)"));
        assert!(!same_shape("", "fn f(a: i32)"), "unknown before: the rule stands");
    }

    /// v79: "`SiteNav`'s signature was changed" on every page that renders
    /// `<SiteNav />`, for a signature that had not changed.
    #[test]
    fn an_unchanged_signature_is_no_rule_and_a_reverted_one_no_finding() {
        let db = db();
        let nav = "website/components/site-nav.tsx";
        let page = "import { SiteNav } from '@/components/site-nav';\n\nexport default function Home() {\n  return <SiteNav />;\n}\n";
        // a modified event whose signature moved only whitespace
        let hot = vec![json!({"symbol": "SiteNav", "kind": "signature", "confidence": "certain",
            "detail": "signature changed to: function SiteNav( )",
            "signature_before": "function SiteNav()", "signature_after": "function SiteNav( )"})];
        update_registry(&db, SCOPE, "bob@x.com", "b1", nav, &hot, now());
        let registry = db.kv_get("semlint", SCOPE).unwrap();
        assert!(registry["signatures"].as_object().map_or(true, |m| m.is_empty()), "{registry}");
        // a real change, then the file reported back as it was (another copy
        // of it on an older base): the graph's latest report contradicts it
        crate::report::observe_batch(&db, SCOPE, &[(nav, "export function SiteNav() {\n  return <nav />;\n}\n", None)], 0.0);
        let hot = vec![json!({"symbol": "SiteNav", "kind": "signature", "confidence": "certain",
            "detail": "signature changed to: function SiteNav({ active }: Props)",
            "signature_before": "function SiteNav()", "signature_after": "function SiteNav({ active }: Props)"})];
        update_registry(&db, SCOPE, "bob@x.com", "b1", nav, &hot, now());
        let saved = db.kv_get("semlint", SCOPE).unwrap()["signatures"][rule_key(nav, "SiteNav")].clone();
        assert_eq!(saved["signature"], "function SiteNav({ active }: Props)", "the signature, not the event's prose");
        assert_eq!(lint(&db, "website/app/page.tsx", page), vec![]);
        // the graph agrees: a real drift, on the lines that import and render it
        crate::report::observe_batch(&db, SCOPE, &[(nav, "export function SiteNav({ active }: Props) {\n  return <nav />;\n}\n", None)], 0.0);
        let graph_signature = crate::codegraph::symbols_of(&db, SCOPE, nav)["SiteNav"]["signature"].clone();
        assert_eq!(squashed(graph_signature.as_str().unwrap()), squashed(saved["signature"].as_str().unwrap()), "fixture: the parser's signature");
        assert_eq!(lint(&db, "website/app/page.tsx", page), found("signature_drift", "SiteNav", &[1, 4]));
        // a page that never imports it is never told
        let about = "export default function About() {\n  return <p>SiteNav</p>;\n}\n";
        assert_eq!(lint(&db, "website/app/about/page.tsx", about), vec![]);
    }

    #[test]
    fn the_same_file_counts_unless_this_copy_still_defines_it() {
        let db = db();
        rule(&db, "removed", "shop/tax.py", "compute_tax", json!({}));
        let uses = "def total(items):\n    return compute_tax(items)\n";
        assert_eq!(lint(&db, "shop/tax.py", uses), found("removed_symbol_use", "compute_tax", &[2]));
        let defines = "def compute_tax(x):\n    return x\n\n\ndef total(items):\n    return compute_tax(items)\n";
        assert_eq!(lint(&db, "shop/tax.py", defines), vec![]);
    }

    #[test]
    fn a_rule_that_names_no_file_lints_nothing() {
        let db = db();
        let old = json!({"renames": {"compute_tax": {"by": "bob@x.com", "ts": now(), "new": "calculate_tax"}}});
        db.kv_put("semlint", SCOPE, &old, now()).unwrap();
        assert_eq!(lint(&db, "shop/billing.py", "from .tax import compute_tax\n\ncompute_tax(1)\n"), vec![]);
    }

    /// One person's agents on other bases: a worktree helper's rename on its
    /// own branch is not this branch's news until it lands.
    #[test]
    fn a_change_reaches_a_file_only_from_its_branch_or_landed() {
        let caller = "from .tax import compute_tax\n\n\ndef bill(items):\n    return compute_tax(items)\n";
        let expect = found("stale_name", "compute_tax", &[1, 5]);
        let fresh = || {
            let db = db();
            crate::branches::note(&db, SCOPE, "alice@x.com", "a1", "dev", "");
            db
        };
        let mine = |db: &Store, extra: Value| {
            let mut extra = extra;
            extra["by"] = json!("alice@x.com");
            extra["session"] = json!("a2");
            extra["new"] = json!("calculate_tax");
            rule(db, "renames", "shop/tax.py", "compute_tax", extra);
        };
        // the same person, another branch: no
        let db = fresh();
        mine(&db, json!({"branch": "worktree-agent-7"}));
        assert_eq!(lint(&db, "shop/billing.py", caller), vec![]);
        // the same person, this branch: yes
        let db = fresh();
        mine(&db, json!({"branch": "dev"}));
        assert_eq!(lint(&db, "shop/billing.py", caller), expect);
        // the same person, branch not known: only once it landed
        let db = fresh();
        mine(&db, json!({}));
        assert_eq!(lint(&db, "shop/billing.py", caller), vec![]);
        db.ledger_append(SCOPE, "landed", &json!({"user": "alice@x.com", "session": "a2"}), now() + 1.0).unwrap();
        assert_eq!(lint(&db, "shop/billing.py", caller), expect);
        // a teammate on another branch: no; branch not known: yes (older hooks)
        let db = fresh();
        rule(&db, "renames", "shop/tax.py", "compute_tax", json!({"new": "calculate_tax", "branch": "feat/x"}));
        assert_eq!(lint(&db, "shop/billing.py", caller), vec![]);
        let db = fresh();
        rule(&db, "renames", "shop/tax.py", "compute_tax", json!({"new": "calculate_tax"}));
        assert_eq!(lint(&db, "shop/billing.py", caller), expect);
        // two days old and never landed: a branch that did not merge is no rule
        let db = fresh();
        let then = now() - 2.0 * 86400.0;
        rule(&db, "renames", "shop/tax.py", "compute_tax", json!({"new": "calculate_tax", "ts": then}));
        assert_eq!(lint(&db, "shop/billing.py", caller), vec![]);
        db.ledger_append(SCOPE, "landed", &json!({"user": "bob@x.com", "session": "b1"}), then + 60.0).unwrap();
        assert_eq!(lint(&db, "shop/billing.py", caller), expect, "landed, it keeps the week");
    }

    #[test]
    fn a_rename_the_graph_does_not_hold_is_an_echo() {
        let db = db();
        crate::report::observe_batch(&db, SCOPE, &[("shop/tax.py", "def compute_tax(x):\n    return x\n", None)], 0.0);
        rule(&db, "renames", "shop/tax.py", "compute_tax", json!({"new": "calculate_tax"}));
        let caller = "from .tax import compute_tax\n\ncompute_tax(1)\n";
        assert_eq!(lint(&db, "shop/billing.py", caller), vec![], "the file was reported back with the old name");
        crate::report::observe_batch(&db, SCOPE, &[("shop/tax.py", "def calculate_tax(x):\n    return x\n", None)], 0.0);
        assert_eq!(lint(&db, "shop/billing.py", caller), found("stale_name", "compute_tax", &[1, 3]));
    }

    #[test]
    fn a_finding_names_the_file_and_how_this_file_reaches_it() {
        let db = db();
        rule(&db, "removed", "shop/tax.py", "compute_tax", json!({}));
        let content = "from .tax import compute_tax as levy\n\nlevy(1)\n";
        let parsed = cork_core::engine::parse("shop/billing.py", content);
        let reader = Reader { path: "shop/billing.py", parsed: parsed.as_ref() };
        let found = lint_report(&db, SCOPE, "alice@x.com", "a1", &reader, content);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0]["in"], "shop/tax.py");
        assert_eq!(found[0]["lines"], json!([1, 3]), "the alias's lines");
        let prompt = text(&found[0]["fix"], "prompt");
        assert!(prompt.contains("removed from shop/tax.py by bob@x.com") && prompt.contains("`levy` imported from shop/tax.py"), "{prompt}");
    }
}
