//! The briefing the hooks print into the agent's context.
//!
//! Two scopes, one renderer. At session start the task is unknown, so the
//! briefing is by recency: the modules changed in the last week, newest
//! first. At prompt submit the task is known, so the hook sends the
//! identifiers it found in the prompt (never the prose) and the briefing
//! narrows to the modules those name — by path, by symbol, by docstring —
//! plus the modules that depend on the matched symbols, ranked, under a
//! byte budget, with a scope line that says what the page covers, what the
//! map knows of the rest (nothing calls X, nothing is named Y), and what is
//! related but only named. The agent starts knowing the code its
//! ticket touches, and knows the edge of what it was told.
//!
//! Every module renders the same way: exact signatures and docstrings from
//! the parser, notes earlier agents anchored, and whether the repo check
//! passed after it was written. Served to the hook only, like `/observe`.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::{json, Value};

use crate::store::{now, Store};

const DEFAULT_WINDOW_S: f64 = 7.0 * 86_400.0;
const DEFAULT_BUDGET: usize = 6_000;
const MAX_BUDGET: usize = 24_000;
/// A prompt's briefing rides every later message of the session (about 17
/// calls in the scaling study): it never takes more than this, whatever the
/// hook asks for.
const PROMPT_MAX_BUDGET: usize = 2_400;
/// A prompt briefing's ceiling: this much, and this much more per file in
/// the task's scope (`prompt_ceiling`).
const PROMPT_BASE: usize = 900;
const PROMPT_PER_FILE: usize = 500;
/// What the scope line may take of it, and how many names each of its lists says.
const SCOPE_LINE_CHARS: usize = 200;
const SCOPE_NAMES: usize = 4;
/// The module blocks always get at least this, whatever rides with them,
/// and one module at least MIN_PROMPT_BLOCK of it.
const MIN_PROMPT_BLOCKS: usize = 500;
const MIN_PROMPT_BLOCK: usize = 420;
/// The session start's, likewise: past it, the modules changed recently are
/// named in its tail, where a write to one still counts as found.
const START_MAX_BUDGET: usize = 4_500;
const MAX_NOTES: usize = 5;
/// A note below this confidence (reality-scored, see memory::outcomes) is
/// not served in briefings; get_symbol still returns it with its score.
const SERVE_CONFIDENCE: f64 = 0.25;
/// A note is a pointer, not the whole memory: the rest is one get_symbol away.
const MAX_NOTE_CHARS: usize = 240;
/// A docstring in a block is the convention, not the manual: clipped on a
/// word boundary, the whole text one get_symbol away.
const MAX_MODULE_DOC_CHARS: usize = 300;
const MAX_SYMBOL_DOC_CHARS: usize = 200;
/// Tenths of a block's cap the symbol lines may use when the module has notes.
const SYMBOL_SHARE: usize = 6;
const MAX_REST: usize = 20;
const MAX_IDENTIFIERS: usize = 40;
/// A module is briefed only on a strong match (path, module name, or an
/// exact symbol); docstring and partial-name hits only rank it.
const STRONG_MATCH: i64 = 3;
/// One module's block never eats the whole budget.
const MAX_BLOCK_SHARE: usize = 3;
const MIN_BLOCK: usize = 1_500;
const MAX_SYMBOL_LINES: usize = 40;
/// A module's constants are named, not listed: at most this many names.
const MAX_VALUE_NAMES: usize = 8;
/// Partial-name and docstring hits count at most this much per module.
const WEAK_CAP: i64 = 2;
/// A symbol name defined in more modules than this is a common name, not a match.
const COMMON_NAME: usize = 5;
/// A module that depends on a matched symbol outranks one that merely shares a name.
const DEPENDENT_BONUS: i64 = 4;
/// An exact symbol match outranks a dependent, which outranks a name-sharer.
const EXACT_SYMBOL: i64 = 5;
/// A second-hop dependent (calls something that calls the symbol).
const DEPENDENT2_BONUS: i64 = 3;
const MAX_DEPENDENT_MODULES: usize = 40;
/// At most this many "right now" lines: who is live, not the whole roster.
const MAX_RIGHT_NOW: usize = 5;
/// The repo on one page, at session start: never more than this many bytes,
/// never more than a third of the budget, skipped when the budget is tiny.
const SHAPE_CAP: usize = 900;
const MIN_SHAPE: usize = 200;
const SHAPE_ITEMS: usize = 4;
const SHAPE_IDLE_S: f64 = 900.0;
/// After a compaction the working set may take up to half the budget.
const MAX_RESUME_SHARE: usize = 2;
/// A module agents keep reading after the briefing (`brieflearn`) is shown
/// whole: its docstrings this long, its block this many times the usual cap.
const FULL_DOC_CHARS: usize = 400;
const FULL_CAP_FACTOR: usize = 2;

pub struct BriefInput<'a> {
    pub repo_id: &'a str,
    pub since: f64,
    pub budget: usize,
    pub identifiers: &'a [String],
    pub exclude: &'a [String],
    /// The hook's session, so a resume can lead with its working set.
    pub session: &'a str,
    /// SessionStart after a compaction or a resume: the context the agent had
    /// is gone, so the briefing leads with what this session was working on.
    pub resume: bool,
    /// The roster's idle threshold, for the "right now" lines: an agent
    /// quiet longer than this is not working right now.
    pub idle_after_s: f64,
    /// The workspace scopes this caller may read (`access::visible_scopes`):
    /// recipes are offered across the workspace, through this allowlist.
    pub visible: &'a std::collections::BTreeSet<String>,
    /// Who is reading, and whether the plan shares teammates' knowledge
    /// (limits.knowledge_sharing): when it does not, the notes and recipes
    /// rendered are the reader's own — the modules, signatures and who
    /// changed them are the radar and always shown.
    pub viewer: &'a str,
    pub share_knowledge: bool,
    /// The hook checks every symbol line against the reader's own copy of the
    /// file before printing (it sends `verifies`, and reads `checks` in the answer): only then
    /// does the briefing say to build on them instead of reading. Several
    /// checkouts report into one map, and a map entry from another one
    /// (a function that is not in this copy, its lines elsewhere) once told
    /// an agent to trust what was not there.
    pub verified: bool,
}

/// How a page of module blocks is drawn, and what it hands the hook to check.
#[derive(Default)]
struct Draw {
    /// Per path, the symbols the prompt named: listed first, in full.
    focus: BTreeMap<String, Vec<String>>,
    /// The budget is split this many ways, at most, per module block.
    share: usize,
    /// Modules here only for calling what the prompt names: their callers
    /// (`focus`) are listed, the rest of the module is counted.
    callers_only: BTreeSet<String>,
    /// Drawing each block whole, to share the page out (`water_fill`).
    dry: bool,
    /// Each symbol line (or names line) as printed, with where it comes
    /// from, for the hook to check against the reader's copy (`checks`).
    checks: Vec<Value>,
}

struct Changed {
    ts: f64,
    user: String,
    agent: String,
    /// What changed, per symbol, newest first: `def serve(x) → def serve(x, region)`.
    changes: Vec<String>,
    /// Why, in the author's words, when they said: the newest reason on
    /// the path, from the edit row or a rationale row that followed it.
    why: String,
    why_by: String,
}
const MAX_CHANGES: usize = 4;
const MAX_CHANGE_LINE: usize = 300;

/// path -> latest write in the window (who, when)
fn recent_writes(store: &Store, scope: &str, since: f64) -> BTreeMap<String, Changed> {
    let mut rows = store.ledger_since(scope, since);
    rows.retain(|row| row.kind == "edit_reported" || row.kind == "rationale");
    rows.sort_by(|a, b| b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: BTreeMap<String, Changed> = BTreeMap::new();
    let mut seen_symbols: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in rows {
        let path = row.payload.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        if path.is_empty() || crate::codegraph::is_collide_artifact(&path) {
            continue;
        }
        // the signature changes this row recorded, newest row first, one line per symbol
        let seen = seen_symbols.entry(path.clone()).or_default();
        let mut changes: Vec<String> = Vec::new();
        for event in row.payload.get("events").and_then(Value::as_array).into_iter().flatten() {
            let symbol = event.get("symbol").and_then(Value::as_str).unwrap_or("");
            if symbol.is_empty() || seen.contains(symbol) {
                continue;
            }
            let kind = event.get("kind").and_then(Value::as_str).unwrap_or("");
            let before = event.get("signature_before").and_then(Value::as_str).unwrap_or("");
            let after = event.get("signature_after").and_then(Value::as_str).unwrap_or("");
            let new_name = event.get("new_name").and_then(Value::as_str).unwrap_or("");
            let line = if kind == "renamed" && !new_name.is_empty() {
                format!("{symbol} renamed → {new_name}")
            } else if kind == "removed" {
                format!("{symbol} removed")
            } else if !before.is_empty() && !after.is_empty() && before != after {
                format!("{before} → {after}")
            } else if kind == "added" && !after.is_empty() {
                format!("+ {after}")
            } else {
                continue; // a body-only edit: the signature the agent has is still right
            };
            seen.insert(symbol.to_string());
            changes.push(line);
        }
        let why = row.payload.get("why").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let user = row.payload.get("user").and_then(Value::as_str).unwrap_or("").to_string();
        match out.get_mut(&path) {
            Some(existing) => {
                for line in changes {
                    if existing.changes.len() < MAX_CHANGES {
                        existing.changes.push(line);
                    }
                }
                if existing.why.is_empty() && !why.is_empty() {
                    existing.why = why;
                    existing.why_by = user.clone();
                }
                // a rationale row came first (newest): the write behind it is
                // still the latest write, with its author and time
                if existing.ts == 0.0 && row.kind == "edit_reported" {
                    existing.ts = row.ts;
                    existing.user = user;
                    existing.agent = row.payload.get("agent").and_then(Value::as_str).unwrap_or("").to_string();
                }
            }
            None => {
                changes.truncate(MAX_CHANGES);
                // a rationale row alone is not a write: it only carries the reason
                let is_write = row.kind == "edit_reported";
                out.insert(
                    path,
                    Changed {
                        ts: if is_write { row.ts } else { 0.0 },
                        user: if is_write { user.clone() } else { String::new() },
                        agent: if is_write { row.payload.get("agent").and_then(Value::as_str).unwrap_or("").to_string() } else { String::new() },
                        changes,
                        why,
                        why_by: user,
                    },
                );
            }
        }
    }
    out
}

/// The `changed:` line a recency block carries, and the author's reason
/// under it when they gave one, or nothing.
fn change_lines(recent: &BTreeMap<String, Changed>) -> BTreeMap<String, Vec<String>> {
    recent
        .iter()
        .filter(|(_, c)| !c.changes.is_empty() || !c.why.is_empty())
        .map(|(path, c)| {
            let mut lines = Vec::new();
            if !c.changes.is_empty() {
                lines.push(clip(&format!("  changed: {}", c.changes.join("; ")), MAX_CHANGE_LINE));
            }
            if !c.why.is_empty() {
                lines.push(clip(&format!("  why ({}): {}", c.why_by, c.why), MAX_CHANGE_LINE));
            }
            (path.clone(), lines)
        })
        .collect()
}

pub fn brief(store: &Store, scope: &str, input: &BriefInput) -> Value {
    let stamp = now();
    let since = if input.since > 0.0 { input.since } else { stamp - DEFAULT_WINDOW_S };
    let budget = if input.budget == 0 { DEFAULT_BUDGET } else { input.budget.min(MAX_BUDGET) };
    let budget = if input.identifiers.is_empty() { budget.min(START_MAX_BUDGET) } else { budget.min(PROMPT_MAX_BUDGET) };
    let exclude: HashSet<&str> = input.exclude.iter().map(String::as_str).collect();
    let recent = recent_writes(store, scope, since);
    // what earlier agents did with their briefings shapes this one
    let mut learned = Learned::default();
    let mut draw = Draw::default();
    // build on the facts only where the hook checks them against the reader's
    // copy; otherwise say where they come from, and to read before relying
    let how_to_read = if input.verified { HOW_TO_READ } else { HOW_TO_READ_UNCHECKED };

    if input.identifiers.is_empty() {
        // session start: the map the last agent here left (the files it read
        // and changed: where things live, so this one need not find them
        // again, trail.rs), the repo's shape on one page, then — after
        // a compaction — this session's working set, then what changed recently
        let map: Vec<String> = if input.resume {
            Vec::new()
        } else {
            crate::trail::brief_lines(store, scope, input.viewer, input.session)
        };
        let shape = if budget / 3 >= MIN_SHAPE { render_shape(store, scope, SHAPE_CAP.min(budget / 3)) } else { String::new() };
        let mut used = if shape.is_empty() { 0 } else { shape.len() + 1 };
        used += map.iter().map(|l| l.len() + 1).sum::<usize>();
        let changed = change_lines(&recent);
        let mut resume_blocks: Vec<String> = Vec::new();
        let mut resume_paths: Vec<String> = Vec::new();
        if input.resume && !input.session.is_empty() {
            let working: Vec<(String, f64, String, String, String)> = crate::deltas::working_set(store, scope, input.session)
                .into_iter()
                .filter(|path| !exclude.contains(path.as_str()) && !crate::codegraph::is_collide_artifact(path))
                .map(|path| match recent.get(&path) {
                    Some(c) => (path.clone(), c.ts, c.user.clone(), c.agent.clone(), String::new()),
                    None => (path, 0.0, String::new(), String::new(), String::new()),
                })
                .collect();
            if !working.is_empty() {
                let (blocks, _rest, shown) =
                    render_all(store, scope, &working, budget / MAX_RESUME_SHARE, stamp, &changed, input, &mut learned, &mut draw);
                used += blocks.iter().map(|b| b.len() + 1).sum::<usize>();
                resume_blocks = blocks;
                resume_paths = shown;
            }
        }
        // by recency: newest write first, minus what the resume already showed
        let mut order: Vec<(&String, &Changed)> = recent.iter().collect();
        order.sort_by(|a, b| b.1.ts.partial_cmp(&a.1.ts).unwrap_or(std::cmp::Ordering::Equal));
        let candidates: Vec<(String, f64, String, String, String)> = order
            .into_iter()
            .filter(|(path, _)| !exclude.contains(path.as_str()) && !resume_paths.contains(path))
            .map(|(path, c)| (path.clone(), c.ts, c.user.clone(), c.agent.clone(), String::new()))
            .collect();
        if candidates.is_empty() && shape.is_empty() && resume_blocks.is_empty() && map.is_empty() {
            return json!({"ok": true, "text": "", "modules": 0, "total": 0, "paths": []});
        }
        let total = candidates.len();
        // what the page and the resume did not use; the newest module always fits
        let remaining = budget.saturating_sub(used);
        let (blocks, rest, shown_paths) = if candidates.is_empty() {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            render_all(store, scope, &candidates, remaining, stamp, &changed, input, &mut learned, &mut draw)
        };
        let shown = blocks.len();
        let mut text = String::new();
        for line in &map {
            text.push_str(line);
            text.push('\n');
        }
        if !resume_blocks.is_empty() {
            text.push_str(&format!(
                "Resuming your session — {} module(s) in your working set, current facts first:",
                resume_blocks.len()
            ));
            for block in &resume_blocks {
                text.push('\n');
                text.push_str(block);
            }
            text.push('\n');
        }
        if !shape.is_empty() {
            text.push_str(&shape);
            text.push('\n');
        }
        if total > 0 {
            text.push_str(&format!(
                "Cork briefing{} — {shown} of {total} module(s) changed recently, newest first. {how_to_read} {RANGE_READ}",
                repo_tag(input.repo_id)
            ));
            let live = right_now(store, scope, input, stamp);
            if !live.is_empty() {
                text.push('\n');
                text.push_str(&live);
            }
            for block in blocks {
                text.push('\n');
                text.push_str(&block);
            }
            if !rest.is_empty() {
                let listed: Vec<&str> = rest.iter().take(MAX_REST).map(String::as_str).collect();
                text.push_str(&format!("\n… {} more changed: {}", rest.len(), listed.join(", ")));
            }
        } else {
            text.push_str(&format!("Cork briefing{} — no module changed recently. {RANGE_READ}", repo_tag(input.repo_id)));
            let live = right_now(store, scope, input, stamp);
            if !live.is_empty() {
                text.push('\n');
                text.push_str(&live);
            }
        }
        let mut paths = resume_paths.clone();
        paths.extend(shown_paths.iter().cloned());
        // the owner's standing decisions over the code this session is
        // about to meet: once per session, before it writes there
        let mut decision_paths = paths.clone();
        decision_paths.extend(rest.iter().take(MAX_REST).cloned());
        for line in crate::decisions::brief_lines(store, scope, input.viewer, input.session, &decision_paths, budget) {
            text.push('\n');
            text.push_str(&line);
        }
        // the one rule that makes the difference visible: the agent says,
        // in the terminal, what of this it acted on and whose it was
        text.push('\n');
        text.push_str(RULE);
        // the modules the tail only NAMED: the hook credits a write to one of
        // them as found, the briefing having said where to look
        let named: Vec<String> = rest.iter().take(MAX_REST).cloned().collect();
        return json!({"ok": true, "text": text, "modules": shown, "total": total, "paths": paths, "named": named,
                      "resumed": resume_paths.len(), "shape": !shape.is_empty(), "map": !map.is_empty(), "learned": learned.to_json(),
                      "checks": draw.checks});
    }

    // scoped to the prompt: rank every module the identifiers touch
    let idents: Vec<String> = input
        .identifiers
        .iter()
        .map(|i| i.trim().to_string())
        .filter(|i| i.len() >= 2)
        .take(MAX_IDENTIFIERS)
        .collect();
    let lowered: Vec<String> = idents.iter().map(|i| i.to_lowercase()).collect();
    let mut scores: BTreeMap<String, (i64, BTreeSet<String>, f64)> = BTreeMap::new(); // path -> (score, why, record ts)
    let mut matched_ids: BTreeSet<String> = BTreeSet::new();
    let mut matched_symbols: Vec<(String, String)> = Vec::new(); // (path, symbol)
    let mut all_dirs: BTreeSet<String> = BTreeSet::new();
    let records: Vec<Value> = store.kv_list(crate::codegraph::GRAPH_BUCKET, &format!("{scope}:")).into_iter().map(|(_, r)| r).collect();
    let mut defined_in: BTreeMap<String, usize> = BTreeMap::new();
    for record in &records {
        if let Some(symbols) = record.get("symbols").and_then(Value::as_object) {
            for ident_l in &lowered {
                if symbols.keys().any(|name| name.to_lowercase() == *ident_l) {
                    *defined_in.entry(ident_l.clone()).or_insert(0) += 1;
                }
            }
        }
    }
    for record in &records {
        let path = record.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        if path.is_empty() {
            continue;
        }
        all_dirs.insert(crate::graphview::top_dir(&path));
        let path_l = path.to_lowercase();
        let basename = path_l.rsplit('/').next().unwrap_or(&path_l).to_string();
        let stem = basename.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(basename.clone());
        let module_doc = record.get("doc").and_then(Value::as_str).unwrap_or("").to_lowercase();
        let symbols = record.get("symbols").and_then(Value::as_object);
        let mut score = 0i64;
        let mut weak = 0i64;
        let mut why: BTreeSet<String> = BTreeSet::new();
        for (ident, ident_l) in idents.iter().zip(lowered.iter()) {
            // a path or module name
            if ident_l.contains('/') || ident_l.contains('.') {
                let as_path = ident_l.trim_start_matches("./").to_string();
                // the identifier IS the path, or the repo path is its tail —
                // and the reverse, because an agent pastes the absolute path
                // its editor shows it and the map stores the relative one
                if path_l == as_path
                    || path_l.ends_with(&format!("/{as_path}"))
                    || as_path.ends_with(&format!("/{path_l}"))
                {
                    score += 6;
                    why.insert(ident.clone());
                    matched_ids.insert(ident.clone());
                    continue;
                }
                let dotted = ident_l.replace('.', "/");
                if path_l.trim_end_matches(".py").ends_with(&dotted) || path_l.contains(&format!("/{dotted}/")) {
                    score += 4;
                    why.insert(ident.clone());
                    matched_ids.insert(ident.clone());
                    continue;
                }
            }
            if stem == *ident_l {
                score += 3;
                why.insert(ident.clone());
                matched_ids.insert(ident.clone());
            }
            if let Some(symbols) = symbols {
                for (name, entry) in symbols {
                    let name_l = name.to_lowercase();
                    if name_l == *ident_l {
                        // a name defined in many modules is common, not a match
                        if defined_in.get(ident_l).copied().unwrap_or(0) <= COMMON_NAME {
                            score += EXACT_SYMBOL;
                        } else {
                            weak += 1;
                        }
                        why.insert(ident.clone());
                        matched_ids.insert(ident.clone());
                        matched_symbols.push((path.clone(), name.clone()));
                    } else if ident_l.len() >= 4 && name_l.contains(ident_l.as_str()) {
                        weak += 1;
                        why.insert(ident.clone());
                        matched_ids.insert(ident.clone());
                    }
                    if ident_l.len() >= 4 {
                        if let Some(doc) = entry.get("doc").and_then(Value::as_str) {
                            if doc.to_lowercase().contains(ident_l.as_str()) {
                                weak += 1;
                                why.insert(ident.clone());
                                matched_ids.insert(ident.clone());
                            }
                        }
                    }
                }
            }
            if ident_l.len() >= 4 && module_doc.contains(ident_l.as_str()) {
                weak += 1;
                why.insert(ident.clone());
                matched_ids.insert(ident.clone());
            }
        }
        score += weak.min(WEAK_CAP);
        if score > 0 {
            let ts = record.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            scores.insert(path, (score, why, ts));
        }
    }
    // a docstring word or a partial name alone is a guess, not a match
    scores.retain(|_, (score, _, _)| *score >= STRONG_MATCH);
    matched_symbols.retain(|(path, _)| scores.contains_key(path));
    // the task's scope: the files the prompt named or whose symbols it named,
    // before their dependents join. The briefing's ceiling scales with it
    let scope_files = scores.len().max(1);
    let ceiling = prompt_ceiling(scope_files).min(budget);
    // the modules that depend on a matched symbol break when it changes —
    // and where nothing does is the negative fact that saves the search.
    // Dependents come from the resolved snapshot (the same graph blast_radius
    // walks), so an aliased import (`serve as serve_a`) counts as a caller.
    let graph = crate::graphview::snapshot(store, scope);
    let mut extra: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut dependent_modules: BTreeSet<String> = BTreeSet::new();
    let mut capped = false;
    let mut with_callers: BTreeSet<String> = BTreeSet::new();
    let mut no_callers: Vec<String> = Vec::new();
    let mut callers_in: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, symbol) in &matched_symbols {
        let nid = format!("{path}::{symbol}");
        let levels = crate::graphview::blast_radius(&graph, &nid, 2);
        let mut caller_dirs: BTreeSet<String> = BTreeSet::new();
        let mut direct = 0usize;
        for (depth, level) in levels.iter().enumerate() {
            for node in level {
                let dep_path = node.get("path").and_then(Value::as_str).unwrap_or("");
                if dep_path.is_empty() || dep_path == path {
                    continue;
                }
                if depth == 0 {
                    direct += 1;
                    caller_dirs.insert(crate::graphview::top_dir(dep_path));
                }
                if dependent_modules.len() >= MAX_DEPENDENT_MODULES && !dependent_modules.contains(dep_path) {
                    capped = true;
                    continue;
                }
                dependent_modules.insert(dep_path.to_string());
                let slot = scores.entry(dep_path.to_string()).or_insert((0, BTreeSet::new(), 0.0));
                if depth == 0 {
                    slot.0 += DEPENDENT_BONUS;
                    slot.1.insert(format!("calls {symbol}"));
                    // what in it does the calling: all a dependent shows
                    if let Some(caller) = node.get("symbol").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                        let named = callers_in.entry(dep_path.to_string()).or_default();
                        if !named.iter().any(|n| n == caller) {
                            named.push(caller.to_string());
                        }
                    }
                } else {
                    slot.0 += DEPENDENT2_BONUS;
                    slot.1.insert(format!("calls a caller of {symbol}"));
                }
            }
        }
        if direct == 0 {
            // where nothing calls it is said once, in the scope line
            if !no_callers.contains(symbol) {
                no_callers.push(symbol.clone());
            }
            continue;
        }
        with_callers.insert(symbol.clone());
        let absent: Vec<String> = all_dirs.difference(&caller_dirs).cloned().collect();
        let line = if absent.is_empty() {
            format!("  {symbol} ← called from {}", caller_dirs.iter().cloned().collect::<Vec<_>>().join(", "))
        } else {
            format!("  {symbol} ← called from {}; none under {}",
                caller_dirs.iter().cloned().collect::<Vec<_>>().join(", "), absent.join(", "))
        };
        extra.entry(path.clone()).or_default().push(line);
    }
    // what changed on a matched module, and why its author said, under the
    // dependents lines: the prompt briefing used to carry only the shape
    for (path, lines) in change_lines(&recent) {
        if scores.contains_key(&path) {
            extra.entry(path).or_default().extend(lines);
        }
    }
    let matched_ids: BTreeSet<String> = scores
        .values()
        .flat_map(|(_, why, _)| why.iter().cloned())
        .filter(|w| !w.starts_with("calls "))
        .collect();
    let unmatched: Vec<&str> = idents
        .iter()
        .filter(|i| !matched_ids.contains(*i) && looks_like_code(i))
        .map(String::as_str)
        .collect();
    // the names this prompt used, for what the session goes on to look for
    crate::brieflearn::note_names(store, scope, input.session, &idents);
    // files earlier sessions went looking for under these names, after a
    // briefing that did not show them: briefed this time
    for (path, name) in crate::brieflearn::sought_for(store, scope, &idents, crate::brieflearn::MAX_ADDED) {
        if scores.contains_key(&path) || exclude.contains(path.as_str()) || !crate::codegraph::has_file(store, scope, &path) {
            continue;
        }
        let ts = store
            .kv_get(crate::codegraph::GRAPH_BUCKET, &format!("{scope}:{}", crate::codegraph::path_key(&path)))
            .and_then(|r| r.get("ts").and_then(Value::as_f64))
            .unwrap_or(0.0);
        scores.insert(path.clone(), (DEPENDENT_BONUS, [format!("sought for {name} before")].into(), ts));
        learned.added.push(path);
    }
    if scores.is_empty() {
        return json!({"ok": true, "text": decisions_alone(store, scope, input, &idents, budget), "modules": 0, "total": 0, "paths": [], "unmatched": unmatched});
    }
    // dependents only named, not shown: test files (what they test is
    // above), and modules agents routinely leave unopened after a briefing
    // shows them; never a module the prompt names
    let mut named_only: Vec<String> = Vec::new();
    let only_dependent = |why: &BTreeSet<String>| why.iter().all(|w| w.starts_with("calls "));
    scores.retain(|path, (_, why, _)| {
        if !only_dependent(why) || exclude.contains(path.as_str()) {
            return true;
        }
        let tests = is_test_path(path);
        let seldom = !tests && crate::brieflearn::shape(store, scope, path, stamp) == crate::brieflearn::Shape::Short;
        if tests || seldom {
            if seldom {
                learned.dropped.push(path.clone());
            }
            named_only.push(path.clone());
            return false;
        }
        true
    });
    let mut ranked: Vec<(String, i64, BTreeSet<String>, f64)> = scores
        .into_iter()
        .filter(|(path, _)| !exclude.contains(path.as_str()))
        .map(|(path, (score, why, ts))| {
            let recency = recent.get(&path).map(|c| c.ts).unwrap_or(ts);
            (path, score, why, recency)
        })
        .collect();
    // what the prompt names first, then what calls it: a dependent two
    // hops deep outscored the module that defines the named symbol, and on
    // a page sized to the task the module the task is about was left out
    let named_here = |why: &BTreeSet<String>| why.iter().any(|w| !w.starts_with("calls "));
    ranked.sort_by(|a, b| {
        named_here(&b.2).cmp(&named_here(&a.2)).then(b.1.cmp(&a.1)).then(b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
    });
    let candidates: Vec<(String, f64, String, String, String)> = ranked
        .iter()
        .map(|(path, _score, why, _)| {
            let (ts, user, agent) = match recent.get(path) {
                Some(c) => (c.ts, c.user.clone(), c.agent.clone()),
                None => (0.0, String::new(), String::new()),
            };
            (path.clone(), ts, user, agent, why_text(why, path))
        })
        .collect();
    if candidates.is_empty() {
        return json!({"ok": true, "text": decisions_alone(store, scope, input, &idents, budget), "modules": 0, "total": 0, "paths": [], "unmatched": unmatched});
    }
    let total = candidates.len();
    // how to read it is said once a session: the first briefing says it
    let how = if input.session.is_empty()
        || crate::access::tell_once(store, &format!("briefhow:{scope}:{}", input.session), 12.0 * 3600.0)
    {
        format!(" {}", if input.verified { HOW_TO_READ } else { HOW_TO_READ_UNCHECKED })
    } else {
        String::new()
    };
    let live = right_now(store, scope, input, stamp);
    // the module blocks get the ceiling less what always rides with them
    // (the header, who is live, the scope line, the tail), each named file
    // its share of it: a two-file task is about those two files
    let around = 64 + how.len() + live.len() + SCOPE_LINE_CHARS + PROMPT_TAIL.len();
    let blocks_budget = ceiling.saturating_sub(around).max(MIN_PROMPT_BLOCKS);
    let mut draw = Draw { share: scope_files.clamp(1, MAX_BLOCK_SHARE + 1), ..Draw::default() };
    for (path, symbol) in &matched_symbols {
        let named = draw.focus.entry(path.clone()).or_default();
        if !named.contains(symbol) {
            named.push(symbol.clone());
        }
    }
    // a module that is here only because it calls what the prompt names
    // shows the symbols that call it, not its whole outline
    for (path, _, why, _) in &ranked {
        if !named_here(why) {
            if let Some(callers) = callers_in.get(path) {
                draw.focus.insert(path.clone(), callers.clone());
                draw.callers_only.insert(path.clone());
            }
        }
    }
    let (blocks, rest, shown_paths) = render_all(store, scope, &candidates, blocks_budget, stamp, &extra, input, &mut learned, &mut draw);
    let shown = blocks.len();
    let mut text = format!("Cork briefing — scoped to your prompt: {shown} of {total} module(s).{how}");
    if !live.is_empty() {
        text.push('\n');
        text.push_str(&live);
    }
    for block in blocks {
        text.push('\n');
        text.push_str(&block);
    }
    // the edge of what was said, as scope: what the page covers, the facts
    // the map holds about what is not on it (nothing calls X, nothing is
    // named Y), and what is related but only named. Said as gaps ("not
    // expanded", "matched nothing") it read as "go look yourself", and
    // agents did, reading every file the briefing had already given them
    let mut scope_parts: Vec<String> = Vec::new();
    // a huge repo's graph is its working set: dependents outside it are not
    // in it, and saying "complete" would send the agent off without them
    if let Some(total) = graph.partial {
        scope_parts.push(format!(
            "the map holds the {} most recently used of this repo's {total} files, so dependents outside them are not \
listed: grep before changing a shared symbol",
            graph.files
        ));
    }
    if rest.is_empty() && !capped && !with_callers.is_empty() && graph.partial.is_none() {
        // every dependent within two hops is on the page: the agent has the
        // whole blast radius and a call to fetch it again would return the same
        scope_parts.push(format!(
            "every dependent of {} within 2 hops is listed here; no blast_radius call needed",
            with_callers.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !no_callers.is_empty() {
        scope_parts.push(format!("nothing in the map calls {}", listed(&no_callers, SCOPE_NAMES)));
    }
    // related modules the page had no room for, and dependents agents
    // routinely leave unopened: by name, one get_symbol away
    let mut by_name: Vec<&str> = rest.iter().take(MAX_REST).map(String::as_str).collect();
    by_name.extend(named_only.iter().take(MAX_REST).map(String::as_str));
    if !by_name.is_empty() {
        let more = rest.len().saturating_sub(MAX_REST);
        let tail = if more > 0 { format!(" (+{more} more)") } else { String::new() };
        scope_parts.push(format!("also related, by name only (get_symbol for one): {}{tail}", by_name.join(", ")));
    } else if capped {
        scope_parts.push(format!("dependents listed up to {MAX_DEPENDENT_MODULES} modules"));
    }
    // an id pasted into the prompt (a task's, a tool call's) names no code:
    // not listed, and with nothing else unmatched the clause is not said
    let unmatched: Vec<&str> = unmatched.into_iter().filter(|t| !cork_core::tokens::looks_like_id(t)).collect();
    if !unmatched.is_empty() {
        scope_parts.push(format!("no code in the map is named {}", listed(&unmatched, SCOPE_NAMES)));
    }
    if !scope_parts.is_empty() {
        text.push_str(&format!("\nScope — {}.", scope_parts.join("; ")));
    }
    // a transformation another agent already made on this symbol: one
    // command replays it — the work is reused, not just the facts
    let recipes = crate::recipes::matching(store, scope, input.visible, &idents, input.viewer, input.share_knowledge);
    for line in crate::recipes::brief_lines(&recipes, stamp) {
        text.push('\n');
        text.push_str(&line);
    }
    // the owner's standing decisions over the modules this prompt is about,
    // and any path it names: once per session
    let mut decision_paths: Vec<String> = shown_paths.clone();
    decision_paths.extend(idents.iter().filter(|i| i.contains('/') || i.contains('.')).cloned());
    for line in crate::decisions::brief_lines(store, scope, input.viewer, input.session, &decision_paths, budget) {
        text.push('\n');
        text.push_str(&line);
    }
    text.push('\n');
    text.push_str(PROMPT_TAIL);
    let mut named: Vec<String> = rest.iter().take(MAX_REST).cloned().collect();
    named.extend(named_only.iter().take(MAX_REST).cloned());
    // the shown modules whose matched symbol had its callers listed above:
    // a write to one of them is credited the callers search it did not need
    let mut covered: Vec<String> = Vec::new();
    for (path, symbol) in &matched_symbols {
        if with_callers.contains(symbol) && shown_paths.contains(path) && !covered.contains(path) {
            covered.push(path.clone());
        }
    }
    // what this briefing handed the agent, by counts and names, for the
    // prompt step's dropdown on the dashboard; the endpoint takes it off
    // the answer before the hook sees it (presence::note_exchange)
    let mut briefed_symbols: Vec<&str> = Vec::new();
    for (path, symbol) in &matched_symbols {
        if shown_paths.contains(path) && !briefed_symbols.contains(&symbol.as_str()) {
            briefed_symbols.push(symbol);
        }
    }
    let notes = text.lines().filter(|line| line.starts_with("  note (")).count();
    let exchange = json!({
        "symbols": briefed_symbols.len(), "symbol_names": briefed_symbols,
        "notes": notes, "recipes": recipes.len(),
        "callers": with_callers.len(), "caller_names": with_callers.iter().collect::<Vec<_>>(),
    });
    json!({"ok": true, "text": text, "modules": shown, "total": total, "paths": shown_paths, "named": named, "covered": covered,
           "ceiling": ceiling, "scope_files": scope_files, "checks": draw.checks,
           "unmatched": unmatched, "learned": learned.to_json(), "exchange": exchange})
}

/// Why a module is in a prompt's briefing, said once: the names it matches
/// (not its own path, which the line starts with), the matched symbols it
/// calls, and those it reaches through a caller.
fn why_text(why: &BTreeSet<String>, path: &str) -> String {
    let path_l = path.to_lowercase();
    let mut names: Vec<&str> = Vec::new();
    let mut calls: Vec<&str> = Vec::new();
    let mut hops: Vec<&str> = Vec::new();
    for w in why {
        if let Some(symbol) = w.strip_prefix("calls a caller of ") {
            hops.push(symbol);
        } else if let Some(symbol) = w.strip_prefix("calls ") {
            calls.push(symbol);
        } else if w.starts_with("sought for ") {
            names.push(w);
        } else {
            let w_l = w.to_lowercase();
            let w_l = w_l.trim_start_matches("./");
            if path_l == w_l || path_l.ends_with(&format!("/{w_l}")) || w_l.ends_with(&format!("/{path_l}")) {
                continue;
            }
            names.push(w);
        }
    }
    hops.retain(|h| !calls.contains(h));
    let mut parts: Vec<String> = Vec::new();
    if !names.is_empty() {
        parts.push(format!("matches {}", names.join(", ")));
    }
    if !calls.is_empty() {
        parts.push(format!("calls {}", calls.join(", ")));
    }
    if !hops.is_empty() {
        parts.push(format!("2 hops from {}", hops.join(", ")));
    }
    parts.join(", ")
}

/// What `brieflearn` changed in one briefing, for the answer's `learned`
/// field: nothing it does is silent.
#[derive(Default)]
struct Learned {
    short: Vec<String>,
    full: Vec<String>,
    dropped: Vec<String>,
    added: Vec<String>,
}

impl Learned {
    fn to_json(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (key, list) in [("short", &self.short), ("full", &self.full), ("dropped", &self.dropped), ("added", &self.added)] {
            if !list.is_empty() {
                out.insert(key.into(), json!(list));
            }
        }
        Value::Object(out)
    }
}

/// How to read a briefing, said once.
/// The repo id, once a session (the session start): every Cork tool
/// takes it, and v76, without it, sent agents hunting for it (`cat
/// .collide/config.json`, `collide status`): 18 commands a run in the
/// scaling study, mostly right after a gate block. The tools default it now
/// too (mcp.rs `default_repo_id`).
pub(crate) fn repo_tag(repo_id: &str) -> String {
    let repo_id = repo_id.trim();
    if repo_id.is_empty() { String::new() } else { format!(" (repo_id `{repo_id}`)") }
}

const HOW_TO_READ: &str = "Signatures, docstrings and [line ranges] are parsed from the code and checked against your copy of each file: build on them instead of reading the files.";
/// For a hook that does not check them (one that sends no `verifies`): where the facts come
/// from, and never "build on them".
const HOW_TO_READ_UNCHECKED: &str = "Signatures, docstrings and [line ranges] are parsed from each file as Cork last saw it, which may not be your copy: Read the range before relying on one.";
/// Said at every session start: an agent that believed Edit needs the whole
/// file read it whole, every time, briefing or not.
const RANGE_READ: &str = "To edit, Read only the range you need (Read with offset/limit): the Edit tool accepts a partial read.";

/// A prompt that matched no module may still name code under an owner's
/// standing decision: then the decision is the briefing.
fn decisions_alone(store: &Store, scope: &str, input: &BriefInput, idents: &[String], budget: usize) -> String {
    let paths: Vec<String> = idents.iter().filter(|i| i.contains('/') || i.contains('.')).cloned().collect();
    let lines = crate::decisions::brief_lines(store, scope, input.viewer, input.session, &paths, budget);
    if lines.is_empty() {
        return String::new();
    }
    format!("{}\n{PROMPT_TAIL}", lines.join("\n"))
}

/// snake_case, a path, a dotted name, or CamelCase — not a capitalized word.
/// Is this prompt asking for work on the code, or talking about something
/// else? A briefing costs the agent its tokens on every message it rides;
/// on "which database should we use?" it is noise. A prompt that names code
/// or asks for a change is briefed; so is a statement (a bug report reads as
/// one: "french customers are getting free shipping by accident"). What is
/// left, a question with neither, is briefed only when its meaning lands
/// close to real code (`meaning_close`, decided by the caller).
pub fn about_the_code(prompt: &str, names_code: bool, meaning_close: bool) -> bool {
    if names_code || meaning_close {
        return true;
    }
    const TASK: [&str; 32] = [
        "fix", "add", "build", "implement", "create", "write", "change", "update", "rename", "remove", "delete",
        "refactor", "move", "replace", "debug", "test", "deploy", "ship", "handle", "support", "integrate",
        "migrate", "port", "optimize", "improve", "clean", "wire", "bump", "revert", "patch", "edit", "make",
    ];
    const ASK: [&str; 18] = [
        "what", "whats", "why", "how", "which", "who", "where", "when", "should", "can", "could", "would", "is",
        "are", "do", "does", "did", "will",
    ];
    const FILLER: [&str; 10] = ["ok", "okay", "so", "also", "now", "then", "and", "yes", "yea", "yeah"];
    const REQUEST: [&str; 6] = ["can you", "could you", "can u", "please", "pls", "i need you to"];
    let lower = prompt.to_lowercase();
    let words_of = |text: &str| -> Vec<String> {
        text.split_whitespace()
            .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>())
            .filter(|w| !w.is_empty())
            .collect()
    };
    let is_task = |word: &str| {
        TASK.iter().any(|t| word == *t || word.strip_suffix('s') == Some(t) || word.strip_suffix("ed") == Some(t))
    };
    // each sentence's first word that is not filler ("Ok build that")
    let sentences: Vec<&str> = lower.split_inclusive(|c| matches!(c, '.' | '!' | '?' | '\n')).collect();
    let openers: Vec<String> = sentences
        .iter()
        .filter_map(|sentence| words_of(sentence).into_iter().find(|w| !FILLER.contains(&w.as_str())))
        .collect();
    // "When it is done, push" opens with a clause, not a question: "when" and
    // "where" ask only in a sentence that ends in a question mark
    let asks = |sentence: &str| {
        words_of(sentence).into_iter().find(|w| !FILLER.contains(&w.as_str())).is_some_and(|w| {
            ASK.contains(&w.as_str()) && (!matches!(w.as_str(), "when" | "where") || sentence.trim_end().ends_with('?'))
        })
    };
    let question = lower.contains('?') || sentences.iter().any(|s| asks(s));
    if !question {
        return true; // a statement: a request, or a bug report
    }
    // a question is work when it asks for it
    openers.iter().any(|w| is_task(w))
        || (REQUEST.iter().any(|r| lower.contains(r)) && words_of(&lower).iter().any(|w| is_task(w)))
}

/// A task that adds code (a feature, a function, a report), as opposed to
/// one that fixes or explains existing code: the one that may rewrite a
/// helper the repo already has.
pub fn adds_code(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| matches!(w, "add" | "adds" | "build" | "create" | "implement" | "write" | "new" | "wants" | "want" | "need" | "needs" | "support"))
}

/// The briefing's line for `embed::reuse_candidates`.
pub fn reuse_line(helpers: &[Value]) -> Option<String> {
    if helpers.is_empty() {
        return None;
    }
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let named: Vec<String> = helpers
        .iter()
        .map(|h| {
            let signature = text(h, "signature");
            let head = if signature.is_empty() { text(h, "symbol") } else { signature };
            let doc = text(h, "doc");
            let first = doc.split(". ").next().unwrap_or("").trim().trim_end_matches('.');
            let used = h.get("used_by").and_then(Value::as_u64).unwrap_or(0);
            let used = if used == 1 { "used in 1 other file".to_string() } else { format!("used in {used} other files") };
            if first.is_empty() {
                format!("`{head}` in {} ({used})", text(h, "path"))
            } else {
                format!("`{head}` in {} ({used}): {}", text(h, "path"), clip(first, 140))
            }
        })
        .collect();
    Some(format!(
        "Already in this repo for parts of your task (reuse them, do not write your own; they keep the house rules): {}.",
        named.join("; ")
    ))
}

/// A task that ends in sharing the work (commit, push, a PR): how to do it in
/// one step. Agents otherwise spend three or four messages on it (test, add,
/// commit, push, and a pull and retry when the push is rejected), each one
/// replaying the whole context, while the push hook already lands a chained
/// command: it rebases onto teammates' commits, reruns the tests and pushes.
/// `commit`: the hooks' own command that commits only the session's lines
/// (`.../cork commit --mine`), sent by hooks that have it; in a checkout
/// other agents share, `git add <file>` would carry their hunks too.
pub fn finish_line(prompt: &str, commit: &str) -> Option<String> {
    let lower = prompt.to_lowercase();
    let words: Vec<String> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let shares = words.iter().any(|w| matches!(w.as_str(), "commit" | "push" | "pr" | "ship" | "land"))
        || lower.contains("pull request");
    if !shares {
        return None;
    }
    let sane = commit.ends_with(" commit --mine")
        && commit.len() <= 300
        && !commit.chars().any(|c| c.is_control() || matches!(c, '`' | '$' | ';' | '&' | '|' | '"' | '\''));
    let commit_step = if sane { format!("{commit} -m \"...\"") } else { "git add <your files> && git commit -m \"...\"".to_string() };
    Some(format!(
        "Finish in one command when your part is done: run the tests && {commit_step} && git push. \
Cork lands the push itself (it rebases onto teammates' commits, reruns the tests and pushes), so a rejected push needs nothing more from you: no manual rebase, no retry."
    ))
}

fn looks_like_code(ident: &str) -> bool {
    if ident.contains('_') || ident.contains('/') || ident.contains('.') {
        return true;
    }
    let humps = ident.chars().filter(|c| c.is_ascii_uppercase()).count();
    humps >= 2 && ident.chars().any(|c| c.is_ascii_lowercase())
}

/// The repo on one page: size, subsystems, the symbols most of the repo
/// leans on, and the conventions its directories follow. Built from the
/// graph, so it costs the agent nothing to learn; capped so it stays a page.
/// Empty when the graph is cold — "not known yet" must not print as zeroes.
fn render_shape(store: &Store, scope: &str, cap: usize) -> String {
    let map = crate::graphview::briefing_map(store, scope, SHAPE_IDLE_S, SHAPE_ITEMS * 3);
    if map.is_null() {
        return String::new();
    }
    let count = |key: &str| map.get(key).and_then(Value::as_u64).unwrap_or(0);
    let mut langs: Vec<(String, i64)> = map
        .get("languages")
        .and_then(Value::as_object)
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0))).collect())
        .unwrap_or_default();
    langs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let langs: Vec<String> = langs.iter().take(3).map(|(k, v)| format!("{k} {v}")).collect();
    let mut head = format!("Repo shape — {} files, {} symbols, {} edges", count("files"), count("symbols"), count("edges"));
    if !langs.is_empty() {
        head.push_str(&format!("; {}", langs.join(", ")));
    }
    head.push('.');
    let mut used = head.len() + 1;
    let mut out = vec![head];
    let rows = |key: &str| -> Vec<Value> { map.get(key).and_then(Value::as_array).cloned().unwrap_or_default() };
    let field = |row: &Value, key: &str| -> String { row.get(key).and_then(Value::as_str).unwrap_or("").to_string() };
    let num = |row: &Value, key: &str| -> u64 { row.get(key).and_then(Value::as_u64).unwrap_or(0) };
    let subsystems: Vec<String> = rows("subsystems")
        .iter()
        .filter_map(|s| {
            let dir = field(s, "dir");
            let name = if dir.is_empty() { field(s, "label") } else { dir };
            // a one-file subsystem is a file, and the hubs line names it
            if name.is_empty() || num(s, "files") < 2 {
                return None;
            }
            // its hubs are on the "most depended on" line: said once
            Some(format!("{name} ({} files)", num(s, "files")))
        })
        .take(SHAPE_ITEMS)
        .collect();
    push_line(&mut out, &mut used, cap, "  subsystems: ", &subsystems);
    let all_hubs: Vec<Value> = rows("hubs").into_iter().filter(|h| !field(h, "symbol").is_empty()).collect();
    let code_hubs: Vec<Value> = all_hubs.iter().filter(|h| !is_test_path(&field(h, "path"))).cloned().collect();
    let picked = if code_hubs.len() >= 2 { code_hubs } else { all_hubs };
    let hubs: Vec<String> = picked
        .iter()
        .take(SHAPE_ITEMS)
        .map(|h| format!("{}::{} ({})", field(h, "path"), field(h, "symbol"), num(h, "dependents")))
        .collect();
    push_line(&mut out, &mut used, cap, "  most depended on: ", &hubs);
    let rules: Vec<String> = rows("conventions")
        .iter()
        .take(SHAPE_ITEMS)
        .map(|r| {
            let exceptions: Vec<&str> = r
                .get("exceptions")
                .and_then(Value::as_array)
                .map(|e| e.iter().filter_map(Value::as_str).take(3).collect())
                .unwrap_or_default();
            let tail = if exceptions.is_empty() { String::new() } else { format!(", except {}", exceptions.join(", ")) };
            format!("{} {} in {} of {}{tail}", field(r, "dir"), field(r, "rule"), num(r, "holds"), num(r, "of"))
        })
        .collect();
    push_line(&mut out, &mut used, cap, "  conventions: ", &rules);
    out.join("\n")
}

/// `prefix` + items joined by " · ", dropping trailing items until the line
/// fits what is left of the cap; nothing when none fit.
fn push_line(out: &mut Vec<String>, used: &mut usize, cap: usize, prefix: &str, items: &[String]) {
    let mut n = items.len();
    while n > 0 {
        let line = format!("{prefix}{}", items[..n].join(" · "));
        if *used + line.len() + 1 <= cap {
            *used += line.len() + 1;
            out.push(line);
            return;
        }
        n -= 1;
    }
}

/// Render candidates in order until the budget is spent; the first always fits.
/// A module's outcomes (`brieflearn`) set how much of it is shown.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn render_all(
    store: &Store, scope: &str, candidates: &[(String, f64, String, String, String)], budget: usize, stamp: f64,
    extra: &BTreeMap<String, Vec<String>>, input: &BriefInput, learned: &mut Learned, draw: &mut Draw,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut blocks: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut shown: Vec<String> = Vec::new();
    let mut used = 0usize;
    // a prompt's page: each block drawn whole once, then the budget shared
    // out smallest block first, so a short module (a test file, a two-line
    // view) leaves its share to a long one instead of the page going short
    let fair: Option<BTreeMap<String, usize>> = (draw.share > 0 && !draw.dry).then(|| {
        let mut dry = Draw { focus: draw.focus.clone(), callers_only: draw.callers_only.clone(), share: 1, dry: true, ..Draw::default() };
        let (whole, _, paths) = render_all(store, scope, candidates, budget, stamp, extra, input, &mut Learned::default(), &mut dry);
        water_fill(&paths.into_iter().zip(whole.iter().map(|b| b.len() + 1)).collect::<Vec<_>>(), budget)
    });
    for (path, ts, user, agent, why) in candidates {
        let view = crate::memory::get_symbol(store, scope, path, "");
        if !view.get("found").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let ts = if *ts > 0.0 { *ts } else { view.get("provenance").and_then(|p| p.get("ts")).and_then(Value::as_f64).unwrap_or(stamp) };
        let shape = crate::brieflearn::shape(store, scope, path, stamp);
        // a prompt's page is split among the files it names, each its share
        // (a module's floor is smaller there: the page is smaller)
        let (share, floor) = if draw.share == 0 { (MAX_BLOCK_SHARE, MIN_BLOCK) } else { (draw.share, MIN_PROMPT_BLOCK) };
        let cap = (budget / share).max(floor.min(budget));
        let cap = if shape == crate::brieflearn::Shape::Full { (cap * FULL_CAP_FACTOR).min(budget.max(MIN_BLOCK)) } else { cap };
        let cap = fair.as_ref().and_then(|f| f.get(path)).map(|c| (*c).max(floor.min(budget) / 2)).unwrap_or(cap);
        let verdict = tests_verdict(store, scope, path);
        // a teammate as the reader knows them (`nickname (email)`), and the
        // harness they work in, never the hook that reported the edit
        let who = if user.is_empty() || user == input.viewer {
            user.clone()
        } else {
            let users: BTreeSet<String> = [user.clone()].into();
            crate::activity::identity_labels(store, input.viewer, &users)
                .get(user).and_then(|l| l.get("label")).and_then(Value::as_str).unwrap_or(user).to_string()
        };
        let agent = crate::presence::agent_name(agent);
        // a module only two hops from what the prompt names breaks only if
        // the hop between changes: its header is enough, unless agents keep
        // reading it
        let drawn = if why.starts_with("2 hops from ") && shape != crate::brieflearn::Shape::Full {
            crate::brieflearn::Shape::Short
        } else {
            shape
        };
        let focus = draw.focus.get(path).cloned().unwrap_or_default();
        let only_focus = draw.callers_only.contains(path);
        // the lines after the outline (who calls it, what changed) keep
        // their room: the outline gets the rest of the block
        let after: usize = extra.get(path).map(|lines| lines.iter().map(|l| l.len() + 1).sum()).unwrap_or(0);
        let outline_cap = cap.saturating_sub(after).max(cap / 2);
        let mut block = render_module(
            path, ts, &who, &agent, why, &view, stamp, outline_cap, input, &verdict, drawn, &focus, only_focus, &mut draw.checks,
        );
        if let Some(lines) = extra.get(path) {
            for line in lines {
                block.push('\n');
                block.push_str(line);
            }
        }
        // the cap holds after the extra lines too, cut at a whole line: one
        // module's dependents list made a single briefing 51k tokens, and a
        // briefing is carried on every later message of the session
        if block.len() > cap {
            let mut kept = String::new();
            for line in block.lines() {
                if !kept.is_empty() && kept.len() + line.len() + 1 > cap {
                    kept.push_str(&format!("\n  … more in {path}"));
                    break;
                }
                if !kept.is_empty() {
                    kept.push('\n');
                }
                kept.push_str(line);
            }
            block = kept;
        }
        if !blocks.is_empty() && used + block.len() > budget && !draw.dry {
            rest.push(path.clone());
            continue;
        }
        used += block.len();
        blocks.push(block);
        shown.push(path.clone());
        match shape {
            crate::brieflearn::Shape::Short => learned.short.push(path.clone()),
            crate::brieflearn::Shape::Full => learned.full.push(path.clone()),
            crate::brieflearn::Shape::Usual => {}
        }
    }
    (blocks, rest, shown)
}

#[allow(clippy::too_many_arguments)]
fn render_module(
    path: &str, ts: f64, user: &str, agent: &str, why: &str, view: &Value, stamp: f64, cap: usize, input: &BriefInput,
    verdict: &str, shape: crate::brieflearn::Shape, focus: &[String], only_focus: bool, checks: &mut Vec<Value>,
) -> String {
    let (viewer, share_knowledge) = (input.viewer, input.share_knowledge);
    // who wrote it and when; a module only indexed or read has no author,
    // and "observed, just now" said nothing
    // (the harness it ran in is on the roster, not here: it changes nothing
    // the reader does)
    let _ = agent;
    let who = if user.is_empty() { String::new() } else { format!("{user}, {}", age_text(stamp - ts)) };
    let who_shown = !who.is_empty();
    let mut meta = match (why.is_empty(), who.is_empty()) {
        (true, true) => String::new(),
        (true, false) => who,
        (false, true) => why.to_string(),
        (false, false) => format!("{why}; {who}"),
    };
    let mut marks: Vec<String> = Vec::new();
    if let Some(cmd) = view.pointer("/provenance/verified/command").and_then(Value::as_str) {
        let ok = view.pointer("/provenance/verified/ok").and_then(Value::as_bool).unwrap_or(true);
        marks.push(if ok { format!("verified by `{cmd}`") } else { format!("check FAILED (`{cmd}`)") });
    }
    if !verdict.is_empty() {
        marks.push(verdict.to_string());
    }
    for mark in marks {
        if !meta.is_empty() {
            meta.push_str(", ");
        }
        meta.push_str(&mark);
    }
    let line = if meta.is_empty() { path.to_string() } else { format!("{path} — {meta}") };
    let mut out = vec![line];
    // routinely left unopened: its header (and what changed, below) is all
    if shape == crate::brieflearn::Shape::Short {
        return out.join("\n");
    }
    let full = shape == crate::brieflearn::Shape::Full;
    // a module here only for calling what the prompt names is its callers:
    // no module doc, no count of the rest
    if let Some(doc) = view.get("doc").and_then(Value::as_str).filter(|d| !d.is_empty() && !only_focus) {
        out.push(format!("  {}", clip(doc, if full { FULL_DOC_CHARS } else { MAX_MODULE_DOC_CHARS })));
    }
    let mut used: usize = out.iter().map(|l| l.len() + 1).sum();
    // symbols leave room for the notes when there are any: on a resume the
    // notes are what the agent most needs back
    let has_notes = view.get("anchored_memories").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false);
    let symbol_cap = if has_notes { cap * SYMBOL_SHARE / 10 } else { cap };
    if let Some(symbols) = view.get("symbols").and_then(Value::as_object) {
        // what the prompt named first, then what the module does (its
        // functions and types) in file order; its constants and values by
        // name, on one line. Listed in full, a module's constants crowded
        // out the functions the task was about (Lattice's db.rs: five
        // constants, then "… 54 more symbol(s)", none of them the ones named)
        let named = |name: &String| focus.contains(name);
        // after what the prompt names: what speaks of it (its row loader's
        // doc names the query it reads), then what is documented, in file
        // order; an undocumented helper last
        let speaks_of_focus = |entry: &Value| {
            let said = format!("{} {}", entry.get("signature").and_then(Value::as_str).unwrap_or(""), entry.get("doc").and_then(Value::as_str).unwrap_or(""));
            focus.iter().any(|f| said.contains(f.as_str()))
        };
        let documented = |entry: &Value| entry.get("doc").and_then(Value::as_str).is_some_and(|d| !d.is_empty());
        let mut ordered: Vec<(&String, &Value)> = symbols.iter().collect();
        ordered.sort_by_key(|(name, entry)| (!named(name), !speaks_of_focus(entry), !documented(entry), span_start(entry)));
        let (values, lines_of): (Vec<(&String, &Value)>, Vec<(&String, &Value)>) =
            ordered.into_iter().partition(|(name, entry)| is_value(entry) && !named(name));
        let mut skipped = 0usize;
        for (index, (name, entry)) in lines_of.iter().enumerate() {
            let line = symbol_line(name, entry, if full { FULL_DOC_CHARS } else { MAX_SYMBOL_DOC_CHARS });
            if index >= MAX_SYMBOL_LINES || (index > 0 && used + line.len() > symbol_cap) || (only_focus && !named(name)) {
                skipped += 1;
                continue;
            }
            used += line.len() + 1;
            checks.push(json!({"kind": "symbol", "path": path, "name": name, "line": line,
                               "signature": entry.get("signature").cloned().unwrap_or(json!("")),
                               "span": entry.get("span").cloned().unwrap_or(json!([0, 0]))}));
            out.push(line);
        }
        if only_focus {
            skipped += values.len();
        } else if !values.is_empty() {
            let names: Vec<&str> = values.iter().map(|(name, _)| name.as_str()).take(MAX_VALUE_NAMES).collect();
            let more = values.len() - names.len();
            let tail = if more > 0 { format!(" (+{more})") } else { String::new() };
            let line = format!("  constants: {}{tail}", names.join(", "));
            if skipped == 0 && used + line.len() <= symbol_cap {
                used += line.len() + 1;
                checks.push(json!({"kind": "names", "path": path, "names": names, "line": line}));
                out.push(line);
            } else {
                skipped += values.len();
            }
        }
        if skipped > 0 && !only_focus {
            out.push(format!("  … {skipped} more symbol(s) in {path}"));
        } else if skipped == 0 && input.verified && !symbols.is_empty() && view.get("freshness").and_then(Value::as_str) != Some(crate::semantics::STALE) {
            // every symbol is above, and the hook checks each against the
            // reader's copy: said, so the agent need not read it to be sure
            // (the hook takes the mark off a module whose copy differs)
            // after "becca, 5m ago" the time is said: unchanged since then
            let mark = if who_shown { "complete · unchanged".to_string() } else { outline_mark(stamp - ts) };
            out[0].push_str(&format!("{}{mark}", if meta.is_empty() { " — " } else { " · " }));
        }
    }
    out.extend(note_lines(path, view, viewer, share_knowledge, cap, &mut used));
    out.join("\n")
}

/// The ceiling a prompt briefing never grows past, in bytes, for a task of
/// `files` files (those it named, or whose symbols it named): PROMPT_BASE
/// and PROMPT_PER_FILE for each, up to PROMPT_MAX_BUDGET: the files and
/// what calls them. A two-file task gets about 475 tokens at most; one got
/// 2.5k, most of it about other code.
pub(crate) fn prompt_ceiling(files: usize) -> usize {
    (PROMPT_BASE + PROMPT_PER_FILE * files.max(1)).min(PROMPT_MAX_BUDGET)
}

/// A prompt briefing with what rides after its module blocks (`units`, in
/// the order they matter, each kept whole), inserted above its tail line,
/// each only while the whole stays under `ceiling`; a unit too long is
/// passed for a shorter one after it. A briefing that matched no code is
/// its first unit and the tail.
pub fn with_extras(text: &str, units: &[String], ceiling: usize) -> String {
    let mut units = units.iter().filter(|u| !u.trim().is_empty());
    let (mut body, tail) = if text.is_empty() {
        match units.next() {
            Some(first) => (first.clone(), PROMPT_TAIL.to_string()),
            None => return String::new(),
        }
    } else {
        match text.rsplit_once('\n') {
            Some((body, tail)) => (body.to_string(), tail.to_string()),
            None => (text.to_string(), String::new()),
        }
    };
    for unit in units {
        if body.len() + unit.len() + tail.len() + 2 <= ceiling {
            body.push('\n');
            body.push_str(unit);
        }
    }
    if tail.is_empty() { body } else { format!("{body}\n{tail}") }
}

/// Each block's room on a page of `budget` bytes, given what each would take
/// whole: smallest first, each the lesser of its size and an even share of
/// what is left, so what a short block does not use goes to the long ones.
fn water_fill(sizes: &[(String, usize)], budget: usize) -> BTreeMap<String, usize> {
    let mut order: Vec<&(String, usize)> = sizes.iter().collect();
    order.sort_by_key(|(_, size)| *size);
    let mut left = budget;
    let mut out = BTreeMap::new();
    for (index, (path, size)) in order.iter().enumerate() {
        let even = left / (order.len() - index);
        let room = (*size).min(even);
        left -= room;
        out.insert(path.clone(), room);
    }
    out
}

/// `a, b, c (+4)`: a list said in at most `max` names.
fn listed<S: AsRef<str>>(items: &[S], max: usize) -> String {
    let shown: Vec<&str> = items.iter().take(max).map(AsRef::as_ref).collect();
    let more = items.len().saturating_sub(shown.len());
    if more > 0 { format!("{} (+{more})", shown.join(", ")) } else { shown.join(", ") }
}

/// A symbol's line range as the briefing prints it (` [120-180]`), empty
/// when the parse has none.
pub(crate) fn span_text(entry: &Value) -> String {
    match entry.get("span").and_then(Value::as_array).map(|s| s.iter().filter_map(Value::as_u64).collect::<Vec<_>>()) {
        Some(s) if s.len() == 2 && s[0] > 0 && s[1] >= s[0] => format!(" [{}-{}]", s[0], s[1]),
        _ => String::new(),
    }
}

/// Where a symbol starts, for file order (0 when the parse has no span).
fn span_start(entry: &Value) -> u64 {
    entry.get("span").and_then(Value::as_array).and_then(|s| s.first()).and_then(Value::as_u64).unwrap_or(0)
}

/// A constant or a plain value (`const COLS = 3`, `pub const STATUSES: …`),
/// not a function bound to a name (`const mix = (a, b) => …`).
fn is_value(entry: &Value) -> bool {
    let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("");
    let signature = entry.get("signature").and_then(Value::as_str).unwrap_or("");
    matches!(kind, "constant" | "variable") && !signature.contains("=>") && !signature.contains("function")
}

/// One symbol as the briefing prints it: `  signature [a-b] — doc`. The hook
/// renders a line it corrects from the reader's copy the same way.
fn symbol_line(name: &str, entry: &Value, doc_chars: usize) -> String {
    let signature = entry.get("signature").and_then(Value::as_str).unwrap_or("");
    let signature = if signature.is_empty() { name } else { signature };
    // where each symbol is, on every line: the read that remains is the
    // lines that matter (Read with offset/limit), never the file
    let span = span_text(entry);
    match entry.get("doc").and_then(Value::as_str).filter(|d| !d.is_empty()) {
        Some(doc) => format!("  {signature}{span} — {}", clip(doc, doc_chars)),
        None => format!("  {signature}{span}"),
    }
}

/// A module whose every symbol was listed, as of its last known change
/// (the map re-parses a file on every write and every session's index):
/// "complete · unchanged since 40m ago".
fn outline_mark(age_s: f64) -> String {
    if age_s < 120.0 {
        "complete · current".to_string()
    } else {
        format!("complete · unchanged since {}", age_text(age_s))
    }
}

/// A module's notes as briefing lines, sharing the block's cap (`used` is
/// what the block already holds): teammates' notes only when the plan shares
/// knowledge, standing decisions left to their own line, the same fact once,
/// and a note reality scored down not served.
fn note_lines(path: &str, view: &Value, viewer: &str, share_knowledge: bool, cap: usize, used: &mut usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(notes) = view.get("anchored_memories").and_then(Value::as_array) {
        // notes share the block's cap with the symbols: a module with long
        // memories still renders as a page, and the same fact twice is once
        let mut seen: HashSet<&str> = HashSet::new();
        let mut shown = 0usize;
        let mut skipped = 0usize;
        for note in notes {
            // teammates' notes are knowledge, and knowledge is Team: without
            // sharing, a note is rendered only when the reader wrote it
            if !share_knowledge && !crate::access::own_knowledge(note, viewer) {
                continue;
            }
            // an owner's standing decision has its own line, once per session
            if crate::decisions::is_decision(note) {
                continue;
            }
            // a turn's closing words are another session's reply, not a fact
            // about the code; and a note on code that changed since it was
            // written has no reason to be here (get_symbol still has both)
            if is_turn_note(note) || note.get("stale").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            let fact = note.get("fact").and_then(Value::as_str).unwrap_or("").trim();
            if fact.is_empty() || !seen.insert(fact) {
                continue;
            }
            // a note reality has scored down is not served: the cache stays
            // small enough to trust, and stale confident advice stops arriving
            if note.get("confidence").and_then(Value::as_f64).unwrap_or(1.0) < SERVE_CONFIDENCE {
                continue;
            }
            let owner = note.get("owner").and_then(Value::as_str).unwrap_or("");
            let flag = if note.get("stale").and_then(Value::as_bool).unwrap_or(false) {
                " [stale]"
            } else if note.get("possibly_stale").is_some() {
                " [possibly stale]"
            } else {
                ""
            };
            let line = format!("  note ({owner}){flag}: {}", clip(fact, MAX_NOTE_CHARS));
            if shown >= MAX_NOTES || *used + line.len() > cap {
                skipped += 1;
                continue;
            }
            *used += line.len() + 1;
            out.push(line);
            shown += 1;
        }
        if skipped > 0 {
            out.push(format!("  … {skipped} more note(s) on {path}"));
        }
    }
    out
}

/// A note a turn's closing message left (intents::settle_turn): its words
/// are the agent's reply to its person.
fn is_turn_note(note: &Value) -> bool {
    note.get("tags").and_then(Value::as_array).is_some_and(|tags| tags.iter().any(|t| t.as_str() == Some("turn")))
}

/// How long a teammate's change to a touched file stays news for a first touch.
const TOUCH_CHANGES_S: f64 = 86_400.0;
/// Files a first touch is briefed on, at most.
const TOUCH_PATHS: usize = 3;
/// Caller lines a first write carries, at most.
const TOUCH_CALLERS: usize = 5;
/// A first touch's briefing never grows past this, whatever it holds.
const TOUCH_CAP: usize = 1_500;
// a first touch rides the session like a prompt briefing: it stays inside
// the prompt briefing's budget, tail included
const _: () = assert!(TOUCH_CAP + PROMPT_TAIL.len() + 1 <= PROMPT_MAX_BUDGET);

/// The briefing for a repo a session reaches after it started: a chat that
/// began in a folder that is not this repo (a project folder, another repo)
/// read or wrote a file here for the first time. It is the least that
/// prevents damage, and nothing when there is nothing to say: who is live in
/// the repo, the owner's standing decisions and the notes on the files
/// touched, and, before a write lands, who calls those files and what a
/// teammate changed in them lately. No repo shape, no recency list, no
/// signatures (a read hands the agent the file anyway).
pub fn touch(store: &Store, scope: &str, input: &BriefInput, paths: &[String], write: bool) -> String {
    let stamp = now();
    let paths: Vec<&String> = paths
        .iter()
        .filter(|p| !p.is_empty() && !crate::codegraph::is_collide_artifact(p))
        .take(TOUCH_PATHS)
        .collect();
    let mut lines: Vec<String> = Vec::new();
    let live = right_now(store, scope, input, stamp);
    if !live.is_empty() {
        lines.push(live);
    }
    let owned: Vec<String> = paths.iter().map(|p| (*p).clone()).collect();
    lines.extend(crate::decisions::brief_lines(store, scope, input.viewer, input.session, &owned, TOUCH_CAP));
    let recent = if write { recent_writes(store, scope, stamp - TOUCH_CHANGES_S) } else { BTreeMap::new() };
    let changed = change_lines(&recent);
    let graph = if write { Some(crate::graphview::snapshot(store, scope)) } else { None };
    for path in &paths {
        let view = crate::memory::get_symbol(store, scope, path, "");
        let mut block: Vec<String> = Vec::new();
        let mut used = 0usize;
        if view.get("found").and_then(Value::as_bool).unwrap_or(false) {
            block.extend(note_lines(path, &view, input.viewer, input.share_knowledge, TOUCH_CAP, &mut used));
        }
        if let Some(graph) = graph.as_ref() {
            // what a teammate changed here lately: the reader's own edits are
            // in its context already
            if recent.get(path.as_str()).is_some_and(|c| c.user != input.viewer) {
                block.extend(changed.get(path.as_str()).cloned().unwrap_or_default());
            }
            // who calls what this file defines, by folder: what the write can break
            let mut callers = 0usize;
            for name in view.get("symbols").and_then(Value::as_object).map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                if callers == TOUCH_CALLERS {
                    break;
                }
                let levels = crate::graphview::blast_radius(graph, &format!("{path}::{name}"), 1);
                let dirs: BTreeSet<String> = levels
                    .first()
                    .into_iter()
                    .flatten()
                    .filter_map(|node| node.get("path").and_then(Value::as_str))
                    .filter(|dep| !dep.is_empty() && dep != path)
                    .map(crate::graphview::top_dir)
                    .collect();
                if !dirs.is_empty() {
                    block.push(format!("  {name} ← called from {}", dirs.into_iter().collect::<Vec<_>>().join(", ")));
                    callers += 1;
                }
            }
        }
        if !block.is_empty() {
            lines.push(format!("{path}:"));
            lines.extend(block);
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut text = format!("Cork — first look at {} this session:", input.repo_id);
    for line in lines {
        if text.len() + line.len() + 1 > TOUCH_CAP {
            break;
        }
        text.push('\n');
        text.push_str(&line);
    }
    text.push('\n');
    text.push_str(PROMPT_TAIL);
    text
}

/// The one rule that makes Cork visible in the terminal: appended to every
/// session-start briefing, so the agent says what of the briefing it acted
/// on and whose work that was, in one sentence, before acting.
const RULE: &str = "Say what you use: when something here changes what you do (a teammate's work, a note, a recipe, a claim), tell the user in one sentence before acting, naming who and what (\"Becca changed save_doc's signature 3m ago, so I'm updating its callers\"). Say nothing about what did not matter.";

/// The prompt briefing's shorter reminder of the same rule.
pub(crate) const PROMPT_TAIL: &str = "Say so to the user, in one sentence naming who, when any of this changes what you do.";

/// Other agents working in this repo right now, one line each, for the
/// briefing: who (by nickname when the reader has one for them), what they
/// are doing, their declared task, how long ago. Empty when nobody else is
/// live, so a solo session pays nothing for it. Another session of the
/// reader's own is named as such.
fn right_now(store: &Store, scope: &str, input: &BriefInput, stamp: f64) -> String {
    let view = crate::activity::list_activity(store, scope, input.repo_id, input.idle_after_s, input.visible);
    let rows: Vec<&Value> = view
        .get("workspaces")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    // rows name the scope's own repo id, which a renamed repo keeps while
    // answering to its new name too; the id the caller asked with may be
    // either, so the scope's is the one to match
    let scope_repo = scope.split_once(':').map(|(_, repo)| repo).unwrap_or(scope);
    let live: Vec<&Value> = rows
        .into_iter()
        .filter(|row| row.get("online").and_then(Value::as_bool).unwrap_or(false))
        .filter(|row| row.get("repo_id").and_then(Value::as_str) == Some(scope_repo))
        .filter(|row| {
            let user = row.get("user").and_then(Value::as_str).unwrap_or("");
            let session = row.get("session").and_then(Value::as_str).unwrap_or("");
            !(user == input.viewer && session == input.session)
        })
        // a later briefing of the same session names only what moved since
        // the last one said it: who is where, once
        .filter(|row| {
            // (a resumed session lost what it was told: it hears it all again)
            if input.session.is_empty() || input.resume {
                return true;
            }
            // where they are is the file, or for a command the kind of
            // step: a teammate running command after command is told once,
            // not again with every command (each one re-sent its text)
            let place = match command_of(row) {
                Some((action, _)) => action.to_string(),
                None => row.get("current_path").and_then(Value::as_str).unwrap_or("").to_string(),
            };
            let said = format!(
                "{}|{}|{}",
                row.get("session").and_then(Value::as_str).unwrap_or(""),
                place,
                row.get("task").and_then(Value::as_str).unwrap_or("")
            );
            let digest: String = sha1_hex(&said).chars().take(12).collect();
            crate::access::tell_once(store, &format!("rightnow:{scope}:{}:{digest}", input.session), 3600.0)
        })
        .take(MAX_RIGHT_NOW)
        .collect();
    if live.is_empty() {
        return String::new();
    }
    let users: BTreeSet<String> = live
        .iter()
        .filter_map(|row| row.get("user").and_then(Value::as_str).map(str::to_string))
        .filter(|user| user != input.viewer)
        .collect();
    let labels = crate::activity::identity_labels(store, input.viewer, &users);
    let mut out = vec!["Right now, other agents in this repo:".to_string()];
    for row in &live {
        let user = row.get("user").and_then(Value::as_str).unwrap_or("");
        let who = if user == input.viewer {
            // by its Claude Code name, the address a message `--to` takes,
            // and its title only while no task (said below) says what it does
            let title = clip(row.get("title").and_then(Value::as_str).unwrap_or(""), 60);
            let has_task = row.get("task").and_then(Value::as_str).is_some_and(|t| !t.is_empty());
            let title = if has_task { String::new() } else { title };
            let named = crate::freshness::quoted_label(&title, row.get("cc_name").and_then(Value::as_str).unwrap_or(""));
            if named.is_empty() { "another session of yours".to_string() } else { format!("another session of yours ({named})") }
        } else {
            // the label carries the address a message takes (`Becca (bob@x.com)`)
            labels.get(user).and_then(|l| l.get("label")).and_then(Value::as_str).unwrap_or(user).to_string()
        };
        let kind = row.pointer("/last_action/kind").and_then(Value::as_str).unwrap_or("");
        let doing = match kind {
            "editing" | "reading" | "running" | "searching" => kind,
            _ => "working on",
        };
        let path = row.get("current_path").and_then(Value::as_str).unwrap_or("");
        let task = row.get("task").and_then(Value::as_str).unwrap_or("");
        let task = if task.is_empty() { String::new() } else { format!(", task \"{}\"", clip(task, 80)) };
        let idle = row.get("last_activity_s").and_then(Value::as_f64).unwrap_or(0.0);
        let _ = stamp;
        // a command is its action and program, never its text
        if let Some((action, command)) = command_of(row) {
            out.push(format!("  {who}: {}{task}, {}", cork_core::privacy::gist(action, command), age_text(idle)));
        } else if path.is_empty() {
            // no file yet: the agent has a prompt and has not opened anything
            out.push(format!("  {who}: starting a task{task}, {}", age_text(idle)));
        } else {
            out.push(format!("  {who}: {doing} {path}{task}, {}", age_text(idle)));
        }
    }
    // no rule here on messaging the others: said to every agent with anyone
    // else at work, it made each one announce its plan to all the others
    // (Study 8, second study: 58-141 message batches per round). The hook
    // says how when the prompt asks to tell others, or when the agent
    // changes code others call.
    out.join("\n")
}

/// The command a roster row's current line stands for, with its action
/// (`running` or `searching`): the command in flight on the focus, else the
/// last action when that was a command. None when the row is on a file.
fn command_of(row: &Value) -> Option<(&str, &str)> {
    let current = row.get("current_path").and_then(Value::as_str).unwrap_or("");
    fn command<'a>(action: Option<&'a Value>, path: Option<&'a Value>) -> Option<(&'a str, &'a str)> {
        let action = action.and_then(Value::as_str).filter(|a| matches!(*a, "running" | "searching"))?;
        Some((action, path.and_then(Value::as_str).unwrap_or("")))
    }
    // a started command stands on the focus while the last action still
    // names the step before it
    if let Some((action, path)) = command(row.pointer("/focus/action"), row.pointer("/focus/path")) {
        if path == current {
            return Some((action, path));
        }
    }
    command(row.pointer("/last_action/kind"), row.pointer("/last_action/path"))
}

/// What the dependents' tests said about the newest live change to `path`,
/// from its hot marker: "tests passed after the change", "tests FAILING
/// after the change (2 failed)", or "" when nothing has been verified.
fn tests_verdict(store: &Store, scope: &str, path: &str) -> String {
    let prefix = format!("hot:{scope}:{}:", crate::repo::path_key(path));
    let newest = store
        .eph_scan(&prefix)
        .into_iter()
        .map(|(_key, marker)| marker)
        .max_by(|a, b| {
            let ta = a.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            let tb = b.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal)
        });
    let Some(marker) = newest else { return String::new() };
    let Some(verified) = marker.get("verified").filter(|v| v.is_object()) else { return String::new() };
    let count = |key: &str| verified.get(key).and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
    match verified.get("status").and_then(Value::as_str).unwrap_or("") {
        "clear" => "tests passed after the change".to_string(),
        "failing" => format!("tests FAILING after the change ({} failed)", count("failed")),
        "partial" => "tests passed after the change, partly covered".to_string(),
        _ => String::new(),
    }
}

fn sha1_hex(text: &str) -> String {
    use sha1::Digest;
    sha1::Sha1::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The first `max` characters of a fact, on a word boundary, with an ellipsis.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    let cut = head.rfind(' ').filter(|&i| i > max / 2).unwrap_or(head.len());
    format!("{}…", head[..cut].trim_end())
}

/// Test scaffolding is what depends on everything; it is not what an agent
/// writing code leans on.
pub(crate) fn is_test_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.contains("/tests/") || path.contains("/test/") || path.starts_with("tests/") || path.starts_with("test/")
        || name.starts_with("test_") || name == "conftest.py" || name.ends_with("_test.rs")
        || name.ends_with(".test.ts") || name.ends_with(".test.tsx") || name.ends_with(".spec.ts")
}

fn age_text(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 120.0 {
        "just now".to_string()
    } else if s < 7_200.0 {
        format!("{}m ago", (s / 60.0) as i64)
    } else if s < 172_800.0 {
        format!("{}h ago", (s / 3_600.0) as i64)
    } else {
        format!("{}d ago", (s / 86_400.0) as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::{about_the_code, age_text, clip, is_test_path};

    #[test]
    fn a_prompt_about_the_code_is_briefed_and_a_conversation_is_not() {
        // Study 5's tickets, vague as they are, are all work on the code
        for task in [
            "the revenue number on the dashboard is way too high, i think refunds are getting counted in it. fix it",
            "french customers are getting free shipping by accident",
            "we arent adding tax for british customers when they check out",
            "the WELCOME10 code is supposed to be one time use per customer but ppl keep reusing it",
            "uk customers arent getting charged vat at checkout?? it should be 20%. can u fix that",
            "loyalty points are way too high, people get points for orders that were cancelled or never paid",
            "Ok build that. Also what do you suggest for embeddings?",
            // a task that says what to do after it: "when" opens a clause here
            "the orders export for accounting should also have the customers email in it. When it is done and the tests pass, commit and push to origin main.",
            "accounting also wants each customer's region as a column in the orders export. When it is done, push. Where the file has a header, keep it.",
        ] {
            assert!(about_the_code(task, false, false), "{task}");
        }
        // this session's strategy questions are not
        for talk in [
            "What about what cursor does? The turbopuffer",
            "Keep it concise. Is collide and its hooks saving me tokens in this session right now",
            "What’s the absolute best option",
            "So it wouldn’t be faster adding this? Then what makes cursor so fast?",
            "Should I make people add a card?",
            "When does the digest go out?",
        ] {
            assert!(!about_the_code(talk, false, false), "{talk}");
        }
        // a question that names code, or means it closely, is still briefed
        assert!(about_the_code("why does levy_for return 0 for UK?", true, false));
        assert!(about_the_code("why do UK customers pay no VAT?", false, true));
    }

    #[test]
    fn a_task_that_adds_code_is_told_which_helpers_exist() {
        assert!(super::adds_code("add `find_customer(email)` in tillhouse/support/lookup_tool.py"));
        assert!(super::adds_code("accounting wants a monthly revenue csv"));
        assert!(!super::adds_code("the revenue number on the dashboard is way too high. fix it"));
        let line = super::reuse_line(&[serde_json::json!({"path": "tillhouse/core/csvout.py", "symbol": "to_csv",
            "signature": "def to_csv(header: list[str], rows: list[list]) -> str",
            "doc": "CSV text with a header row. Fields are quoted when they need it.", "used_by": 2})]).unwrap();
        assert!(line.contains("`def to_csv(header: list[str], rows: list[list]) -> str` in tillhouse/core/csvout.py (used in 2 other files): CSV text with a header row"), "{line}");
        assert!(super::reuse_line(&[]).is_none());
    }

    #[test]
    fn a_task_that_ends_in_a_push_is_told_to_finish_in_one_command() {
        assert!(super::finish_line("fix it. When it is done, commit and push to origin main", "").unwrap().contains("git add <your files>"));
        assert!(super::finish_line("open a pull request for the fix", "").is_some());
        assert!(super::finish_line("fix the refunds on the dashboard", "").is_none());
        assert!(super::finish_line("the pushback on the proposal was fair", "").is_none());
        // hooks that commit only the session's own lines say how
        let mine = super::finish_line("commit and push it", "/Users/a/.collide/bin/collide commit --mine").unwrap();
        assert!(mine.contains("&& /Users/a/.collide/bin/collide commit --mine -m \"...\" && git push"), "{mine}");
        assert!(!mine.contains("git add"));
        // anything else is never echoed into a command
        let odd = super::finish_line("commit and push it", "rm -rf ~; collide commit --mine").unwrap();
        assert!(odd.contains("git add <your files>") && !odd.contains("rm -rf"));
    }

    #[test]
    fn a_note_clips_on_a_word_boundary_with_an_ellipsis() {
        assert_eq!(clip("short fact", 240), "short fact");
        let long = "word ".repeat(100);
        let clipped = clip(&long, 40);
        assert!(clipped.ends_with('…') && clipped.chars().count() <= 41, "{clipped}");
        assert!(!clipped.contains("  "), "no dangling space before the ellipsis");
    }

    #[test]
    fn test_scaffolding_is_recognised_by_path() {
        assert!(is_test_path("server/tests/conftest.py"));
        assert!(is_test_path("tests/spec.py"));
        assert!(is_test_path("src/app/test_routes.py"));
        assert!(is_test_path("web/src/Button.test.tsx"));
        assert!(!is_test_path("server/cork-rs/src/store.rs"));
        assert!(!is_test_path("src/collide/contest.py"));
    }

    fn teammate_runs(store: &crate::store::Store, session: &str, action: &str, path: &str) {
        crate::presence::report_presence(store, &crate::presence::PresenceInput {
            scope: "w:github.com/acme/a", user_id: "bob@x.io", session, path, action, agent: "claude-code", model: "opus", effort: Default::default(),
            branch: "dev", tokens: 0, turn_id: "", context: 0, via: "", worker: "",
        });
    }

    fn right_now_for(store: &crate::store::Store, session: &str) -> String {
        let visible: std::collections::BTreeSet<String> = ["w:github.com/acme/a".to_string()].into_iter().collect();
        let input = super::BriefInput {
            repo_id: "github.com/acme/a", since: 0.0, budget: 0, identifiers: &[], exclude: &[], session, resume: false,
            idle_after_s: 300.0, visible: &visible, viewer: "kevin@x.com", share_knowledge: true, verified: true,
        };
        super::right_now(store, "w:github.com/acme/a", &input, crate::store::now())
    }

    /// The field report: an MCP briefing an hour earlier had seated the
    /// person with no session. Their live session then wrote a file (in a
    /// worktree, on another branch), and its own next briefing said
    /// "another session of yours: working on <that file>, just now": the
    /// sessionless seat had claimed the live session's edit.
    #[test]
    fn a_sessionless_seat_never_claims_the_live_sessions_edit() {
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:github.com/acme/a";
        crate::presence::touch_presence(&store, scope, "kevin@x.com", "", "claude", "mcp");
        for key in ["focus:w:kevin@x.com", "lastact:w:kevin@x.com"] {
            let mut seat = store.eph_get(key).expect("the sessionless seat");
            seat["ts"] = serde_json::json!(crate::store::now() - 3600.0);
            store.eph_set(key, &seat, Some(86_400.0)).unwrap();
        }
        crate::presence::report_presence(&store, &crate::presence::PresenceInput {
            scope, user_id: "kevin@x.com", session: "s-me", path: "sed -n 1,20p notify.rs", action: "running", agent: "claude-code",
            model: "opus", effort: Default::default(), branch: "main", tokens: 0, turn_id: "", context: 0, via: "", worker: "",
        });
        crate::report::report_edit(&store, &crate::report::ReportInput {
            scope, user_id: "kevin@x.com", path: "server/notify.rs", content: "fn a() {}\n", local: None, agent: "claude-code",
            model: "opus", effort: Default::default(), branch: "signup-news", session: "s-me", tokens: 0,
            input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0,
            turn_id: "t1", context: 0, why: "", hot_ttl_s: 600.0, via: "", workspace: "w", draft: false,
            auto: true, verified: None, worker: "", resent: false,
        });
        assert_eq!(right_now_for(&store, "s-me"), "", "the session itself is the only one here");
        let view = crate::activity::list_activity(&store, scope, "github.com/acme/a", 300.0, &[scope.to_string()].into_iter().collect());
        let rows = view["workspaces"].as_array().unwrap();
        let seat = rows.iter().find(|r| r["session"] == serde_json::json!("")).expect("the seat is still listed");
        assert_eq!(seat["online"], serde_json::json!(false), "an hour-old seat is not live: {seat}");
        let live = rows.iter().find(|r| r["session"] == serde_json::json!("s-me")).unwrap();
        assert_eq!(live["current_path"], serde_json::json!("server/notify.rs"), "the edit stays its writer's");
        // someone else still sees the live session
        assert!(right_now_for(&store, "s-other").contains("another session of yours"));
    }

    #[test]
    fn a_teammates_next_command_is_not_announced_again() {
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        teammate_runs(&store, "s-bob", "running", "cargo test --release -- brief::");
        let first = right_now_for(&store, "s-me");
        assert!(first.contains("bob@x.io: running cargo test, just now"), "{first}");
        assert!(!first.contains("--release") && !first.contains("brief::"), "{first}");
        // another command, then a search: the same place (a command), said already
        teammate_runs(&store, "s-bob", "running", "cargo build --release -j 4");
        assert_eq!(right_now_for(&store, "s-me"), "", "a new command is not news");
        teammate_runs(&store, "s-bob", "running", "cargo clippy --release");
        assert_eq!(right_now_for(&store, "s-me"), "");
        // a different file is
        teammate_runs(&store, "s-bob", "reading", "src/billing.py");
        let moved = right_now_for(&store, "s-me");
        assert!(moved.contains("bob@x.io: reading src/billing.py"), "{moved}");
        teammate_runs(&store, "s-bob", "reading", "src/billing.py");
        assert_eq!(right_now_for(&store, "s-me"), "", "the same file again is not news");
        // back to commands after the file: a command was said this hour
        teammate_runs(&store, "s-bob", "running", "npm run build");
        assert_eq!(right_now_for(&store, "s-me"), "");
        // another reader hears it all
        assert!(right_now_for(&store, "s-other").contains("bob@x.io: running npm run build, just now"));
    }

    #[test]
    fn a_long_command_is_told_as_its_action_and_program() {
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let long = format!(
            "cd /Users/bob/code/app && OPENAI_API_KEY=sk-abcdefghijklmnopqrstuv .venv/bin/python -m pytest -q {} 2>&1 | tail -40",
            (0..40).map(|i| format!("tests/test_mod_{i}.py")).collect::<Vec<_>>().join(" ")
        );
        teammate_runs(&store, "s-bob", "running", &long);
        let text = right_now_for(&store, "s-me");
        let line = text.lines().nth(1).expect("a line for bob");
        assert_eq!(line, "  bob@x.io: running pytest, just now");
        // a search names no pattern
        teammate_runs(&store, "s-bob2", "searching", "\"compute_tax\"");
        let text = right_now_for(&store, "s-me2");
        assert!(text.contains("bob@x.io: searching, just now") && !text.contains("compute_tax"), "{text}");
    }

    /// Another session of the reader's is listed by its title and the name
    /// Claude Code gives it, either of which `--to` takes.
    #[test]
    fn another_session_of_mine_is_listed_by_its_title_and_its_claude_code_name() {
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:github.com/acme/a";
        crate::presence::report_presence(&store, &crate::presence::PresenceInput {
            scope, user_id: "kevin@x.com", session: "s-two", path: "src/api.rs", action: "editing", agent: "claude-code",
            model: "opus", effort: Default::default(), branch: "dev", tokens: 0, turn_id: "", context: 0, via: "", worker: "",
        });
        crate::freshness::note_title(&store, scope, "kevin@x.com", "s-two", "Cork version number");
        crate::freshness::note_session(&store, scope, "kevin@x.com", "s-two", "cork-linear-test-e3", "claude", "m1");
        let text = right_now_for(&store, "s-me");
        assert!(text.contains("another session of yours (\"Cork version number\", cork-linear-test-e3): reading src/api.rs"), "{text}");
    }

    #[test]
    fn ages_read_coarsely() {
        assert_eq!(age_text(5.0), "just now");
        assert_eq!(age_text(300.0), "5m ago");
        assert_eq!(age_text(10_000.0), "2h ago");
        assert_eq!(age_text(300_000.0), "3d ago");
    }
}

#[cfg(test)]
#[path = "brief_budget_tests.rs"]
mod budget_tests;
#[cfg(test)]
#[path = "brief_ab_tests.rs"]
mod ab_tests;
