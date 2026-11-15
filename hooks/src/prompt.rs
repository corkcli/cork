//! The prompt-scoped briefing and the grep companion — the Rust twins of
//! the Python hook's `_run_user_prompt` / `_grep_companion`, byte for byte.
//!
//! At prompt submit the task is known: the hook extracts the identifiers
//! the prompt names (paths, snake_case, CamelCase, dotted names, backticked
//! tokens — never the prose) and asks `/brief` for the modules they touch.
//! After a grep, the name the agent searched for goes to `/attribute` and
//! the map's structured answer rides the same tool result.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

use crate::check::state_path;
use crate::config::{self, Env};
use crate::http;
use crate::report::{save_json, text, user_agent, BRIEF_TIMEOUT, TOTAL_BUDGET};

const MAX_PROMPT_IDENTIFIERS: usize = 40;
const MAX_GREP_SYMBOLS: usize = 5;
const GREP_NOTE_CHARS: usize = 1500;
/// The most one Read puts into the context: Claude Code's Read refuses past
/// 25k tokens, so a skipped read is never priced above it.
pub const READ_CAP_TOKENS: u64 = 25_000;
/// A prompt briefing's budget, in characters (the session start's is
/// report::BRIEF_BUDGET, 6000; the server holds them to 2,400 and 4,500).
const PROMPT_BRIEF_BUDGET: usize = 2400;
/// Paths one outcome report names per kind.
const MAX_OUTCOME_PATHS: usize = 60;
const IDENT_STOP: [&str; 15] = [
    "e.g", "i.e", "etc", "vs", "python3", "python", "node", "npm", "npx", "git", "cd", "ls", "pytest", "cargo",
    "tests.check",
];
const IDENT_STOP_MORE: [&str; 2] = ["readme.md", "package.json"];
/// Function words, shell verbs and primitive type names. Reachable in practice
/// only through a bare backtick span, where nothing else can tell `run` the
/// verb from `run` the symbol. FROZEN at this size by policy: it is cheap
/// insurance against the long tail, never an English dictionary, and it
/// deliberately omits doc, recap, remember, session, name, call, auto, since,
/// setup and auth — those are real Cork symbols and briefing them is right.
const IDENT_PROSE: [&str; 135] = [
    "add", "added", "all", "also", "and", "any", "are", "as", "at", "awk", "be", "been", "bool",
    "build", "but", "by", "can", "cat", "change", "changed", "clone", "commit", "curl", "debug",
    "delete", "dict", "did", "diff", "do", "does", "done", "echo", "else", "error", "false",
    "fetch", "fix", "fixed", "for", "from", "grep", "had", "has", "have", "he", "help", "her",
    "his", "if", "in", "info", "int", "into", "is", "it", "its", "just", "kill", "len", "list",
    "log", "make", "me", "merge", "must", "my", "new", "no", "none", "not", "now", "null", "of",
    "off", "ok", "okay", "old", "on", "one", "only", "open", "or", "our", "out", "over", "per",
    "print", "pull", "push", "rebase", "remove", "removed", "return", "run", "sed", "self", "set",
    "she", "should", "start", "stash", "status", "still", "stop", "str", "test", "tests", "that",
    "the", "them", "then", "these", "they", "this", "those", "to", "true", "two", "type", "under",
    "up", "update", "updated", "via", "warn", "was", "we", "wget", "when", "while", "will", "with",
    "yes", "you", "your"
];

/// Every pattern that proposes a candidate. The ASCII-only ones carry `(?-u)`
/// so Rust's `\w` and `\b` mean what Python's `re.ASCII` means; the two that
/// must accept arbitrary text (the backtick span, the URL) stay Unicode in
/// both twins, which is also agreement. Anything non-ASCII is rejected by the
/// shape guard downstream regardless.
struct IdentRes {
    file: Regex,
    tick: Regex,
    tick_token: Regex,
    word: Regex,
    shape: Regex,
    url: Regex,
}

/// Source extensions, plus the repo-data and doc extensions a tracked file
/// really carries (CLAUDE.md, Cargo.toml, blocks_golden.json). Image and
/// binary extensions are deliberately absent: that is what keeps a pasted
/// `Screenshot 2026-09-17 at 12.42.34 PM.png` from ever proposing a token.
const CODE_EXT: &str = "py|pyi|ts|tsx|js|jsx|mjs|cjs|go|rs|java|cs|rb|c|h|cc|cpp|hpp|php";
const DATA_EXT: &str =
    "md|txt|json|jsonl|toml|yaml|yml|sql|html|css|scss|sh|lock|cfg|ini|rst|xml|proto|graphql|env";

fn ident_res() -> IdentRes {
    IdentRes {
        file: Regex::new(&format!(r"(?-u)[\w./-]*[\w-]+\.(?:{CODE_EXT}|{DATA_EXT})\b")).expect("static regex"),
        tick: Regex::new(r"`([^`\n]{1,200})`").expect("static regex"),
        tick_token: Regex::new(r"(?-u)[A-Za-z_][\w./:-]*").expect("static regex"),
        word: Regex::new(r"(?-u)[A-Za-z_][A-Za-z0-9_]*").expect("static regex"),
        shape: Regex::new(r"(?-u)^[\w./:-]+$").expect("static regex"),
        url: Regex::new(r"(?:https?://|www\.)\S+").expect("static regex"),
    }
}

fn ascii_lower(t: &str) -> String {
    t.chars().map(|c| if c.is_ascii_uppercase() { ((c as u8) + 32) as char } else { c }).collect()
}

fn alnum_underscore(t: &str) -> bool {
    t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A token whose SHAPE is a code marker English prose cannot produce. Written
/// with explicit byte comparisons, no regex and no locale, so the Python twin
/// transcribes character for character.
fn is_code(t: &str) -> bool {
    if t.is_empty() || !t.is_ascii() {
        return false;
    }
    let uppers = t.chars().filter(char::is_ascii_uppercase).count();
    let has_lower = t.chars().any(|c| c.is_ascii_lowercase());
    let has_alpha = t.chars().any(|c| c.is_ascii_alphabetic());
    // snake_case, _leading, SCREAMING_SNAKE, FACTOR_5
    if t.contains('_') && t.len() >= 3 && has_alpha && alnum_underscore(t) {
        return true;
    }
    // CamelCase: ToolSearch, BriefInput, CorkMCP
    !t.contains('_') && t.len() >= 4 && alnum_underscore(t) && uppers >= 2 && has_lower
}

/// A bare word that only a backtick makes an identifier. Keeps `compute` and
/// `recap`; refuses `Bash`, `Cork`, `REGION`, `EU`.
fn is_bare_name(t: &str) -> bool {
    if t.is_empty() || !t.is_ascii() || !alnum_underscore(t) || t.len() < 3 {
        return false;
    }
    let first = t.chars().next().expect("non-empty");
    let uppers = t.chars().filter(char::is_ascii_uppercase).count();
    let has_lower = t.chars().any(|c| c.is_ascii_lowercase());
    if first.is_ascii_uppercase() && has_lower && uppers < 2 {
        return false; // Bash, Edit, Cork, Session
    }
    if !has_lower && uppers >= 1 {
        return false; // bare ALL-CAPS: PM, JSON, EU
    }
    !first.is_ascii_digit()
}

/// A pasted git sha, not a name.
fn is_hexish(t: &str) -> bool {
    t.len() >= 7
        && ascii_lower(t).chars().all(|c| "0123456789abcdef".contains(c))
        && t.chars().any(|c| c.is_ascii_digit())
}

/// Harness text in a prompt that names no code: a background task's
/// notification (its ids, its usage counters: `subagent_tokens`,
/// `tool_uses`, `duration_ms`) and the reminders a harness adds. Keys of
/// JSON a person pasted stay: an error body's `error_description` can be
/// a field in this very repo.
fn pasted_noise() -> &'static [Regex] {
    static RES: std::sync::OnceLock<Vec<Regex>> = std::sync::OnceLock::new();
    RES.get_or_init(|| {
        ["task-notification", "usage", "task-id", "tool-use-id", "output-file", "system-reminder"]
            .iter()
            .map(|tag| Regex::new(&format!(r"(?s)<{tag}>.*?</{tag}>")).expect("pasted block"))
            .collect()
    })
}

/// How much of a prompt's own words the briefing may match by meaning.
pub const PROMPT_CHARS: usize = 2_000;

/// The identifiers a prompt names, never its prose.
///
/// Three producing rules in a load-bearing order — paths, then the author's
/// explicit backticks, then shape — each candidate routed through one guard
/// chain. Shape is the only evidence a bare word gets: a capitalized English
/// word is not a name, because `Likely` exact-matching a constant named
/// `LIKELY` scores above the briefing's strong-match threshold and spends a
/// module block on nothing. Measured over 312 recorded prompts against the
/// real index: precision 0.54 before, 0.65 after, with more productive
/// identifiers than the stricter alternative that deletes snake_case.
pub fn prompt_identifiers(prompt: &str) -> Vec<String> {
    let res = ident_res();
    // the code a prompt says to leave alone ("Don't touch views/space.js")
    // is not what the task is about: briefed on it, the agent was also told
    // the sessions working there were in its way
    let prompt = without_off_limits(prompt);
    // a harness's own notifications name no code: a background task's ids
    // and usage counters
    let prompt = pasted_noise().iter().fold(prompt, |text, re| re.replace_all(&text, " ").into_owned());
    // a URL is a path-shaped thing that names no file in this repo
    let text = res.url.replace_all(&prompt, " ").into_owned();
    let mut out: Vec<String> = Vec::new();

    let take = |raw: &str, span: Option<(usize, usize)>, out: &mut Vec<String>| {
        if let Some((start, end)) = span {
            let before = if start > 0 { text[..start].chars().last() } else { None };
            let after = text[end..].chars().next();
            if matches!(before, Some('/') | Some('-') | Some('\\'))
                || matches!(after, Some('/') | Some('-') | Some('\\'))
            {
                return; // a path segment or a slug, not a name
            }
        }
        // trailing punctuation goes; a LEADING dot stays on a path, so
        // `.collide/report_hook.py` keeps the name the map stores it under
        let mut tok = raw.trim().trim_matches(|c| ",;:()".contains(c)).to_string();
        while tok.ends_with('.') {
            tok.pop();
        }
        if !tok.contains('/') {
            tok = tok.trim_start_matches('.').to_string();
        }
        if tok.is_empty() {
            return;
        }
        let first = tok.chars().next().expect("non-empty");
        let last = tok.chars().last().expect("non-empty");
        if first == '-' || first == '\\' || "/-\\.".contains(last) {
            return;
        }
        if tok.contains("://") || !tok.is_ascii() || !res.shape.is_match(&tok) {
            return;
        }
        if is_hexish(&tok) || cork_core::tokens::looks_like_id(&tok) {
            return;
        }
        if tok.len() > 128 || (tok.len() > 32 && !tok.contains(['_', '/', '.'])) {
            return;
        }
        let lower = ascii_lower(&tok);
        if IDENT_STOP.contains(&lower.as_str())
            || IDENT_STOP_MORE.contains(&lower.as_str())
            || IDENT_PROSE.contains(&lower.as_str())
        {
            return;
        }
        // the briefing lowercases every identifier, so the dedupe must too
        if out.iter().any(|seen| {
            let seen_l = ascii_lower(seen);
            seen_l == lower || seen_l.ends_with(&format!("/{lower}"))
        }) {
            return;
        }
        out.push(tok);
    };

    // a path with a known extension, taken whole and without the neighbour
    // guard: its own slashes must not disqualify it
    for m in res.file.find_iter(&text) {
        take(m.as_str(), None, &mut out);
    }

    // what the author marked with backticks. One token that IS the whole span
    // is a name they chose; several tokens is a code fragment, so each is
    // judged on shape alone and a dotted or `::` name is split rather than
    // sent whole, because the briefing can match neither.
    let segments = |t: &str| -> Vec<String> {
        t.split("::").flat_map(|part| part.split('.')).map(str::to_string).collect()
    };
    for caps in res.tick.captures_iter(&text) {
        let Some(span) = caps.get(1).map(|m| m.as_str().trim()) else { continue };
        let tokens: Vec<&str> = res.tick_token.find_iter(span).map(|m| m.as_str()).collect();
        if tokens.len() == 1 && tokens[0] == span {
            let tok = tokens[0];
            if res.file.find(tok).map(|m| m.as_str() == tok).unwrap_or(false) || is_code(tok) {
                take(tok, None, &mut out);
            } else if is_bare_name(tok) {
                take(tok, None, &mut out);
            } else {
                for seg in segments(tok) {
                    if is_code(&seg) {
                        take(&seg, None, &mut out);
                    }
                }
            }
        } else {
            for tok in tokens {
                if res.file.find(tok).map(|m| m.as_str() == tok).unwrap_or(false) || is_code(tok) {
                    take(tok, None, &mut out);
                    continue;
                }
                for seg in segments(tok) {
                    if is_code(&seg) {
                        take(&seg, None, &mut out);
                    }
                }
            }
        }
    }

    // the prose scan: shape is the only evidence. No word boundary and no
    // lowercase-start test, so `_run_user_prompt` and `FACTOR_5` arrive; the
    // scan splits on '.', so `region.startswith` never forms.
    for m in res.word.find_iter(&text) {
        if is_code(m.as_str()) {
            take(m.as_str(), Some((m.start(), m.end())), &mut out);
        }
    }

    out.truncate(MAX_PROMPT_IDENTIFIERS);
    out
}

fn strings(state: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    state
        .get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// How much context a briefing is: its bytes over four, the usual token
/// estimate. Both hooks count the same bytes so the two post the same cost.
pub fn brief_tokens(text: &str) -> u64 {
    (text.len() as u64 + 3) / 4
}

/// The briefing just landed in the session: the modules it rendered with
/// facts are BRIEFED, the ones it only named (the "N more changed" tail) are
/// NAMED, the briefed ones whose callers it listed in full are COVERED, and
/// the text itself is context the session now pays to carry on every message
/// until a compaction drops it. Mirrors `_remember_briefed`.
pub fn remember_briefed(session: &str, paths: &[Value], named: &[Value], text: &str, covered: &[Value], env: &Env) {
    let named: Vec<String> = named.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
    let covered: Vec<String> = covered.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
    let tokens = brief_tokens(text);
    if paths.is_empty() && named.is_empty() && covered.is_empty() && tokens == 0 {
        return;
    }
    let path = state_path(session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let mut briefed = strings(&state, "briefed_paths");
    for p in paths {
        if let Some(p) = p.as_str() {
            if !briefed.iter().any(|s| s == p) {
                briefed.push(p.to_string());
            }
        }
    }
    let mut known = strings(&state, "named_paths");
    for p in named {
        if !known.contains(&p) && !briefed.contains(&p) {
            known.push(p);
        }
    }
    state.insert("briefed_paths".into(), json!(briefed));
    state.insert("named_paths".into(), json!(known));
    let mut listed = strings(&state, "covered_paths");
    for p in covered {
        if !listed.contains(&p) {
            listed.push(p);
        }
    }
    state.insert("covered_paths".into(), json!(listed));
    // the text's cost is counted with everything else the hook printed
    // (note_injected), so it is not counted twice here
    let _ = tokens;
    save_json(&path, &Value::Object(state));
}

/// What one hook event put into the context, in characters: it is carried on
/// every later message, so it joins the cost the briefing outcome reports.
/// The briefing used to be the only text counted; the grep companion, the
/// deltas, notes and messages rode free.
pub fn note_injected(session: &str, chars: usize, env: &Env) {
    if session.is_empty() || chars == 0 {
        return;
    }
    let tokens = (chars as u64 + 3) / 4;
    let path = state_path(session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let total = state.get("brief_tokens").and_then(Value::as_u64).unwrap_or(0);
    state.insert("brief_tokens".into(), json!(total + tokens));
    let injected = state.get("brief_injected").and_then(Value::as_u64).unwrap_or(0);
    state.insert("brief_injected".into(), json!(injected + tokens));
    save_json(&path, &Value::Object(state));
}

/// A compaction dropped the context: a briefed module the session never
/// touched is unknown again, and the briefing text is no longer carried. A
/// path already read or written keeps its place, so a read made before the
/// compaction still settles as NEEDED (or a miss) after it, instead of the
/// credit vanishing with the context. Mirrors `_forget_briefed`.
pub fn forget_briefed(session: &str, env: &Env) {
    let path = state_path(session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let touched: BTreeSet<String> =
        strings(&state, "read_paths").into_iter().chain(strings(&state, "written_paths")).collect();
    let mut changed = false;
    for key in ["briefed_paths", "named_paths", "covered_paths"] {
        let before = strings(&state, key);
        let after: Vec<String> = before.iter().filter(|p| touched.contains(*p)).cloned().collect();
        if after != before {
            state.insert(key.into(), json!(after));
            changed = true;
        }
    }
    if state.get("brief_tokens").and_then(Value::as_u64).unwrap_or(0) > 0 {
        state.insert("brief_tokens".into(), json!(0));
        changed = true;
    }
    // the reads it skipped would have been compacted away too
    if state.get("avoided").and_then(Value::as_object).is_some_and(|a| !a.is_empty()) {
        state.insert("avoided".into(), json!({}));
        changed = true;
    }
    if changed {
        save_json(&path, &Value::Object(state));
    }
}

/// One more message replayed the context. The briefing text riding in it is
/// re-read at the cache-read rate every time, and that is the briefing's own
/// running cost, counted once per assistant message: `message` is its id,
/// and the several tool calls one message makes count it once (an unknown
/// id counts every call, as before). A subagent's call (`worker`) is a
/// message in the subagent's own context, which carries neither the
/// briefing nor the reads it replaced: it counts nothing here.
pub fn note_call(session: &str, worker: bool, message: &str, env: &Env) {
    if session.is_empty() || worker {
        return;
    }
    let path = state_path(session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    if !message.is_empty() && state.get("carry_message").and_then(Value::as_str) == Some(message) {
        return;
    }
    let carried = state.get("brief_tokens").and_then(Value::as_u64).unwrap_or(0);
    // a read Cork made unnecessary would have ridden in this message too:
    // its file, at the cache-read rate, for as long as it stays unread
    let (mut brief, mut map) = (0u64, 0u64);
    if let Some(avoided) = state.get("avoided").and_then(Value::as_object) {
        for entry in avoided.values() {
            let tokens = entry.get("t").and_then(Value::as_u64).unwrap_or(0);
            if entry.get("s").and_then(Value::as_str) == Some("map") { map += tokens } else { brief += tokens }
        }
    }
    if carried == 0 && brief == 0 && map == 0 {
        return;
    }
    for (key, add) in [("brief_carried", carried), ("read_avoided_carried", brief), ("map_avoided_carried", map)] {
        if add > 0 {
            let so_far = state.get(key).and_then(Value::as_u64).unwrap_or(0);
            state.insert(key.into(), json!(so_far + add));
        }
    }
    if !message.is_empty() {
        state.insert("carry_message".into(), json!(message));
    }
    save_json(&path, &Value::Object(state));
}

/// How much context reading a file of `bytes` would have added: its bytes
/// over four, capped at what one Read can return.
pub fn read_tokens(bytes: u64) -> u64 {
    ((bytes + 3) / 4).min(READ_CAP_TOKENS)
}

/// A read this session did not make because Cork answered it: `source`
/// is "brief" (a briefed file written without a read) or "map" (a grep the
/// map answered). From here on, until the file is read after all or a
/// compaction empties the context, every message is one that did not carry
/// it. Mirrors `_note_avoided`.
pub fn note_avoided(session: &str, source: &str, rel: &str, bytes: u64, env: &Env) {
    if session.is_empty() || rel.is_empty() || bytes == 0 {
        return;
    }
    let path = state_path(session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    if strings(&state, "read_paths").iter().any(|p| p == rel) {
        return;
    }
    let mut avoided = state.get("avoided").and_then(Value::as_object).cloned().unwrap_or_default();
    if avoided.contains_key(rel) || avoided.len() >= 200 {
        return;
    }
    avoided.insert(rel.to_string(), json!({"t": read_tokens(bytes), "s": source}));
    state.insert("avoided".into(), Value::Object(avoided));
    save_json(&path, &Value::Object(state));
}

/// A file the session just wrote: when the briefing covered it and it was
/// never read, the read the briefing replaced starts counting. Mirrors
/// `_note_written_avoided`.
pub fn note_written_avoided(session: &str, rel: &str, bytes: u64, env: &Env) {
    if session.is_empty() || rel.is_empty() {
        return;
    }
    let state = config::load_json(&state_path(session, env)).as_object().cloned().unwrap_or_default();
    if strings(&state, "briefed_paths").iter().any(|p| p == rel) {
        note_avoided(session, "brief", rel, bytes, env);
    }
}

/// Remember that this session read or wrote a path — the other half of the
/// briefing's hit/miss ledger (the briefed half is `remember_briefed`).
pub fn note_touch(session: &str, kind: &str, rel: &str, env: &Env) {
    if session.is_empty() || rel.is_empty() {
        return;
    }
    let path = state_path(session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let key = format!("{kind}_paths");
    let mut seen: Vec<String> = state
        .get(&key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let mut changed = false;
    if kind == "read" {
        // read after all: from here on the file rides in the context anyway
        if let Some(avoided) = state.get_mut("avoided").and_then(Value::as_object_mut) {
            changed = avoided.remove(rel).is_some();
        }
    }
    if seen.iter().any(|s| s == rel) {
        if changed {
            save_json(&path, &Value::Object(state));
        }
        return;
    }
    seen.push(rel.to_string());
    if seen.len() > 400 {
        seen.drain(..seen.len() - 400);
    }
    state.insert(key, json!(seen));
    save_json(&path, &Value::Object(state));
}

/// What the briefing was worth, settled once per path. A briefed path the
/// session WROTE is a HIT when it was never read (the facts were enough) and
/// NEEDED when it was read first (the Edit tool insists on its own Read). A
/// path the briefing only NAMED, in its "N more changed" tail, that the
/// session then wrote is FOUND: the briefing said where to look. A written
/// briefed path whose CALLERS the briefing listed in full is one more search
/// not sent, the callers search. A briefed path read and never written is a
/// MISS, known only when the session ends
/// (`last`): a read this turn is very often the read before an edit a few
/// turns later, and calling it a miss one turn later, as this used to, wrote
/// most of them off. With the counts go the repo-relative paths each settled
/// on (and, at the end, the briefed paths never opened and the files the
/// session wrote that no briefing showed), so the server can shape the
/// repo's next briefings by them; never a file's text. Plus what the
/// briefing itself cost to carry since the last report.
pub fn brief_outcome(hook_input: &Value, env: &Env, started: std::time::Instant, last: bool) {
    let session = text(hook_input, "session_id");
    if session.is_empty() {
        return;
    }
    let path = state_path(&session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let briefed = strings(&state, "briefed_paths");
    let named: Vec<String> = strings(&state, "named_paths").into_iter().filter(|p| !briefed.contains(p)).collect();
    let read_avoided = state.get("read_avoided_carried").and_then(Value::as_u64).unwrap_or(0);
    let map_avoided = state.get("map_avoided_carried").and_then(Value::as_u64).unwrap_or(0);
    if briefed.is_empty() && named.is_empty() && read_avoided == 0 && map_avoided == 0 {
        return;
    }
    let read: BTreeSet<String> = strings(&state, "read_paths").into_iter().collect();
    let written: BTreeSet<String> = strings(&state, "written_paths").into_iter().collect();
    let covered: BTreeSet<String> = strings(&state, "covered_paths").into_iter().collect();
    let mut done: BTreeSet<String> = strings(&state, "outcome_seen").into_iter().collect();
    let (mut hits, mut misses, mut needed, mut found, mut callers) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut newly: Vec<String> = Vec::new();
    // which path settled how: the server shapes the repo's next briefings
    // by them (repo-relative paths only, never the files)
    let mut by_path: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut settle = |kind: &str, p: &str| {
        let list = by_path.entry(kind.to_string()).or_insert_with(|| json!([]));
        if let Some(items) = list.as_array_mut() {
            if items.len() < MAX_OUTCOME_PATHS {
                items.push(json!(p));
            }
        }
    };
    for p in &briefed {
        if done.contains(p) {
            continue;
        }
        if written.contains(p) && read.contains(p) {
            needed += 1;
            settle("needed", p);
        } else if written.contains(p) {
            hits += 1;
            settle("hit", p);
        } else if read.contains(p) && last {
            misses += 1;
            settle("miss", p);
        } else if last {
            // briefed, and the session never opened it: not counted as a
            // miss (the tally's meaning stays), only told per path
            settle("unused", p);
        } else {
            continue;
        }
        if written.contains(p) && covered.contains(p) {
            callers += 1;
        }
        newly.push(p.clone());
    }
    for p in &named {
        if done.contains(p) || !written.contains(p) {
            continue;
        }
        found += 1;
        settle("found", p);
        newly.push(p.clone());
    }
    // what the session had to find for itself: files it wrote that no
    // briefing showed or named
    if last {
        for p in &written {
            if !briefed.contains(p) && !named.contains(p) && crate::report::is_map_source(p) {
                settle("sought", p);
            }
        }
    }
    let injected = state.get("brief_injected").and_then(Value::as_u64).unwrap_or(0);
    let carried = state.get("brief_carried").and_then(Value::as_u64).unwrap_or(0);
    let avoided = read_avoided > 0 || map_avoided > 0;
    if newly.is_empty() && !(last && (injected > 0 || carried > 0 || avoided)) {
        return;
    }
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(from_input) }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project_dir.is_empty()).then(|| PathBuf::from(project_dir));
    let root = config::find_repo_root(&[Some(cwd.clone()), project]).unwrap_or(cwd);
    let cfg = config::config(Some(&root), env);
    let remaining = TOTAL_BUDGET.saturating_sub(started.elapsed());
    let mut posted = false;
    if cfg.usable() && !remaining.is_zero() {
        // what a message was worth: the context this session was carrying (a
        // message it did not send would have replayed all of it), and the
        // model's rate; and what the briefing cost to inject and carry
        let meta = crate::transcript::turn_meta_in(&text(hook_input, "transcript_path"), env);
        let mut payload = json!({"repo_id": cfg.repo_id, "session": session, "hits": hits, "misses": misses, "needed": needed,
                                 "found": found, "callers": callers, "brief_injected": injected, "brief_carried": carried,
                                 "read_avoided_carried": read_avoided, "map_avoided_carried": map_avoided});
        if let Some(map) = payload.as_object_mut() {
            if !by_path.is_empty() {
                map.insert("paths".into(), Value::Object(by_path));
            }
            if let Some(context) = meta.get("context").and_then(Value::as_i64).filter(|c| *c > 0) {
                map.insert("context_tokens".into(), json!(context));
            }
            if let Some(model) = meta.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
                map.insert("model".into(), json!(model));
                // and how hard it was working: an xhigh message is worth more
                crate::transcript::copy_mode(&meta, map);
            }
        }
        posted = http::post(&cfg.server, "/brief/outcome", &cfg.token, &crate::report::user_agent(), &payload,
            remaining.min(std::time::Duration::from_secs(5))).is_ok();
    }
    if posted {
        // the cost is reported once; a post that failed keeps it for the next
        state.insert("brief_injected".into(), json!(0));
        state.insert("brief_carried".into(), json!(0));
        state.insert("read_avoided_carried".into(), json!(0));
        state.insert("map_avoided_carried".into(), json!(0));
    }
    done.extend(newly);
    state.insert("outcome_seen".into(), json!(done.into_iter().collect::<Vec<_>>()));
    state.remove("outcome_pending");
    save_json(&path, &Value::Object(state));
}

/// Whether a prompt asks the agent to tell the other agents or people
/// something ("tell the others", "let the team know", "give everyone a
/// heads-up", "notify the other agents").
pub fn asks_to_tell_others(prompt: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)\b(?:tell|inform|notify|warn|ping|message|alert)\b[^.?!\n]{0,30}",
            r"\b(?:others|other agents|other sessions|everyone|everybody|team|teammates)\b",
            r"|\blet\b[^.?!\n]{0,30}\b(?:others|other agents|everyone|everybody|team|teammates)\b[^.?!\n]{0,12}\bknow\b",
            r"|\bheads[- ]up\b"
        ))
        .expect("tell pattern")
    })
    .is_match(prompt)
}

/// Whether the prompt asks for work to be handed to another agent: "send
/// this to Cork", "hand this off", "give it to the other session".
pub fn asks_to_hand_off(prompt: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)\b(?:send|hand|pass|give|push|drop|put)\b[^.?!\n]{0,40}\b(?:to|in|into|through|via|on)\s+cork\b",
            r"|\bhand\s+(?:it|this|that|these|them|the\s+\w+)\s+off\b|\bhand\s+off\b",
            r"|\b(?:send|hand|pass|give)\b[^.?!\n]{0,40}\bto\s+(?:the\s+|my\s+|your\s+)?other\s+(?:session|agent|claude|codex|window|terminal|chat)\b"
        ))
        .expect("handoff pattern")
    })
    .is_match(prompt)
}

/// UserPromptSubmit: the briefing narrows to what the prompt names.
pub fn run_user_prompt(hook_input: &Value, env: &Env) -> i32 {
    let prompt = text(hook_input, "prompt");
    // what it asks is the session's task now, and what its work is labelled by
    crate::transcript::note_task(&text(hook_input, "session_id"), &crate::transcript::task_of(&prompt), env);
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(from_input) }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project_dir.is_empty()).then(|| PathBuf::from(project_dir));
    let Some(root) = config::find_repo_root(&[Some(cwd), project]) else { return 0 };
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let identifiers = prompt_identifiers(&prompt);
    // the prompt's words too, capped: a request that names no code ("the
    // revenue on the dashboard is too high") is matched by meaning on the
    // server, used for that lookup and dropped. Twin of report_hook.py.
    // What it says not to touch is not what it is about.
    let meaning: String = without_off_limits(&prompt).chars().take(PROMPT_CHARS).collect();
    if identifiers.is_empty() && meaning.trim().is_empty() {
        return 0;
    }
    let session = text(hook_input, "session_id");
    let state = config::load_json(&state_path(&session, env));
    let exclude: Vec<String> = state
        .get("briefed_paths")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    // a prompt's briefing rides every later message of the session: it gets
    // a smaller budget than the session start's
    // `commit`: this program commits only the session's own lines; the
    // briefing's finish line names it instead of `git add <files>`
    // `verifies`: every symbol line is checked against this copy before it
    // is printed (mapcheck.rs), so the server may say to build on them
    let mut payload = json!({"repo_id": cfg.repo_id, "identifiers": identifiers, "exclude": exclude, "budget": PROMPT_BRIEF_BUDGET, "session": session, "prompt": meaning,
                         "commit": crate::journal::commit_hint(env), "verifies": true});
    // a person typed it: they are here, which the next morning's "since you
    // left" is counted from
    if !crate::harness::headless(hook_input, env) {
        payload["here"] = json!(true);
    }
    let Ok(brief) = http::post(&cfg.server, "/brief", &cfg.token, &user_agent(), &payload, BRIEF_TIMEOUT) else { return 0 };
    // the map holds each file as some checkout last reported it: what this
    // copy does not have is never printed (mapcheck.rs)
    let checked = crate::mapcheck::apply(&root, &brief).text;
    let mut said = checked.clone();
    // trunk mode: the setting it carried kept, and a session on the trunk
    // caught up with it when it moved (trunk.rs)
    let caught = crate::trunk::at_prompt(&root, &cfg, &session, &brief, env);
    if !caught.is_empty() {
        said = if said.is_empty() { caught } else { format!("{said}\n{caught}") };
    }
    // the prompt asks to tell the others: how, in one shell command, aimed
    // at the agents the news is for (never offered unasked: at session start
    // it made every agent broadcast its plan to every other)
    if asks_to_tell_others(&prompt) {
        let line = format!(
            "Cork: to tell the other agents, run `{} --about <path.py::symbol> \"...\"`: it reaches the agents working on \
that code. Without --about it reaches the agents whose files the message names, and everyone at work here only when \
it names no code. Say what changed for them, not your plan.",
            crate::machine::message_command(env)
        );
        said = if said.is_empty() { line } else { format!("{said}\n{line}") };
    }
    // the prompt hands work to another agent: through Cork, as one spec,
    // never by asking the person to copy and paste it
    if asks_to_hand_off(&prompt) {
        let line = format!(
            "Cork: the user wants this handed to another agent. Write it as one self-contained spec (the goal, what was \
decided and why, the files involved, what done looks like) and run `{} \"<spec>\"`: it reaches the user's other session \
open in this repo, which starts on it by itself, or waits for the next one. Never ask the user to copy and paste it. Then \
tell the user in one line where it went.",
            crate::machine::own_command(env, "handoff")
        );
        said = if said.is_empty() { line } else { format!("{said}\n{line}") };
    }
    // messages this session's listener held back for its next hook (peer.rs)
    let held = crate::peer::take_held(&session, env);
    if !held.is_empty() {
        said = if said.is_empty() { held } else { format!("{held}\n{said}") };
    }
    let rendered = crate::harness::render_context("UserPromptSubmit", &said);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    let paths = brief.get("paths").and_then(Value::as_array).cloned().unwrap_or_default();
    let named = brief.get("named").and_then(Value::as_array).cloned().unwrap_or_default();
    let covered = brief.get("covered").and_then(Value::as_array).cloned().unwrap_or_default();
    remember_briefed(&session, &paths, &named, &checked, &covered, env);
    0
}

/// The prompt without its sentences that put code off limits ("Don't touch
/// static/js/engine/, views/space.js.", "leave the router alone").
pub fn without_off_limits(prompt: &str) -> String {
    static SENTENCE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static OFF: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // a sentence ends at a period, ! or ? before a space or the end, or a
    // line break: never at the dot of a path (`space.js,`)
    let sentence = SENTENCE.get_or_init(|| Regex::new(r"[^\n]*?(?:[.!?](?:\s|$)|\n|$)").expect("sentence pattern"));
    let off = OFF.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)\b(?:don'?t|do\s+not|never|must\s+not|should\s+not|avoid)\s+(?:touch|touching|change|changing|modify|modifying|edit|editing)\b",
            r"|\bleave\b[^.!?\n]{0,80}\balone\b|\bhands\s+off\b|\bout\s+of\s+scope\b|\boff[- ]limits\b"
        ))
        .expect("off-limits pattern")
    });
    sentence.find_iter(prompt).map(|m| m.as_str()).filter(|s| !off.is_match(s)).collect()
}

fn grep_identifiers(command: &str) -> Vec<String> {
    let cmd_re = Regex::new(r"(?:^|[;&|(]\s*)(?:git\s+grep|rg|grep|ag|ack)\b").expect("static regex");
    let Some(first) = cmd_re.find(command) else { return Vec::new() };
    // a grep fed by a pipe filters another command's output (`ls | grep
    // linear`): a search of that output, not of the code
    let piped = first.as_str().starts_with('|') && !command[..first.start()].ends_with('|');
    if piped {
        return Vec::new();
    }
    let split_re = Regex::new(r"(?:git\s+grep|rg|grep|ag|ack)\b(.*)$").expect("static regex");
    let rest = split_re.captures(command).and_then(|c| c.get(1)).map(|m| m.as_str()).unwrap_or("");
    let quoted_re = Regex::new(r#"'([^']+)'|"([^"]+)""#).expect("static regex");
    let mut candidates: Vec<String> = quoted_re
        .captures_iter(rest)
        .filter_map(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_string()))
        .take(1)
        .collect();
    if candidates.is_empty() {
        let stripped = quoted_re.replace_all(rest, " ");
        candidates = stripped
            .split_whitespace()
            .filter(|t| !t.starts_with('-') && !t.contains('/') && !t.starts_with('.') && !matches!(*t, "|" | "&&" | ";"))
            .take(1)
            .map(str::to_string)
            .collect();
    }
    let word_re = Regex::new(r"[A-Za-z_]\w{2,}").expect("static regex");
    let mut out: Vec<String> = Vec::new();
    for cand in &candidates {
        for m in word_re.find_iter(cand) {
            let w = m.as_str().to_string();
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out.truncate(MAX_GREP_SYMBOLS);
    out
}

/// Whether a search command looks in this repo's code: every file or
/// folder it names is inside `root`, and none is a file the map does not
/// hold (`grep -n x run.sh`, a log, notes). A search that names no path
/// searches the working folder, which is the repo's.
fn searches_code(command: &str, root: &std::path::Path) -> bool {
    let root = crate::config::absolute(root);
    for token in command.split_whitespace().map(|t| t.trim_matches(|c| c == '"' || c == '\'')) {
        if token.starts_with('-') || !(token.contains('/') || token.contains('.')) || token.contains('*') {
            continue;
        }
        let path = std::path::Path::new(token);
        let full = if path.is_absolute() { path.to_path_buf() } else { root.join(path) };
        if path.is_absolute() && !full.starts_with(&root) {
            return false;
        }
        if full.is_file() && !crate::report::is_map_source(&token.trim_start_matches("./").to_string()) {
            return false;
        }
    }
    true
}

/// One name as the map answers it: its file, signature, doc and callers,
/// and the other files that define a name of the same spelling. A name the
/// session's own work does not tell apart (`ambiguous`) is each of its
/// files, briefly, never one of them as the answer. The map's entry is some
/// checkout's copy of the file: the line says what THIS copy has (its
/// signature, range and doc), and there is no line when this copy has no
/// such symbol (an agent was pointed at a `fn date` its file did not have).
fn map_line(root: &std::path::Path, name: &str, fact: &serde_json::Map<String, Value>) -> Option<String> {
    let listed = |key: &str| -> Vec<&str> {
        fact.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect()
    };
    if fact.get("ambiguous").is_some() {
        return Some(format!("Cork map — `{name}` is defined in more than one file: {}", listed("ambiguous").join(", ")));
    }
    let path = fact.get("path").and_then(Value::as_str).unwrap_or("");
    let local = crate::mapcheck::local_fact(root, path, name)?;
    let sig = if local.signature.is_empty() { name } else { local.signature.as_str() };
    let doc = local.doc.as_str();
    // a caller is named only while its file here still says the name
    let callers: Vec<&str> = listed("callers")
        .into_iter()
        .filter(|id| {
            let file = id.split("::").next().unwrap_or("");
            crate::report::source_of(&root.join(file)).is_some_and(|text| text.contains(name))
        })
        .collect();
    // `src/api.rs:120-180`: what to Read (offset/limit), not the file
    let at = if local.span.0 > 0 && local.span.1 >= local.span.0 { format!("{path}:{}-{}", local.span.0, local.span.1) } else { path.to_string() };
    let mut line = format!("Cork map — `{name}` ({at}): {sig}");
    if !doc.is_empty() {
        line.push_str(&format!(" — {doc}"));
    }
    if !callers.is_empty() {
        let shown: Vec<&str> = callers.iter().take(8).cloned().collect();
        line.push_str(&format!(" · callers: {}", shown.join(", ")));
        if callers.len() > 8 {
            line.push_str(&format!(" (+{})", callers.len() - 8));
        }
    }
    // the one this session's work points at; the others exist
    let elsewhere = listed("elsewhere");
    if !elsewhere.is_empty() {
        line.push_str(&format!(" · another `{name}` in {}", elsewhere.join(", ")));
    }
    Some(line)
}

/// The map's structured answer to the name the agent just searched for. A
/// subagent's search (`worker`) is answered the same, but the read it
/// replaced was the subagent's: it never rides in the parent's messages.
pub fn grep_companion(command: &str, cfg: &config::Config, root: &std::path::Path, budget: Duration, session: &str, worker: bool, env: &Env) -> String {
    // a symbol already answered this session is in the context already: the
    // same note repeated on every matching search was context paid for twice
    let state_file = (!session.is_empty()).then(|| state_path(session, env));
    let mut state = state_file.as_ref().map(|p| config::load_json(p).as_object().cloned().unwrap_or_default()).unwrap_or_default();
    let told = strings(&state, "grep_told");
    // a search of something other than this repo's code (a script, a log,
    // a file elsewhere) is not a search the map can answer
    if !searches_code(command, root) {
        return String::new();
    }
    let symbols: Vec<String> = grep_identifiers(command).into_iter().filter(|s| !told.contains(s)).collect();
    if symbols.is_empty() || budget.is_zero() {
        return String::new();
    }
    let payload = {
        let mut payload = json!({"repo_id": cfg.repo_id, "symbols": symbols});
        if !session.is_empty() {
            // the answer is a read not sent, priced against this session
            payload["session"] = json!(session);
        }
        payload
    };
    let Ok(answer) = http::post(&cfg.server, "/attribute", &cfg.token, &user_agent(), &payload, budget.min(Duration::from_secs(3))) else {
        return String::new();
    };
    let Some(facts) = answer.get("facts").and_then(Value::as_object) else { return String::new() };
    if facts.is_empty() {
        return String::new();
    }
    let lines: Vec<(&String, String)> =
        symbols.iter().filter_map(|name| Some((name, map_line(root, name, facts.get(name)?.as_object()?)?))).collect();
    // whole lines within the cap, never one cut mid-sentence (the first line
    // alone over the cap is cut, and says so)
    let mut out: Vec<String> = Vec::new();
    let mut shown: Vec<String> = Vec::new();
    let mut used = 0usize;
    for (name, line) in lines {
        let size = line.chars().count() + usize::from(!out.is_empty());
        if used + size > GREP_NOTE_CHARS {
            if out.is_empty() {
                out.push(format!("{}…", line.chars().take(GREP_NOTE_CHARS - 1).collect::<String>()));
                shown.push(name.clone());
            }
            break;
        }
        used += size;
        out.push(line);
        shown.push(name.clone());
    }
    if let Some(path) = state_file {
        let mut all = told;
        all.extend(shown.iter().cloned());
        state.insert("grep_told".into(), json!(all));
        save_json(&path, &Value::Object(state));
        // each answer stands for a read of the file that defines the name
        // (an ambiguous one names no file, and saved none)
        for name in shown.iter().filter(|_| !worker) {
            let rel = facts.get(name).and_then(|f| f.get("path")).and_then(Value::as_str).unwrap_or("");
            if rel.is_empty() {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(root.join(rel)) {
                note_avoided(session, "map", rel, meta.len(), env);
            }
        }
    }
    out.join("\n")
}

/// The read answered that another agent wrote this file moments ago.
/// `lands`: this session's pushes land through Cork, so a teammate's
/// pushed change is something landing rebases onto, not a pull-now.
pub fn recent_write_note(resp: &Value, path: &str, lands: bool) -> String {
    let Some(who) = resp.get("recent_write").and_then(Value::as_object) else { return String::new() };
    if who.get("mine").and_then(Value::as_bool).unwrap_or(false) {
        return String::new();
    }
    let whom = if who.get("same_user").and_then(Value::as_bool).unwrap_or(false) {
        "another session of yours"
    } else {
        who.get("who")
            .and_then(Value::as_str)
            .filter(|w| !w.is_empty())
            .or_else(|| who.get("user").and_then(Value::as_str).filter(|u| !u.is_empty()))
            .unwrap_or("a teammate")
    };
    let agent = agent_suffix(who.get("agent").and_then(Value::as_str).unwrap_or(""));
    let age = age_text(who.get("age_s").and_then(Value::as_f64).unwrap_or(0.0));
    // each agent has its own checkout: the server says whether their change
    // is in what was just read, and whether it is pushed yet
    let in_copy = who.get("in_copy").and_then(Value::as_bool).unwrap_or(true);
    let pushed = who.get("pushed").and_then(Value::as_bool).unwrap_or(false);
    if in_copy {
        format!(
            "Cork: {whom}{agent} wrote {path} {age}; what you just read includes their change. Build on it: keep their lines and make your own edit now, there is nothing to wait for. Say so to the user if it changes what you do."
        )
    } else if pushed && lands {
        format!(
            "Cork: {whom}{agent} changed {path} {age} and pushed it; your copy does not have it yet. Landing will rebase onto it when you push: carry on, and keep your edit to your own part. Say so to the user if it changes what you do."
        )
    } else if pushed {
        format!(
            "Cork: {whom}{agent} changed {path} {age} and pushed it, but your copy does not have it yet. Pull first (git pull --rebase), re-read {path}, then make your edit on top of theirs. Say so to the user if it changes what you do."
        )
    } else {
        format!(
            "Cork: {whom}{agent} is changing {path} in their own checkout ({age}); it is not pushed, so your copy does not have it. Do not redo their change: make your edit fit theirs; you merge when you push (Cork rebases for you). Say so to the user if it changes what you do."
        )
    }
}

/// ` (claude)` after a teammate's name: the harness they work in, never the
/// program that reported their edit (`cork-report-hook/88`, or
/// `collide-report-hook/75` from before the rename, read as a name
/// in Study 8). Empty when that is all there is.
pub fn agent_suffix(agent: &str) -> String {
    let agent = agent.trim();
    let lowered = agent.to_ascii_lowercase();
    if agent.is_empty() || lowered.starts_with("cork") || lowered.starts_with("collide") || lowered.contains("hook") || lowered.contains('/') {
        String::new()
    } else {
        format!(" ({agent})")
    }
}

pub(crate) fn age_text(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 120.0 {
        "just now".to_string()
    } else if s < 7200.0 {
        format!("{}m ago", (s / 60.0) as i64)
    } else if s < 172_800.0 {
        format!("{}h ago", (s / 3600.0) as i64)
    } else {
        format!("{}d ago", (s / 86_400.0) as i64)
    }
}

pub fn join_notes(parts: &[&str]) -> String {
    parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {

    /// The field report (v103): the map named a script's `emit` while the
    /// agent worked on Rust's `AppState::emit`. The server now picks the one
    /// the session's files point at and says the other exists, or, when
    /// nothing tells them apart, names both and neither as the answer.
    fn checkout(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("cork-mapline-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, text) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        root
    }

    #[test]
    fn a_name_defined_twice_is_never_answered_as_the_wrong_one() {
        let root = checkout("twice", &[
            ("src/state.rs", "pub fn emit(event: Event) {\n    let _ = event;\n}\n"),
            ("src/api.rs", "pub fn create_issue() {\n    emit(Event::Created);\n}\n"),
        ]);
        let fact = |v: serde_json::Value| v.as_object().cloned().unwrap();
        let picked = fact(serde_json::json!({"path": "src/state.rs", "signature": "pub fn emit(event: Event)", "span": [1, 3],
            "callers": ["src/api.rs::create_issue", "src/gone.rs::old_caller"], "elsewhere": ["static/js/store.js"]}));
        assert_eq!(super::map_line(&root, "emit", &picked).as_deref(), Some("Cork map — `emit` (src/state.rs:1-3): pub fn emit(event: Event) · \
callers: src/api.rs::create_issue · another `emit` in static/js/store.js"));
        let both = fact(serde_json::json!({"ambiguous": ["src/state.rs", "static/js/store.js"]}));
        assert_eq!(super::map_line(&root, "emit", &both).as_deref(), Some("Cork map — `emit` is defined in more than one file: src/state.rs, static/js/store.js"));
    }

    /// Study, round 2 (Lattice): the map said `fn date` at src/api.rs:91-97
    /// with an older signature, from another checkout's copy of the file. The
    /// answer is now this copy's symbol, or nothing.
    #[test]
    fn a_map_answer_is_this_copys_symbol_or_no_answer() {
        let root = checkout("stale", &[(
            "src/api.rs",
            "/// Parses YYYY-MM-DD.\npub fn parse_date(v: &str) -> Option<String> {\n    Some(v.into())\n}\n\npub fn create_issue(me: Me, body: Bytes) -> Json {\n    todo!()\n}\n",
        )]);
        let fact = |v: serde_json::Value| v.as_object().cloned().unwrap();
        let gone = fact(serde_json::json!({"path": "src/api.rs", "signature": "fn date(v: &Value, field: &str) -> Result<String>", "span": [91, 97]}));
        assert_eq!(super::map_line(&root, "date", &gone), None);
        let moved = fact(serde_json::json!({"path": "src/api.rs", "signature": "pub async fn create_issue(State(app): State<Arc<AppState>>)", "span": [194, 238]}));
        assert_eq!(super::map_line(&root, "create_issue", &moved).as_deref(), Some("Cork map — `create_issue` (src/api.rs:6-8): pub fn create_issue(me: Me, body: Bytes) -> Json"));
    }

    /// The code a prompt puts off limits is not what it is about (Lattice:
    /// "Don't touch views/space.js" briefed space.js and warned of the
    /// sessions working there); a grep over another command's output is not
    /// a search of the code (`ls | grep linear` answered with a function).
    #[test]
    fn off_limits_code_and_piped_greps_are_not_the_task() {
        let spec = "Add start_date in src/db.rs and src/api.rs, validated with the `date()` helper.\nDon't commit. Don't touch static/js/engine/, views/space.js, views/studio.js.\nTests go in tests/api.rs.";
        let ids = super::prompt_identifiers(spec);
        assert!(ids.iter().any(|i| i == "src/db.rs") && ids.iter().any(|i| i == "tests/api.rs") && ids.iter().any(|i| i == "start_date"), "{ids:?}");
        assert!(!ids.iter().any(|i| i.contains("space.js") || i.contains("studio.js") || i.contains("engine")), "{ids:?}");
        assert!(super::without_off_limits("Leave the router alone. Fix the parser.").trim() == "Fix the parser.");
        assert!(super::grep_identifiers("ls ~/.claude/projects/ | grep -i \"linear\" | head").is_empty());
        assert_eq!(super::grep_identifiers("rg -n \"fn parse_date\" src | head"), vec!["parse_date".to_string()]);
        assert_eq!(super::grep_identifiers("cd src || grep -rn create_issue ."), vec!["create_issue".to_string()]);
        // and a search of a script or a file outside the repo is not a code search
        let root = checkout("search", &[("src/api.rs", "pub fn create_issue() {}\n"), ("run.sh", "SERVER=1\n")]);
        assert!(super::searches_code("grep -rn create_issue src", &root));
        assert!(super::searches_code("rg create_issue", &root));
        assert!(!super::searches_code("grep -n SERVER run.sh", &root));
        assert!(!super::searches_code("grep -n local /tmp/elsewhere/run.sh", &root));
    }

    /// A pasted task notification named ids and usage keys as if they were
    /// code ("matched nothing in the map: toolu_01…, subagent_tokens,
    /// tool_uses, duration_ms"): they are not sent; the names around them are.
    #[test]
    fn pasted_ids_and_usage_keys_are_not_identifiers() {
        let prompt = "fix `mountIssue` in static/js/issue.js\n<task-notification>\n<task-id>b6x30h54c</task-id>\n\
<tool-use-id>toolu_01NptqZqEmu3CZcyS7tLaxcs</tool-use-id>\n<status>completed</status>\n\
<usage><subagent_tokens>193752</subagent_tokens><tool_uses>37</tool_uses><duration_ms>340608</duration_ms></usage>\n\
</task-notification>\nand a5194030f6f9e7184 ran";
        let ids = super::prompt_identifiers(prompt);
        assert!(ids.iter().any(|i| i == "mountIssue") && ids.iter().any(|i| i == "static/js/issue.js"), "{ids:?}");
        for noise in ["b6x30h54c", "toolu_01NptqZqEmu3CZcyS7tLaxcs", "subagent_tokens", "tool_uses", "duration_ms", "a5194030f6f9e7184"] {
            assert!(!ids.iter().any(|i| i == noise), "{noise} in {ids:?}");
        }
    }

    use super::asks_to_hand_off;

    #[test]
    fn handing_work_to_another_agent_is_heard_in_the_prompt() {
        for asked in ["send this to cork", "Send the spec to Cork please", "hand this off", "can you hand it off?",
                      "give this to the other session", "pass the plan to my other agent", "put it in cork"] {
            assert!(asks_to_hand_off(asked), "{asked}");
        }
        for not in ["the handoff command should be simple", "cork the bottle", "send the email to ana", "fix the other session's bug"] {
            assert!(!asks_to_hand_off(not), "{not}");
        }
    }

    use super::*;

    fn env_in(name: &str) -> Env {
        let dir = std::env::temp_dir().join(format!("collide-carry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        env
    }

    fn carried(session: &str, env: &Env) -> (u64, u64, u64) {
        let state = config::load_json(&state_path(session, env));
        let get = |k: &str| state.get(k).and_then(Value::as_u64).unwrap_or(0);
        (get("brief_carried"), get("read_avoided_carried"), get("map_avoided_carried"))
    }

    #[test]
    fn the_carry_counts_each_message_once_and_never_a_subagents() {
        let env = env_in("once");
        let session = "carry-session-1";
        note_injected(session, 400, &env); // 100 briefing tokens in the context
        note_avoided(session, "map", "pkg/a.py", 4000, &env); // a 1000-token read not sent
        // one message, three parallel tool calls: one message carried it
        for _ in 0..3 {
            note_call(session, false, "msg_1", &env);
        }
        assert_eq!(carried(session, &env), (100, 0, 1000));
        // a subagent's calls are messages in its own context
        note_call(session, true, "msg_sub", &env);
        assert_eq!(carried(session, &env), (100, 0, 1000));
        // the next message carries it again
        note_call(session, false, "msg_2", &env);
        assert_eq!(carried(session, &env), (200, 0, 2000));
        // no id known: every call counts, as before
        note_call(session, false, "", &env);
        note_call(session, false, "", &env);
        assert_eq!(carried(session, &env), (400, 0, 4000));
    }
}
