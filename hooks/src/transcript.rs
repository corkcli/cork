//! Reading the harness transcript: what the session spent since it was last
//! billed, what it is called, the context it carries now, and whether the
//! agent just ran out of quota. Billing and the title read only the bytes
//! added since their last read, the rest only the tail, so all of it stays
//! well inside the hook's budget; any problem yields nothing rather than
//! failing.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::config::Env;

pub const TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;
const LIMIT_TAIL_LINES: usize = 40;
/// The harness's limit notice is one short line that says the limit was hit
/// and when it resets. An agent writing ABOUT limits — "rate-limiting for
/// the free tier" in a summary of its own work — is prose: long, and saying
/// neither. Without these two tests that prose read as "hit its usage
/// limit" on every teammate's dashboard. Twin of the Python hook's.
const LIMIT_MAX_ENTRY_CHARS: usize = 300;
const LIMIT_WORDS: [&str; 4] = ["reset", "reached", "exceeded", "hit your"];

fn limit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)((?:session|usage|weekly|daily|monthly|5-hour|rate)[ -]limit\b[^\n]{0,120})")
            .expect("limit pattern")
    })
}

fn tail(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.seek(SeekFrom::End(0)).ok()?;
    file.seek(SeekFrom::Start(size.saturating_sub(TRANSCRIPT_TAIL_BYTES))).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn take_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Every usage counter a turn is billed on, plus `tokens` — the pre-v20
/// field, which was uncached input + output only. Cleared together when a
/// turn spent nothing (a limit hit).
pub const USAGE_KEYS: [&str; 5] =
    ["tokens", "input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens"];

/// The latest assistant message's usage, its model, and its id (`turn_id`):
/// what the context is now and which message made this tool call. Billing
/// no longer rides here — `unbilled` sums every message at Stop — but the
/// context prices a saving, and the id counts each message once.
///
/// All four counters are sent, not one sum: cache creation costs 1.25x an
/// input token and a cache read a tenth of one, and in a cached agent loop
/// those two ARE most of the bill, so no single blended rate over
/// uncached-input + output can stand in for them. `tokens` keeps its old
/// meaning for a server that predates the four.
///
/// Beside the model ride its `effort` and `ultracode` (see [`Mode`]), each
/// left out when the harness did not say. This reads the tail only, so a
/// standing ultracode entered megabytes back goes unseen: [`turn_meta_in`]
/// asks the bill marker too.
pub fn turn_meta(path: &str) -> Map<String, Value> {
    turn_meta_with(path, None)
}

/// [`turn_meta`], with the ultracode the transcript's bill marker last saw
/// standing in for what the tail does not show (the bill reads every byte;
/// the tail only the last 256K, and ultracode's reminders are megabytes
/// apart).
pub fn turn_meta_in(path: &str, env: &Env) -> Map<String, Value> {
    turn_meta_with(path, Some(env))
}

fn turn_meta_with(path: &str, env: Option<&Env>) -> Map<String, Value> {
    let mut meta = Map::new();
    let Some(text) = tail(path) else { return meta };
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() || !line.contains("\"assistant\"") {
            continue;
        }
        // a partial first line from the tail cut simply fails to parse
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let message = entry.get("message").cloned().unwrap_or(Value::Null);
        let usage = message.get("usage").cloned().unwrap_or(Value::Null);
        if !usage.is_object() || usage.as_object().map(|m| m.is_empty()).unwrap_or(true) {
            continue;
        }
        let count = |field: &str| usage.get(field).and_then(Value::as_i64).unwrap_or(0);
        let (inp, out) = (count("input_tokens"), count("output_tokens"));
        let (cache_creation, cache_read) =
            (count("cache_creation_input_tokens"), count("cache_read_input_tokens"));
        // the API message's id: one message is several transcript lines
        // (a line per content block), and they are all the same message
        let turn_id = message
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| entry.get("uuid").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let model = message.get("model").and_then(Value::as_str).unwrap_or("").to_string();
        // the context the next message would replay: everything read, cached or not
        let context = inp + cache_creation + cache_read;
        meta.insert("tokens".into(), Value::from(inp + out));
        meta.insert("input_tokens".into(), Value::from(inp));
        meta.insert("output_tokens".into(), Value::from(out));
        meta.insert("cache_read_tokens".into(), Value::from(cache_read));
        meta.insert("cache_creation_tokens".into(), Value::from(cache_creation));
        meta.insert("context".into(), Value::from(context));
        meta.insert("turn_id".into(), Value::from(turn_id));
        meta.insert("model".into(), Value::from(model));
        let mode = Mode { effort: claude_effort(&entry), ultracode: ultra_with(&text, path, env) };
        mode.put(&mut meta);
        return meta;
    }
    codex_turn_meta(&text)
}

/// One tool call's cost, for the dashboard's line and never for the bill:
/// the four counters of the message that made it (`turn_meta`), and that
/// message's id, shortened, so the dashboard shows a message's usage once
/// when it made several calls. None when the message spent nothing.
pub fn step_of(meta: &Map<String, Value>, message: &str) -> Option<Value> {
    let count = |key: &str| meta.get(key).and_then(Value::as_i64).unwrap_or(0).max(0);
    let (input, output, cache_read, cache_write) =
        (count("input_tokens"), count("output_tokens"), count("cache_read_tokens"), count("cache_creation_tokens"));
    if input + output + cache_read + cache_write == 0 {
        return None;
    }
    let digest = crate::report::sha256_hex(message.as_bytes());
    Some(json!({
        "msg": if message.is_empty() { String::new() } else { digest[..12].to_string() },
        "input": input, "output": output, "cache_read": cache_read, "cache_write": cache_write,
    }))
}

/// The same counters from a Codex rollout. A current Codex logs each model
/// response as a `token_usage_record` (its `response_id` is the call's id);
/// an older one as a `token_count` event, whose timestamp stands in for the
/// id. Either way the many tool calls of one response bill it once. The model
/// is in `turn_context`. OpenAI counts cached input inside `input_tokens`, so
/// the uncached part is the difference.
fn codex_turn_meta(text: &str) -> Map<String, Value> {
    let mut meta = Map::new();
    let lines: Vec<&str> = text.lines().collect();
    for (at, line) in lines.iter().enumerate().rev() {
        if !line.contains("\"token_usage_record\"") && !line.contains("\"token_count\"") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else { continue };
        let payload = entry.get("payload").cloned().unwrap_or(Value::Null);
        let stamp = entry.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let (usage, turn_id) = match (entry.get("type").and_then(Value::as_str), payload.get("type").and_then(Value::as_str)) {
            (Some("token_usage_record"), _) => (
                payload.get("usage").cloned(),
                payload.get("response_id").and_then(Value::as_str).map(|id| format!("codex:{id}")).unwrap_or_default(),
            ),
            (Some("event_msg"), Some("token_count")) => (
                payload.get("info").and_then(|i| i.get("last_token_usage")).cloned(),
                if stamp.is_empty() { String::new() } else { format!("codex:{stamp}") },
            ),
            _ => continue,
        };
        let Some(usage) = usage.filter(Value::is_object) else { continue };
        let count = |field: &str| usage.get(field).and_then(Value::as_i64).unwrap_or(0).max(0);
        let (read, cached, written, out) =
            (count("input_tokens"), count("cached_input_tokens"), count("cache_write_input_tokens"), count("output_tokens"));
        let inp = (read - cached - written).max(0);
        // the model and the effort both come from the turn it ran in
        let context = lines[..at].iter().rev()
            .filter(|l| l.contains("\"turn_context\""))
            .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
            .filter_map(|e| e.get("payload").cloned())
            .find(|p| p.get("model").and_then(Value::as_str).is_some())
            .unwrap_or(Value::Null);
        let model = context.get("model").and_then(Value::as_str).unwrap_or("").to_string();
        meta.insert("tokens".into(), Value::from(inp + out));
        meta.insert("input_tokens".into(), Value::from(inp));
        meta.insert("output_tokens".into(), Value::from(out));
        meta.insert("cache_read_tokens".into(), Value::from(cached));
        meta.insert("cache_creation_tokens".into(), Value::from(written));
        meta.insert("context".into(), Value::from(read));
        meta.insert("turn_id".into(), Value::from(turn_id));
        meta.insert("model".into(), Value::from(model));
        Mode { effort: codex_effort(&context), ultracode: false }.put(&mut meta);
        return meta;
    }
    meta
}

// ------------------------------------------------------------ effort

/// How the harness had the model work, beside the model in every payload
/// that names one: the reasoning effort exactly as the harness names it
/// ("medium", "xhigh"; "" when it did not say, and then nothing is sent
/// rather than a guessed default), and whether Claude Code's ultracode was
/// on for the message. Two messages of one model at different efforts are
/// different spends, so the dashboard can tell an xhigh hour from a medium
/// one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Mode {
    pub effort: String,
    pub ultracode: bool,
}

impl Mode {
    /// Into a payload: `effort` when known, `ultracode` only when on.
    pub fn put(&self, map: &mut Map<String, Value>) {
        if !self.effort.is_empty() {
            map.insert("effort".into(), json!(self.effort));
        }
        if self.ultracode {
            map.insert("ultracode".into(), json!(true));
        }
    }
}

/// Copies `effort` and `ultracode` from a [`turn_meta`] into a payload
/// that names its model.
pub fn copy_mode(meta: &Map<String, Value>, to: &mut Map<String, Value>) {
    if let Some(effort) = meta.get("effort").and_then(Value::as_str).filter(|e| !e.is_empty()) {
        to.insert("effort".into(), json!(effort));
    }
    if meta.get("ultracode").and_then(Value::as_bool) == Some(true) {
        to.insert("ultracode".into(), json!(true));
    }
}

/// An effort's name is one word; whatever longer a transcript holds in its
/// place is cut to this, so the bill marker and every post stay small.
const EFFORT_MAX_CHARS: usize = 32;

/// The effort a Claude Code assistant line ran at: the turn's own
/// (`perTurnEffort`), else the session's (`effort`); a synthetic line
/// carries only the first.
fn claude_effort(entry: &Value) -> String {
    ["perTurnEffort", "effort"]
        .iter()
        .find_map(|key| entry.get(*key).and_then(Value::as_str).filter(|e| !e.is_empty()))
        .map(|e| take_chars(e, EFFORT_MAX_CHARS))
        .unwrap_or_default()
}

/// The effort a Codex `turn_context` payload names: its collaboration
/// mode's setting (null when the person left the default, and the default
/// lives in a config Codex may override, so it stays unknown), else an
/// older rollout's `effort` or `reasoning_effort`.
fn codex_effort(payload: &Value) -> String {
    let mode = payload.get("collaboration_mode").and_then(|m| m.get("settings")).and_then(|s| s.get("reasoning_effort"));
    [mode, payload.get("effort"), payload.get("reasoning_effort")]
        .into_iter()
        .flatten()
        .find_map(|v| v.as_str().filter(|e| !e.is_empty()))
        .map(|e| take_chars(e, EFFORT_MAX_CHARS))
        .unwrap_or_default()
}

/// Claude Code's ultracode as the transcript shows it: a standing mode,
/// entered and left by `ultra_effort_enter` / `ultra_effort_exit`
/// attachments (the enter reminder comes again now and then, megabytes
/// apart), and the one turn the person typed the keyword in, marked by a
/// `workflow_keyword_request` attachment after that turn's prompt.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Ultra {
    /// the standing mode is on
    pub on: bool,
    /// this turn asked for it with the keyword
    pub turn: bool,
}

impl Ultra {
    fn mark(&mut self, mark: Mark) {
        match mark {
            Mark::Enter => self.on = true,
            Mark::Exit => self.on = false,
            Mark::Keyword => self.turn = true,
            Mark::Prompt => self.turn = false,
        }
    }

    pub fn active(self) -> bool {
        self.on || self.turn
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Enter,
    Exit,
    Keyword,
    Prompt,
}

/// What a line says about ultracode, from its strings (no JSON parse: a
/// tool result's line can be megabytes). Matched as the transcript writes
/// an attachment, unescaped, so a tool's output that only quotes one (this
/// very code's tests, a grep of a transcript) is escaped and never counts.
/// A person's prompt starts a turn the keyword has not reached yet; the
/// harness's own user lines (a skill's text, a compaction summary, the
/// notification of a background task the request started, which wakes that
/// same request up; an older transcript's inlined subagent) do not.
fn mark_of(line: &str) -> Option<Mark> {
    if line.contains("\"type\":\"assistant\"") {
        return None;
    }
    if line.contains("\"type\":\"attachment\"") {
        for (marker, mark) in [
            ("\"attachment\":{\"type\":\"ultra_effort_enter\"", Mark::Enter),
            ("\"attachment\":{\"type\":\"ultra_effort_exit\"", Mark::Exit),
            ("\"attachment\":{\"type\":\"workflow_keyword_request\"", Mark::Keyword),
        ] {
            if line.contains(marker) {
                return Some(mark);
            }
        }
        return None;
    }
    let prompt = line.contains("\"type\":\"user\"")
        && !line.contains("\"tool_result\"")
        && !line.contains("\"isMeta\":true")
        && !line.contains("\"isCompactSummary\":true")
        && !line.contains("\"promptSource\":\"system\"")
        && !line.contains("\"isSidechain\":true");
    prompt.then_some(Mark::Prompt)
}

/// The ultracode a stretch of transcript decides: (standing, this turn),
/// each None when no line in it says (no enter or exit; not the turn's
/// prompt).
fn ultra_in(text: &str) -> (Option<bool>, Option<bool>) {
    let (mut on, mut turn) = (None, None);
    for line in text.lines() {
        match mark_of(line.trim()) {
            Some(Mark::Enter) => on = Some(true),
            Some(Mark::Exit) => on = Some(false),
            Some(Mark::Keyword) => turn = Some(true),
            Some(Mark::Prompt) => turn = Some(false),
            None => {}
        }
    }
    (on, turn)
}

/// Whether a transcript is in ultracode now: what its tail says, else what
/// its bill marker last saw (when there is an `env` to find it by).
fn ultra_with(tail: &str, path: &str, env: Option<&Env>) -> bool {
    let (on, turn) = ultra_in(tail);
    let marked = env.map(|env| bill_ultra(path, env)).unwrap_or_default();
    on.unwrap_or(marked.on) || turn.unwrap_or(marked.turn)
}

/// The session transcript a subagent's own transcript belongs to:
/// `<session>.jsonl` for `<session>/subagents/agent-<id>.jsonl` and for a
/// workflow's `<session>/subagents/workflows/<run>/agent-<id>.jsonl`.
fn parent_transcript(path: &str) -> Option<String> {
    let subagents = Path::new(path).ancestors().skip(1).take(3).find(|dir| dir.file_name().is_some_and(|n| n == "subagents"))?;
    let session = subagents.parent()?;
    Some(session.with_extension("jsonl").to_string_lossy().into_owned())
}

/// The ultracode a subagent starts in: its parent's when it is first
/// billed. The markers live in the parent's transcript only, and most of
/// an ultracode turn's spend is the subagents it starts, which would
/// otherwise all bill as plain effort. A subagent's own user lines are
/// sidechain lines, never a prompt, so what it starts in lasts its life.
fn subagent_ultra(path: &str, env: &Env) -> Ultra {
    let on = parent_transcript(path)
        .is_some_and(|parent| tail(&parent).is_some_and(|text| ultra_with(&text, &parent, Some(env))));
    Ultra { on, turn: false }
}

/// The ultracode a transcript's last bill ended in.
fn bill_ultra(transcript: &str, env: &Env) -> Ultra {
    if transcript.is_empty() {
        return Ultra::default();
    }
    let state = crate::config::load_json(&bill_state_path(transcript, env));
    let flag = |key: &str| state.get(key).and_then(Value::as_bool).unwrap_or(false);
    Ultra { on: flag("ultracode_on"), turn: flag("ultracode_turn") }
}

// ------------------------------------------------------------ billing
//
// A turn used to be billed by the edits it made: each carried the usage of
// the assistant message just before it, so every message that ended in a
// read, a search, a command or plain text was never billed (one session
// billed 32 of its 311 messages). Now the transcript itself is the bill: at
// Stop, SessionEnd and SubagentStop every assistant message written since
// the last bill is summed, once per API message, per model, and the sums
// ride one /presence post. Edits carry no usage at all.

/// The most bytes one bill reads: a transcript larger than this since its
/// last bill is read in more than one go, the rest at the next Stop.
const BILL_READ_MAX: u64 = 64 * 1024 * 1024;
/// Message ids remembered past the marker, so a message whose lines straddle
/// it is still counted once.
const BILL_SEEN_KEEP: usize = 64;

/// One model's share of a stretch of transcript, served one way: messages
/// in fast mode, with US-only inference, or past a long-context line are
/// priced differently, so each such mix is a share of its own.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Spend {
    pub model: String,
    /// the effort and ultracode its messages ran at: one share per mix
    pub mode: Mode,
    pub serving: Serving,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    /// the part of `cache_creation` written for an hour (Claude Code's main
    /// conversation), priced at 2x input against 1.25x for 5 minutes
    pub cache_creation_1h: i64,
    /// API messages summed
    pub messages: u64,
    /// the newest message's input + cache read + cache creation: the context
    pub context: i64,
    /// the summed messages' ids, in transcript order
    pub ids: Vec<String>,
}

/// What a stretch of transcript billed, and where the next bill starts.
#[derive(Debug, Default)]
pub struct Bill {
    pub spend: Vec<Spend>,
    /// the end of the last whole line read
    pub offset: u64,
    /// the agent's active seconds in the stretch (see [`active_seconds`]):
    /// the agent-hours Cork meters, never wall-clock idle
    pub active_s: f64,
    /// the stretch's last timed line, so the next bill counts the gap from
    /// it: (timestamp, beat)
    pub last_beat: Option<(f64, Beat)>,
    /// the newest message ids counted, with what was counted for each (the
    /// next bill counts only what a later line of one adds)
    pub seen: Vec<Seen>,
    /// the model a Codex rollout was last on: its `turn_context` may lie
    /// before the next bill's start
    pub model: String,
    /// and the effort it named there
    pub effort: String,
    /// the ultracode the stretch ended in, for the next bill to start from
    pub ultra: Ultra,
}

/// Where a transcript left off, carried from one bill to the next: what a
/// stretch's first messages ran under when the line that said so came
/// before it.
#[derive(Debug, Default, Clone)]
pub struct Carry {
    pub model: String,
    pub effort: String,
    pub ultra: Ultra,
    pub last_beat: Option<(f64, Beat)>,
}

/// What a transcript line is, for the active-time count: a person's prompt;
/// the model at work (a thought, text mid-turn); a tool call it made; the
/// tool's result; or the end of its turn, after which the next prompt is the
/// person's time. Between a call and its result the tool is running, and
/// that is not the agent's time either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Beat {
    Prompt,
    Work,
    TurnEnd,
    Call,
    Result,
}

impl Beat {
    pub fn code(self) -> u8 {
        match self {
            Beat::Prompt => 0,
            Beat::Work => 1,
            Beat::TurnEnd => 2,
            Beat::Call => 3,
            Beat::Result => 4,
        }
    }

    pub fn from_code(code: u8) -> Beat {
        match code {
            0 => Beat::Prompt,
            2 => Beat::TurnEnd,
            3 => Beat::Call,
            4 => Beat::Result,
            _ => Beat::Work,
        }
    }
}

/// A gap between two timed lines longer than this is idle whatever the
/// lines say: a permission prompt waited on, a laptop closed mid-call.
pub const ACTIVE_GAP_MAX_S: f64 = 600.0;

/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` (the transcripts' timestamps) as epoch
/// seconds; None for anything else. Days from civil (Howard Hinnant).
pub fn epoch_of(stamp: &str) -> Option<f64> {
    let s = stamp.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day) = (d.next()?.parse::<i64>().ok()?, d.next()?.parse::<i64>().ok()?, d.next()?.parse::<i64>().ok()?);
    let mut t = time.split(':');
    let (h, mi) = (t.next()?.parse::<i64>().ok()?, t.next()?.parse::<i64>().ok()?);
    let sec: f64 = t.next()?.split(|c| c == '+' || c == '-').next()?.parse::<f64>().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60) as f64 + sec)
}

/// A line's timestamp and beat, from the strings it carries (no JSON parse:
/// a tool result's line can be megabytes). Claude Code: a `user` line is a
/// prompt unless it carries a tool result; an `assistant` line ends the turn
/// when its stop reason does. Codex: a user `response_item` is a prompt, an
/// assistant one with output text ends the turn, events are work.
pub fn beat_of(line: &str) -> Option<(f64, Beat)> {
    let at = line.find("\"timestamp\":\"")?;
    let rest = &line[at + 13..];
    let end = rest.find('"')?;
    let ts = epoch_of(&rest[..end])?;
    let beat = if line.contains("\"type\":\"assistant\"") {
        if line.contains("\"stop_reason\":\"end_turn\"") || line.contains("\"stop_reason\":\"stop_sequence\"") {
            Beat::TurnEnd
        } else if line.contains("\"type\":\"tool_use\"") {
            Beat::Call
        } else {
            Beat::Work
        }
    } else if line.contains("\"type\":\"user\"") {
        if line.contains("\"tool_result\"") { Beat::Result } else { Beat::Prompt }
    } else if line.contains("\"response_item\"") {
        if line.contains("\"role\":\"user\"") {
            Beat::Prompt
        } else if line.contains("\"function_call_output\"") {
            Beat::Result
        } else if line.contains("\"function_call\"") {
            Beat::Call
        } else if line.contains("\"role\":\"assistant\"") && line.contains("output_text") {
            Beat::TurnEnd
        } else {
            Beat::Work
        }
    } else if line.contains("\"event_msg\"") || line.contains("\"token_usage_record\"") || line.contains("\"turn_context\"") {
        Beat::Work
    } else {
        return None;
    };
    Some((ts, beat))
}

/// The seconds the agent was at work between two timed lines: the whole gap
/// (capped at [`ACTIVE_GAP_MAX_S`]), except two that are not the model's:
/// from the end of its turn to the person's next prompt (the person's
/// time), and from a tool call to that tool's result (the tool running: a
/// ten-minute test run costs nothing). The gap from a prompt to the first
/// model line, and from a result to the model's next line, is the model
/// thinking and writing: that is the agent hour.
pub fn active_seconds(from: (f64, Beat), to: (f64, Beat)) -> f64 {
    let gap = to.0 - from.0;
    if gap <= 0.0 || (from.1 == Beat::TurnEnd && to.1 == Beat::Prompt) || (from.1 == Beat::Call && to.1 == Beat::Result) {
        return 0.0;
    }
    gap.min(ACTIVE_GAP_MAX_S)
}

/// How a message was served, as far as its price is concerned.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Serving {
    /// Anthropic `speed: fast`: every counter at 2x
    pub fast: bool,
    /// Anthropic `inference_geo: us`: every counter at 1.1x
    pub us: bool,
    /// the largest of [`PROMPT_LINES`] the prompt (input + cache read +
    /// cache write) was over, 0 = none: Haiku 5.5 prices a prompt over 100K
    /// at 5x, OpenAI's GPT-5.4 and later one over 272K at 2x in, 1.5x out
    pub prompt_over: i64,
}

/// The long-context lines providers price at (Anthropic Haiku 5.5: 100K;
/// OpenAI: 272K). The server knows which applies to which model.
const PROMPT_LINES: [i64; 2] = [272_000, 100_000];

fn prompt_over(prompt: i64) -> i64 {
    PROMPT_LINES.into_iter().find(|line| prompt > *line).unwrap_or(0)
}

/// One message's usage: (id, model, its effort and ultracode, how it was
/// served, [input, output, cache_read, cache_creation, cache_creation_1h]).
type Message = (String, String, Mode, Serving, [i64; 5]);

/// A message id a bill counted, and the counts it counted (None: a marker
/// written before hooks v79, which kept ids only).
pub type Seen = (String, Option<[i64; 5]>);

/// A Claude Code assistant line's message, or None; `ultra` is where the
/// transcript stood when it was written.
fn claude_message(entry: &Value, ultra: Ultra) -> Option<Message> {
    if entry.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = entry.get("message")?;
    let usage = message.get("usage").filter(|u| u.as_object().is_some_and(|m| !m.is_empty()))?;
    let count = |field: &str| usage.get(field).and_then(Value::as_i64).unwrap_or(0).max(0);
    let written = count("cache_creation_input_tokens");
    // `cache_creation: {ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}`
    let hour = usage.get("cache_creation").and_then(|c| c.get("ephemeral_1h_input_tokens"))
        .and_then(Value::as_i64).unwrap_or(0).clamp(0, written);
    let counts = [count("input_tokens"), count("output_tokens"), count("cache_read_input_tokens"), written, hour];
    let text = |field: &str| usage.get(field).and_then(Value::as_str).unwrap_or("");
    let serving = Serving {
        fast: text("speed") == "fast",
        us: text("inference_geo") == "us",
        prompt_over: prompt_over(counts[0] + counts[2] + counts[3]),
    };
    let id = message.get("id").and_then(Value::as_str)
        .or_else(|| entry.get("uuid").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let model = message.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let mode = Mode { effort: claude_effort(entry), ultracode: ultra.active() };
    Some((id, model, mode, serving, counts))
}

/// A Codex rollout line's model call, or None: a `token_usage_record` by its
/// response id, or a `token_count` event by the running total it reports (a
/// repeated event repeats the total, so it is the same call). OpenAI counts
/// cached input and cache writes inside `input_tokens`; the uncached part
/// is the difference. Reasoning tokens are inside `output_tokens` already
/// (`total_tokens` = input + output in every rollout read), so they are
/// never added on top.
fn codex_message(entry: &Value, model: &str, effort: &str) -> Option<(bool, Message)> {
    let payload = entry.get("payload")?;
    let (record, usage, id) = match (entry.get("type").and_then(Value::as_str), payload.get("type").and_then(Value::as_str)) {
        (Some("token_usage_record"), _) => (
            true,
            payload.get("usage")?,
            format!("codex:{}", payload.get("response_id").and_then(Value::as_str).unwrap_or("")),
        ),
        (Some("event_msg"), Some("token_count")) => {
            let info = payload.get("info")?;
            let total = info.get("total_token_usage").and_then(|t| t.get("total_tokens")).and_then(Value::as_i64);
            let id = match total {
                Some(total) => format!("codex-total:{total}"),
                None => format!("codex:{}", entry.get("timestamp").and_then(Value::as_str).unwrap_or("")),
            };
            (false, info.get("last_token_usage")?, id)
        }
        _ => return None,
    };
    let count = |field: &str| usage.get(field).and_then(Value::as_i64).unwrap_or(0).max(0);
    let (read, cached, written, out) =
        (count("input_tokens"), count("cached_input_tokens"), count("cache_write_input_tokens"), count("output_tokens"));
    let counts = [(read - cached - written).max(0), out, cached, written, 0];
    let serving = Serving { prompt_over: prompt_over(read), ..Serving::default() };
    let mode = Mode { effort: effort.to_string(), ultracode: false };
    Some((record, (id, model.to_string(), mode, serving, counts)))
}

/// Every assistant message in `path` from byte `offset` on, summed per
/// model in first-seen order, each API message once: a message is several
/// transcript lines with one id, the last carrying its final counts, and an
/// id in `seen` was counted by the last bill — up to the counts it saw
/// then, so a message whose lines straddled that bill (a subagent's last
/// one, written as its SubagentStop read) adds only what came after. Only
/// whole lines are read; a transcript shorter than `offset` was rewritten
/// and is read from its start. None when the file cannot be read.
pub fn unbilled(path: &str, offset: u64, seen: &[Seen], model: &str) -> Option<Bill> {
    unbilled_from(path, offset, seen, model, None)
}

/// [`unbilled`] counting the agent's active time on from `last_beat`, the
/// previous bill's last timed line.
pub fn unbilled_from(path: &str, offset: u64, seen: &[Seen], model: &str, last_beat: Option<(f64, Beat)>) -> Option<Bill> {
    unbilled_carry(path, offset, seen, Carry { model: model.to_string(), last_beat, ..Carry::default() })
}

/// [`unbilled`] going on from everything the previous bill ended in.
pub fn unbilled_carry(path: &str, offset: u64, seen: &[Seen], carry: Carry) -> Option<Bill> {
    if path.is_empty() {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.seek(SeekFrom::End(0)).ok()?;
    let start = if offset > size { 0 } else { offset };
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(BILL_READ_MAX).read_to_end(&mut bytes).ok()?;
    // a line still being written is left for the next bill
    let whole = bytes.iter().rposition(|b| *b == b'\n').map(|at| at + 1).unwrap_or(0);
    let text = String::from_utf8_lossy(&bytes[..whole]);
    let Carry { mut model, mut effort, mut ultra, mut last_beat } = carry;
    // claude: id -> message (the last line of a message has its final counts)
    let mut order: Vec<String> = Vec::new();
    let mut messages: std::collections::HashMap<String, Message> = std::collections::HashMap::new();
    let mut records: Vec<Message> = Vec::new();
    let mut counts: Vec<Message> = Vec::new();
    let mut active_s = 0.0;
    for line in text.lines() {
        let line = line.trim();
        if let Some(mark) = mark_of(line) {
            ultra.mark(mark);
        }
        if let Some(beat) = beat_of(line) {
            if let Some(from) = last_beat {
                active_s += active_seconds(from, beat);
            }
            last_beat = Some(beat);
        }
        let claude = line.contains("\"assistant\"") && line.contains("\"usage\"");
        let codex = line.contains("\"turn_context\"") || line.contains("\"token_usage_record\"") || line.contains("\"token_count\"");
        if !claude && !codex {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        if entry.get("type").and_then(Value::as_str) == Some("turn_context") {
            let payload = entry.get("payload").cloned().unwrap_or(Value::Null);
            if let Some(named) = payload.get("model").and_then(Value::as_str) {
                model = named.to_string();
            }
            // a turn that names no effort ran at the default, which is unknown
            effort = codex_effort(&payload);
            continue;
        }
        if let Some(message) = claude_message(&entry, ultra) {
            if !messages.contains_key(&message.0) {
                order.push(message.0.clone());
            }
            messages.insert(message.0.clone(), message);
        } else if let Some((record, message)) = codex_message(&entry, &model, &effort) {
            if record { records.push(message) } else { counts.push(message) }
        }
    }
    // a Codex that logs both a record and a count for each call: the records
    let mut all: Vec<Message> = order.into_iter().filter_map(|id| messages.remove(&id)).collect();
    all.extend(if records.is_empty() { counts } else { records });
    let mut bill = Bill { offset: start + whole as u64, model, effort, ultra, active_s, last_beat, ..Bill::default() };
    let mut seen: Vec<Seen> = seen.to_vec();
    for (id, model, mode, serving, counts) in all {
        if counts.iter().all(|c| *c == 0) {
            continue;
        }
        let earlier = if id.is_empty() { None } else { seen.iter().position(|(s, _)| *s == id) };
        let before = match earlier.map(|at| seen[at].1) {
            None => [0i64; 5],
            Some(Some(counted)) => counted,
            Some(None) => continue, // counted by an older hook, its counts unknown
        };
        let mut add = [0i64; 5];
        for slot in 0..5 {
            add[slot] = (counts[slot] - before[slot]).max(0);
        }
        if add.iter().all(|c| *c == 0) {
            continue;
        }
        let at = match bill.spend.iter().position(|s| s.model == model && s.mode == mode && s.serving == serving) {
            Some(at) => at,
            None => {
                bill.spend.push(Spend { model: model.clone(), mode, serving, ..Spend::default() });
                bill.spend.len() - 1
            }
        };
        let spend = &mut bill.spend[at];
        spend.input += add[0];
        spend.output += add[1];
        spend.cache_read += add[2];
        spend.cache_creation += add[3];
        spend.cache_creation_1h += add[4];
        if earlier.is_none() {
            spend.messages += 1;
        }
        spend.context = counts[0] + counts[2] + counts[3];
        if !id.is_empty() {
            // the id names the counts too: a later share of one message is a
            // bill of its own, never taken for the one already sent
            spend.ids.push(format!("{id}#{}", counts.iter().sum::<i64>()));
            match earlier {
                Some(at) => seen[at].1 = Some(counts),
                None => seen.push((id, Some(counts))),
            }
        }
    }
    let keep = seen.len().saturating_sub(BILL_SEEN_KEEP);
    bill.seen = seen.split_off(keep);
    Some(bill)
}

/// Where a transcript's bill marker lives: one file per transcript, so a
/// session's Stop and its subagents' SubagentStops never race on one file.
fn bill_state_path(transcript: &str, env: &Env) -> PathBuf {
    let digest = crate::report::sha256_hex(transcript.as_bytes());
    crate::config::data_dir(env).join("check").join(format!("bill-{}.json", &digest[..16]))
}

/// One model's spend as a /presence post's `usage` entry. Its `turn_id`
/// names the messages it sums, so a post sent twice is billed once.
fn usage_entry(spend: &Spend) -> Value {
    let digest = crate::report::sha256_hex(spend.ids.join("\n").as_bytes());
    let mut entry = json!({
        "turn_id": format!("bill:{}", &digest[..16]),
        "model": spend.model,
        "tokens": spend.input + spend.output,
        "input_tokens": spend.input,
        "output_tokens": spend.output,
        "cache_read_tokens": spend.cache_read,
        "cache_creation_tokens": spend.cache_creation,
        "messages": spend.messages,
        "context": spend.context,
    });
    if let Some(map) = entry.as_object_mut() {
        spend.mode.put(map);
    }
    // only what is not the standard way, so an ordinary bill reads as before
    if spend.cache_creation_1h != 0 {
        entry["cache_creation_1h_tokens"] = json!(spend.cache_creation_1h);
    }
    if spend.serving.fast {
        entry["speed"] = json!("fast");
    }
    if spend.serving.us {
        entry["inference_geo"] = json!("us");
    }
    if spend.serving.prompt_over != 0 {
        entry["prompt_over"] = json!(spend.serving.prompt_over);
    }
    entry
}

/// Holds a transcript's bill marker for one read-modify-write: a session's
/// Stop, a subagent's own SubagentStop and the sweep that bills a
/// subagent's last message (which can land after its SubagentStop has
/// read) may all bill one transcript at once, and two that read the same
/// marker would bill the same messages under two ids.
struct BillLock(Option<std::fs::File>);

impl BillLock {
    fn take(state_path: &Path) -> BillLock {
        let path = state_path.with_extension("lock");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).ok();
        BillLock(file.filter(|f| f.lock().is_ok()))
    }
}

impl Drop for BillLock {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            let _ = file.unlock();
        }
    }
}

/// What this transcript billed since its last bill, as the `usage` entries
/// a post carries, plus any an earlier post failed to deliver. The marker
/// moves BEFORE the post goes, with the entries kept as owed: a later bill
/// never counts these messages again, and an owed entry goes again with the
/// same id. `billed` clears what was owed once a post lands.
pub fn take_unbilled(transcript: &str, env: &Env) -> Vec<Value> {
    if transcript.is_empty() {
        return Vec::new();
    }
    let state_path = bill_state_path(transcript, env);
    let _held = BillLock::take(&state_path);
    let mut state = crate::config::load_json(&state_path).as_object().cloned().unwrap_or_default();
    let mut owed: Vec<Value> = state.get("owed").and_then(Value::as_array).cloned().unwrap_or_default();
    let offset = state.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let counted = state.get("seen_counts").and_then(Value::as_object).cloned().unwrap_or_default();
    let seen: Vec<Seen> = state.get("seen").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<String>>())
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let counts = counted.get(&id).and_then(Value::as_array).filter(|c| c.len() == 5).map(|c| {
                let mut out = [0i64; 5];
                for (slot, v) in c.iter().enumerate() {
                    out[slot] = v.as_i64().unwrap_or(0);
                }
                out
            });
            (id, counts)
        })
        .collect();
    let word = |key: &str| state.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let flag = |key: &str| state.get(key).and_then(Value::as_bool).unwrap_or(false);
    let last_beat = state.get("last_ts").and_then(Value::as_f64).filter(|t| *t > 0.0).map(|ts| {
        (ts, Beat::from_code(state.get("last_beat").and_then(Value::as_u64).unwrap_or(1) as u8))
    });
    let carry = Carry {
        model: word("model"),
        effort: word("effort"),
        // a marker from before hooks v88, or none yet: a subagent's first
        // bill starts in its parent's ultracode
        ultra: if state.contains_key("ultracode_on") {
            Ultra { on: flag("ultracode_on"), turn: flag("ultracode_turn") }
        } else {
            subagent_ultra(transcript, env)
        },
        last_beat,
    };
    let Some(bill) = unbilled_carry(transcript, offset, &seen, carry) else { return owed };
    if bill.offset == offset && bill.spend.is_empty() {
        return owed;
    }
    // the stretch's active time rides its first entry (the server bills an
    // entry once, by id, so a post sent again never counts it twice); a
    // stretch that spent nothing carries its time to the next
    let carried = state.get("active_carry").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
    let active_s = carried + bill.active_s;
    let mut fresh: Vec<Value> = bill.spend.iter().map(usage_entry).collect();
    if active_s > 0.0 {
        if let Some(first) = fresh.first_mut() {
            first["active_s"] = json!((active_s * 10.0).round() / 10.0);
            state.remove("active_carry");
        } else {
            state.insert("active_carry".into(), json!(active_s));
        }
    }
    owed.extend(fresh);
    if let Some((ts, beat)) = bill.last_beat {
        state.insert("last_ts".into(), json!(ts));
        state.insert("last_beat".into(), json!(beat.code()));
    }
    // at most a day of failed posts is owed; older is dropped, not grown
    let keep = owed.len().saturating_sub(64);
    let owed = owed.split_off(keep);
    state.insert("offset".into(), json!(bill.offset));
    state.insert("seen".into(), json!(bill.seen.iter().map(|(id, _)| id).collect::<Vec<_>>()));
    let counted: Map<String, Value> =
        bill.seen.iter().filter_map(|(id, c)| c.map(|c| (id.clone(), json!(c)))).collect();
    state.insert("seen_counts".into(), Value::Object(counted));
    state.insert("model".into(), json!(bill.model));
    state.insert("effort".into(), json!(bill.effort));
    state.insert("ultracode_on".into(), json!(bill.ultra.on));
    state.insert("ultracode_turn".into(), json!(bill.ultra.turn));
    state.insert("owed".into(), json!(owed));
    crate::report::save_json(&state_path, &Value::Object(state));
    owed
}

/// How often a long turn bills what it has spent so far (hooks v85+): the
/// server's agent-hours meter then sees a three-hour autonomous turn as it
/// goes, not as one lump at its Stop, and the month's hard stop can land
/// mid-turn.
pub const BILL_EVERY_S: u64 = 300;

/// Whether this transcript's bill is older than [`BILL_EVERY_S`] (or was
/// never taken): the PostToolUse hook then bills mid-turn.
pub fn bill_due(transcript: &str, env: &Env) -> bool {
    if transcript.is_empty() {
        return false;
    }
    let state = bill_state_path(transcript, env);
    let age = std::fs::metadata(&state).and_then(|m| m.modified()).ok()
        .and_then(|t| t.elapsed().ok()).map(|d| d.as_secs());
    match age {
        Some(age) => age >= BILL_EVERY_S,
        // never billed: once the transcript has something to bill
        None => std::fs::metadata(transcript).map(|m| m.len() > 0).unwrap_or(false),
    }
}

/// The post carrying `sent` landed: those entries are owed no more. Entries
/// a concurrent bill added since stay owed for its own post.
pub fn billed(transcript: &str, sent: &[Value], env: &Env) {
    let state_path = bill_state_path(transcript, env);
    let _held = BillLock::take(&state_path);
    let mut state = crate::config::load_json(&state_path).as_object().cloned().unwrap_or_default();
    let Some(owed) = state.get("owed").and_then(Value::as_array).filter(|o| !o.is_empty()) else { return };
    let id = |v: &Value| v.get("turn_id").and_then(Value::as_str).unwrap_or("").to_string();
    let gone: std::collections::HashSet<String> = sent.iter().map(id).collect();
    let left: Vec<Value> = owed.iter().filter(|v| !gone.contains(&id(v))).cloned().collect();
    state.insert("owed".into(), json!(left));
    crate::report::save_json(&state_path, &Value::Object(state));
}

/// What a transcript still owes (a bill whose post failed), without
/// reading anything new.
pub fn owed(transcript: &str, env: &Env) -> Vec<Value> {
    if transcript.is_empty() {
        return Vec::new();
    }
    let state = crate::config::load_json(&bill_state_path(transcript, env));
    state.get("owed").and_then(Value::as_array).cloned().unwrap_or_default()
}

/// Whether this session's transcript still owes a bill, or one of its
/// subagent transcripts has grown past its own bill marker: a cheap check
/// (a stat and a small JSON read per file) before anything is spawned.
/// Where a transcript that still owes a bill is remembered, so the bill
/// outlives the detached sender that could not deliver it: one small file
/// per transcript (no shared index for concurrent hooks to race on).
fn owing_dir(env: &Env) -> PathBuf {
    crate::config::data_dir(env).join("check").join("owed")
}

fn owing_path(transcript: &str, env: &Env) -> PathBuf {
    let digest = crate::report::sha256_hex(transcript.as_bytes());
    owing_dir(env).join(format!("{}.json", &digest[..16]))
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// A sender is about to try `transcript`'s bill: remember where it belongs,
/// in case it never lands.
pub fn note_owing(transcript: &str, root: &Path, session: &str, env: &Env) {
    let path = owing_path(transcript, env);
    let _ = std::fs::create_dir_all(owing_dir(env));
    let now = now_secs();
    let born = crate::config::load_json(&path).get("born").and_then(Value::as_f64).unwrap_or(now);
    crate::report::save_json(
        &path,
        &json!({"transcript": transcript, "root": root.to_string_lossy(), "session": session, "ts": now, "born": born}),
    );
}

/// Nothing is owed for `transcript` any more.
pub fn paid_up(transcript: &str, env: &Env) {
    let _ = std::fs::remove_file(owing_path(transcript, env));
}

/// A bill another sender gave up on is tried again after this long.
const OWING_RETRY_S: f64 = 300.0;
/// And dropped after this long: a week-old bill's server has long moved on.
const OWING_MAX_AGE_S: f64 = 7.0 * 86_400.0;
/// At most this many are handed out per hook run.
const OWING_PER_RUN: usize = 4;

/// Bills of other transcripts on this machine that are still owed and that
/// no sender has tried for a while: (root, transcript, session), each
/// stamped as tried now so the next hook run leaves it to this one. A
/// transcript whose bill was paid since is forgotten.
pub fn owing_elsewhere(current: &str, env: &Env) -> Vec<(PathBuf, String, String)> {
    let Ok(entries) = std::fs::read_dir(owing_dir(env)) else { return Vec::new() };
    let now = now_secs();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if out.len() >= OWING_PER_RUN {
            break;
        }
        let file = entry.path();
        let mut marker = crate::config::load_json(&file);
        let transcript = marker.get("transcript").and_then(Value::as_str).unwrap_or("").to_string();
        let tried = marker.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        let born = marker.get("born").and_then(Value::as_f64).unwrap_or(tried);
        if transcript.is_empty() || now - born > OWING_MAX_AGE_S || !anything_unbilled(&transcript, env) {
            let _ = std::fs::remove_file(&file);
            continue;
        }
        if transcript == current || now - tried < OWING_RETRY_S {
            continue;
        }
        if let Some(map) = marker.as_object_mut() {
            map.insert("born".into(), json!(born));
            map.insert("ts".into(), json!(now));
        }
        crate::report::save_json(&file, &marker);
        let root = PathBuf::from(marker.get("root").and_then(Value::as_str).unwrap_or(""));
        let session = marker.get("session").and_then(Value::as_str).unwrap_or("").to_string();
        out.push((root, transcript, session));
    }
    out
}

pub fn anything_unbilled(transcript: &str, env: &Env) -> bool {
    if !owed(transcript, env).is_empty() {
        return true;
    }
    subagent_transcripts(transcript).iter().any(|(_, file)| {
        let state = crate::config::load_json(&bill_state_path(file, env));
        let offset = state.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let owes = state.get("owed").and_then(Value::as_array).is_some_and(|o| !o.is_empty());
        owes || std::fs::metadata(file).map(|m| m.len() > offset).unwrap_or(false)
    })
}

/// Every subagent transcript of this session's transcript, with its agent
/// id: `<session>/subagents/agent-<id>.jsonl`.
pub fn subagent_transcripts(transcript: &str) -> Vec<(String, String)> {
    if transcript.is_empty() {
        return Vec::new();
    }
    let dir = Path::new(transcript).with_extension("").join("subagents");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut found: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_prefix("agent-")?.strip_suffix(".jsonl")?.to_string();
            Some((id, e.path().to_string_lossy().into_owned()))
        })
        .collect();
    found.sort();
    found
}

/// A resumed or compacted session's transcript starts billing where it is
/// now, unless it has a marker already: what came before was billed by the
/// run that wrote it (a resume may copy that history into a new file).
pub fn start_billing_here(transcript: &str, env: &Env) {
    if transcript.is_empty() {
        return;
    }
    let state_path = bill_state_path(transcript, env);
    if state_path.exists() {
        return;
    }
    let Ok(meta) = std::fs::metadata(transcript) else { return };
    crate::report::save_json(&state_path, &json!({"offset": meta.len(), "seen": [], "model": "", "owed": []}));
}

/// A subagent's own transcript: the path the harness names, else
/// `<session>/subagents/agent-<id>.jsonl` beside the session's transcript.
pub fn subagent_transcript(given: &str, transcript: &str, agent_id: &str) -> String {
    if !given.is_empty() {
        return given.to_string();
    }
    subagent_file(transcript, agent_id, "jsonl").map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

fn subagent_file(transcript: &str, agent_id: &str, ext: &str) -> Option<PathBuf> {
    if transcript.is_empty() || agent_id.is_empty() {
        return None;
    }
    let transcript = Path::new(transcript);
    // a subagent's own transcript names its siblings
    let subagents = match transcript.parent() {
        Some(dir) if dir.file_name().is_some_and(|n| n == "subagents") => dir.to_path_buf(),
        _ => transcript.with_extension("").join("subagents"),
    };
    Some(subagents.join(format!("agent-{agent_id}.{ext}")))
}

// ------------------------------------------------------------ names

/// The name a subagent was given when it was started, as Claude Code's
/// `.meta.json` keeps it: the `name` a teammate is addressed by
/// ("lattice-views", what its lead's SendMessage uses), else the
/// `description` ("Audit savings and spend reporting"), else the
/// `agentType`; "" when there is no meta. `cork message --to` takes it.
pub fn worker_name(transcript: &str, agent_id: &str) -> String {
    let Some(path) = subagent_file(transcript, agent_id, "meta.json") else { return String::new() };
    let meta = crate::config::load_json(&path);
    ["name", "description", "agentType"]
        .iter()
        .map(|key| take_chars(meta.get(*key).and_then(Value::as_str).unwrap_or("").trim(), 120))
        .find(|name| !name.is_empty())
        .unwrap_or_default()
}

/// How long a session whose Claude Code name was not found waits before the
/// registry is read again: Claude Code may write it a moment after the
/// session starts.
const NAME_RETRY_S: f64 = 120.0;

/// The name Claude Code gives the session (`cork-linear-test-e3`), the one
/// its own ListAgents and SendMessage use, from the registry it keeps of
/// its live sessions (`~/.claude/sessions/<pid>.json`: `sessionId`,
/// `name`). Once found it is kept, and the registry is read again only at
/// session start (`fresh`: a resumed session may have been renamed), or,
/// while none is found, every `NAME_RETRY_S`. "" outside Claude Code.
pub fn session_name(session: &str, fresh: bool, env: &Env) -> String {
    if session.is_empty() || crate::harness::current() != "claude" {
        return String::new();
    }
    let digest = crate::report::sha256_hex(session.as_bytes());
    let state_path = crate::config::data_dir(env).join("check").join(format!("ccname-{}.json", &digest[..16]));
    let kept = crate::config::load_json(&state_path);
    let name = kept.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let checked = kept.get("checked").and_then(Value::as_f64).unwrap_or(0.0);
    let now = now_secs();
    if !fresh && (!name.is_empty() || now - checked < NAME_RETRY_S) {
        return name;
    }
    let found = registry_name(session, env);
    let name = if found.is_empty() { name } else { found };
    crate::report::save_json(&state_path, &json!({"name": name, "checked": now}));
    name
}

/// The `name` of the registry entry whose `sessionId` is this session.
fn registry_name(session: &str, env: &Env) -> String {
    registry(env)
        .into_iter()
        .find(|(_, record)| record.get("sessionId").and_then(Value::as_str) == Some(session))
        .map(|(_, record)| take_chars(record.get("name").and_then(Value::as_str).unwrap_or("").trim(), 80))
        .unwrap_or_default()
}

/// The name Claude Code gives another session on this machine, read off its
/// registry now (not kept: asked only when a write is stopped); "" when it
/// is not one.
pub fn name_of(session: &str, env: &Env) -> String {
    if session.is_empty() {
        return String::new();
    }
    registry_name(session, env)
}

/// Where Claude Code keeps its registry of live sessions.
pub fn registry_dir(env: &Env) -> PathBuf {
    let given = crate::config::get(env, "CLAUDE_CONFIG_DIR");
    let base = if given.is_empty() { Path::new(&crate::check::home(env)).join(".claude") } else { PathBuf::from(given) };
    base.join("sessions")
}

/// Every entry of Claude Code's session registry: `(file, record)`, one per
/// live session (`sessionId`, `name`, `messagingSocketPath`, ...).
pub fn registry(env: &Env) -> Vec<(PathBuf, Value)> {
    let Ok(entries) = std::fs::read_dir(registry_dir(env)) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter(|e| e.metadata().is_ok_and(|m| m.len() <= 65_536))
        .map(|e| (e.path(), crate::config::load_json(&e.path())))
        .filter(|(_, record)| record.is_object())
        .collect()
}

// ------------------------------------------------------------ the task

fn task_path(session: &str, env: &Env) -> PathBuf {
    let digest = crate::report::sha256_hex(session.as_bytes());
    crate::config::data_dir(env).join("check").join(format!("task-{}.json", &digest[..16]))
}

/// What a prompt asks, in a few words: the line that names the task when it
/// has one ("YOUR TASK: add an issue `kind` field ..."), else its first
/// sentence that is not about who the agent is ("You are session B ...").
/// "" for a prompt that names no task: a slash command, "still?", "yes".
pub fn task_of(prompt: &str) -> String {
    static NAMED: OnceLock<Regex> = OnceLock::new();
    static ROLE: OnceLock<Regex> = OnceLock::new();
    let named = NAMED.get_or_init(|| Regex::new(r"(?i)^(?:your task|the task|task|goal|todo)\s*[:\-]\s*(.+)$").expect("task pattern"));
    let role = ROLE.get_or_init(|| {
        Regex::new(r"(?i)^(?:you are|you're|hi|hello|hey|context|background|note)\b").expect("role pattern")
    });
    let prompt = prompt.trim();
    if prompt.starts_with('/') {
        return String::new();
    }
    let clean = |line: &str| line.trim().trim_start_matches(['#', '-', '*', '>', ' ']).trim().to_string();
    let found = prompt
        .lines()
        .map(clean)
        .find_map(|line| named.captures(&line).and_then(|c| c.get(1)).map(|m| m.as_str().trim().to_string()))
        .or_else(|| {
            prompt
                .split(['\n', '.', '!', '?'])
                .map(clean)
                .find(|s| s.split_whitespace().count() >= 3 && !role.is_match(s))
        })
        .unwrap_or_default();
    let found = found.trim_end_matches([':', ',', ';']).trim();
    if found.split_whitespace().count() < 3 {
        return String::new();
    }
    if found.chars().count() <= 80 {
        return found.to_string();
    }
    let head: String = found.chars().take(79).collect();
    let cut = head.rfind(' ').filter(|&at| at > 40).unwrap_or(head.len());
    format!("{}…", head[..cut].trim_end())
}

/// The session's current task, kept from its latest prompt that named one,
/// or the item its todo list says it is on now (`note_task`). A title
/// Claude Code generated at the start of a session outlives the task: a
/// session's edits were labelled "Lattice 3D engine" while it added an
/// issue `kind` field (v104's test).
pub fn note_task(session: &str, task: &str, env: &Env) {
    let task = task.trim();
    if session.is_empty() || task.is_empty() || current_task(session, env) == task {
        return;
    }
    crate::report::save_json(&task_path(session, env), &json!({"task": take_chars(task, 120)}));
}

/// What `note_task` kept for the session; "" when nothing named a task.
pub fn current_task(session: &str, env: &Env) -> String {
    if session.is_empty() {
        return String::new();
    }
    crate::config::load_json(&task_path(session, env)).get("task").and_then(Value::as_str).unwrap_or("").to_string()
}

/// The item a todo list (Claude Code's TodoWrite) says is in progress.
pub fn todo_in_progress(tool_input: &Value) -> String {
    tool_input
        .get("todos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|t| t.get("status").and_then(Value::as_str) == Some("in_progress"))
        .and_then(|t| t.get("content").and_then(Value::as_str))
        .map(|t| take_chars(t.trim(), 120))
        .unwrap_or_default()
}

/// The session's name: the newest title the person gave it (`custom-title`),
/// else the newest the harness generated (`ai-title`). Only the bytes added
/// since the last call are read; what was found is kept beside the bill.
pub fn title(transcript: &str, env: &Env) -> String {
    if transcript.is_empty() {
        return String::new();
    }
    let digest = crate::report::sha256_hex(transcript.as_bytes());
    let state_path = crate::config::data_dir(env).join("check").join(format!("title-{}.json", &digest[..16]));
    let mut state = crate::config::load_json(&state_path).as_object().cloned().unwrap_or_default();
    let field = |state: &Map<String, Value>, key: &str| state.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let offset = state.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let (mut ai, mut custom) = (field(&state, "ai"), field(&state, "custom"));
    let named = |ai: &str, custom: &str| if custom.is_empty() { ai.to_string() } else { custom.to_string() };
    let Ok(mut file) = std::fs::File::open(transcript) else { return named(&ai, &custom) };
    let Ok(size) = file.seek(SeekFrom::End(0)) else { return named(&ai, &custom) };
    let start = if offset > size { 0 } else { offset };
    if start == size {
        return named(&ai, &custom);
    }
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.take(BILL_READ_MAX).read_to_end(&mut bytes).is_err() {
        return named(&ai, &custom);
    }
    let whole = bytes.iter().rposition(|b| *b == b'\n').map(|at| at + 1).unwrap_or(0);
    for line in String::from_utf8_lossy(&bytes[..whole]).lines() {
        if !line.contains("-title\"") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else { continue };
        let said = |key: &str| take_chars(entry.get(key).and_then(Value::as_str).unwrap_or("").trim(), 120);
        match entry.get("type").and_then(Value::as_str) {
            Some("ai-title") if !said("aiTitle").is_empty() => ai = said("aiTitle"),
            Some("custom-title") if !said("customTitle").is_empty() => custom = said("customTitle"),
            _ => {}
        }
    }
    state.insert("offset".into(), json!(start + whole as u64));
    state.insert("ai".into(), json!(ai));
    state.insert("custom".into(), json!(custom));
    crate::report::save_json(&state_path, &Value::Object(state));
    named(&ai, &custom)
}

// ------------------------------------------------------------ peer delivery
//
// Another Claude Code session on this machine may post a Cork message into
// this one through Claude Code's peer messaging (`machine::deliver_to_peers`).
// The socket never answers, so Cork keeps its copy held until this
// session's hooks find the message in the session's own transcript.

/// How long a posted message is looked for: past it, Cork's server has let
/// it go the usual way long since.
const PEER_LOOK_S: f64 = 600.0;

fn peer_state_path(session: &str, env: &Env) -> PathBuf {
    let digest = crate::report::sha256_hex(session.as_bytes());
    crate::config::data_dir(env).join("check").join(format!("peer-{}.json", &digest[..16]))
}

/// The sender posted message `id` into `session`: its hooks look for it.
pub fn note_peer_sent(session: &str, id: &str, env: &Env) {
    if session.is_empty() || id.is_empty() {
        return;
    }
    let path = peer_state_path(session, env);
    let mut state = crate::config::load_json(&path).as_object().cloned().unwrap_or_default();
    let mut pending = state.get("pending").and_then(Value::as_object).cloned().unwrap_or_default();
    pending.insert(id.to_string(), json!(now_secs()));
    state.insert("pending".into(), Value::Object(pending));
    crate::report::save_json(&path, &Value::Object(state));
}

/// The messages posted into this session that its transcript shows arrived:
/// a user turn (not a tool's output) naming `[<id>]`, as the posted text
/// does. Read only while one is looked for, and only the bytes added since
/// the last look (the first look reads the tail). Each is said once.
pub fn peer_seen(session: &str, transcript: &str, env: &Env) -> Vec<String> {
    if session.is_empty() || transcript.is_empty() {
        return Vec::new();
    }
    let path = peer_state_path(session, env);
    let Some(mut state) = crate::config::load_json(&path).as_object().cloned() else { return Vec::new() };
    let stamp = now_secs();
    let mut pending = state.get("pending").and_then(Value::as_object).cloned().unwrap_or_default();
    pending.retain(|_, at| stamp - at.as_f64().unwrap_or(0.0) < PEER_LOOK_S);
    if pending.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Vec::new();
    }
    let Ok(mut file) = std::fs::File::open(transcript) else { return Vec::new() };
    let Ok(size) = file.seek(SeekFrom::End(0)) else { return Vec::new() };
    let start = match state.get("offset").and_then(Value::as_u64) {
        Some(offset) if offset <= size => offset,
        _ => size.saturating_sub(TRANSCRIPT_TAIL_BYTES),
    };
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.take(BILL_READ_MAX).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let whole = bytes.iter().rposition(|b| *b == b'\n').map(|at| at + 1).unwrap_or(0);
    let mut found: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&bytes[..whole]).lines() {
        let named: Vec<&String> = pending.keys().filter(|id| line.contains(&format!("[{id}]"))).collect();
        if named.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if entry.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let said = entry_text(line);
        for id in named {
            if said.contains(&format!("[{id}]")) && !found.contains(id) {
                found.push(id.clone());
            }
        }
    }
    for id in &found {
        pending.remove(id);
    }
    state.insert("offset".into(), json!(start + whole as u64));
    state.insert("pending".into(), Value::Object(pending));
    crate::report::save_json(&path, &Value::Object(state));
    found
}

/// A Codex rollout line's assistant text: a `response_item` message's
/// output text, or an `agent_message` event's.
fn codex_reply(entry: &Value) -> String {
    let Some(payload) = entry.get("payload") else { return String::new() };
    match (entry.get("type").and_then(Value::as_str), payload.get("type").and_then(Value::as_str)) {
        (Some("response_item"), Some("message")) if payload.get("role").and_then(Value::as_str) == Some("assistant") => payload
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("output_text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        (Some("event_msg"), Some("agent_message")) => {
            payload.get("message").and_then(Value::as_str).unwrap_or("").to_string()
        }
        _ => String::new(),
    }
}

/// The human-readable text of one transcript line: message content when it
/// parses as JSON, else the raw line — so escapes never split a match.
fn entry_text(line: &str) -> String {
    let Ok(entry) = serde_json::from_str::<Value>(line) else {
        return line.to_string();
    };
    let content = entry.get("message").and_then(|m| m.get("content"));
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .map(|b| b.get("text").and_then(Value::as_str).unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => line.to_string(),
    }
}

/// A Codex rollout line that says its turn failed: the error message of a
/// `task_complete` (Codex writes `{"error": {"message": "You've hit your
/// usage limit ... try again at Nov 15th, 2026 3:09 PM.", "codex_error_info":
/// "usage_limit_exceeded"}}` there) or of an `error` event. None for any
/// other line.
fn codex_error(entry: &Value) -> Option<String> {
    let payload = entry.get("payload")?;
    if entry.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let said = match payload.get("type").and_then(Value::as_str) {
        Some("task_complete") | Some("turn_aborted") => payload.get("error")?.get("message")?.as_str()?,
        Some("error") | Some("stream_error") => payload.get("message")?.as_str()?,
        _ => return None,
    };
    Some(said.to_string())
}

/// A line written after a turn that went through: an assistant reply that is
/// not the harness's error notice, or a Codex turn that completed without
/// one. A limit before it was lifted; it is history, not the agent's state.
fn turn_went_through(entry: &Value) -> bool {
    if entry.get("type").and_then(Value::as_str) == Some("assistant") {
        let error = entry.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true)
            || entry.get("error").is_some_and(|e| !e.is_null());
        let has_text = entry.get("message").and_then(|m| m.get("content")).and_then(Value::as_array)
            .is_some_and(|blocks| blocks.iter().any(|b| matches!(b.get("type").and_then(Value::as_str), Some("text" | "tool_use"))));
        return !error && has_text;
    }
    if entry.get("type").and_then(Value::as_str) == Some("event_msg") {
        let payload = entry.get("payload");
        let kind = payload.and_then(|p| p.get("type")).and_then(Value::as_str);
        if kind == Some("task_complete") {
            return payload.and_then(|p| p.get("error")).is_none_or(Value::is_null);
        }
        return kind == Some("agent_message");
    }
    !codex_reply(entry).trim().is_empty()
}

fn reset_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:resets?|try\s+again)\s+(?:(?:at|in|on)\s+)?([^.\n·]{1,60})").expect("reset pattern")
    })
}

/// The harness's limit notice as the dashboard shows it after "hit its
/// usage limit · ": when it resets ("resets 4:20pm (America/Los_Angeles)",
/// "resets Nov 15th, 2026 3:09 PM"), with the limit's name when it is not
/// the plain session or usage limit ("weekly limit · resets Monday"), else
/// the notice's own words. Empty when `text` is not a limit notice: an
/// agent's prose about limits is long and says neither that a limit was
/// hit nor when it resets.
pub fn limit_from_text(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > LIMIT_MAX_ENTRY_CHARS {
        return String::new();
    }
    let lower = text.to_lowercase();
    if !LIMIT_WORDS.iter().any(|word| lower.contains(word)) {
        return String::new();
    }
    let Some(found) = limit_re().captures(text).and_then(|c| c.get(1)) else { return String::new() };
    let phrase = found.as_str().trim();
    let kind = phrase.split(|c: char| c == ' ' || c == '-').next().unwrap_or("").to_lowercase();
    let reset = reset_re()
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().trim_end_matches([',', ';', ':']).trim().to_string())
        .filter(|r| !r.is_empty());
    match reset {
        Some(when) if matches!(kind.as_str(), "session" | "usage") => take_chars(&format!("resets {when}"), 160),
        Some(when) => take_chars(&format!("{kind} limit · resets {when}"), 160),
        None => take_chars(phrase, 160),
    }
}

/// The agent hit a usage limit: the harness wrote a limit message into the
/// transcript's last few entries (Claude Code's `isApiErrorMessage` line,
/// Codex's failed `task_complete`). A limit the agent has since worked past
/// is not reported: a later reply means the limit was lifted.
pub fn limit_note(path: &str) -> String {
    let Some(text) = tail(path) else { return String::new() };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(LIMIT_TAIL_LINES);
    for line in lines[start..].iter().rev() {
        let line = line.trim();
        let parsed = serde_json::from_str::<Value>(line).ok();
        if let Some(said) = parsed.as_ref().and_then(codex_error) {
            let note = limit_from_text(&said);
            if !note.is_empty() {
                return note;
            }
            continue;
        }
        let text = entry_text(line);
        if text == line && line.starts_with('{') {
            if parsed.as_ref().is_some_and(turn_went_through) {
                return String::new();
            }
            continue; // a record with no message text (tool results, metadata): never a notice
        }
        let note = limit_from_text(&text);
        if !note.is_empty() {
            return note;
        }
        if parsed.as_ref().is_some_and(turn_went_through) {
            return String::new();
        }
    }
    String::new()
}

/// The agent's closing words for this turn: the text of the last assistant
/// message in the transcript's tail. The server keeps its first paragraph
/// as the note on whatever the turn changed (`/presence` action
/// "settled"); empty when there is none.
pub fn last_reply(path: &str) -> String {
    let Some(text) = tail(path) else { return String::new() };
    for line in text.lines().rev() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let codex = codex_reply(&entry);
        if !codex.trim().is_empty() {
            return take_chars(codex.trim(), 4000);
        }
        if entry.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let said = entry_text(line);
        if !said.trim().is_empty() && said != line {
            return take_chars(said.trim(), 4000);
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLLOUT: &str = r#"{"timestamp":"2026-10-04T06:00:00.000Z","type":"turn_context","payload":{"cwd":"/r","model":"gpt-5-codex"}}
{"timestamp":"2026-10-04T06:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix it"}]}}
{"timestamp":"2026-10-04T06:00:05.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":90000,"cached_input_tokens":80000,"output_tokens":900},"last_token_usage":{"input_tokens":30000,"cached_input_tokens":25000,"output_tokens":400,"reasoning_output_tokens":100,"total_tokens":30400}}}}
{"timestamp":"2026-10-04T06:00:06.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Parked the funnels work."}]}}
"#;

    #[test]
    fn a_calls_step_is_its_messages_four_counters_and_a_short_id() {
        let meta = turn_meta(&rollout());
        let step = step_of(&meta, "codex:2026-10-04T06:00:05.000Z").expect("a step");
        assert_eq!(
            [step["input"].clone(), step["output"].clone(), step["cache_read"].clone(), step["cache_write"].clone()],
            [json!(5000), json!(400), json!(25000), json!(0)]
        );
        assert_eq!(step["msg"].as_str().unwrap().len(), 12, "the message id, hashed and short");
        assert_eq!(step_of(&meta, "codex:2026-10-04T06:00:05.000Z"), Some(step), "the same message, the same id");
        assert!(step_of(&Map::new(), "m").is_none(), "a message that spent nothing is no step");
    }

    fn rollout() -> String {
        // one file per call: tests run in parallel, and a shared file was
        // rewritten under a reader mid-test
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("collide-rollout-{}-{n}.jsonl", std::process::id()));
        std::fs::write(&path, ROLLOUT).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_codex_rollout_bills_its_last_model_call() {
        let meta = turn_meta(&rollout());
        assert_eq!(meta["input_tokens"], 5000, "cached input is counted inside OpenAI's input_tokens");
        assert_eq!(meta["cache_read_tokens"], 25000);
        assert_eq!(meta["output_tokens"], 400);
        assert_eq!(meta["context"], 30000);
        assert_eq!(meta["model"], "gpt-5-codex");
        assert_eq!(meta["turn_id"], "codex:2026-10-04T06:00:05.000Z");
    }

    #[test]
    fn a_current_codex_rollout_bills_by_response_id() {
        let path = std::env::temp_dir().join(format!("collide-rollout-new-{}.jsonl", std::process::id()));
        std::fs::write(&path, format!("{ROLLOUT}{}\n", r#"{"timestamp":"2026-10-04T06:00:07.000Z","type":"token_usage_record","payload":{"turn_id":"t1","response_id":"resp_9","usage":{"input_tokens":40000,"cached_input_tokens":30000,"cache_write_input_tokens":2000,"output_tokens":700,"reasoning_output_tokens":0,"total_tokens":40700}}}"#)).unwrap();
        let meta = turn_meta(&path.to_string_lossy());
        assert_eq!(meta["turn_id"], "codex:resp_9");
        assert_eq!(meta["input_tokens"], 8000);
        assert_eq!(meta["cache_creation_tokens"], 2000);
        assert_eq!(meta["model"], "gpt-5-codex");
    }

    #[test]
    fn a_codex_rollout_gives_its_closing_reply() {
        assert_eq!(last_reply(&rollout()), "Parked the funnels work.");
    }

    /// A scratch directory of its own per test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("collide-bill-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn assistant(id: &str, model: &str, usage: (i64, i64, i64, i64), block: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"u-{id}-{block}","message":{{"id":"{id}","model":"{model}","role":"assistant","content":[{{"type":"{block}"}}],"usage":{{"input_tokens":{},"output_tokens":{},"cache_read_input_tokens":{},"cache_creation_input_tokens":{}}}}}}}"#,
            usage.0, usage.1, usage.2, usage.3
        ) + "\n"
    }

    /// A timed line as Claude Code writes it: `kind` user|assistant, the
    /// content block's type, the stop reason, and usage for an assistant.
    fn timed(ts: &str, kind: &str, block: &str, stop: &str, id: &str) -> String {
        let usage = if kind == "assistant" {
            format!(r#","usage":{{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"id":"{id}","model":"claude-opus-5-5""#)
        } else {
            String::new()
        };
        let stop = if stop.is_empty() { String::new() } else { format!(r#","stop_reason":"{stop}""#) };
        format!(r#"{{"type":"{kind}","timestamp":"{ts}","uuid":"u-{ts}-{block}","message":{{"role":"{kind}","content":[{{"type":"{block}"}}]{stop}{usage}}}}}"#) + "\n"
    }

    #[test]
    fn active_time_is_the_agents_not_the_persons() {
        let dir = scratch("active");
        let path = dir.join("s.jsonl");
        let mut text = String::new();
        text += &timed("2026-10-03T11:00:00.000Z", "user", "text", "", "");
        text += &timed("2026-10-03T11:00:02.000Z", "assistant", "tool_use", "tool_use", "m1"); // 2s thinking
        text += &timed("2026-10-03T11:00:30.000Z", "user", "tool_result", "", ""); // 28s tool
        text += &timed("2026-10-03T11:00:35.000Z", "assistant", "text", "end_turn", "m2"); // 5s answer
        // an hour away from the keyboard: the person's time
        text += &timed("2026-10-03T12:00:35.000Z", "user", "text", "", "");
        text += &timed("2026-10-03T12:00:40.000Z", "assistant", "text", "end_turn", "m3"); // 5s
        std::fs::write(&path, &text).unwrap();
        let bill = unbilled(&path.to_string_lossy(), 0, &[], "").unwrap();
        assert_eq!(bill.active_s, 12.0, "2 + 5 + 5: never the tool's 28 s, never the idle hour");
        assert_eq!(bill.last_beat, Some((epoch_of("2026-10-03T12:00:40.000Z").unwrap(), Beat::TurnEnd)));
        // the next bill counts on from the last line it saw
        let mut more = text.clone();
        more += &timed("2026-10-03T12:00:50.000Z", "user", "text", "", ""); // after a turn end: idle
        more += &timed("2026-10-03T12:30:50.000Z", "assistant", "tool_use", "tool_use", "m4"); // over the cap: 600
        std::fs::write(&path, &more).unwrap();
        let next = unbilled_from(&path.to_string_lossy(), bill.offset, &bill.seen, "", bill.last_beat).unwrap();
        assert_eq!(next.active_s, ACTIVE_GAP_MAX_S, "a gap over the cap is the cap");
        // through take_unbilled the time rides the first entry, once
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        let path_str = path.to_string_lossy().into_owned();
        std::fs::write(&path, &text).unwrap();
        let entries = take_unbilled(&path_str, &env);
        assert_eq!(entries[0]["active_s"], json!(12.0));
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn transcript_timestamps_read_as_epoch_seconds() {
        assert_eq!(epoch_of("1970-01-01T00:00:00.000Z"), Some(0.0));
        assert_eq!(epoch_of("2026-10-03T11:04:17.562Z"), Some(1_791_025_457.562));
        assert_eq!(epoch_of("2026-10-03T11:04:17Z"), Some(1_791_025_457.0));
        assert_eq!(epoch_of("not a time"), None);
        assert_eq!(beat_of(r#"{"type":"user","timestamp":"2026-10-03T11:04:17Z","message":{"content":"hi"}}"#).map(|b| b.1), Some(Beat::Prompt));
        assert_eq!(beat_of(r#"{"type":"assistant","timestamp":"2026-10-03T11:04:18Z","message":{"content":[{"type":"tool_use"}],"stop_reason":"tool_use"}}"#).map(|b| b.1), Some(Beat::Call));
        assert_eq!(beat_of(r#"{"type":"user","timestamp":"2026-10-03T11:04:19Z","message":{"content":[{"type":"tool_result"}]}}"#).map(|b| b.1), Some(Beat::Result));
        assert_eq!(active_seconds((0.0, Beat::Call), (300.0, Beat::Result)), 0.0, "a tool running is not the agent's time");
        assert_eq!(active_seconds((0.0, Beat::Result), (4.0, Beat::Work)), 4.0, "thinking after the result is");
        assert_eq!(beat_of(r#"{"type":"attachment","timestamp":"2026-10-03T11:04:17Z"}"#), None);
    }

    #[test]
    fn every_message_is_billed_once_per_model_however_it_ended() {
        let dir = scratch("sum");
        let path = dir.join("s.jsonl");
        let mut text = String::new();
        // one API message written as three lines (thinking, text, tool_use): one message
        for block in ["thinking", "text", "tool_use"] {
            text += &assistant("msg_1", "claude-opus-5-5", (10, 100, 1000, 50), block);
        }
        text += r#"{"type":"user","message":{"role":"user","content":"tool result"}}"#;
        text += "\n";
        // a message that ended in plain text, never followed by an edit
        text += &assistant("msg_2", "claude-opus-5-5", (5, 40, 2000, 0), "text");
        text += &assistant("msg_3", "claude-haiku-4-5", (1, 2, 3, 4), "text");
        // a synthetic message spent nothing
        text += &assistant("msg_4", "<synthetic>", (0, 0, 0, 0), "text");
        std::fs::write(&path, &text).unwrap();
        let bill = unbilled(&path.to_string_lossy(), 0, &[], "").unwrap();
        assert_eq!(bill.offset, text.len() as u64);
        assert_eq!(bill.spend.len(), 2, "{:?}", bill.spend);
        let opus = &bill.spend[0];
        assert_eq!((opus.model.as_str(), opus.messages), ("claude-opus-5-5", 2));
        assert_eq!((opus.input, opus.output, opus.cache_read, opus.cache_creation), (15, 140, 3000, 50));
        assert_eq!(opus.context, 5 + 2000, "the newest message's context");
        assert_eq!(opus.ids, vec!["msg_1#1160", "msg_2#2045"], "each id names the counts it billed");
        assert_eq!(bill.spend[1].messages, 1);
    }

    #[test]
    fn a_bill_starts_at_its_marker_and_never_counts_a_message_twice() {
        let dir = scratch("marker");
        let path = dir.join("s.jsonl");
        let first = assistant("msg_1", "claude-opus-5-5", (1, 1, 1, 1), "text");
        // the second message's first line is in, its last still being written
        let partial = assistant("msg_2", "claude-opus-5-5", (2, 2, 2, 2), "text");
        std::fs::write(&path, format!("{first}{}{}", partial, &partial[..20])).unwrap();
        let bill = unbilled(&path.to_string_lossy(), 0, &[], "").unwrap();
        assert_eq!(bill.spend[0].messages, 2);
        assert_eq!(bill.offset, (first.len() + partial.len()) as u64, "a half-written line waits");
        // the same message's next line lands after the marker: already counted
        std::fs::write(&path, format!("{first}{partial}{partial}")).unwrap();
        let next = unbilled(&path.to_string_lossy(), bill.offset, &bill.seen, "").unwrap();
        assert!(next.spend.is_empty(), "{:?}", next.spend);
    }

    #[test]
    fn a_stop_bills_what_is_new_and_an_owed_post_goes_again_with_the_same_id() {
        let dir = scratch("owed");
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        let path = dir.join("s.jsonl");
        let path_str = path.to_string_lossy().into_owned();
        std::fs::write(&path, assistant("msg_1", "claude-opus-5-5", (1, 2, 3, 4), "text")).unwrap();
        let first = take_unbilled(&path_str, &env);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["input_tokens"], 1);
        assert_eq!(first[0]["tokens"], 3);
        assert_eq!(first[0]["messages"], 1);
        // the post failed: the next bill sends it again, unchanged, beside what is new
        let mut more = std::fs::read_to_string(&path).unwrap();
        more += &assistant("msg_2", "claude-opus-5-5", (10, 20, 30, 40), "text");
        std::fs::write(&path, more).unwrap();
        let second = take_unbilled(&path_str, &env);
        assert_eq!(second.len(), 2);
        assert_eq!(second[0], first[0], "same entry, same id: the server bills it once");
        assert_eq!(second[1]["output_tokens"], 20);
        // landed: nothing owed, nothing new
        billed(&path_str, &second, &env);
        assert!(take_unbilled(&path_str, &env).is_empty());
    }

    #[test]
    fn a_resumed_transcript_bills_only_what_comes_after() {
        let dir = scratch("resume");
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        let path = dir.join("s.jsonl");
        let path_str = path.to_string_lossy().into_owned();
        std::fs::write(&path, assistant("msg_old", "claude-opus-5-5", (9, 9, 9, 9), "text")).unwrap();
        start_billing_here(&path_str, &env);
        let mut more = std::fs::read_to_string(&path).unwrap();
        more += &assistant("msg_new", "claude-opus-5-5", (1, 1, 1, 1), "text");
        std::fs::write(&path, more).unwrap();
        let usage = take_unbilled(&path_str, &env);
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0]["input_tokens"], 1);
        // a marker that exists is never moved by a later resume
        start_billing_here(&path_str, &env);
        billed(&path_str, &usage, &env);
        assert!(take_unbilled(&path_str, &env).is_empty());
    }

    #[test]
    fn a_codex_rollout_is_billed_by_call_with_its_model() {
        let dir = scratch("codex");
        let path = dir.join("rollout.jsonl");
        // the same token_count event logged twice is one call
        let repeat = ROLLOUT.lines().nth(2).unwrap();
        std::fs::write(&path, format!("{ROLLOUT}{repeat}\n")).unwrap();
        let bill = unbilled(&path.to_string_lossy(), 0, &[], "").unwrap();
        assert_eq!(bill.spend.len(), 1);
        let spend = &bill.spend[0];
        assert_eq!((spend.model.as_str(), spend.messages), ("gpt-5-codex", 1));
        assert_eq!((spend.input, spend.cache_read, spend.output), (5000, 25000, 400));
        // a later bill that starts past the turn_context keeps the model
        let more = r#"{"timestamp":"2026-10-04T06:00:09.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":99999},"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10}}}}"#;
        std::fs::write(&path, format!("{ROLLOUT}{repeat}\n{more}\n")).unwrap();
        let next = unbilled(&path.to_string_lossy(), bill.offset, &bill.seen, &bill.model).unwrap();
        assert_eq!(next.spend[0].model, "gpt-5-codex");
        assert_eq!(next.spend[0].input, 100);
    }

    #[test]
    fn a_subagent_is_billed_from_its_own_transcript_and_named_from_its_meta() {
        let dir = scratch("subagent");
        let parent = dir.join("sess.jsonl");
        std::fs::write(&parent, "").unwrap();
        let subagents = dir.join("sess").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(subagents.join("agent-a1.jsonl"), assistant("msg_s", "claude-opus-5-5", (3, 4, 5, 6), "text")).unwrap();
        std::fs::write(subagents.join("agent-a1.meta.json"),
            r#"{"agentType":"Explore","description":"Audit savings and spend reporting","toolUseId":"t"}"#).unwrap();
        let parent_str = parent.to_string_lossy().into_owned();
        // the hook input's own path wins; without it, the file beside the session's
        let found = subagent_transcript("", &parent_str, "a1");
        assert_eq!(found, subagents.join("agent-a1.jsonl").to_string_lossy());
        assert_eq!(subagent_transcript("/given.jsonl", &parent_str, "a1"), "/given.jsonl");
        let bill = unbilled(&found, 0, &[], "").unwrap();
        assert_eq!(bill.spend[0].output, 4);
        assert_eq!(worker_name(&parent_str, "a1"), "Audit savings and spend reporting");
        // asked from the subagent's own transcript, the sibling meta answers
        assert_eq!(worker_name(&found, "a1"), "Audit savings and spend reporting");
        assert_eq!(worker_name(&parent_str, "nobody"), "");
        // a teammate by the name its lead gave it, before its description
        std::fs::write(subagents.join("agent-alattice-views-59e1.meta.json"),
            r#"{"agentType":"lattice-views","description":"Build Lattice frontend views","name":"lattice-views","taskKind":"in_process_teammate"}"#).unwrap();
        assert_eq!(worker_name(&parent_str, "alattice-views-59e1"), "lattice-views");
        std::fs::write(subagents.join("agent-a2.meta.json"), r#"{"agentType":"Explore"}"#).unwrap();
        assert_eq!(worker_name(&parent_str, "a2"), "Explore");
    }

    #[test]
    fn the_title_is_the_newest_a_person_gave_else_the_newest_generated() {
        let dir = scratch("title");
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        let path = dir.join("s.jsonl");
        let path_str = path.to_string_lossy().into_owned();
        assert_eq!(title(&path_str, &env), "", "no transcript yet");
        let mut text = String::from("{\"type\":\"ai-title\",\"aiTitle\":\"First guess\",\"sessionId\":\"s\"}\n");
        text += "{\"type\":\"ai-title\",\"aiTitle\":\"Fix spend accounting\",\"sessionId\":\"s\"}\n";
        std::fs::write(&path, &text).unwrap();
        assert_eq!(title(&path_str, &env), "Fix spend accounting");
        // read once: the cache answers without the bytes already seen
        assert_eq!(title(&path_str, &env), "Fix spend accounting");
        text += "{\"type\":\"custom-title\",\"customTitle\":\"Billing rework\",\"sessionId\":\"s\"}\n";
        text += "{\"type\":\"ai-title\",\"aiTitle\":\"Later guess\",\"sessionId\":\"s\"}\n";
        std::fs::write(&path, &text).unwrap();
        assert_eq!(title(&path_str, &env), "Billing rework", "a person's name beats a generated one");
    }

    /// A session's work is labelled by what its latest prompt asks, not by a
    /// title generated for an earlier task (v104's test: "Lattice 3D engine"
    /// on edits that added an issue `kind` field).
    #[test]
    fn the_task_is_what_the_latest_prompt_asks() {
        let spec = "You are session B in a deliberate Cork coordination test in the Lattice repo (this directory: Rust axum \
backend in src/). Another Claude session is AT THE SAME TIME adding an issue `kind` field.\n\nYOUR TASK: add an issue \
`start_date` field (Option<String>, YYYY-MM-DD), end to end:\n1. src/db.rs: column on issues";
        assert_eq!(task_of(spec), "add an issue `start_date` field (Option<String>, YYYY-MM-DD), end to end");
        assert_eq!(task_of("You are the lead. Add a kind field to issues: bug, feature, chore. Keep it small."),
            "Add a kind field to issues: bug, feature, chore");
        let long = "Rework the whole board view so that every column scrolls on its own and the header stays put while it does";
        let cut = task_of(long);
        assert!(cut.ends_with('…') && cut.chars().count() <= 80 && long.starts_with(cut.trim_end_matches('…')), "{cut}");
        // a prompt that names no task leaves the one before
        for none in ["still?", "yes", "/compact", "ok go"] {
            assert_eq!(task_of(none), "", "{none}");
        }
        let dir = scratch("task");
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        note_task("s1", &task_of(spec), &env);
        note_task("s1", &task_of("still?"), &env);
        assert!(current_task("s1", &env).starts_with("add an issue `start_date` field"));
        // the todo list's item in progress is more precise still
        let todos = json!({"todos": [{"content": "Add the column", "status": "completed"},
                                     {"content": "Validate start_date against due_date", "status": "in_progress"}]});
        note_task("s1", &todo_in_progress(&todos), &env);
        assert_eq!(current_task("s1", &env), "Validate start_date against due_date");
    }

    /// The name Claude Code's ListAgents and SendMessage use for a session,
    /// read off its registry once, then kept: a later call reads no file.
    #[test]
    fn the_claude_code_name_is_read_off_its_registry_once() {
        let dir = scratch("ccname");
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        env.insert("CLAUDE_CONFIG_DIR".into(), dir.join("claude").to_string_lossy().into_owned());
        let sessions = dir.join("claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("5038.json"), r#"{"pid":5038,"sessionId":"88831a49","cwd":"/r","name":"cork-linear-test-0e","nameSource":"derived","status":"idle"}"#).unwrap();
        std::fs::write(sessions.join("4572.json"), r#"{"pid":4572,"sessionId":"191ff2f3","cwd":"/r","name":"cork-linear-test-e3","nameSource":"derived","status":"idle"}"#).unwrap();
        std::fs::write(sessions.join("4572.abc.key"), "not json").unwrap();
        assert_eq!(session_name("191ff2f3", false, &env), "cork-linear-test-e3");
        // kept: the registry is not read again until the session starts again
        std::fs::write(sessions.join("4572.json"), r#"{"sessionId":"191ff2f3","name":"renamed"}"#).unwrap();
        assert_eq!(session_name("191ff2f3", false, &env), "cork-linear-test-e3");
        assert_eq!(session_name("191ff2f3", true, &env), "renamed");
        // a session the registry does not know has no name, and is not looked up on every call
        assert_eq!(session_name("nope", false, &env), "");
        std::fs::write(sessions.join("9.json"), r#"{"sessionId":"nope","name":"late"}"#).unwrap();
        assert_eq!(session_name("nope", false, &env), "", "asked again only after a while");
        assert_eq!(session_name("nope", true, &env), "late");
    }

    /// A message posted into this session through Claude Code is found in
    /// its transcript as a user turn naming its id, and only there (a tool's
    /// output that happens to name it is not the message arriving); each
    /// once, and the transcript is read only while one is looked for.
    #[test]
    fn a_posted_message_is_found_in_the_transcript_as_a_user_turn() {
        let dir = scratch("peerseen");
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        let path = dir.join("s.jsonl");
        let transcript = path.to_string_lossy().into_owned();
        let user = |text: &str| json!({"type": "user", "message": {"role": "user", "content": text}}).to_string() + "\n";
        let tool = |text: &str| json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": text}]}}).to_string() + "\n";
        std::fs::write(&path, user("earlier prompt") + &tool("grep found [m2] in a log")).unwrap();
        // nothing looked for: nothing read, nothing said
        assert!(peer_seen("s2", &transcript, &env).is_empty());
        note_peer_sent("s2", "m1", &env);
        note_peer_sent("s2", "m2", &env);
        assert!(peer_seen("s2", &transcript, &env).is_empty(), "not arrived yet; a tool's output is not it");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text += &user("Cork: A message from teammates' agents (not your user)...\n[m1] kevin@x.com, just now: the API is up");
        std::fs::write(&path, &text).unwrap();
        assert_eq!(peer_seen("s2", &transcript, &env), vec!["m1".to_string()]);
        assert!(peer_seen("s2", &transcript, &env).is_empty(), "said once");
        // another session's look is its own
        assert!(peer_seen("s3", &transcript, &env).is_empty());
    }

    // ---- real samples (tests/fixtures/billing): the usage lines of real
    // sessions run on 2026-10-17, text and paths stripped, checked against
    // what the provider's own client reported for the same session

    const MAIN: &str = include_str!("../tests/fixtures/billing/claude_main_with_subagent.jsonl");
    const SUBAGENT: &str = include_str!("../tests/fixtures/billing/claude_subagent.jsonl");
    const RESUMED: &str = include_str!("../tests/fixtures/billing/claude_resumed.jsonl");
    const CODEX: &str = include_str!("../tests/fixtures/billing/codex_exec_rollout.jsonl");

    fn totals(bill: &Bill) -> [i64; 5] {
        bill.spend.iter().fold([0; 5], |t, s| {
            [t[0] + s.input, t[1] + s.output, t[2] + s.cache_read, t[3] + s.cache_creation, t[4] + s.cache_creation_1h]
        })
    }

    #[test]
    fn a_real_claude_code_session_bills_what_claude_reported_with_its_hour_long_writes() {
        // `claude -p --output-format json` modelUsage, claude-sonnet-5: in 4, out 524,
        // cache read 54,176, cache write 6,138 (every one an hour-long write)
        let dir = scratch("real-main");
        let path = dir.join("main.jsonl");
        std::fs::write(&path, MAIN).unwrap();
        let bill = unbilled(&path.to_string_lossy(), 0, &[], "").unwrap();
        assert_eq!(bill.spend.len(), 1);
        assert_eq!(bill.spend[0].model, "claude-sonnet-5");
        assert_eq!(bill.spend[0].messages, 2, "a message written as three lines is one message");
        assert_eq!(totals(&bill), [4, 524, 54_176, 6_138, 6_138]);
        let entry = usage_entry(&bill.spend[0]);
        assert_eq!(entry["cache_creation_1h_tokens"], 6_138);
        assert!(entry.get("speed").is_none() && entry.get("inference_geo").is_none() && entry.get("prompt_over").is_none(),
                "a standard bill carries nothing extra: {entry}");
    }

    #[test]
    fn a_subagents_last_message_written_after_its_stop_is_billed_by_the_sweep() {
        // the subagent's modelUsage, claude-haiku-4-5-20251001: in 18, out 339,
        // cache read 12,143, cache write 13,216 (all 5-minute writes). Its
        // SubagentStop read the file mid-message (the first line of message 1
        // carried output 6 of 280) and before message 2 was written.
        let dir = scratch("real-sub");
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        let parent = dir.join("sess.jsonl");
        std::fs::write(&parent, MAIN).unwrap();
        let subagents = dir.join("sess").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let file = subagents.join("agent-ad1598a4691b02510.jsonl");
        let file_str = file.to_string_lossy().into_owned();
        let lines: Vec<&str> = SUBAGENT.split_inclusive('\n').collect();
        std::fs::write(&file, lines[..2].concat()).unwrap();
        let at_stop = take_unbilled(&file_str, &env);
        assert_eq!(at_stop.len(), 1);
        assert_eq!((at_stop[0]["output_tokens"].as_i64(), at_stop[0]["messages"].as_i64()), (Some(6), Some(1)));
        billed(&file_str, &at_stop, &env);
        // the rest lands; the parent's Stop finds it through the session's transcript
        std::fs::write(&file, SUBAGENT).unwrap();
        let parent_str = parent.to_string_lossy().into_owned();
        assert!(anything_unbilled(&parent_str, &env));
        let found = subagent_transcripts(&parent_str);
        assert_eq!(found, vec![("ad1598a4691b02510".to_string(), file_str.clone())]);
        let swept = take_unbilled(&file_str, &env);
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0]["messages"], 1, "message 1 was counted once; only message 2 is new");
        assert_ne!(swept[0]["turn_id"], at_stop[0]["turn_id"], "the rest of a message is a bill of its own");
        let sum = |field: &str| at_stop[0][field].as_i64().unwrap() + swept[0][field].as_i64().unwrap();
        assert_eq!([sum("input_tokens"), sum("output_tokens"), sum("cache_read_tokens"), sum("cache_creation_tokens")],
                   [18, 339, 12_143, 13_216]);
        assert!(swept[0].get("cache_creation_1h_tokens").is_none());
        billed(&file_str, &swept, &env);
        assert!(!anything_unbilled(&parent_str, &env));
    }

    #[test]
    fn a_resumed_session_bills_the_resume_alone() {
        // the same session id resumed: Claude Code appends to the same file.
        // First run's modelUsage: 6 / 366 / 75,865 / 10,104; the resumed run's
        // own `usage`: 4 / 348 / 59,465 / 1,952 (its modelUsage is cumulative)
        let dir = scratch("real-resume");
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        let path = dir.join("s.jsonl");
        let path_str = path.to_string_lossy().into_owned();
        let lines: Vec<&str> = RESUMED.split_inclusive('\n').collect();
        std::fs::write(&path, lines[..7].concat()).unwrap();
        let first = take_unbilled(&path_str, &env);
        billed(&path_str, &first, &env);
        let field = |e: &Value, f: &str| e[f].as_i64().unwrap();
        let counts = |e: &Value| [field(e, "input_tokens"), field(e, "output_tokens"), field(e, "cache_read_tokens"), field(e, "cache_creation_tokens")];
        assert_eq!(counts(&first[0]), [6, 366, 75_865, 10_104]);
        // the resume's SessionStart leaves a marker that exists alone
        start_billing_here(&path_str, &env);
        std::fs::write(&path, RESUMED).unwrap();
        let resumed = take_unbilled(&path_str, &env);
        assert_eq!(resumed.len(), 1);
        assert_eq!(counts(&resumed[0]), [4, 348, 59_465, 1_952]);
        assert_eq!(field(&resumed[0], "cache_creation_1h_tokens"), 1_952);
    }

    #[test]
    fn a_real_codex_exec_rollout_bills_each_response_once_reasoning_inside_output() {
        // `codex exec --json` turn.completed: input 87,278 of which cached 57,984,
        // output 384 of which reasoning 115; gpt-6-luna, five responses, each
        // logged as a token_usage_record AND a token_count event
        let dir = scratch("real-codex");
        let path = dir.join("rollout.jsonl");
        std::fs::write(&path, CODEX).unwrap();
        let bill = unbilled(&path.to_string_lossy(), 0, &[], "").unwrap();
        assert_eq!(bill.spend.len(), 1);
        assert_eq!((bill.spend[0].model.as_str(), bill.spend[0].messages), ("gpt-6-luna", 5));
        assert_eq!(totals(&bill), [87_278 - 57_984, 384, 57_984, 0, 0]);
        // read again from the start with everything seen: nothing twice
        let again = unbilled(&path.to_string_lossy(), 0, &bill.seen, &bill.model).unwrap();
        assert!(again.spend.is_empty(), "{:?}", again.spend);
    }

    #[test]
    fn fast_us_only_and_long_prompts_are_billed_apart() {
        let dir = scratch("serving");
        let path = dir.join("s.jsonl");
        let line = |id: &str, speed: &str, geo: &str, read: i64| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"{id}","model":"claude-opus-5-5","usage":{{"input_tokens":1,"output_tokens":10,"cache_read_input_tokens":{read},"cache_creation_input_tokens":100,"cache_creation":{{"ephemeral_5m_input_tokens":40,"ephemeral_1h_input_tokens":60}},"speed":"{speed}","inference_geo":"{geo}"}}}}}}"#
            ) + "\n"
        };
        let text = line("a", "standard", "global", 1000) + &line("b", "fast", "global", 1000)
            + &line("c", "standard", "us", 1000) + &line("d", "standard", "global", 150_000)
            + &line("e", "standard", "global", 300_000);
        std::fs::write(&path, text).unwrap();
        let bill = unbilled(&path.to_string_lossy(), 0, &[], "").unwrap();
        let entries: Vec<Value> = bill.spend.iter().map(usage_entry).collect();
        assert_eq!(entries.len(), 5, "{entries:?}");
        assert_eq!(entries[0]["cache_creation_1h_tokens"], 60);
        assert_eq!(entries[1]["speed"], "fast");
        assert_eq!(entries[2]["inference_geo"], "us");
        assert_eq!(entries[3]["prompt_over"], 100_000);
        assert_eq!(entries[4]["prompt_over"], 272_000);
    }

    fn write_lines(name: &str, lines: &[&str]) -> String {
        let path = std::env::temp_dir().join(format!("collide-limit-{}-{name}.jsonl", std::process::id()));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path.to_string_lossy().into_owned()
    }

    /// The harnesses' own limit notices, word for word from Study 9's runs,
    /// as the dashboard shows them after "hit its usage limit · ".
    #[test]
    fn a_limit_notice_reads_as_when_it_resets() {
        assert_eq!(limit_from_text("You've hit your session limit · resets 4:20pm (America/Los_Angeles)"),
                   "resets 4:20pm (America/Los_Angeles)");
        assert_eq!(limit_from_text("You\u{2019}ve hit your usage limit. Upgrade to Plus to continue using Codex \
                                    (https://chatgpt.com/explore/plus), or try again at Nov 15th, 2026 3:09 PM."),
                   "resets Nov 15th, 2026 3:09 PM");
        assert_eq!(limit_from_text("You've reached your weekly limit · resets Mon 9:00am"), "weekly limit · resets Mon 9:00am");
        assert_eq!(limit_from_text("Claude usage limit reached"), "usage limit reached");
        // prose about limits is not a notice
        assert_eq!(limit_from_text("Added rate-limiting to the API; the usage limit is read from the plan table."), "");
    }

    /// Claude Code's limit line (an `isApiErrorMessage` assistant entry) and
    /// Codex's failed `task_complete` are both read off the transcript; a
    /// limit the agent has since worked past is not.
    #[test]
    fn the_limit_is_read_off_both_harnesses_transcripts_until_the_agent_works_again() {
        let claude = r#"{"type":"assistant","message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"You've hit your session limit · resets 4:20pm (America/Los_Angeles)"}]},"error":"rate_limit","isApiErrorMessage":true}"#;
        let path = write_lines("claude", &[r#"{"type":"user","message":{"content":"go"}}"#, claude, r#"{"type":"system","subtype":"turn_duration"}"#]);
        assert_eq!(limit_note(&path), "resets 4:20pm (America/Los_Angeles)");
        let reply = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Back at it: the tests pass."}]}}"#;
        let later = write_lines("claude-later", &[claude, r#"{"type":"user","message":{"content":"continue"}}"#, reply]);
        assert_eq!(limit_note(&later), "", "the limit was lifted: the agent replied since");

        let codex = r#"{"timestamp":"2026-10-17T19:24:31.555Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":null,"error":{"message":"You’ve hit your usage limit. Upgrade to Plus to continue using Codex (https://chatgpt.com/explore/plus), or try again at Nov 15th, 2026 3:09 PM.","codex_error_info":"usage_limit_exceeded"}}}"#;
        let count = r#"{"timestamp":"2026-10-17T19:25:24.878Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"premium"}}}"#;
        let rollout = write_lines("codex", &[r#"{"type":"turn_context","payload":{"cwd":"/r","model":"gpt-5.6-terra"}}"#, count, codex]);
        assert_eq!(limit_note(&rollout), "resets Nov 15th, 2026 3:09 PM");
        let done = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t2","last_agent_message":"done","error":null}}"#;
        let lifted = write_lines("codex-later", &[codex, done]);
        assert_eq!(limit_note(&lifted), "");
        for p in [path, later, rollout, lifted] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// A bill the server never took is remembered beside its session, handed
    /// to the next hook run once its sender has had a while, and forgotten
    /// once paid.
    #[test]
    fn an_unpaid_bill_is_remembered_until_it_is_paid() {
        let home = std::env::temp_dir().join(format!("collide-owing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut env = Env::default();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        env.insert("CORK_HOME".into(), home.to_string_lossy().to_string());
        let transcript = home.join("s1.jsonl");
        let line = r#"{"type":"assistant","message":{"id":"m1","model":"claude-sonnet-5","usage":{"input_tokens":3,"output_tokens":9}}}"#;
        std::fs::write(&transcript, format!("{line}\n")).unwrap();
        let t = transcript.to_string_lossy().to_string();
        let owed = take_unbilled(&t, &env);
        assert_eq!(owed.len(), 1, "taken, not yet delivered");
        note_owing(&t, Path::new("/repo"), "s1", &env);
        // its own hook run never hands it out, nor one inside the retry wait
        assert!(owing_elsewhere(&t, &env).is_empty());
        assert!(owing_elsewhere("/other.jsonl", &env).is_empty());
        // a while later another session's hook run picks it up, once
        let marker = owing_path(&t, &env);
        let mut aged = crate::config::load_json(&marker);
        aged["ts"] = json!(now_secs() - OWING_RETRY_S - 1.0);
        crate::report::save_json(&marker, &aged);
        let handed = owing_elsewhere("/other.jsonl", &env);
        assert_eq!(handed, vec![(PathBuf::from("/repo"), t.clone(), "s1".to_string())]);
        assert!(owing_elsewhere("/other.jsonl", &env).is_empty(), "stamped as tried");
        // paid: nothing owed, and the marker goes at the next look
        billed(&t, &owed, &env);
        crate::report::save_json(&marker, &aged);
        assert!(owing_elsewhere("/other.jsonl", &env).is_empty());
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    // ------------------------------------------------ effort and ultracode

    fn claude_line(id: &str, effort: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"u-{id}",{effort}"message":{{"id":"{id}","model":"claude-opus-5-5","role":"assistant","content":[{{"type":"text"}}],"usage":{{"input_tokens":1,"output_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
        )
    }

    const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"ship it"},"uuid":"p1"}"#;
    const META_USER: &str = r#"{"type":"user","message":{"role":"user","content":"<command-name>workflow-authoring</command-name>"},"isMeta":true}"#;
    const KEYWORD: &str = r#"{"parentUuid":"p1","isSidechain":false,"attachment":{"type":"workflow_keyword_request"},"type":"attachment","uuid":"k1"}"#;
    const ENTER: &str = r#"{"parentUuid":"p1","isSidechain":false,"attachment":{"type":"ultra_effort_enter","reminderType":"full"},"type":"attachment","uuid":"e1"}"#;
    const EXIT: &str = r#"{"parentUuid":"p1","isSidechain":false,"attachment":{"type":"ultra_effort_exit"},"type":"attachment","uuid":"x1"}"#;

    #[test]
    fn claude_codes_effort_is_the_turns_own_else_the_sessions() {
        let both = claude_line("m1", r#""effort":"high","perTurnEffort":"xhigh","#);
        let meta = turn_meta(&write_lines("effort-both", &[&both]));
        assert_eq!(meta["effort"], "xhigh", "perTurnEffort first");
        let session = claude_line("m2", r#""effort":"medium","perTurnEffort":null,"#);
        let meta = turn_meta(&write_lines("effort-session", &[&session]));
        assert_eq!(meta["effort"], "medium", "effort when the turn names none");
        // and the bill's share names it beside the model
        let path = write_lines("effort-bill", &[&both, &session]);
        let bill = unbilled(&path, 0, &[], "").unwrap();
        let efforts: Vec<&str> = bill.spend.iter().map(|s| s.mode.effort.as_str()).collect();
        assert_eq!(efforts, ["xhigh", "medium"], "one share per effort");
        let entry = usage_entry(&bill.spend[0]);
        assert_eq!((entry["model"].as_str(), entry["effort"].as_str()), (Some("claude-opus-5-5"), Some("xhigh")));
        assert!(entry.get("ultracode").is_none(), "off is never sent");
    }

    #[test]
    fn an_unknown_effort_is_left_out_not_guessed() {
        let meta = turn_meta(&write_lines("effort-none", &[&claude_line("m1", "")]));
        assert_eq!(meta["model"], "claude-opus-5-5");
        assert!(meta.get("effort").is_none() && meta.get("ultracode").is_none(), "{meta:?}");
        let bill = unbilled(&write_lines("effort-none-bill", &[&claude_line("m1", "")]), 0, &[], "").unwrap();
        let entry = usage_entry(&bill.spend[0]);
        assert!(entry.get("effort").is_none() && entry.get("ultracode").is_none(), "{entry}");
        // a name past one word's length is cut, never carried whole
        let long = claude_line("m2", &format!(r#""effort":"{}","#, "x".repeat(500)));
        let meta = turn_meta(&write_lines("effort-long", &[&long]));
        assert_eq!(meta["effort"].as_str().map(str::len), Some(EFFORT_MAX_CHARS));
        // a Codex turn on its default effort: null, so unknown
        let meta = turn_meta(&rollout());
        assert_eq!(meta["model"], "gpt-5-codex");
        assert!(meta.get("effort").is_none(), "{meta:?}");
    }

    #[test]
    fn the_ultracode_keyword_marks_its_own_turn_only() {
        let lines = [PROMPT, KEYWORD, META_USER, &claude_line("m1", r#""perTurnEffort":"xhigh","#)];
        let meta = turn_meta(&write_lines("ultra-keyword", &lines));
        assert_eq!(meta["ultracode"], true, "the harness's own user line after it does not end the turn");
        // nor does a background task's notification waking the same request
        let woken = r#"{"type":"user","message":{"role":"user","content":"<task-notification>done</task-notification>"},"origin":{"kind":"task-notification"},"promptSource":"system"}"#;
        let mut woke = lines.to_vec();
        let after = claude_line("m3", r#""perTurnEffort":"xhigh","#);
        woke.extend([woken, after.as_str()]);
        assert_eq!(turn_meta(&write_lines("ultra-keyword-woken", &woke))["ultracode"], true);
        // the next prompt, typed without it, is an ordinary turn
        let next = r#"{"type":"user","message":{"role":"user","content":"and the docs"},"uuid":"p2"}"#;
        let mut later = lines.to_vec();
        let second = claude_line("m2", r#""perTurnEffort":"xhigh","#);
        later.extend([next, second.as_str()]);
        let path = write_lines("ultra-keyword-next", &later);
        assert!(turn_meta(&path).get("ultracode").is_none());
        let bill = unbilled(&path, 0, &[], "").unwrap();
        let ultra: Vec<bool> = bill.spend.iter().map(|s| s.mode.ultracode).collect();
        assert_eq!(ultra, [true, false], "the keyword's turn and the next are separate shares");
        assert_eq!(usage_entry(&bill.spend[0])["ultracode"], true);
        // a tool's output quoting the attachment is escaped, and never counts
        let quoted = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"{\"attachment\":{\"type\":\"ultra_effort_enter\"},\"type\":\"attachment\"}"}]}}"#;
        assert_eq!(mark_of(quoted), None);
    }

    #[test]
    fn standing_ultracode_lasts_from_enter_to_exit_across_bills() {
        let home = std::env::temp_dir().join(format!("collide-ultra-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut env = Env::default();
        env.insert("CORK_HOME".into(), home.to_string_lossy().to_string());
        let transcript = home.join("s1.jsonl");
        let t = transcript.to_string_lossy().to_string();
        let first = claude_line("m1", r#""effort":"xhigh","#);
        std::fs::write(&transcript, [PROMPT, ENTER, first.as_str()].join("\n") + "\n").unwrap();
        let owed = take_unbilled(&t, &env);
        assert_eq!((owed[0]["ultracode"].clone(), owed[0]["effort"].clone()), (json!(true), json!("xhigh")));
        billed(&t, &owed, &env);
        // a later stretch past the enter marker (and a tail that no longer
        // shows it) is still in ultracode: the bill marker remembers
        let filler = format!(r#"{{"type":"system","content":"{}"}}"#, "x".repeat(TRANSCRIPT_TAIL_BYTES as usize));
        let second = claude_line("m2", r#""effort":"xhigh","#);
        let mut file = std::fs::OpenOptions::new().append(true).open(&transcript).unwrap();
        std::io::Write::write_all(&mut file, [PROMPT, filler.as_str(), second.as_str()].join("\n").as_bytes()).unwrap();
        std::io::Write::write_all(&mut file, b"\n").unwrap();
        assert!(turn_meta(&t).get("ultracode").is_none(), "the tail alone cannot tell");
        assert_eq!(turn_meta_in(&t, &env)["ultracode"], true, "the bill marker can");
        let owed = take_unbilled(&t, &env);
        assert_eq!(owed[0]["ultracode"], true);
        billed(&t, &owed, &env);
        // until it is left
        let third = claude_line("m3", r#""effort":"high","#);
        let mut file = std::fs::OpenOptions::new().append(true).open(&transcript).unwrap();
        std::io::Write::write_all(&mut file, ([EXIT, PROMPT, third.as_str()].join("\n") + "\n").as_bytes()).unwrap();
        let owed = take_unbilled(&t, &env);
        assert!(owed[0].get("ultracode").is_none(), "{}", owed[0]);
        assert_eq!(owed[0]["effort"], "high");
        assert!(turn_meta_in(&t, &env).get("ultracode").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_subagent_bills_in_the_ultracode_its_parent_started_it_in() {
        let home = std::env::temp_dir().join(format!("collide-ultra-sub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let runs = home.join("s1").join("subagents").join("workflows").join("wf_1");
        std::fs::create_dir_all(&runs).unwrap();
        let mut env = Env::default();
        env.insert("CORK_HOME".into(), home.to_string_lossy().to_string());
        let parent = home.join("s1.jsonl");
        let asked = claude_line("m1", r#""perTurnEffort":"xhigh","#);
        std::fs::write(&parent, [PROMPT, KEYWORD, asked.as_str()].join("\n") + "\n").unwrap();
        // the subagent's own lines are sidechain lines at its own effort
        let brief = r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"trace it"}}"#;
        let worker = runs.join("agent-a1.jsonl");
        let w = worker.to_string_lossy().to_string();
        let work = claude_line("s1", r#""effort":"high","perTurnEffort":"high","#);
        std::fs::write(&worker, [brief, work.as_str()].join("\n") + "\n").unwrap();
        assert_eq!(parent_transcript(&w).as_deref(), Some(parent.to_string_lossy().as_ref()));
        let owed = take_unbilled(&w, &env);
        assert_eq!((owed[0]["ultracode"].clone(), owed[0]["effort"].clone()), (json!(true), json!("high")));
        billed(&w, &owed, &env);
        // the parent moves on to an ordinary turn: the subagent it started
        // in the keyword's turn bills on as it started
        let next = r#"{"type":"user","message":{"role":"user","content":"and the docs"},"uuid":"p2"}"#;
        let mut file = std::fs::OpenOptions::new().append(true).open(&parent).unwrap();
        std::io::Write::write_all(&mut file, format!("{next}\n").as_bytes()).unwrap();
        let more = claude_line("s2", r#""perTurnEffort":"high","#);
        let mut file = std::fs::OpenOptions::new().append(true).open(&worker).unwrap();
        std::io::Write::write_all(&mut file, format!("{more}\n").as_bytes()).unwrap();
        assert_eq!(take_unbilled(&w, &env)[0]["ultracode"], true);
        // and one started in the ordinary turn is not ultracode
        let plain = home.join("s1").join("subagents").join("agent-a2.jsonl");
        std::fs::write(&plain, [brief, work.as_str()].join("\n") + "\n").unwrap();
        let owed = take_unbilled(&plain.to_string_lossy(), &env);
        assert!(owed[0].get("ultracode").is_none(), "{}", owed[0]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_codex_turn_names_its_effort_in_its_collaboration_mode() {
        let context = r#"{"timestamp":"2026-10-16T06:00:00.000Z","type":"turn_context","payload":{"cwd":"/r","model":"gpt-6-luna","effort":"low","collaboration_mode":{"mode":"default","settings":{"model":"gpt-6-luna","reasoning_effort":"medium"}}}}"#;
        let older = r#"{"timestamp":"2026-10-16T06:00:00.000Z","type":"turn_context","payload":{"cwd":"/r","model":"gpt-6-luna","reasoning_effort":"high"}}"#;
        let count = r#"{"timestamp":"2026-10-16T06:00:05.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":400},"last_token_usage":{"input_tokens":300,"cached_input_tokens":0,"output_tokens":100}}}}"#;
        let path = write_lines("codex-effort", &[context, count]);
        assert_eq!(turn_meta(&path)["effort"], "medium", "the collaboration mode's setting first");
        let bill = unbilled(&path, 0, &[], "").unwrap();
        assert_eq!((bill.spend[0].mode.effort.as_str(), bill.effort.as_str()), ("medium", "medium"));
        assert_eq!(usage_entry(&bill.spend[0])["effort"], "medium");
        assert!(usage_entry(&bill.spend[0]).get("ultracode").is_none(), "Codex has no ultracode");
        assert_eq!(turn_meta(&write_lines("codex-effort-older", &[older, count]))["effort"], "high", "an older rollout's field");
        // a bill that starts past the turn_context keeps its effort, carried
        let carry = Carry { model: bill.model.clone(), effort: bill.effort.clone(), ..Carry::default() };
        let next = write_lines("codex-effort-next", &[count.replace("400", "900").as_str()]);
        let later = unbilled_carry(&next, 0, &[], carry).unwrap();
        assert_eq!(later.spend[0].mode.effort, "medium");
    }
}
