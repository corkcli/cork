//! Read-side answers over the ledger: `recap`, the "what happened here"
//! question.
//!
//! `journal` folds ledger rows into agent SESSIONS — one entry per
//! (user, session-or-agent) run, split on a 30-minute silence — and `costs`
//! folds the same window into token/dollar accounting. `recap` is the two
//! together, which is what an agent asking "what did my agents do / what
//! changed today" actually wants: git records committed OUTCOMES, the
//! ledger records the PROCESS — uncommitted edits, which model spent which
//! tokens, sessions that never got committed at all.
//!
//! Everything here is pure read; nothing mutates state.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::compat::python_round;
use crate::store::{now, LedgerRow, Store};

/// Hard bound on ledger rows walked per question: on a very busy scope this
/// reports on the most
/// RECENT rows in the window rather than growing without bound.
const MAX_ROWS: usize = 4000;
const SESSION_GAP_S: f64 = 30.0 * 60.0;

// Prices live in `pricing.rs`: each provider's list price per counter
// (input, output, cache read, 5-minute and 1-hour cache writes), read off
// their pricing pages, never one blended number.

/// The four usage counters a hook-reported row can carry, plus `tokens`:
/// the pre-v20 single field, which was uncached input + output only.
const USAGE_FIELDS: [&str; 4] =
    ["input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens"];

/// (input, output) $/Mtok for a model id.
#[cfg(test)]
pub(crate) fn usd_per_mtok(model: &str) -> (f64, f64) {
    let p = crate::pricing::price(model);
    (p.input, p.output)
}

/// What a cache-read token costs on this model: its own listed rate.
pub(crate) fn cache_read_usd_per_mtok(model: &str) -> f64 {
    crate::pricing::price(model).cache_read
}

/// What a token entering an agent's context for the first time costs on
/// this model, when it is then carried in every later message: a cache
/// write. Claude Code writes its main conversation's cache for an hour
/// (300 main transcripts read on 2026-10-17: every write a 1-hour one;
/// subagents write for 5 minutes), so a Claude model's is the 1-hour rate;
/// an OpenAI model's is its cache-write rate (its input rate where none is
/// charged).
pub(crate) fn context_write_usd_per_mtok(model: &str) -> f64 {
    let p = crate::pricing::price(model);
    if crate::pricing::normalize(model).starts_with("claude") { p.write_1h } else { p.write_5m }
}

/// (input, output, cache_read, cache_creation, legacy) for one ledger row.
///
/// A row written by a v20+ hook carries the four real counters; an older row
/// carries only `tokens`, which was input+output with the cache fields
/// deliberately dropped. The two are never mixed: when the four are present
/// `tokens` is the same turn counted worse, so it is ignored.
pub(crate) fn row_usage(payload: &Value) -> [i64; 5] {
    let mut counts = [0i64; 5];
    for (slot, field) in USAGE_FIELDS.iter().enumerate() {
        counts[slot] = int(payload, field);
    }
    if counts.iter().any(|c| *c != 0) {
        return counts;
    }
    [0, 0, 0, 0, int(payload, "tokens")]
}

/// Every token the turn was billed for, cached ones included.
pub(crate) fn usage_tokens(usage: &[i64; 5]) -> i64 {
    usage[0] + usage[1] + usage[2] + usage[3] + usage[4]
}

/// What that usage cost, each counter at its own rate and every cache write
/// at the 5-minute rate (`row_usd` prices a row that says more).
///
/// The legacy `tokens` field is priced at the OUTPUT rate: it is uncached
/// input + output, and in a cached agent loop uncached input is a rounding
/// error next to output (~100 vs ~1500 tokens a turn), so output is the rate
/// that fits what the field actually held.
#[cfg(test)]
pub(crate) fn usage_usd(model: &str, usage: &[i64; 5]) -> f64 {
    let counts = crate::pricing::Counts {
        input: usage[0], output: usage[1] + usage[4], cache_read: usage[2], cache_write: usage[3], cache_write_1h: 0,
    };
    crate::pricing::usd(model, &counts, &crate::pricing::Serving::default())
}

/// What one ledger row's usage cost: its model, its counters, the part of
/// its cache writes that lived an hour (hooks v79+; an older row's writes
/// are all priced at the 5-minute rate), and how it was served (fast mode,
/// US-only inference, a long-context prompt).
pub(crate) fn row_usd(payload: &Value) -> f64 {
    let usage = row_usage(payload);
    let counts = crate::pricing::Counts {
        input: usage[0], output: usage[1] + usage[4], cache_read: usage[2], cache_write: usage[3],
        cache_write_1h: int(payload, "cache_creation_1h_tokens"),
    };
    crate::pricing::usd(&text(payload, "model"), &counts, &crate::pricing::Serving::of(payload))
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn int(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// `YYYY-MM-DD` in UTC for a ledger row's timestamp.
pub(crate) fn day_key(ts: f64) -> String {
    let secs = ts.floor() as i64;
    let nanos = ((ts - ts.floor()).max(0.0) * 1e9).round() as u32;
    DateTime::<Utc>::from_timestamp(secs, nanos).unwrap_or_default().format("%Y-%m-%d").to_string()
}

/// A counter that remembers what it saw first — see `briefing::Tally`, which
/// this mirrors: ties break by "first seen", not alphabetically.
#[derive(Default)]
struct Tally {
    counts: BTreeMap<String, i64>,
    order: Vec<String>,
}

impl Tally {
    fn add(&mut self, key: &str, n: i64) {
        match self.counts.get_mut(key) {
            Some(count) => *count += n,
            None => {
                self.counts.insert(key.to_string(), n);
                self.order.push(key.to_string());
            }
        }
    }

    fn get(&self, key: &str) -> i64 {
        self.counts.get(key).copied().unwrap_or(0)
    }

    /// Key/count pairs, most first, ties in first-seen order; capped at
    /// `limit` when given.
    fn entries_desc(&self, limit: Option<usize>) -> Vec<(String, i64)> {
        let mut ranked: Vec<(usize, &String)> = self.order.iter().enumerate().collect();
        ranked.sort_by_key(|(position, key)| (-self.counts[*key], *position));
        let entries = ranked.into_iter().map(|(_, key)| (key.clone(), self.get(key)));
        match limit {
            Some(n) => entries.take(n).collect(),
            None => entries.collect(),
        }
    }

    fn most_common(&self, limit: usize) -> Vec<String> {
        self.entries_desc(Some(limit)).into_iter().map(|(key, _)| key).collect()
    }
}

fn tally_object(tally: &Tally, limit: Option<usize>) -> Value {
    let mut map = serde_json::Map::new();
    for (key, count) in tally.entries_desc(limit) {
        map.insert(key, json!(count));
    }
    Value::Object(map)
}

/// Ledger rows for this scope at or after `since_ts`, oldest first, capped
/// to the most recent `MAX_ROWS`. The store answers the scope+ts range
/// in one query; what is left here is the bound: on a scope
/// with more than `MAX_ROWS` rows inside the window, keep the newest ones
/// (the tail of this ascending vec), same as walking backward from "now"
/// and stopping at `MAX_ROWS`.
fn walk_ledger(store: &Store, scope: &str, since_ts: f64) -> Vec<LedgerRow> {
    let mut rows = store.ledger_since(scope, since_ts);
    if rows.len() > MAX_ROWS {
        rows = rows.split_off(rows.len() - MAX_ROWS);
    }
    rows
}

/// `user` or `owner`, whichever the row carries.
fn session_user(payload: &Value) -> String {
    let user = text(payload, "user");
    if !user.is_empty() {
        return user;
    }
    text(payload, "owner")
}

/// The key a row's session groups under: an explicit session id, else the
/// agent name, else `work`.
fn session_subkey(payload: &Value) -> String {
    let session = text(payload, "session");
    if !session.is_empty() {
        return session;
    }
    let agent = text(payload, "agent");
    if !agent.is_empty() {
        return agent;
    }
    "work".to_string()
}

/// First `n` Unicode characters — by code point, not byte.
fn truncate_chars(value: &str, n: usize) -> String {
    value.chars().take(n).collect()
}

struct Session {
    user: String,
    session: String,
    agent: String,
    model: String,
    /// the effort `model` ran at, read off the same row
    effort: crate::presence::Effort,
    start: f64,
    end: f64,
    edits: i64,
    tokens: i64,
    lines_added: i64,
    lines_removed: i64,
    parse_failures: i64,
    paths: Tally,
    rationales: Vec<String>,
    intents_completed: i64,
    watchdog: i64,
    /// what the usage counted in `tokens` cost, row by row, the way `costs`
    /// prices it
    usd: f64,
}

/// One session summed over its stretches in the window (`journal`'s `whole`).
#[derive(Default)]
struct Whole {
    stretches: usize,
    start: f64,
    end: f64,
    active_s: f64,
    usd: f64,
    tokens: i64,
    edits: i64,
    lines_added: i64,
    lines_removed: i64,
    parse_failures: i64,
    watchdog: i64,
    rationales: Vec<String>,
    paths: Tally,
}

/// Ledger rows folded into sessions: one entry per (user, session-id or
/// agent) run, split on a 30-minute silence. An entry with a session id
/// also carries `whole`: that session summed over all its runs here.
pub fn journal(store: &Store, scope: &str, days: f64) -> Value {
    let since = now() - days.max(0.02) * 86_400.0;
    let rows = walk_ledger(store, scope, since); // oldest first already

    let mut sessions: Vec<Session> = Vec::new();
    let mut open_by_key: HashMap<String, usize> = HashMap::new();
    // one turn's tokens, however many files it touched
    let mut seen_turns: BTreeSet<String> = BTreeSet::new();

    for row in &rows {
        let payload = &row.payload;
        let user = session_user(payload);
        if user.is_empty() {
            continue;
        }
        let agent = text(payload, "agent");
        let key = format!("{user}:{}", session_subkey(payload));
        let ts = row.ts;

        let needs_new = match open_by_key.get(&key) {
            Some(&idx) => ts - sessions[idx].end > SESSION_GAP_S,
            None => true,
        };
        if needs_new {
            sessions.push(Session {
                user: user.clone(),
                session: String::new(),
                agent: agent.clone(),
                model: text(payload, "model"),
                effort: crate::presence::Effort::of(payload),
                start: ts,
                end: ts,
                edits: 0,
                tokens: 0,
                lines_added: 0,
                lines_removed: 0,
                parse_failures: 0,
                paths: Tally::default(),
                rationales: Vec::new(),
                intents_completed: 0,
                watchdog: 0,
                usd: 0.0,
            });
            open_by_key.insert(key.clone(), sessions.len() - 1);
        }
        let idx = open_by_key[&key];
        let current = &mut sessions[idx];
        current.end = current.end.max(ts);
        // a run grouped by agent name may pick its session id up later
        if current.session.is_empty() {
            current.session = text(payload, "session");
        }

        // what the run spent: an edit's own usage (hooks before v72), or a
        // bill the session's transcript was summed into (`turn_usage`, v72+,
        // and an unchanged re-report's turn before that), each turn once
        if matches!(row.kind.as_str(), "edit_reported" | "turn_usage") {
            let turn = text(payload, "turn_id");
            if !seen_turns.contains(&turn) {
                if !turn.is_empty() {
                    seen_turns.insert(turn);
                }
                let usage = row_usage(payload);
                current.tokens += usage_tokens(&usage);
                if usage_tokens(&usage) != 0 {
                    current.usd += row_usd(payload);
                }
            }
        }

        match row.kind.as_str() {
            "turn_usage" => {
                if current.model.is_empty() && crate::compat::truthy(payload.get("model")) {
                    current.model = text(payload, "model");
                    current.effort = crate::presence::Effort::of(payload);
                }
            }
            "edit_reported" => {
                current.edits += 1;
                current.lines_added += int(payload, "lines_added");
                current.lines_removed += int(payload, "lines_removed");
                if crate::compat::truthy(payload.get("model")) {
                    current.model = text(payload, "model");
                    current.effort = crate::presence::Effort::of(payload);
                }
                // "partial" = source that failed to parse cleanly (a real
                // smell); "unsupported" is just a non-code file and must
                // NOT count
                if text(payload, "parse") == "partial" {
                    current.parse_failures += 1;
                }
                if crate::compat::truthy(payload.get("path")) {
                    current.paths.add(&text(payload, "path"), 1);
                }
            }
            "intent_completed" => {
                current.intents_completed += 1;
                if crate::compat::truthy(payload.get("rationale")) {
                    current.rationales.push(truncate_chars(&text(payload, "rationale"), 240));
                }
            }
            "watchdog" => current.watchdog += 1,
            _ => {}
        }
    }

    // a session runs on past a 30-minute silence: the entries above are its
    // stretches of work, and `whole` is the session itself, summed over the
    // stretches in the window. One agent's page shows the session; showing
    // its newest stretch read as the cost resetting after every pause.
    let mut wholes: HashMap<String, Whole> = HashMap::new();
    for s in sessions.iter().filter(|s| !s.session.is_empty()) {
        let whole = wholes.entry(format!("{}#{}", s.user, s.session)).or_insert_with(|| Whole {
            start: s.start,
            end: s.end,
            ..Whole::default()
        });
        whole.stretches += 1;
        whole.start = whole.start.min(s.start);
        whole.end = whole.end.max(s.end);
        whole.active_s += s.end - s.start;
        whole.usd += s.usd;
        whole.tokens += s.tokens;
        whole.edits += s.edits;
        whole.lines_added += s.lines_added;
        whole.lines_removed += s.lines_removed;
        whole.parse_failures += s.parse_failures;
        whole.watchdog += s.watchdog;
        whole.rationales.extend(s.rationales.iter().cloned());
        for (path, count) in s.paths.entries_desc(None) {
            whole.paths.add(&path, count);
        }
    }

    // most recently started first, ties in the order sessions were opened
    sessions.sort_by(|a, b| b.start.partial_cmp(&a.start).unwrap_or(std::cmp::Ordering::Equal));

    let out: Vec<Value> = sessions
        .iter()
        .map(|s| {
            let whole = wholes.get(&format!("{}#{}", s.user, s.session)).map(|w| {
                json!({
                    "stretches": w.stretches,
                    "start": w.start, "end": w.end,
                    // time at work: the stretches' lengths, not the pauses between them
                    "active_s": python_round(w.active_s, 1),
                    "edits": w.edits, "tokens": w.tokens,
                    "lines_added": w.lines_added, "lines_removed": w.lines_removed,
                    "est_usd": python_round(w.usd, 2),
                    "files": w.paths.order.len(),
                    "top_paths": w.paths.most_common(4),
                    "parse_failures": w.parse_failures,
                    "watchdog": w.watchdog,
                    "rationales": w.rationales,
                })
            });
            json!({
                "whole": whole,
                "user": s.user, "agent": s.agent, "model": s.model,
                "effort": s.effort.effort, "ultracode": s.effort.ultracode,
                "start": s.start, "end": s.end,
                "edits": s.edits, "tokens": s.tokens,
                "lines_added": s.lines_added, "lines_removed": s.lines_removed,
                "parse_failures": s.parse_failures,
                "rationales": s.rationales,
                "intents_completed": s.intents_completed,
                "watchdog": s.watchdog,
                // the raw per-session counter never reaches the response,
                // only its top 4
                "top_paths": s.paths.most_common(4),
                "session": s.session,
                // priced per model, rounded, the way `costs` prices the window
                "est_usd": python_round(s.usd, 2),
                "files": s.paths.order.len(),
            })
        })
        .collect();

    json!({"sessions": out, "window_days": days})
}

/// Token accounting for the window, counting each TURN once.
///
/// One assistant turn writes one ledger row per file it touched, every row
/// carrying that whole turn's usage — so summing the rows multiplied the
/// bill by files-per-turn. `turn_id` rides the row precisely so this can fold
/// them back together; rows from before it existed carry none and are counted
/// individually, as they always were.
pub fn costs(store: &Store, scope: &str, days: f64) -> Value {
    costs_across(store, &[scope.to_string()], days)
}

/// [`costs`] over several repos at once (a workspace's, as the dashboard
/// shows them), with `by_repo` and `repos` when there is more than one. One
/// scope reads exactly as `costs` always did. Each model's tokens and
/// dollars are also split by the effort it ran at (`by_model_effort`,
/// `usd_by_model_effort`: model -> effort -> count, "" for a turn that named
/// none), and its ultracode turns counted apart (`ultracode_by_model`).
pub fn costs_across(store: &Store, scopes: &[String], days: f64) -> Value {
    let since = now() - days.max(0.02) * 86_400.0;
    // walked newest-first: that order decides which row of a turn is the one
    // kept, which entry wins a tie in the sorted-by-count output below, and
    // the summation order for est_usd (float addition is not associative)
    let mut rows: Vec<(String, LedgerRow)> = Vec::new();
    for scope in scopes {
        let repo = scope.split_once(':').map(|(_, repo)| repo).unwrap_or(scope).to_string();
        let mut mine = walk_ledger(store, scope, since);
        mine.reverse();
        rows.extend(mine.into_iter().map(|row| (repo.clone(), row)));
    }
    if scopes.len() > 1 {
        rows.sort_by(|(_, a), (_, b)| b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal).then(b.seq.cmp(&a.seq)));
    }
    let mut by_repo = Tally::default();

    let mut by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut by_model = Tally::default();
    let mut by_user = Tally::default();
    let mut by_path_root = Tally::default();
    let mut usd_by_model: BTreeMap<String, f64> = BTreeMap::new();
    // the same tokens and dollars split by the effort each model ran at
    // ("" for a turn that named none), and the share run in ultracode
    let mut by_model_effort: BTreeMap<String, Tally> = BTreeMap::new();
    let mut usd_by_model_effort: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    let mut ultracode_by_model = Tally::default();
    let mut usd_ultracode_by_model: BTreeMap<String, f64> = BTreeMap::new();
    let mut unpriced: BTreeSet<String> = BTreeSet::new();
    let mut by_kind = [0i64; 5];
    let mut cache_creation_1h: i64 = 0;
    let mut est: f64 = 0.0;
    let mut seen_turns: BTreeSet<String> = BTreeSet::new();
    let mut total: i64 = 0;

    for (repo, row) in &rows {
        let payload = &row.payload;
        let usage = row_usage(payload);
        let tokens = usage_tokens(&usage);
        if tokens == 0 {
            continue;
        }
        let turn = text(payload, "turn_id");
        if !turn.is_empty() {
            if seen_turns.contains(&turn) {
                continue; // the same turn, reported again for another file
            }
            seen_turns.insert(turn);
        }
        total += tokens;
        by_repo.add(repo, tokens);
        for slot in 0..5 {
            by_kind[slot] += usage[slot];
        }
        cache_creation_1h += int(payload, "cache_creation_1h_tokens");

        *by_day.entry(day_key(row.ts)).or_insert(0) += tokens;

        let model = text(payload, "model");
        let model = if model.is_empty() { "unknown".to_string() } else { model };
        by_model.add(&model, tokens);
        // each row at its own model's rates, its write TTLs and its serving
        let usd = row_usd(payload);
        est += usd;
        *usd_by_model.entry(model.clone()).or_insert(0.0) += usd;
        let effort = crate::presence::Effort::of(payload);
        by_model_effort.entry(model.clone()).or_default().add(&effort.effort, tokens);
        *usd_by_model_effort.entry(model.clone()).or_default().entry(effort.effort).or_insert(0.0) += usd;
        if effort.ultracode {
            ultracode_by_model.add(&model, tokens);
            *usd_ultracode_by_model.entry(model.clone()).or_insert(0.0) += usd;
        }
        if !crate::pricing::lookup(&model).1 {
            unpriced.insert(model);
        }

        let user = text(payload, "user");
        if !user.is_empty() {
            by_user.add(&user, tokens);
        }

        let path = text(payload, "path");
        let root = path.split('/').next().unwrap_or("");
        by_path_root.add(if root.is_empty() { "(root)" } else { root }, tokens);
    }

    let mut out = json!({
        "window_days": days,
        "total_tokens": total,
        "by_day": by_day,
        "by_model": tally_object(&by_model, None),
        "by_user": tally_object(&by_user, None),
        "by_path_root": tally_object(&by_path_root, Some(8)),
        "by_kind": {"input": by_kind[0], "output": by_kind[1], "cache_read": by_kind[2],
                    "cache_creation": by_kind[3], "cache_creation_1h": cache_creation_1h, "legacy": by_kind[4]},
        "turns": seen_turns.len(),
        "est_usd": python_round(est, 2),
        "usd_by_model": usd_by_model.iter().map(|(m, usd)| (m.clone(), json!(python_round(*usd, 2)))).collect::<serde_json::Map<_, _>>(),
        "by_model_effort": by_model_effort.iter().map(|(m, efforts)| (m.clone(), tally_object(efforts, None))).collect::<serde_json::Map<_, _>>(),
        "usd_by_model_effort": usd_by_model_effort.iter().map(|(m, efforts)| {
            (m.clone(), json!(efforts.iter().map(|(e, usd)| (e.clone(), json!(python_round(*usd, 2)))).collect::<serde_json::Map<_, _>>()))
        }).collect::<serde_json::Map<_, _>>(),
        "ultracode_by_model": tally_object(&ultracode_by_model, None),
        "usd_ultracode_by_model": usd_ultracode_by_model.iter().map(|(m, usd)| (m.clone(), json!(python_round(*usd, 2)))).collect::<serde_json::Map<_, _>>(),
        "unpriced_models": unpriced.into_iter().collect::<Vec<_>>(),
        "estimate_note": "each model's list price per counter: input, output, cache reads, and cache \
                          writes at the 5-minute or 1-hour rate as the transcript recorded them, fast \
                          mode and US-only inference included; unpriced_models are priced at $3/$15 — \
                          list prices, not an invoice",
    });
    if scopes.len() > 1 {
        out["by_repo"] = tally_object(&by_repo, None);
        out["repos"] = json!(scopes.len());
    }
    out
}

/// What happened in this repo, from the ledger: agent sessions plus the
/// window's token/cost accounting. PREFER THIS OVER `git log` for "what
/// changed / what did my agents do / summarize today" — git records
/// committed OUTCOMES; the ledger records the PROCESS: uncommitted edits,
/// which model spent which tokens, decisions with their rationales, and
/// attempts that were reverted.
pub fn recap(store: &Store, scope: &str, days: f64) -> Value {
    let window = days.max(0.02).min(90.0);
    let mut out = journal(store, scope, window);
    let costs_value = costs(store, scope, window);
    if let Some(map) = out.as_object_mut() {
        map.insert("costs".to_string(), costs_value);
    }
    out
}

// ------------------------------------------------------------------ search

/// Notes store anchors as `{"kind","path","symbol"}` objects, or (older
/// notes) as a bare `"path::symbol"` string — normalize both to (path,
/// symbol). Duplicated from `agenttools::note_anchor` (private there, and
/// small enough that sharing it was not worth a new `pub` seam) — see
/// `Tally` above for the same call on `briefing::Tally`.
fn note_anchor(note: &Value) -> (String, String) {
    match note.get("anchor") {
        Some(Value::Object(map)) => (
            map.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
            map.get("symbol").and_then(Value::as_str).unwrap_or("").to_string(),
        ),
        Some(Value::String(raw)) => match raw.split_once("::") {
            Some((path, symbol)) => (path.to_string(), symbol.to_string()),
            None => (raw.clone(), String::new()),
        },
        _ => (String::new(), String::new()),
    }
}

fn note_text(note: &Value) -> String {
    let t = text(note, "text");
    if !t.is_empty() { t } else { text(note, "fact") }
}

/// Substring search across ledger facts and memory notes, case-insensitive.
/// Mirrors `insights.search`.
///
/// Matches are collected newest-first from each source (ledger, then
/// memory) and capped at `limit` DURING the scan — so on a scope with more
/// hits than `limit`, which rows make it in depends on scan order, even
/// though the printed order (sorted by `ts` descending, below) does not.
/// `walk_ledger` returns oldest-first capped to the most recent `MAX_ROWS`;
/// it is reversed here to scan newest-first.
pub fn search(store: &Store, scope: &str, q: &str, limit: usize) -> Value {
    let needle = q.trim().to_lowercase();
    if needle.is_empty() {
        return json!({"q": q, "results": []});
    }
    // (ts, entry); a final stable sort by -ts (a missing ts sorts as 0)
    let mut results: Vec<(f64, Value)> = Vec::new();

    let rows = walk_ledger(store, scope, 0.0);
    for row in rows.iter().rev() {
        if results.len() >= limit {
            break;
        }
        let payload = &row.payload;
        let symbols: Vec<String> = payload
            .get("symbols_changed")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let hay = format!(
            "{} {} {} {} {} {} {}",
            text(payload, "path"), symbols.join(" "), text(payload, "rationale"),
            text(payload, "intent_id"), text(payload, "user"), text(payload, "owner"),
            row.kind,
        )
        .to_lowercase();
        if !hay.contains(&needle) {
            continue;
        }
        let by = {
            let user = text(payload, "user");
            if !user.is_empty() { user } else { text(payload, "owner") }
        };
        let mut symbols_sorted = symbols;
        symbols_sorted.sort();
        symbols_sorted.truncate(6);
        let rationale = text(payload, "rationale");
        let mut entry = json!({
            "type": if row.kind == "intent_completed" && crate::compat::truthy(payload.get("rationale")) {
                "rationale"
            } else {
                "ledger"
            },
            "kind": row.kind, "ts": row.ts, "seq": row.seq,
            "by": by, "path": text(payload, "path"),
            "symbols": symbols_sorted,
        });
        if crate::compat::truthy(payload.get("rationale")) {
            entry["text"] = json!(truncate_chars(&rationale, 240));
        }
        results.push((row.ts, entry));
    }

    for (_key, note) in store.kv_list("memory", &format!("{scope}:")) {
        if results.len() >= limit {
            break;
        }
        let (a_path, a_symbol) = note_anchor(&note);
        let tags: Vec<String> = note
            .get("tags")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let hay = format!("{} {} {} {}", note_text(&note), a_path, a_symbol, tags.join(" ")).to_lowercase();
        if !hay.contains(&needle) {
            continue;
        }
        let created = note.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        let anchor = if !a_symbol.is_empty() { format!("{a_path}::{a_symbol}") } else { a_path };
        let entry = json!({
            "type": if note.get("auto").and_then(Value::as_str) == Some("scar") { "scar" } else { "note" },
            "ts": created, "by": text(&note, "by"),
            "text": truncate_chars(&note_text(&note), 240),
            "anchor": anchor,
        });
        results.push((created, entry));
    }

    // stable: ties (equal ts) keep ledger-before-memory / scan order
    results.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);
    let out: Vec<Value> = results.into_iter().map(|(_, entry)| entry).collect();
    json!({"q": q, "results": out})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_model_reads_its_cache_at_its_own_rate() {
        // the longer prefix first: Opus 5.5 is not priced as Opus 5
        assert_eq!(usd_per_mtok("claude-opus-5-5"), (4.0, 20.0));
        assert_eq!(cache_read_usd_per_mtok("claude-opus-5-5"), 0.20);
        assert_eq!(usd_per_mtok("claude-opus-5"), (5.0, 25.0));
        assert_eq!(cache_read_usd_per_mtok("claude-opus-5"), 0.5);
        assert_eq!(usd_per_mtok("claude-opus-4-5"), (5.0, 25.0));
        assert_eq!(cache_read_usd_per_mtok("claude-fable-5-1"), 0.25);
        assert_eq!(cache_read_usd_per_mtok("claude-fable-5"), 1.0);
        assert_eq!(cache_read_usd_per_mtok("claude-sonnet-5"), 0.2);
        assert_eq!(cache_read_usd_per_mtok("claude-haiku-4-5"), 0.1);
        assert_eq!(usd_per_mtok("some-model-we-have-never-seen"), (3.0, 15.0));
        assert_eq!(cache_read_usd_per_mtok("some-model-we-have-never-seen"), 0.3);
        // 1M cache reads + 100K cache writes + 10K out on Opus 5.5: $0.20 + $0.50 + $0.20
        assert!((usage_usd("claude-opus-5-5", &[0, 10_000, 1_000_000, 100_000, 0]) - 0.9).abs() < 1e-9);
    }

    #[test]
    fn a_sessions_bill_is_its_spend_once_per_turn() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let scope = "w:github.com/acme/a";
        let stamp = now();
        let entries = [
            json!({"turn_id": "bill:aaa", "model": "claude-opus-5-5", "input_tokens": 100, "output_tokens": 10_000,
                   "cache_read_tokens": 1_000_000, "cache_creation_tokens": 100_000, "messages": 40, "context": 90_000}),
            json!({"turn_id": "bill:bbb", "model": "claude-haiku-4-5", "input_tokens": 0, "output_tokens": 1_000_000,
                   "cache_read_tokens": 0, "cache_creation_tokens": 0}),
        ];
        let billed = crate::report::bill_usage(&store, scope, "u1", "s1", "hook", "", &entries, stamp).tokens;
        assert_eq!(billed, 100 + 10_000 + 1_000_000);
        // the hook sent it again (its post timed out after landing): billed once
        assert_eq!(crate::report::bill_usage(&store, scope, "u1", "s1", "hook", "", &entries, stamp).tokens, 0);
        let window = costs(&store, scope, 1.0);
        assert_eq!(window["turns"], json!(2));
        // Opus 5.5: $0.0004 in + $0.20 out + $0.20 cache read + $0.50 cache write; Haiku: $5 out
        assert_eq!(window["est_usd"], json!(5.9));
        let sessions = journal(&store, scope, 1.0)["sessions"].clone();
        assert_eq!(sessions[0]["tokens"], json!(100 + 10_000 + 1_000_000 + 100_000 + 1_000_000));
        assert_eq!(sessions[0]["edits"], json!(0));
        assert_eq!(sessions[0]["model"], json!("claude-opus-5-5"));
    }

    #[test]
    fn a_bill_keeps_the_effort_its_model_ran_at_and_costs_split_the_model_by_it() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let scope = "w:github.com/acme/a";
        let stamp = now();
        let entries = [
            json!({"turn_id": "bill:hi", "model": "claude-opus-5-5", "effort": " XHigh ", "ultracode": true, "output_tokens": 1_000_000}),
            json!({"turn_id": "bill:md", "model": "claude-opus-5-5", "effort": "medium", "ultracode": false, "output_tokens": 500_000}),
            json!({"turn_id": "bill:no", "model": "claude-opus-5-5", "output_tokens": 100}),
        ];
        crate::report::bill_usage(&store, scope, "u1", "s1", "hook", "", &entries, stamp);
        let rows = walk_ledger(&store, scope, stamp - 60.0);
        let row = |turn: &str| rows.iter().find(|r| text(&r.payload, "turn_id") == turn).expect("row").payload.clone();
        // the effort as sent, trimmed and lowercased; ultracode only when true
        assert_eq!((row("bill:hi")["effort"].clone(), row("bill:hi")["ultracode"].clone()), (json!("xhigh"), json!(true)));
        assert_eq!(row("bill:md")["effort"], json!("medium"));
        assert!(row("bill:md").get("ultracode").is_none());
        assert!(row("bill:no").get("effort").is_none() && row("bill:no").get("ultracode").is_none());

        let window = costs(&store, scope, 1.0);
        // the by-model totals are as they were; the split is beside them
        assert_eq!(window["by_model"], json!({"claude-opus-5-5": 1_500_100}));
        assert_eq!(window["by_model_effort"], json!({"claude-opus-5-5": {"xhigh": 1_000_000, "medium": 500_000, "": 100}}));
        let usd = |n: i64| python_round(usage_usd("claude-opus-5-5", &[0, n, 0, 0, 0]), 2);
        assert_eq!(window["usd_by_model_effort"]["claude-opus-5-5"]["xhigh"], json!(usd(1_000_000)));
        assert_eq!(window["usd_by_model_effort"]["claude-opus-5-5"]["medium"], json!(usd(500_000)));
        assert_eq!(window["ultracode_by_model"], json!({"claude-opus-5-5": 1_000_000}));
        assert_eq!(window["usd_ultracode_by_model"], json!({"claude-opus-5-5": usd(1_000_000)}));

        // the journal names the effort beside the model it read it with
        let sessions = journal(&store, scope, 1.0)["sessions"].clone();
        assert_eq!(sessions[0]["model"], json!("claude-opus-5-5"));
        assert_eq!((sessions[0]["effort"].clone(), sessions[0]["ultracode"].clone()), (json!("xhigh"), json!(true)));
    }

    #[test]
    fn a_session_that_paused_is_still_one_session_in_whole() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let scope = "w:github.com/acme/a";
        let stamp = now();
        let bill = |turn: &str, at: f64| {
            let entry = json!({"turn_id": turn, "model": "claude-haiku-4-5", "input_tokens": 0, "output_tokens": 1_000_000,
                               "cache_read_tokens": 0, "cache_creation_tokens": 0});
            crate::report::bill_usage(&store, scope, "u1", "s1", "hook", "", &[entry], at);
        };
        let edit = |path: &str, at: f64| {
            store.ledger_append(scope, "edit_reported", &json!({"user": "u1", "session": "s1", "path": path,
                                                                 "lines_added": 3, "lines_removed": 1}), at).unwrap();
        };
        // two hours ago for ten minutes, then an hour of silence, then now
        bill("bill:one", stamp - 7200.0);
        edit("src/a.py", stamp - 6600.0);
        bill("bill:two", stamp - 600.0);
        edit("src/b.py", stamp - 300.0);
        edit("src/a.py", stamp - 60.0);
        let sessions = journal(&store, scope, 1.0)["sessions"].as_array().cloned().unwrap();
        assert_eq!(sessions.len(), 2, "two stretches: {sessions:?}");
        // the newest stretch alone is a fraction of the session
        assert_eq!(sessions[0]["est_usd"], json!(5.0));
        for stretch in &sessions {
            let whole = &stretch["whole"];
            assert_eq!(whole["stretches"], json!(2));
            assert_eq!(whole["est_usd"], json!(10.0), "both bills");
            assert_eq!(whole["edits"], json!(3));
            assert_eq!(whole["files"], json!(2), "a.py once, though both stretches edited it");
            assert_eq!(whole["lines_added"], json!(9));
            // ten minutes and nine: the hour between them is no work
            assert_eq!(whole["active_s"], json!(600.0 + 540.0));
        }
    }
}

#[cfg(test)]
mod provider_truth_tests {
    use super::*;

    /// A real `claude -p` run (2026-10-17) with a Haiku subagent, as hooks
    /// v79 bill it, against Claude Code's own `total_cost_usd` 0.0600825.
    #[test]
    fn a_billed_session_costs_what_the_provider_said() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("store");
        let scope = "w:github.com/acme/a";
        let stamp = now();
        let main = [json!({"turn_id": "bill:m", "model": "claude-sonnet-5", "input_tokens": 4, "output_tokens": 524,
            "cache_read_tokens": 54_176, "cache_creation_tokens": 6_138, "cache_creation_1h_tokens": 6_138, "messages": 2})];
        let sub = [json!({"turn_id": "bill:s", "model": "claude-haiku-4-5-20251001", "input_tokens": 18, "output_tokens": 339,
            "cache_read_tokens": 12_143, "cache_creation_tokens": 13_216, "messages": 2})];
        crate::report::bill_usage(&store, scope, "u1", "s1", "hook", "", &main, stamp);
        crate::report::bill_usage(&store, scope, "u1", "s1", "hook", "a1", &sub, stamp);
        let rows = store.ledger_since(scope, stamp - 10.0);
        assert_eq!(rows[0].payload["cache_creation_1h_tokens"], json!(6_138), "the 1-hour share is kept on the row");
        let total: f64 = rows.iter().map(|r| row_usd(&r.payload)).sum();
        assert!((total - 0.0600825).abs() < 1e-9, "{total}");
        let window = costs(&store, scope, 1.0);
        assert_eq!(window["by_kind"]["cache_creation_1h"], json!(6_138));
        assert_eq!(window["usd_by_model"]["claude-sonnet-5"], json!(0.04));
        assert_eq!(window["unpriced_models"], json!([]));
        // served fast and US-only: kept on the row, priced 2 x 1.1
        let fast = [json!({"turn_id": "bill:f", "model": "claude-opus-5-5", "output_tokens": 1_000_000,
            "speed": "fast", "inference_geo": "us", "prompt_over": 100_000})];
        crate::report::bill_usage(&store, scope, "u1", "s2", "hook", "", &fast, stamp);
        let row = store.ledger_since(scope, stamp - 10.0).pop().unwrap();
        assert_eq!((row.payload["speed"].clone(), row.payload["inference_geo"].clone()), (json!("fast"), json!("us")));
        assert!((row_usd(&row.payload) - 20.0 * 2.2).abs() < 1e-9);
        // a model the table does not know is named, and priced at the default
        let odd = [json!({"turn_id": "bill:o", "model": "gpt-reserve", "output_tokens": 10})];
        crate::report::bill_usage(&store, scope, "u1", "s3", "hook", "", &odd, stamp);
        assert_eq!(costs(&store, scope, 1.0)["unpriced_models"], json!(["gpt-reserve"]));
    }

    /// A saving is priced when it is read, at the model's rate today: a row
    /// written while gpt-6-luna fell to the $0.30 default reads at $0.01.
    #[test]
    fn a_saving_is_repriced_from_its_tokens_and_model() {
        let row = json!({"model": "gpt-6-luna", "tokens_saved": 1_000_000, "est_usd": 0.30,
            "brief_injected": 1_000_000, "brief_carried": 1_000_000, "brief_cost_usd": 0.675});
        let worth = crate::briefstat::row_saving("brief_outcome", &row).unwrap();
        assert!((worth.est_usd - 0.01).abs() < 1e-12);
        // injected at its cache-write rate ($0.125), carried at cache read ($0.01)
        assert!((worth.cost_usd - 0.135).abs() < 1e-12);
        // Claude Code's main conversation writes for an hour: 2x input
        let opus = json!({"model": "claude-opus-5-5", "tokens_saved": 0, "brief_injected": 1_000_000, "brief_carried": 0});
        assert!((crate::briefstat::row_saving("brief_outcome", &opus).unwrap().cost_usd - 8.0).abs() < 1e-12);
        // no model: the dollars it was written with
        let bare = json!({"tokens_saved": 5, "est_usd": 0.5});
        assert_eq!(crate::briefstat::row_saving("map_answered", &bare).unwrap().est_usd, 0.5);
    }
}

#[cfg(test)]
mod pricing_ported_tests {
    use super::*;

    #[test]
    fn the_rate_table_is_input_and_output_never_a_blend() {
        assert_eq!(usd_per_mtok("claude-opus-5"), (5.0, 25.0));
        assert_eq!(usd_per_mtok("claude-sonnet-5"), (2.0, 10.0));
        assert_eq!(usd_per_mtok("claude-haiku-4-5"), (1.0, 5.0));
        assert_eq!(usd_per_mtok("claude-fable-5"), (10.0, 50.0));
        assert_eq!(usd_per_mtok("claude-fable-5-1"), (10.0, 50.0));
        let default = crate::pricing::DEFAULT;
        assert_eq!((default.input, default.output, default.cache_read), (3.0, 15.0, 0.3));
    }

    #[test]
    fn a_cached_turn_is_priced_on_all_four_counters() {
        let usage = row_usage(&json!({"input_tokens": 100, "output_tokens": 1500,
            "cache_read_tokens": 100_000, "cache_creation_tokens": 5_000, "tokens": 1600}));
        // the legacy field is ignored when the real counters are there
        assert_eq!(usage, [100, 1500, 100_000, 5_000, 0]);
        assert_eq!(usage_tokens(&usage), 106_600);
        assert!((usage_usd("claude-opus-5", &usage) - 0.11925).abs() < 1e-9);
        // a row from a pre-v20 hook carries only `tokens`, priced at the output rate
        let legacy = row_usage(&json!({"tokens": 1600}));
        assert_eq!(legacy, [0, 0, 0, 0, 1600]);
        assert!((usage_usd("claude-opus-5", &legacy) - 0.04).abs() < 1e-9);
    }
}
