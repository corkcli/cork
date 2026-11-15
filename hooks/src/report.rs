//! PostToolUse / Stop: report EVERY file touch to Cork, mechanically.
//!
//! Writes post the file's current content to /report, which hashes, diffs and
//! discards it. Reads post only a path to /presence — no content ever leaves
//! the machine for a read. Shell calls are reported as writes when the
//! filesystem says they actually changed a file, otherwise as presence.
//!
//! HARD FAIL-OPEN: 500ms budget, always exits 0.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::config::{self, Env};
use crate::git;
use crate::http;
use crate::shell;
use crate::transcript;

pub const HOOK_VERSION: u32 = 114; // must match blocks.HOOK_ARTIFACT_VERSION
pub const TOTAL_BUDGET: Duration = Duration::from_millis(500);
const MAX_FILE_BYTES: u64 = 512 * 1024;

pub(crate) const WRITE_TOOLS: [&str; 5] = ["Edit", "Write", "MultiEdit", "NotebookEdit", "apply_patch"];
const READ_TOOLS: [&str; 4] = ["Read", "Grep", "Glob", "Bash"];
const SHELL_TOOLS: [&str; 3] = ["Bash", "shell", "run_terminal_cmd"];

const SENSITIVE_PATH_MARKERS: [&str; 12] = [
    ".ssh", ".gnupg", ".aws", ".cork", ".collide", "credential", "secret", ".env", ".pem", ".key",
    "id_rsa", "token",
];

pub(crate) fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The server keeps 300 characters of a presence path; a longer command is
/// cut there and says so, instead of ending mid-word. Byte-identical to
/// the Python hook's `_shown_command`.
const SHOWN_COMMAND_MAX: usize = 300;

fn shown_command(command: &str) -> String {
    let command = redact_command(command);
    if command.chars().count() <= SHOWN_COMMAND_MAX {
        return command;
    }
    format!("{}\u{2026}", take_chars(&command, SHOWN_COMMAND_MAX - 1))
}

/// Secrets never leave the machine inside a command line. The dashboard
/// still shows `npm run seed`; the key in front of it is gone. Applied in
/// order: known token shapes, then every `NAME=value` assignment's value,
/// then auth headers, then passwords inside URLs. The Python hook's
/// `_redact_command`, pattern for pattern.
pub(crate) fn redact_command(command: &str) -> String {
    use std::sync::OnceLock;
    static RULES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        REDACTIONS.iter().map(|(pattern, with)| (regex::Regex::new(pattern).expect("static regex"), *with)).collect()
    });
    let mut out = command.to_string();
    for (pattern, with) in rules {
        out = pattern.replace_all(&out, *with).into_owned();
    }
    out
}

/// (pattern, replacement) — kept byte-identical with report_hook.py's
/// _REDACTIONS; `${n}` is a capture group in both engines' syntax once the
/// Python side maps it to `\g<n>`.
const REDACTIONS: [(&str, &str); 7] = [
    (r"(sk-[A-Za-z0-9_-]{16,}|sk_(?:live|test)_[A-Za-z0-9]{8,}|rk_(?:live|test)_[A-Za-z0-9]{8,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|xox[abposr]-[A-Za-z0-9-]{10,}|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}|cat_[A-Za-z0-9_]{16,}|re_[A-Za-z0-9_]{16,}|AIza[0-9A-Za-z_-]{30,})", "[redacted]"),
    (r#"(^|[\s;&|(])([A-Z_][A-Z0-9_]*)=("[^"]*"|'[^']*'|[^\s;&|]+)"#, "${1}${2}=[redacted]"),
    (r"(?i)(authorization:\s*(?:bearer|basic|token)\s+)[^\s'\x22]+", "${1}[redacted]"),
    (r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]{8,}", "${1}[redacted]"),
    (r"(://[^/\s:@]+:)[^@\s/]+@", "${1}[redacted]@"),
    (r"(?i)(--(?:password|passwd|token|secret|api-key|apikey)[=\s]+)[^\s;&|]+", "${1}[redacted]"),
    (r"(?i)([?&](?:token|key|secret|password|sig|signature|access_token|api_key|apikey)=)[^&\s'\x22]+", "${1}[redacted]"),
];

/// What a build or an interpreter writes, never anyone's work, even in a
/// repo whose .gitignore does not say so: running `python` beside an edit
/// must not tell the other agents that `__pycache__` changed.
fn is_build_output(rel: &str) -> bool {
    const DIRS: [&str; 6] = ["__pycache__", "node_modules", ".mypy_cache", ".ruff_cache", ".pytest_cache", ".next"];
    const EXTS: [&str; 5] = [".pyc", ".pyo", ".class", ".o", ".tsbuildinfo"];
    rel.split('/').any(|part| DIRS.contains(&part)) || EXTS.iter().any(|ext| rel.ends_with(ext))
}

fn take_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// This machine, as a random id made once and kept beside the credential:
/// no hostname, nothing that names the person. Lets the server see one login
/// at work on several machines. Twin of report_hook.py `_machine_id`.
pub(crate) fn machine_id(env: &Env) -> String {
    let Some(dir) = config::credentials_path(env).parent().map(Path::to_path_buf) else { return String::new() };
    let path = dir.join("machine-id");
    if let Ok(found) = std::fs::read_to_string(&path) {
        let found = found.trim();
        if !found.is_empty() {
            return found.chars().take(32).collect();
        }
    }
    let mut bytes = [0u8; 8];
    let filled = std::fs::File::open("/dev/urandom").and_then(|mut f| { use std::io::Read; f.read_exact(&mut bytes) }).is_ok();
    if !filled {
        let seed = format!("{:?}{}{:p}", std::time::SystemTime::now(), std::process::id(), &bytes);
        let digest = <sha2::Sha256 as sha2::Digest>::digest(seed.as_bytes());
        bytes.copy_from_slice(&digest[..8]);
    }
    let made: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::write(&path, format!("{made}\n")).is_err() {
        return String::new();
    }
    made
}

/// The editor the agent works in and its workspace (see ide.rs), beside the
/// branch: the dashboard's breadcrumb. Only when known, so
/// an agent in a plain terminal sends what it always sent.
fn add_ide(meta: &mut serde_json::Map<String, Value>, root: &Path, env: &Env) {
    let found = crate::ide::of(root, env);
    if !found.ide.is_empty() {
        meta.insert("ide".into(), json!(found.ide));
    }
    if !found.workspace.is_empty() {
        meta.insert("ide_workspace".into(), json!(found.workspace));
    }
}

pub(crate) fn user_agent() -> String {
    format!("cork-report-hook/{HOOK_VERSION}")
}

/// Out-of-repo READS are presence too — the path travels, content never does.
/// Anything that smells like keys or credentials stays unreported.
fn external_label(file_path: &str, env: &Env) -> Option<String> {
    let lowered = file_path.to_lowercase();
    if SENSITIVE_PATH_MARKERS.iter().any(|m| lowered.contains(m)) {
        return None;
    }
    let home = config::get(env, "HOME");
    let shown = if !home.is_empty() && file_path.starts_with(home) {
        format!("~{}", &file_path[home.len()..])
    } else {
        file_path.to_string()
    };
    Some(take_chars(&shown, 160))
}

fn block_version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"cross-agent collision awareness \(v(\d+)\)").expect("block version pattern")
    })
}

/// The Cork block version this repo's AGENTS.md actually carries; 0 when
/// there is no block. Sent with every edit so the server can compare ground
/// truth against the version it ships.
fn agents_block_version(root: &Path) -> u32 {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let Ok(body) = std::fs::read_to_string(root.join(name)) else { continue };
        let head: String = body.chars().take(200_000).collect();
        if let Some(caps) = block_version_re().captures(&head) {
            if let Some(m) = caps.get(1) {
                if let Ok(version) = m.as_str().parse() {
                    return version;
                }
            }
        }
    }
    0
}

/// Cross-agent awareness, pushed INTO the session: teammates' certain
/// interface changes since this agent last talked to Cork, so it adapts
/// within one tool call of a teammate's save.
/// The server asked for one line of rationale: the symbol just written is
/// hot (other agents on the file) and the write carried no reason. Mirrors
/// `_ask_why_note`, byte for byte.
/// The server predicted the file this agent touches next and sent its
/// facts: printed once per session per file, and remembered as briefed.
/// Mirrors `_prefetch_note`.
fn prefetch_note(response: &Value, session: &str, env: &Env) -> String {
    let Some(next) = response.get("prefetch").filter(|n| n.is_object()) else { return String::new() };
    let path = text(next, "path");
    let note = text(next, "text");
    if path.is_empty() || note.is_empty() {
        return String::new();
    }
    if !session.is_empty() {
        let state = config::load_json(&crate::check::state_path(session, env));
        let briefed = state.get("briefed_paths").and_then(Value::as_array).map(|a| a.iter().any(|v| v.as_str() == Some(path.as_str()))).unwrap_or(false);
        if briefed {
            return String::new();
        }
        crate::prompt::remember_briefed(session, &[json!(path)], &[], &note, &[], env);
    }
    note
}

fn ask_why_note(response: &Value) -> String {
    let Some(ask) = response.get("ask_why").filter(|a| a.is_object()) else { return String::new() };
    let path = text(ask, "path");
    if path.is_empty() {
        return String::new();
    }
    let symbol = text(ask, "symbol");
    let where_ = if symbol.is_empty() { path.clone() } else { format!("{path}::{symbol}") };
    let others = ask.get("others").and_then(Value::as_i64).unwrap_or(0);
    // no tool to call for it: a tool's schema load and call cost two messages
    format!(
        "Cork: {where_} is hot ({others} other agent(s) on this file). Say in one line, to the user and in your \
commit message, why you changed it."
    )
}

/// The write changed a function other files call (the server's
/// `dependents`): once per session per symbol, how to tell only the agents
/// working on those callers. The only time the message command is offered
/// unasked: at session start it made every agent announce its plan.
fn dependents_note(response: &Value, session: &str, env: &Env) -> String {
    let Some(hint) = response.get("dependents").filter(|d| d.is_object()) else { return String::new() };
    let path = text(hint, "path");
    let symbols: Vec<String> = hint.get("symbols").and_then(Value::as_array).into_iter().flatten()
        .filter_map(Value::as_str).map(str::to_string).collect();
    let Some(symbol) = symbols.first() else { return String::new() };
    if path.is_empty() || session.is_empty() {
        return String::new();
    }
    let mut told = crate::deps::Told::load(env, session);
    let fact = format!("tell|{path}::{symbol}");
    if told.knows(&fact) {
        return String::new();
    }
    told.note(&fact, "", false);
    told.save();
    let files = hint.get("files").and_then(Value::as_u64).unwrap_or(0);
    format!(
        "Cork: other code calls `{symbol}` ({files} file(s)). If its callers need to know, \
`{} --about {path}::{symbol} \"...\"` reaches only the agents working on them.",
        crate::machine::message_command(env)
    )
}

fn awareness_context(response: &Value) -> String {
    let deltas = response
        .get("since_your_last_call")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    for delta in &deltas {
        let kind = text(delta, "kind");
        if kind == "symbol" && text(delta, "confidence") == "certain" {
            let detail = delta.get("detail");
            let symbol = text(delta, "symbol");
            let what = if let Some(map) = detail.and_then(Value::as_object) {
                let new_name = map.get("new_name").and_then(Value::as_str).unwrap_or("");
                if new_name.is_empty() {
                    continue;
                }
                let signature = map.get("signature").and_then(Value::as_str).unwrap_or("");
                format!("renamed {symbol} -> {new_name} ({signature})")
            } else {
                let change = text(delta, "change");
                // what can break a caller: a removal or a new signature. An
                // addition or a body edit broke nothing, and Study 5's agents
                // answered each one "unrelated, no action needed"
                let resigned = change == "modified"
                    && detail.and_then(Value::as_str).is_some_and(|s| s.starts_with("signature changed"));
                if change != "removed" && !resigned {
                    continue;
                }
                let mut what = format!("{change} {symbol}");
                if let Some(Value::String(s)) = detail {
                    if !s.is_empty() && change != "removed" {
                        what.push_str(&format!(" ({})", take_chars(s, 120)));
                    }
                }
                what
            };
            let who = {
                let named = text(delta, "who");
                if named.is_empty() {
                    let user = text(delta, "user");
                    if user.is_empty() { "a teammate".to_string() } else { user }
                } else {
                    named
                }
            };
            let path = {
                let p = text(delta, "path");
                if p.is_empty() { "?".to_string() } else { p }
            };
            lines.push(format!("- {who} {what} in {path}"));
        } else if kind == "tripwire" {
            let note = text(delta, "note");
            if note.is_empty() {
                continue;
            }
            let by = {
                let b = text(delta, "by");
                if b.is_empty() { "?".to_string() } else { b }
            };
            let path = {
                let p = text(delta, "path");
                if p.is_empty() { "?".to_string() } else { p }
            };
            lines.push(format!("- tripwire from {by} on {path}: {}", take_chars(&note, 160)));
        }
        if lines.len() >= 4 {
            break;
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "Cork: teammates changed code you may depend on since your last sync. Adapt to the \
NEW names/signatures before writing against the old ones (verify with \
check_collisions/get_symbol if unsure), and say so to the user:\n{}",
        lines.join("\n")
    )
}

/// The settings file this harness installs the hooks into.
fn settings_file(harness: &str) -> &'static str {
    match harness {
        "codex" => ".codex/hooks.json",
        "cursor" => ".cursor/hooks.json",
        // Hermes keeps its hooks in the user's own config, outside the repo,
        // so there is nothing here to fingerprint and nothing is claimed
        "hermes" => "",
        _ => ".claude/settings.json",
    }
}

/// A shell command that runs one of Cork's hooks, in any dialect.
fn is_collide_command(text: &str) -> bool {
    text.contains("collide")
        && (text.contains(".collide/report_hook.py")
            || text.contains(".collide/gate_hook.py")
            || text.contains(".collide/bin/collide-hook")
            || text.contains("collide-hook")
            || text.contains("collide-report-hook")
            || text.contains("collide-gate-hook"))
}

/// Every Cork command, tagged with the event it is wired under — the
/// nearest enclosing key that is not the dialect's own "hooks"/"command"
/// plumbing — so wiring the same script to one more event changes the
/// digest.
fn walk_commands(node: &Value, event: &str, found: &mut std::collections::BTreeSet<String>) {
    match node {
        Value::Object(map) => {
            for (key, value) in map {
                let under = if key == "hooks" || key == "command" { event } else { key.as_str() };
                walk_commands(value, under, found);
            }
        }
        Value::Array(items) => {
            for value in items {
                walk_commands(value, event, found);
            }
        }
        Value::String(text) if is_collide_command(text) => {
            found.insert(format!("{event}\t{text}"));
        }
        _ => {}
    }
}

/// Fingerprint of the Cork hooks this repo's settings file installs, or
/// empty when there is no such file or it installs none of ours.
///
/// The SCRIPTS carry a version of their own. The settings block does not,
/// and it is what decides whether those scripts are ever reached — which
/// events are wired, and whether the native-binary branch exists at all. So
/// a repo can be perfectly current on the scripts and years behind here,
/// which nothing could see before. Empty is the safe answer everywhere: the
/// server only compares a digest it was actually given.
fn settings_digest(root: &Path) -> String {
    let name = settings_file(crate::harness::current());
    if name.is_empty() {
        return String::new();
    }
    let Ok(body) = std::fs::read_to_string(root.join(name)) else { return String::new() };
    let Ok(parsed) = serde_json::from_str::<Value>(&body) else { return String::new() };
    let mut found = std::collections::BTreeSet::new();
    walk_commands(&parsed, "", &mut found);
    if found.is_empty() {
        return String::new();
    }
    let joined = found.into_iter().collect::<Vec<_>>().join("\n");
    sha256_hex(joined.as_bytes())[..12].to_string()
}

/// The staleness notice, said once rather than on every single edit.
///
/// It used to ride every response unfiltered, so a repo that could not fix
/// itself (a stale binary shadowing a newer committed script) repeated it
/// forever.
/// The stamp carries this hook's own version too, so a LATER version that is
/// still behind gets to speak again instead of inheriting the old silence.
fn artifacts_note(response: &Value, root: &Path, server: &str, env: &Env) -> String {
    let notice = text(response, "artifacts_outdated");
    if notice.is_empty() {
        return String::new();
    }
    // the machine install updates itself: it starts that now, and the agent
    // is told nothing (an installer it ran itself cost a turn, and every
    // other repo's session on the machine was told again)
    if crate::selfupdate::update_now(server, env) {
        return String::new();
    }
    let stamp = git::stamp_path(root, "artifacts");
    let stored = config::load_json(&stamp);
    let told = stored.get("notified").and_then(Value::as_str).unwrap_or("");
    let told_at = stored.get("version").and_then(Value::as_u64).unwrap_or(0);
    if told == notice && told_at == HOOK_VERSION as u64 {
        return String::new();
    }
    let _ = std::fs::write(
        &stamp,
        json!({"notified": notice, "version": HOOK_VERSION, "ts": 0}).to_string(),
    );
    notice
}

/// The setup check rides the CONSTANT call, so a repo whose protocol block is
/// missing or behind is caught within one edit. Repeats only when the notice
/// CHANGES — told once, never nagged.
fn setup_note(response: &Value, root: &Path) -> String {
    let hint = text(response, "setup_pending");
    if hint.is_empty() {
        return String::new();
    }
    let stamp = git::stamp_path(root, "setup");
    let stored = config::load_json(&stamp);
    if stored.get("notified").and_then(Value::as_str) == Some(hint.as_str()) {
        return String::new();
    }
    let _ = std::fs::write(&stamp, json!({"notified": hint, "ts": 0}).to_string());
    format!(
        "Cork: {hint} Act on this unprompted at your next clean moment: call the cork tool \
sync_agents_block with this repo's AGENTS.md, write back what it returns, and commit the result \
— until then the whole team is following an out-of-date protocol."
    )
}

/// This repo still carries Cork's old per-repo hooks, which the machine
/// install replaced. Its own key, not `artifacts_outdated`: a managed binary
/// answers that one by updating itself and saying nothing, and no update
/// can take a repo's committed files out. Repeats only when it changes.
fn retired_note(response: &Value, root: &Path) -> String {
    let notice = text(response, "repo_hooks_retired");
    if notice.is_empty() {
        return String::new();
    }
    let stamp = git::stamp_path(root, "retired");
    if config::load_json(&stamp).get("notified").and_then(Value::as_str) == Some(notice.as_str()) {
        return String::new();
    }
    let _ = std::fs::write(&stamp, json!({"notified": notice, "ts": 0}).to_string());
    notice
}

/// Where a teammate's new name is, as this checkout sees it: in the working
/// tree ("here"), only on origin ("origin"), or not landed at all. Study 5's
/// agents switched to a rename still in flight, then waited on failing tests.
fn name_state(root: &Path, name: &str) -> &'static str {
    let found = |args: &[&str]| crate::git::git(root, args, Duration::from_secs(2)).is_some();
    // no git to ask: the name is taken as here, the advice as it always was
    if name.is_empty() || !found(&["rev-parse", "--git-dir"]) || found(&["grep", "-q", "-w", "-e", name, "--", ":/"]) {
        "here"
    } else if found(&["grep", "-q", "-w", "-e", name, "@{u}", "--", ":/"]) {
        "origin"
    } else {
        "nowhere"
    }
}

/// The response without the lint findings this file does not bear out: a
/// rename or removal whose old name it no longer uses, a new signature
/// every call of which already fits (`fixlines::lines_for`, read here from
/// the file as written). Lattice test, round 2 (v104): five "signature
/// changed" findings for 14-hour-old parameter renames, every call already
/// matching, cost each session a verification step.
fn biting(response: &Value, content: &str) -> Value {
    let mut out = response.clone();
    if let Some(findings) = out.get_mut("findings").and_then(Value::as_array_mut) {
        findings.retain(|f| {
            if !matches!(text(f, "rule").as_str(), "signature_drift" | "stale_name" | "removed_symbol_use") {
                return true;
            }
            crate::fixlines::from_finding(f)
                .is_some_and(|fix| !crate::fixlines::lines_for(content, &Default::default(), &fix).is_empty())
        });
    }
    out
}

/// The response without the lint findings this session was already told
/// about (`told`: `(bare symbol, sig|ren|del)` from deps.rs's Told).
fn strip_told(response: &Value, told: &std::collections::BTreeSet<(String, String)>) -> Value {
    if told.is_empty() {
        return response.clone();
    }
    let mut out = response.clone();
    if let Some(findings) = out.get_mut("findings").and_then(Value::as_array_mut) {
        findings.retain(|f| {
            let kind = match text(f, "rule").as_str() {
                "signature_drift" => "sig",
                "stale_name" => "ren",
                "removed_symbol_use" => "del",
                _ => return true,
            };
            let symbol = text(f, "symbol");
            let bare = symbol.rsplit('.').next().unwrap_or(&symbol).to_string();
            !told.contains(&(bare, kind.to_string()))
        });
    }
    out
}

/// The lint findings this session has not yet heard for `rel`, marked
/// heard: the same finding came back on every edit of a file before (and
/// at 16 agents each one was read again and again).
fn once_per_session(response: &Value, rel: &str, root: &Path, told: &mut crate::deps::Told) -> Value {
    let mut out = response.clone();
    let mut said = false;
    if let Some(findings) = out.get_mut("findings").and_then(Value::as_array_mut) {
        findings.retain(|f| {
            // a rename that reached this checkout since is news again: now
            // the mechanical fix applies
            let state = if text(f, "rule") == "stale_name" { name_state(root, &text(f, "renamed_to")) } else { "" };
            let fact = format!("lint:{}|{}|{}|{state}", text(f, "rule"), text(f, "symbol"), text(f, "renamed_to"));
            if told.knows_file(&fact, rel) {
                return false;
            }
            told.note(&fact, rel, false);
            said = true;
            true
        });
    }
    if said {
        told.save();
    }
    out
}

/// Findings from the semantic linter, phrased for the agent's context.
pub(crate) fn lint_context(response: &Value, skip_mechanical: bool, root: &Path, lands: bool) -> String {
    let findings = response.get("findings").and_then(Value::as_array).cloned().unwrap_or_default();
    if findings.is_empty() {
        return String::new();
    }
    let mode = {
        let m = text(response, "autofix_mode");
        if m.is_empty() { "findings".to_string() } else { m }
    };
    let mut lines = vec!["Cork lint — teammates' in-flight changes affect this file:".to_string()];
    let mut judgment_present = false;
    for finding in findings.iter().take(5) {
        let fix = finding.get("fix").cloned().unwrap_or(Value::Null);
        if skip_mechanical && text(&fix, "class") == "mechanical" {
            continue;
        }
        let lines_at = finding
            .get("lines")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "None".into());
        // a rename reached through an alias or a module path is the
        // import's to fix: it reads as a judgment finding, with its prompt
        if text(finding, "rule") == "stale_name" && text(&fix, "class") != "judgment" {
            let by = {
                let b = text(finding, "by");
                if b.is_empty() { "a teammate".to_string() } else { b }
            };
            let (old, new) = (text(finding, "symbol"), text(finding, "renamed_to"));
            // which file's symbol (v80): a bare name could be anyone's
            let at = {
                let defined_in = text(finding, "in");
                if defined_in.is_empty() { String::new() } else { format!(" in {defined_in}") }
            };
            lines.push(match name_state(root, &new) {
                "here" => format!(
                    "- `{old}`{at} was renamed to `{new}` by {by}; this file still uses the old name (lines {lines_at}). \
Mechanical fix: replace the identifier."
                ),
                "origin" if lands => format!(
                    "- `{old}`{at} was renamed to `{new}` by {by}; it is on origin but not in your checkout (lines {lines_at} use \
the old name): write against `{new}`; landing will rebase onto their change."
                ),
                "origin" => format!(
                    "- `{old}`{at} was renamed to `{new}` by {by}; it is on origin but not in your checkout (lines {lines_at} use \
the old name). Pull or rebase onto it first, then replace the identifier."
                ),
                _ => format!(
                    "- `{old}`{at} was renamed to `{new}` by {by}, not pushed yet (lines {lines_at} use the old name): write \
against `{new}`; landing will rebase onto their change."
                ),
            });
        } else {
            judgment_present = true;
            lines.push(format!(
                "- {}: `{}` (lines {}) — {}",
                text(finding, "rule"),
                text(finding, "symbol"),
                lines_at,
                take_chars(&text(&fix, "prompt"), 300)
            ));
        }
    }
    if lines.len() == 1 {
        return String::new();
    }
    if judgment_present && (mode == "propose" || mode == "auto") {
        lines.push(
            "Dispatch rule: for judgment-class findings, spawn ONE scoped fixer per finding if \
your harness allows it (Claude Code: Task tool or `claude -p '<fix>' &`; Codex: `codex exec \
'<fix>' &`; otherwise fix inline). Fixers must verify with tests/differential_check and must \
NEVER spawn further fixers."
                .to_string(),
        );
    }
    lines.join("\n")
}

/// Certified applier, auto mode only: word-boundary rename substitutions,
/// applied ONLY if the file on disk is byte-identical to what was just
/// reported (compare-and-swap — never guess on a moved file). The fix is
/// re-reported under agent=collide-autofix and its response discarded: fixers
/// never chain, which is the loop guard.
#[allow(clippy::too_many_arguments)]
fn apply_mechanical(
    response: &Value,
    root: &Path,
    target: &Path,
    rel: &str,
    reported_content: &str,
    cfg: &config::Config,
    meta: &Map<String, Value>,
    budget: Duration,
) -> String {
    if text(response, "autofix_mode") != "auto" {
        return String::new();
    }
    let renames: Vec<Value> = response
        .get("findings")
        .and_then(Value::as_array)
        .map(|findings| {
            findings
                .iter()
                .filter_map(|f| f.get("fix").cloned())
                .filter(|fix| text(fix, "class") == "mechanical" && text(fix, "kind") == "rename")
                // only a rename this checkout already has: one still in flight would break it
                .filter(|fix| name_state(root, &text(fix, "to")) == "here")
                .collect()
        })
        .unwrap_or_default();
    if renames.is_empty() || budget.is_zero() {
        return String::new();
    }
    let Ok(on_disk) = std::fs::read_to_string(target) else { return String::new() };
    if on_disk != reported_content {
        return String::new();
    }
    let mut fixed = on_disk;
    let mut applied: Vec<String> = Vec::new();
    for fix in &renames {
        let from = text(fix, "from");
        let to = text(fix, "to");
        if from.is_empty() {
            continue;
        }
        let Ok(pattern) = Regex::new(&format!(r"\b{}\b", regex::escape(&from))) else { continue };
        let replaced = pattern.replace_all(&fixed, to.as_str()).into_owned();
        if replaced != fixed {
            applied.push(format!("{from} -> {to}"));
            fixed = replaced;
        }
    }
    if applied.is_empty() || std::fs::write(target, &fixed).is_err() {
        return String::new();
    }
    let mut payload = meta.clone();
    payload.insert("repo_id".into(), json!(cfg.repo_id));
    payload.insert("path".into(), json!(rel));
    payload.extend(cfg.source_fields(&rel, &fixed));
    payload.insert("agent".into(), json!("collide-autofix"));
    let _ = http::post(
        &cfg.server,
        "/report",
        &cfg.token,
        &user_agent(),
        &Value::Object(payload),
        budget,
    );
    format!(
        "Cork AUTOFIX rewrote {rel} on disk ({}) — the file changed since your last read; \
re-read it before editing again.",
        applied.join("; ")
    )
}

/// A repo renamed on the dashboard rides back on /report as
/// preferred_repo_id. Adopt it — but ONLY when this checkout's own git origin
/// already points at that repo. The origin match is the whole safety story: a
/// stale or wrong checkout can never be hijacked onto another scope.
fn adopt_preferred_repo_id(response: &Value, root: &Path, current_repo_id: &str) -> String {
    let preferred = text(response, "preferred_repo_id").trim().to_string();
    if preferred.is_empty() || preferred == current_repo_id {
        return String::new();
    }
    let origin = git::origin_repo_id(root);
    if origin.is_empty() || origin != git::normalize_repo_url(&preferred) {
        return String::new(); // can't confirm this checkout is the renamed repo
    }
    let cfg_path = config::repo_config_path(root);
    let mut cfg = config::load_json(&cfg_path);
    if cfg.get("repo_id").and_then(Value::as_str) == Some(preferred.as_str()) {
        return String::new();
    }
    let Some(map) = cfg.as_object_mut() else { return String::new() };
    map.insert("repo_id".into(), json!(preferred));
    let Ok(body) = serde_json::to_string_pretty(&cfg) else { return String::new() };
    if std::fs::write(&cfg_path, format!("{body}\n")).is_err() {
        return String::new();
    }
    format!(
        "Cork: this repo was renamed to {preferred} — updated {} to match. \
Commit it so teammates follow.",
        cfg_path.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| ".cork/config.json".into())
    )
}

/// A failed tool call (PostToolUseFailure): presence only. It reports that
/// the agent is running and what it tried, and prints nothing: the event has
/// no context channel we rely on, and whatever is pending waits for the next.
static FAILED_CALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The session whose protocol spool this event drains (proto.rs).
static NOTICES_FOR: std::sync::Mutex<Option<(Env, String)>> = std::sync::Mutex::new(None);

/// The session whose listener's held-back messages this event prints (peer.rs).
static HELD_FOR: std::sync::Mutex<Option<(Env, String)>> = std::sync::Mutex::new(None);

/// The first-touch briefing this call earned (touch.rs), said with
/// whatever else the call has to say.
static FIRST_TOUCH: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The line about this folder's branch moving under the session (switches.rs).
static SWITCHED: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn emit(note: &str) -> i32 {
    if FAILED_CALL.load(std::sync::atomic::Ordering::Relaxed) {
        return 0;
    }
    let first = FIRST_TOUCH.lock().map(|mut slot| std::mem::take(&mut *slot)).unwrap_or_default();
    let switched = SWITCHED.lock().map(|mut slot| std::mem::take(&mut *slot)).unwrap_or_default();
    let note = crate::prompt::join_notes(&[&switched, &first, note]);
    let note = note.as_str();
    // teammates' live changes and refused ops, queued by the session's daemon
    let live = NOTICES_FOR
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|(env, session)| crate::proto::take_notices(env, session)))
        .unwrap_or_default();
    // messages the session's listener could not post without their being held
    let held = HELD_FOR
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|(env, session)| crate::peer::take_held(session, env)))
        .unwrap_or_default();
    let note = crate::prompt::join_notes(&[&held, &live, note]);
    let note = crate::check::with_pending(&note);
    // the context lands in the agent's session: awareness arrives with the
    // write, not at the next voluntary check. A harness that reads nothing
    // back queues it for the next turn instead.
    let rendered = crate::harness::render_context("PostToolUse", &note);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    0
}

/// Stop hook: the turn ended. If it ended because the agent ran out of quota,
/// tell Cork so the team sees "hit its limit" on the Now panel instead of a
/// teammate that silently went dark mid-task.
/// ConfigChange: the settings file just changed, and THIS hook firing is
/// proof Cork's hooks are loaded in this session.
///
/// Until now the setup nag cleared only when a file write was reported, so a
/// session that installed the hooks and then only read kept being told to
/// install them. Says nothing: the event discards context, so the only output
/// is the exit code.
fn run_config_change(hook_input: &Value, env: &Env) -> i32 {
    let file_path = text(hook_input, "file_path");
    let settings = if file_path.is_empty() { Value::Null } else { config::load_json(Path::new(&file_path)) };
    let hooks = settings.get("hooks").cloned().unwrap_or(Value::Null).to_string();
    if !hooks.contains("report_hook.py") && !hooks.contains("collide-hook") && !hooks.contains("collide-report-hook") {
        return 0; // an unrelated settings edit is not evidence about Cork
    }
    let session = text(hook_input, "session_id");
    let state_file = crate::check::state_path(&session, env);
    let mut state = config::load_json(&state_file).as_object().cloned().unwrap_or_default();
    if state.get("config_alive_sent").and_then(Value::as_bool).unwrap_or(false) {
        return 0; // ConfigChange fires on every settings write; report once
    }
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(from_input) }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project_dir.is_empty()).then(|| PathBuf::from(project_dir));
    let root = config::find_repo_root(&[Some(cwd.clone()), project]).unwrap_or(cwd);
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let payload = json!({"repo_id": cfg.repo_id, "session": session, "event": "ConfigChange",
                         "source": text(hook_input, "source"), "hook_version": HOOK_VERSION});
    if http::post(&cfg.server, "/hook-alive", &cfg.token, &user_agent(), &payload, Duration::from_secs(3)).is_err() {
        return 0;
    }
    state.insert("config_alive_sent".into(), json!(true));
    save_json(&state_file, &Value::Object(state));
    0
}

/// The limit a turn that ended on an API error ran into (Claude Code's
/// StopFailure, which fires INSTEAD of Stop: a session that hit its limit
/// never sent a Stop, so the limit never reached Cork). Its
/// `last_assistant_message` is the harness's notice ("You've hit your
/// session limit · resets 4:20pm (America/Los_Angeles)"); the transcript's
/// tail says the same. A rate limit with no words of its own is still one.
fn stop_failure_note(hook_input: &Value, path: &str) -> String {
    let said = transcript::limit_from_text(&text(hook_input, "last_assistant_message"));
    if !said.is_empty() {
        return said;
    }
    let tail = transcript::limit_note(path);
    if !tail.is_empty() {
        return tail;
    }
    match text(hook_input, "error").as_str() {
        "rate_limit" => "usage limit reached".to_string(),
        "billing_error" => "billing limit reached".to_string(),
        _ => String::new(),
    }
}

fn run_stop(hook_input: &Value, env: &Env, started: Instant) -> i32 {
    // a long turn finishing: the person may have walked away from it
    let ran = crate::pings::turn_ended(&text(hook_input, "session_id"), env);
    if ran >= crate::pings::LONG_RUN_S {
        crate::pings::ping(env, crate::pings::Kind::LongRun, &PathBuf::from(text(hook_input, "cwd")), "Cork: an agent finished",
            &format!("After {} of work, its turn is done.", crate::pings::duration_words(ran)));
    }
    // the turn's last message replayed the context too; then the briefing's
    // tally for the turn, then the limit note
    let path = text(hook_input, "transcript_path");
    let failed = text(hook_input, "hook_event_name") == "StopFailure";
    let message = transcript::turn_meta(&path).get("turn_id").and_then(Value::as_str).unwrap_or("").to_string();
    crate::prompt::note_call(&text(hook_input, "session_id"), !text(hook_input, "agent_id").is_empty(), &message, env);
    if !failed {
        crate::prompt::brief_outcome(hook_input, env, started, false);
    }
    let note = if failed { stop_failure_note(hook_input, &path) } else { transcript::limit_note(&path) };
    // what the agent said it did: the note on the code this turn changed
    // (a turn that ended on an API error said nothing: the harness's error
    // text is not the agent's closing words)
    let reply = if failed {
        String::new()
    } else {
        let given = text(hook_input, "last_assistant_message");
        if given.trim().is_empty() { transcript::last_reply(&path) } else { given }
    };
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(from_input)
        }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let root = config::find_repo_root(&[
        Some(cwd.clone()),
        (!project_dir.is_empty()).then(|| PathBuf::from(project_dir)),
    ])
    .unwrap_or_else(|| {
        if project_dir.is_empty() { cwd.clone() } else { PathBuf::from(project_dir) }
    });
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    // whatever this hook cannot deliver in its budget, and every subagent's
    // messages its own SubagentStop did not see, go with a detached sender
    let _later = BillLater::new(&root, &path, &text(hook_input, "session_id"), env);
    // every message since the last bill, the ones that ended in a read, a
    // command or plain text included: the turn's whole cost
    let usage = transcript::take_unbilled(&path, env);
    if note.is_empty() && reply.trim().is_empty() && usage.is_empty() {
        return 0;
    }
    let remaining = TOTAL_BUDGET.checked_sub(started.elapsed()).unwrap_or_default();
    if remaining.is_zero() {
        return 0; // what it spent is owed, and goes with the next bill
    }
    let session = text(hook_input, "session_id");
    let title = transcript::title(&path, env);
    if note.is_empty() {
        let mut settled = json!({"repo_id": cfg.repo_id, "session": session, "agent": user_agent()});
        if reply.trim().is_empty() {
            settled["action"] = json!("usage");
        } else {
            settled["action"] = json!("settled");
            settled["text"] = json!(reply);
        }
        if !usage.is_empty() {
            settled["usage"] = json!(usage);
        }
        if !title.is_empty() {
            settled["title"] = json!(title);
        }
        let cc_name = transcript::session_name(&session, false, env);
        if !cc_name.is_empty() {
            settled["cc_name"] = json!(cc_name);
        }
        settled["harness"] = json!(crate::harness::current());
        // a turn a posted message started may have made no tool call
        let peer_seen = transcript::peer_seen(&session, &path, env);
        if !peer_seen.is_empty() {
            settled["peer_seen"] = json!(peer_seen);
        }
        if bill_landed(&http::post(&cfg.server, "/presence", &cfg.token, &user_agent(), &settled, remaining)) && !usage.is_empty() {
            transcript::billed(&path, &usage, env);
        }
        return 0;
    }
    let mut meta = transcript::turn_meta_in(&path, env);
    for key in transcript::USAGE_KEYS {
        meta.remove(key); // the limit row itself spends nothing: the bill rides beside it
    }
    meta.remove("turn_id");
    if !usage.is_empty() {
        meta.insert("usage".into(), json!(usage));
    }
    if !title.is_empty() {
        meta.insert("title".into(), json!(title));
    }
    if !session.is_empty() {
        meta.insert("session".into(), json!(session));
    }
    let branch = git::branch(&root);
    if !branch.is_empty() {
        meta.insert("branch".into(), json!(branch));
    }
    add_ide(&mut meta, &root, env);
    meta.insert("repo_id".into(), json!(cfg.repo_id));
    meta.insert("path".into(), json!(note));
    meta.insert("action".into(), json!("limit"));
    let posted = http::post(
        &cfg.server,
        "/presence",
        &cfg.token,
        &user_agent(),
        &Value::Object(meta),
        remaining,
    );
    if bill_landed(&posted) && !usage.is_empty() {
        transcript::billed(&path, &usage, env);
    }
    0
}

/// SubagentStop: a subagent finished. Its messages live in its own
/// transcript (`<session>/subagents/agent-<id>.jsonl`), which no Stop of
/// the session reads: summed here, with a marker of its own, and billed
/// under the parent session with the worker named. The worker's marker goes
/// too, so the roster stops counting a subagent that is done.
fn run_subagent_stop(hook_input: &Value, env: &Env, started: Instant) -> i32 {
    let worker = text(hook_input, "agent_id");
    let parent = text(hook_input, "transcript_path");
    let path = transcript::subagent_transcript(&text(hook_input, "agent_transcript_path"), &parent, &worker);
    if worker.is_empty() || path.is_empty() {
        return 0;
    }
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
    let _later = BillLater::new(&root, &parent, &text(hook_input, "session_id"), env);
    // what it still owed the server is history now: never sent again, so
    // its parent's posts cannot draw it live once more (spool.rs)
    crate::spool::worker_ended(env, &cfg.server, &cfg.repo_id, &worker);
    // trunk mode: its branch, all committed, lands into the trunk (detached)
    crate::trunk::on_done(&root, &text(hook_input, "session_id"), &worker, "stop", env);
    let usage = transcript::take_unbilled(&path, env);
    let remaining = TOTAL_BUDGET.checked_sub(started.elapsed()).unwrap_or_default();
    if remaining.is_zero() {
        return 0;
    }
    let mut payload = json!({"repo_id": cfg.repo_id, "session": text(hook_input, "session_id"), "action": "usage",
                             "agent": user_agent(), "worker": worker, "worker_ended": true});
    for (field, value) in [("worker_type", text(hook_input, "agent_type")), ("worker_name", transcript::worker_name(&parent, &worker))] {
        if !value.is_empty() {
            payload[field] = json!(value);
        }
    }
    if !usage.is_empty() {
        payload["usage"] = json!(usage);
    }
    if bill_landed(&http::post(&cfg.server, "/presence", &cfg.token, &user_agent(), &payload, remaining)) && !usage.is_empty() {
        transcript::billed(&path, &usage, env);
    }
    0
}

/// Whatever a bill still owes when the hook returns: a post that failed or
/// never fit the hook's budget (a Codex `exec` run has one Stop and no
/// SessionEnd, so its bill was otherwise never sent), and every subagent
/// transcript of the session with messages no bill has counted (a
/// subagent's last message is often written after its SubagentStop read
/// the file). Handed, on drop, to a detached `collide-hook bill-send`,
/// which has the time to deliver it.
struct BillLater {
    root: PathBuf,
    transcript: String,
    session: String,
    env: Env,
}

impl BillLater {
    fn new(root: &Path, transcript: &str, session: &str, env: &Env) -> BillLater {
        BillLater { root: root.to_path_buf(), transcript: transcript.to_string(), session: session.to_string(), env: env.clone() }
    }
}

impl Drop for BillLater {
    fn drop(&mut self) {
        if !self.transcript.is_empty() && transcript::anything_unbilled(&self.transcript, &self.env) {
            // remembered on disk first: a sender that dies, or a machine that
            // sleeps, leaves the bill for the next hook run to send
            transcript::note_owing(&self.transcript, &self.root, &self.session, &self.env);
            spawn_bill_send(&self.root, &self.transcript, &self.session);
        }
        // and the bills earlier sessions on this machine still owe (their
        // sender gave up while the server was out of reach)
        for (root, path, session) in transcript::owing_elsewhere(&self.transcript, &self.env) {
            spawn_bill_send(&root, &path, &session);
        }
    }
}

/// A detached `collide-hook bill-send` for one transcript.
fn spawn_bill_send(root: &Path, transcript: &str, session: &str) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = std::process::Command::new(exe);
    command
        .arg("bill-send")
        .arg(root)
        .arg(transcript)
        .arg(session)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// A bill post landed only when the server says so: a 200 whose answer is
/// `ok: false` (rate limited, the repo refused, signed out) recorded
/// nothing, and taking it for delivered is how Study 9's session burst lost
/// whole sessions' usage.
fn bill_landed(posted: &Result<Value, ()>) -> bool {
    matches!(posted, Ok(answer) if answer.get("ok").and_then(Value::as_bool) == Some(true))
}

/// How long the detached sender waits before it reads: a subagent's last
/// lines land just after the hook that started it.
const BILL_SEND_SETTLE: Duration = Duration::from_secs(2);
/// And how long each of its posts may take.
const BILL_SEND_TIMEOUT: Duration = Duration::from_secs(15);
/// The waits between its tries while the server does not take the bill:
/// about four minutes in all, past a burst of sessions or a redeploy. What
/// is still owed after that stays on disk for the next hook run to send.
const BILL_SEND_RETRIES: [Duration; 5] =
    [Duration::from_secs(3), Duration::from_secs(10), Duration::from_secs(30), Duration::from_secs(60), Duration::from_secs(120)];

/// `collide-hook bill-send <root> <transcript> <session>`, detached: post
/// what the session's transcript still owes, then bill each of its subagent
/// transcripts, under the subagent's name, trying again while the server
/// does not take it. Every entry carries the ids of the messages it sums,
/// so a post the hook itself also landed is billed once.
pub fn bill_send_command(args: &[String], env: &Env) -> i32 {
    let arg = |at: usize| args.get(at).cloned().unwrap_or_default();
    let (root, path, session) = (PathBuf::from(arg(1)), arg(2), arg(3));
    if path.is_empty() {
        return 0;
    }
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let settle = config::get(env, "CORK_BILL_SETTLE_MS").parse::<u64>().map(Duration::from_millis).unwrap_or(BILL_SEND_SETTLE);
    std::thread::sleep(settle);
    let mut waits = BILL_SEND_RETRIES.iter();
    loop {
        if send_owed(&cfg, &path, &session, env) {
            transcript::paid_up(&path, env);
            return 0;
        }
        let Some(wait) = waits.next() else { return 0 };
        std::thread::sleep(*wait);
    }
}

/// One try at everything `path` and its subagents owe; true when nothing is
/// owed any more.
fn send_owed(cfg: &config::Config, path: &str, session: &str, env: &Env) -> bool {
    let base = json!({"repo_id": cfg.repo_id, "session": session, "action": "usage", "agent": user_agent()});
    let mut clear = true;
    let owed = transcript::owed(path, env);
    if !owed.is_empty() {
        let mut payload = base.clone();
        payload["usage"] = json!(owed);
        if bill_landed(&http::post(&cfg.server, "/presence", &cfg.token, &user_agent(), &payload, BILL_SEND_TIMEOUT)) {
            transcript::billed(path, &owed, env);
        } else {
            clear = false;
        }
    }
    for (worker, file) in transcript::subagent_transcripts(path) {
        let usage = transcript::take_unbilled(&file, env);
        if usage.is_empty() {
            continue;
        }
        let mut payload = base.clone();
        payload["usage"] = json!(usage);
        payload["worker"] = json!(worker);
        let name = transcript::worker_name(path, &worker);
        if !name.is_empty() {
            payload["worker_name"] = json!(name);
        }
        if bill_landed(&http::post(&cfg.server, "/presence", &cfg.token, &user_agent(), &payload, BILL_SEND_TIMEOUT)) {
            transcript::billed(&file, &usage, env);
        } else {
            clear = false;
        }
    }
    clear && !transcript::anything_unbilled(path, env)
}

/// What a session's end may spend asking git about the files it wrote,
/// out of `TOTAL_BUDGET`: the post that ends the session must not starve.
const GIT_STATE_BUDGET: Duration = Duration::from_millis(200);

/// SessionEnd: the conversation is over — Claude Code's exit, /clear or
/// logout; Codex's close, or 30 idle minutes; Cursor's sessionEnd. Tell
/// Cork, so the Now panel stops counting this agent the moment it is gone
/// instead of an hour later, when silence is finally taken for absence. Fire
/// and forget: nothing is printed and nothing can block. Whatever the last
/// Stop did not bill rides along. Then the briefing tally's last report for
/// the session.
fn run_session_end(hook_input: &Value, env: &Env, started: Instant) -> i32 {
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(from_input)
        }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let root = config::find_repo_root(&[
        Some(cwd.clone()),
        (!project_dir.is_empty()).then(|| PathBuf::from(project_dir)),
    ])
    .unwrap_or_else(|| {
        if project_dir.is_empty() { cwd.clone() } else { PathBuf::from(project_dir) }
    });
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let path = text(hook_input, "transcript_path");
    let _later = BillLater::new(&root, &path, &text(hook_input, "session_id"), env);
    let remaining = TOTAL_BUDGET.checked_sub(started.elapsed()).unwrap_or_default();
    if remaining.is_zero() {
        return 0;
    }
    let mut meta = transcript::turn_meta_in(&path, env);
    for key in transcript::USAGE_KEYS {
        meta.remove(key);
    }
    meta.remove("turn_id");
    let usage = transcript::take_unbilled(&path, env);
    if !usage.is_empty() {
        meta.insert("usage".into(), json!(usage));
    }
    let session = text(hook_input, "session_id");
    if !session.is_empty() {
        meta.insert("session".into(), json!(session));
    }
    let branch = git::branch(&root);
    if !branch.is_empty() {
        meta.insert("branch".into(), json!(branch));
    }
    add_ide(&mut meta, &root, env);
    meta.insert("repo_id".into(), json!(cfg.repo_id));
    // a session that ended on its usage limit (`claude -p` and `codex exec`
    // exit right after it; Codex sends no Stop for a failed turn, only
    // this) stays shown as limited, with when it resets, not as ended
    let limit = transcript::limit_note(&path);
    if limit.is_empty() {
        meta.insert("action".into(), json!("ended"));
    } else {
        meta.insert("action".into(), json!("limit"));
        meta.insert("path".into(), json!(limit));
    }
    // what git says of the files it wrote (uncommitted, committed, pushed),
    // so its handoff never passes last night's edits off as fresh work
    if limit.is_empty() && remaining > GIT_STATE_BUDGET * 2 {
        let state = config::load_json(&crate::check::state_path(&session, env));
        let written: Vec<String> = state
            .get("written_paths")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if let Some(files) = git::files_state(&root, &written, GIT_STATE_BUDGET) {
            meta.insert("git_files".into(), files);
        }
    }
    // the session's protocol daemon releases its leases and ends
    crate::proto::bye(env, &text(hook_input, "session_id"));
    let remaining = TOTAL_BUDGET.checked_sub(started.elapsed()).unwrap_or_default();
    let posted = http::post(
        &cfg.server,
        "/presence",
        &cfg.token,
        &user_agent(),
        &Value::Object(meta),
        remaining,
    );
    if bill_landed(&posted) && !usage.is_empty() {
        transcript::billed(&path, &usage, env);
    }
    // then the briefing's final word for this session: every briefed path
    // still only read is a miss now, and the cost of carrying the briefing
    // is reported whether or not a path ever settled. Its own budget: the
    // retirement above is the post that must not be starved.
    crate::prompt::brief_outcome(hook_input, env, Instant::now(), true);
    // trunk mode: a session that ended on a branch of its own, everything
    // committed, has it landed into the trunk (detached)
    crate::trunk::on_done(&root, &text(hook_input, "session_id"), "", "end", env);
    0
}

// ------------------------------------------------------------ the map
//
// The map — what blast_radius and the repo map answer from — is built from
// file CONTENT, and it used to learn a file only when someone wrote it, so a
// symbol's callers stayed invisible until an earlier agent happened to edit
// them. Two more doors: the tracked tree is indexed once at session start
// (then only what changed), and a Read posts the file it read. Only the
// languages the parser knows are worth sending. Mirrors report_hook.py.

pub(crate) const MAP_EXTS: [&str; 24] = [
    ".py", ".pyi", ".ts", ".mts", ".cts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".go", ".rs",
    ".java", ".cs", ".rb", ".c", ".h", ".cc", ".cpp", ".cxx", ".hpp", ".hh", ".hxx", ".php",
];
const OBSERVE_MAX_BYTES: u64 = 512_000;
/// A session start may wait this long; the rest continues detached.
const INDEX_BUDGET: Duration = Duration::from_secs(20);
const INDEX_BATCH_FILES: usize = 64;
const INDEX_BATCH_BYTES: usize = 3_000_000;

pub fn is_map_source(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    MAP_EXTS.iter().any(|ext| lower.ends_with(ext)) && !is_collide_artifact(rel)
}

/// Cork's own installed files (`.collide/report_hook.py`, the gate, the
/// config in `.cork/` or `.collide/`) are tracked by git in every repo setup
/// touches. They are not the user's code: never indexed, reported, or counted
/// as the session's work.
pub fn is_collide_artifact(rel: &str) -> bool {
    [".cork/", ".collide/"].iter().any(|dir| rel.starts_with(dir) || rel.contains(&format!("/{dir}")))
}

/// The file's text, or None when it is too large or unreadable.
pub(crate) fn source_of(full: &Path) -> Option<String> {
    let meta = std::fs::metadata(full).ok()?;
    if meta.len() > OBSERVE_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(full).ok()?;
    Some(text_of_source(&bytes))
}

/// A source file's text as every OS reports it: UTF-8 (lossy), Windows line
/// endings as \n, the way git's autocrlf and Python's text mode see it. A
/// file checked out on Windows would otherwise differ, line for line, from
/// the same file on a teammate's Mac. Twin of report_hook.py's
/// `_text_of_source`.
pub(crate) fn text_of_source(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.contains("\r\n") { text.replace("\r\n", "\n") } else { text.into_owned() }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Per-checkout memory of what the map already holds: path -> content
/// hash. Lives beside the credentials, never in the repo.
/// Keyed by the checkout AND where it reports: a checkout moved to another
/// server, workspace or repo id starts a fresh index there instead of
/// believing the new map already holds its files.
pub(crate) fn index_state_path(root: &Path, env: &Env, server: &str, repo_id: &str) -> PathBuf {
    let key = format!("{}\n{server}\n{repo_id}", config::absolute(root).to_string_lossy());
    let digest = sha256_hex(key.as_bytes());
    config::data_dir(env).join("index").join(format!("{}.json", &digest[..16]))
}

pub(crate) fn save_json(path: &Path, value: &Value) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, value.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Every git-tracked file in a language the parser knows.
pub(crate) fn tracked_sources(root: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new("git")
        .args(["-C", &root.to_string_lossy(), "ls-files", "-z"])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty() && is_map_source(p))
        .map(str::to_string)
        .collect()
}

/// Folders that hold someone else's code, checked in: never anyone's work
/// here, and in a monorepo that vendors a whole product, most of the map.
const VENDORED_DIRS: [&str; 11] = [
    "vendor", "vendors", "third_party", "third-party", "thirdparty", "bower_components", "Pods", "Carthage",
    "__generated__", "generated", ".yarn",
];

/// Files a generator or bundler wrote.
fn is_generated_name(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_ascii_lowercase();
    name.ends_with(".min.js") || name.ends_with(".bundle.js") || name.ends_with(".pb.go") || name.ends_with("_pb2.py")
        || name.ends_with("_pb2_grpc.py") || name.contains(".generated.") || name.ends_with(".gen.go")
}

/// What the bulk index skips: vendored or generated code, by folder, by
/// name, or by the repo's own `.gitattributes` (`linguist-vendored`,
/// `linguist-generated`). An agent that reads or edits one of these files
/// still brings it into the map.
fn skipped_by_index(root: &Path, files: &[String]) -> std::collections::HashSet<String> {
    let mut skip: std::collections::HashSet<String> = files
        .iter()
        .filter(|rel| rel.split('/').any(|part| VENDORED_DIRS.contains(&part)) || is_generated_name(rel))
        .cloned()
        .collect();
    let has_attributes = root.join(".gitattributes").exists()
        || files.iter().any(|f| f.ends_with("/.gitattributes"));
    if !has_attributes {
        return skip;
    }
    use std::io::Write;
    let child = std::process::Command::new("git")
        .args(["-C", &root.to_string_lossy(), "check-attr", "-z", "--stdin", "linguist-vendored", "linguist-generated"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return skip };
    let list: Vec<u8> = files.iter().flat_map(|f| f.bytes().chain(std::iter::once(0u8))).collect();
    let writer = child.stdin.take().map(|mut stdin| std::thread::spawn(move || {
        let _ = stdin.write_all(&list);
    }));
    let Ok(out) = child.wait_with_output() else { return skip };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    // path NUL attribute NUL value NUL, repeated
    let text = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = text.split('\0').collect();
    for triple in parts.chunks(3) {
        if let [path, _attr, value] = triple {
            if matches!(*value, "set" | "true") {
                skip.insert(path.to_string());
            }
        }
    }
    skip
}

/// How many files the bulk index sends: past this, a repo's map starts from
/// the code people work on (`CORK_INDEX_MAX_FILES`).
fn index_max_files(env: &Env) -> usize {
    config::get(env, "CORK_INDEX_MAX_FILES").trim().parse().unwrap_or(6_000)
}

/// Files changed in git in the last half year, newest first.
fn recently_changed(root: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new("git")
        .args(["-C", &root.to_string_lossy(), "log", "--since=180.days", "--name-only", "--format=", "-n", "2000"])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty() && seen.insert(l.to_string()))
        .map(str::to_string)
        .collect()
}

/// The tracked sources the bulk index covers, best first: past
/// `limit`, the files changed most recently in git, then the files in the
/// folders with the most recent changes. Below the limit, every file, in
/// path order.
fn index_candidates(root: &Path, env: &Env) -> Vec<String> {
    let mut files = tracked_sources(root);
    let skip = skipped_by_index(root, &files);
    files.retain(|f| !skip.contains(f));
    let limit = index_max_files(env);
    if files.len() <= limit {
        return files;
    }
    let recent = recently_changed(root);
    let rank: std::collections::HashMap<&str, usize> = recent.iter().enumerate().map(|(i, p)| (p.as_str(), i)).collect();
    // a folder's activity: its recent changes, at two levels deep
    let folder = |p: &str| -> String { p.split('/').take(2).collect::<Vec<_>>().join("/") };
    let mut activity: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for p in &recent {
        *activity.entry(folder(p)).or_default() += 1;
    }
    files.sort_by_cached_key(|f| {
        let r = rank.get(f.as_str()).copied();
        (r.is_none(), r.unwrap_or(0), std::cmp::Reverse(activity.get(&folder(f)).copied().unwrap_or(0)), f.clone())
    });
    files.truncate(limit);
    files.sort();
    files
}

/// (path, content, sha) for every tracked source the map has not seen in
/// this exact form — the whole tree the first time, then only changes.
fn index_plan(root: &Path, state: &Value, env: &Env) -> Vec<(String, String, String)> {
    let known = state.get("files").and_then(Value::as_object);
    let mut plan = Vec::new();
    for rel in index_candidates(root, env) {
        let Some(content) = source_of(&root.join(&rel)) else { continue };
        let sha = sha256_hex(content.as_bytes());
        if known.and_then(|k| k.get(&rel)).and_then(Value::as_str) == Some(sha.as_str()) {
            continue;
        }
        plan.push((rel, content, sha));
    }
    plan
}

/// Send what the map lacks, in batches, remembering each batch as it lands
/// so an interrupted run never repeats itself. Returns (sent, left).
pub fn index_repo(root: &Path, cfg: &config::Config, env: &Env, budget: Duration) -> (usize, usize) {
    let started = Instant::now();
    let state_path = index_state_path(root, env, &cfg.server, &cfg.repo_id);
    let state = config::load_json(&state_path);
    let plan = index_plan(root, &state, env);
    let mut files = state.get("files").and_then(Value::as_object).cloned().unwrap_or_default();
    let (mut sent, mut i) = (0usize, 0usize);
    while i < plan.len() {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining <= Duration::from_secs(1) {
            break;
        }
        let start = i;
        let mut batch = Vec::new();
        let mut size = 0usize;
        while i < plan.len() && batch.len() < INDEX_BATCH_FILES && size < INDEX_BATCH_BYTES {
            let (rel, content, _sha) = &plan[i];
            let mut file = cfg.source_fields(rel, content);
            file.insert("path".into(), json!(rel));
            batch.push(Value::Object(file));
            size += content.len();
            i += 1;
        }
        let payload = json!({"repo_id": cfg.repo_id, "files": batch});
        if http::post(&cfg.server, "/observe", &cfg.token, &user_agent(), &payload,
                      remaining.min(Duration::from_secs(30))).is_err() {
            i = start;
            break; // unreachable server: stop here, the next session start resumes
        }
        for (rel, _content, sha) in &plan[start..i] {
            files.insert(rel.clone(), json!(sha));
        }
        sent += i - start;
        save_json(&state_path, &json!({"files": files, "ts": now_s()}));
    }
    (sent, plan.len() - i)
}

fn now_s() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// SessionStart: put the tracked tree in the map before the first tool
/// call, so the first agent's blast_radius is as complete as the fourth's.
/// stdout becomes session context, so it says one line and only when it
/// did something.
pub(crate) const BRIEF_BUDGET: u64 = 6_000;
pub(crate) const BRIEF_TIMEOUT: Duration = Duration::from_secs(10);
/// The handoffs waiting for this person in this repo, taken by this session
/// (`cork handoff` sent while none of their sessions was open here). Fails
/// open: no answer is no handoff.
fn take_handoffs(cfg: &config::Config, session: &str) -> Vec<Value> {
    if session.is_empty() {
        return Vec::new();
    }
    let payload = json!({"repo_id": cfg.repo_id, "session": session});
    http::post(&cfg.server, "/handoffs/take", &cfg.token, &user_agent(), &payload, BRIEF_TIMEOUT)
        .ok()
        .and_then(|answer| answer.get("handoffs").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// SessionStart sources after which the agent's context is gone.
pub(crate) const RESUME_SOURCES: [&str; 2] = ["compact", "resume"];

/// The dashboard behind a server address: CORK_DASHBOARD_URL when set,
/// else the site the mcp.* (or api.*) host belongs to. Any other server —
/// localhost, a self-hosted box — has no known dashboard, so none opens.
/// Twin of the Python hook's `_dashboard_url`.
pub(crate) fn dashboard_url(server: &str, env: &Env) -> String {
    let explicit = config::get(env, "CORK_DASHBOARD_URL").trim_end_matches('/');
    if !explicit.is_empty() {
        return format!("{explicit}/dashboard");
    }
    static HOST: OnceLock<Regex> = OnceLock::new();
    let host = HOST.get_or_init(|| Regex::new(r"^(https?)://(?:mcp|api)\.([^/:]+)").expect("valid regex"));
    host.captures(server).map(|c| format!("{}://{}/dashboard", &c[1], &c[2])).unwrap_or_default()
}

/// A browser opens only for a person at a desktop: never in CI, over SSH,
/// in a test run, on a headless Linux box, or with CORK_NO_BROWSER=1.
pub(crate) fn may_open_browser(env: &Env) -> bool {
    let off = config::get(env, "CORK_NO_BROWSER");
    if !off.is_empty() && off != "0" {
        return false;
    }
    let markers = ["CI", "GITHUB_ACTIONS", "SSH_CONNECTION", "SSH_TTY", "CODESPACES", "PYTEST_CURRENT_TEST"];
    if markers.iter().any(|m| !config::get(env, m).is_empty()) {
        return false;
    }
    if cfg!(target_os = "linux") && config::get(env, "DISPLAY").is_empty() && config::get(env, "WAYLAND_DISPLAY").is_empty() {
        return false;
    }
    true
}

/// A repo no workspace has yet: the browser opens on the question of where
/// it goes (the site's /add-repo page, one click with the workspace its
/// remote names already picked), once per repo on this machine, and on a
/// Mac the app the agent runs in comes back to the front once it is
/// answered (back.rs). A repo already in a workspace opens nothing: the
/// browser used to open on every first session in a workspace, wanted or
/// not. Remembered in ~/.collide/dashboard_opened.json BEFORE opening, so a
/// browser that fails to launch is never retried every session. The URL
/// when it opened, else empty.
fn ask_where_once(server: &str, repo_id: &str, root: &Path, env: &Env) -> String {
    if dashboard_url(server, env).is_empty() || repo_id.is_empty() || !may_open_browser(env) {
        return String::new();
    }
    let key = format!("add|{server}|{repo_id}");
    let path = config::data_dir(env).join("dashboard_opened.json");
    let mut opened = config::load_json(&path);
    if opened.get(&key).is_some() {
        return String::new();
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if !opened.is_object() {
        opened = json!({});
    }
    opened[key.as_str()] = json!(stamp);
    save_json(&path, &opened);
    let app = crate::back::host_app();
    let url = add_repo_url(server, repo_id, app.as_ref().map(|a| a.name.as_str()).unwrap_or(""), env);
    if url.is_empty() || !open_url(&url) {
        return String::new();
    }
    if let Some(app) = &app {
        crate::back::spawn_watch(root, repo_id, app, env);
    }
    url
}

/// Whether a session start asks the person where a new repo goes: the
/// server says none of their workspaces has it, it is not a resumed
/// session, and a person is there to answer. A headless run (`claude -p`,
/// `codex exec`, CI) asks nothing: nobody would see the page, and the app
/// it runs in would be pulled to the front for nothing.
fn asks_where(brief: &Value, machine_local: bool, resume: bool, headless: bool) -> bool {
    brief.get("new_repo").and_then(Value::as_bool) == Some(true) && !machine_local && !resume && !headless
}

/// The site's add-repo page for `repo_id`: `from` names the app the person
/// goes back to once it is added. Signed out, the page sends them through
/// sign-in and back to it.
fn add_repo_url(server: &str, repo_id: &str, from: &str, env: &Env) -> String {
    let base = dashboard_url(server, env);
    let Some(site) = base.strip_suffix("/dashboard") else { return String::new() };
    if repo_id.is_empty() {
        return String::new();
    }
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    let mut url = format!("{site}/add-repo?repo={}", encode(repo_id));
    if !from.is_empty() {
        url.push_str(&format!("&from={}", encode(from)));
    }
    url
}

/// The default browser on `url`; whether it launched.
pub(crate) fn open_url(url: &str) -> bool {
    let mut command = if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" });
        c.arg(url);
        c
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command.spawn().is_ok()
}

/// How to hand work on, said last at session start, where every session
/// reads it (nothing is written into the repo's own files); and, unless the
/// person switched it off, the line its pull requests end with, to a
/// harness whose hooks cannot add it themselves (prbadge.rs).
fn on_here_line(env: &Env, harness: &str) -> String {
    let mut line = format!(
        "Cork is on here. When the user wants work handed to another agent or session (\"send this to Cork\", a spec, \
a task), run `{} \"<self-contained spec>\"`: it reaches their other session open in this repo, which starts on it by \
itself, or the next one they start. Never ask them to copy and paste between agents.",
        crate::machine::own_command(env, "handoff")
    );
    if crate::machine::pr_badge_on(env) && !crate::prbadge::covers(harness) {
        line.push_str(&format!(
            " When you open a pull request (gh pr create or any tool's), end its description with this line, after any \
other attribution lines: {}",
            crate::machine::PR_BADGE_LINE
        ));
    }
    line
}

fn run_session_start(hook_input: &Value, env: &Env) -> i32 {
    // a signed-in machine still local (signed in on Free before v100) moves
    // over first, so this session already starts on Cork's servers
    let moved = crate::machine::plan_check(env);
    let moved: Vec<String> = moved.into_iter().chain(crate::harnesses::session_start(hook_input, env)).collect(); // tools installed since Cork
    // a resumed or compacted conversation bills from here on: what it said
    // before was billed by the run that said it
    if RESUME_SOURCES.contains(&text(hook_input, "source").as_str()) {
        transcript::start_billing_here(&text(hook_input, "transcript_path"), env);
    }
    // a compacted or cleared context lost every briefing in it: the next
    // call in each repo briefs again (touch.rs)
    if matches!(text(hook_input, "source").as_str(), "compact" | "clear") {
        crate::touch::clear(&text(hook_input, "session_id"), env);
    }
    // what Claude Code calls this session, read again now: a resumed one may
    // have been renamed (every later call reads the kept name)
    transcript::session_name(&text(hook_input, "session_id"), true, env);
    // a machine install updates itself (both binaries) from Cork's
    // releases, now and then; the local server has none to offer
    if crate::machine::installed(env) {
        crate::selfupdate::spawn_if_due(&crate::machine::update_server(env), env);
        // a program that updated itself brings its hook list up to date too
        crate::machine::refresh_settings(env);
    }
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
    // a folder that was another repo id (its origin changed): said before
    // anything is reported under the new id, so the server can carry the
    // old repo's history over instead of starting the new id apart
    if !cfg.machine_local {
        crate::renames::report_repo_rename(&root, &cfg, env);
    }
    let (sent, left) = index_repo(&root, &cfg, env, INDEX_BUDGET);
    crate::machine::mark_indexed(&root, &cfg.server, env);
    let mut said: Vec<String> = moved.into_iter().collect();
    // the free version: the other agents are on this machine, and the way
    // to reach them is a command (the MCP tools may not be connected)
    crate::machine::mark_welcomed(&text(hook_input, "session_id"), env);
    if cfg.machine_local && !RESUME_SOURCES.contains(&text(hook_input, "source").as_str()) {
        said.push(crate::machine::free_note(env));
    }
    // a repo covered through this machine's install: the agent may offer
    // the user Cork's AGENTS.md section (it never writes it unasked)
    // (only to a session with a person at it: a headless agent cannot ask)
    let fresh = !RESUME_SOURCES.contains(&text(hook_input, "source").as_str());
    if fresh && crate::machine::installed(env) && !crate::harness::headless(hook_input, env) {
        if let Some(ask) = crate::machine::agents_md_ask(&root, env) {
            said.push(ask);
        }
    }
    // no tip on how to message the others here: said at session start, it
    // made every agent announce its plan to every other (Study 8, second
    // study: 58-141 message batches per round). It is said when the prompt
    // asks to tell others (prompt.rs) or the agent changes code others call
    // (`dependents_note`).
    if left > 0 {
        // a large tree finishes detached rather than holding the session
        if let Ok(exe) = std::env::current_exe() {
            let mut command = std::process::Command::new(exe);
            command
                .arg("index")
                .arg(&root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            let _ = command.spawn();
        }
    }
    // the repo's own history as weekly counts, at most daily, detached
    crate::gitsignal::spawn_baseline_if_due(&root, env);
    // files and folders git saw renamed since the last look, detached
    crate::renames::spawn_if_due(&root, env);
    if sent > 0 || left > 0 {
        let tail = if left > 0 { format!(", {left} more in the background") } else { String::new() };
        said.push(format!("Cork: map indexed {sent} source file(s){tail}."));
    }
    // the briefing: the repo's shape on one page and the recently changed
    // modules as exact facts, printed into the session so the first message
    // starts oriented. After a compaction the context the agent had is gone:
    // the briefing then leads with this session's working set, the last check
    // verdict is said first, and what was briefed before counts as unseen.
    let session = text(hook_input, "session_id");
    let resume = RESUME_SOURCES.contains(&text(hook_input, "source").as_str());
    // `verifies`: each symbol line is checked against this copy before it is
    // printed (mapcheck.rs), so the server may say to build on them
    let mut payload = json!({"repo_id": cfg.repo_id, "budget": BRIEF_BUDGET, "verifies": true});
    // every branch of the repo, by name, on every start (a resumed or
    // compacted session too: most sessions only ever start that way, and
    // the server forgets a list after a week): the dashboard's branch
    // switcher lists them all, not only the ones an agent is on now
    if let Some(map) = payload.as_object_mut() {
        map.insert("branches".into(), json!(git::branches(&root)));
        if !resume {
            add_ide(map, &root, env);
        }
    }
    if resume {
        if let Some(map) = payload.as_object_mut() {
            map.insert("session".into(), json!(session));
            map.insert("resume".into(), json!(true));
        }
        crate::prompt::forget_briefed(&session, env);
        let line = crate::check::resume_line(&root, &session, env);
        if !line.is_empty() {
            said.push(line);
        }
    }
    // the worktrees agent tools make for parallel sessions (any tool's): why
    // this checkout doesn't need one, and tidying the finished ones
    // (detached, never one with work in it)
    if !resume {
        if let Some(note) = crate::worktrees::session_start(&root, env) {
            said.push(note);
        }
    }
    if !cfg.machine_local {
        // where this repo lives, for the wake watcher; a listener that hands
        // teammates' messages to this session the moment they are sent
        crate::listen::note_checkout(&root, &cfg.server, &cfg.repo_id, env);
        crate::listen::spawn_for_session(&root, &session, env);
        if let Some(note) = crate::wake::woken_note(&cfg.repo_id, env) {
            said.push(note);
        }
    } else if crate::proto::enabled(&root, &cfg, env) {
        // the local server with the protocol switched on: the session's daemon only
        crate::proto::spawn_daemon(&root, &session, env);
    }
    // the handoffs that waited for a session of this person here, taken by
    // this one as its to-do
    let picked = take_handoffs(&cfg, &session);
    for handoff in &picked {
        let from = text(handoff, "from");
        // its reply to the sender is the person's sign it is done (pings.rs)
        crate::pings::took_handoff(env, &root, &session, &from, &text(handoff, "message"));
        said.push(format!(
            "Cork: a handoff waited for this session, from {from} ({}): it is your to-do now. Start on it, tell the user \
in one line that you picked it up, and when it is done tell the sender: `{} --to {from} \"...\"`.{}\n{}",
            crate::listen::age_words(handoff.get("age_s").and_then(Value::as_f64).unwrap_or(0.0)),
            crate::machine::own_command(env, "message"),
            // who Cork says sent it, which its own words cannot change
            crate::listen::provenance(handoff),
            text(handoff, "message").trim(),
        ));
    }
    // a session with a person at it: the server says, on their first start
    // here after a while away, what happened since they left
    if let Some(map) = payload.as_object_mut().filter(|_| !crate::harness::headless(hook_input, env)) {
        if !matches!(text(hook_input, "source").as_str(), "compact" | "clear") {
            map.insert("morning".into(), json!(true));
            map.insert("picked".into(), json!(picked.len()));
        }
    }
    if let Ok(brief) = http::post(&cfg.server, "/brief", &cfg.token, &user_agent(), &payload, BRIEF_TIMEOUT) {
        // briefed on this repo: its first file is not a first touch
        if brief.get("ok").and_then(Value::as_bool) == Some(true) {
            crate::touch::mark(&session, &cfg.repo_id, env);
        }
        // the repo's trunk mode rides every briefing (trunk.rs)
        crate::trunk::remember(&cfg.repo_id, &brief, env);
        let morning: Vec<&str> = brief.get("morning").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        if !morning.is_empty() {
            said.push(format!("{}\nTell the user this in one or two lines before anything else.", morning.join("\n")));
        }
        // a repo no workspace has: the site asks where it goes
        if asks_where(&brief, cfg.machine_local, resume, crate::harness::headless(hook_input, env)) {
            let asked = ask_where_once(&cfg.server, &cfg.repo_id, &root, env);
            if !asked.is_empty() {
                said.push(format!(
                    "Cork: this repo is in none of the user's workspaces yet, so the browser opened to add it \
({asked}). Tell the user in one line to pick its workspace there."
                ));
            }
        }
        // what this copy of each file does not have is never printed
        let checked = crate::mapcheck::apply(&root, &brief).text;
        let text = checked.as_str();
        if !text.is_empty() {
            said.push(text.to_string());
        }
        let paths = brief.get("paths").and_then(Value::as_array).cloned().unwrap_or_default();
        let named = brief.get("named").and_then(Value::as_array).cloned().unwrap_or_default();
        let covered = brief.get("covered").and_then(Value::as_array).cloned().unwrap_or_default();
        crate::prompt::remember_briefed(&session, &paths, &named, text, &covered, env);
    }
    // trunk mode: where work lands here, said over the agent's own "branch
    // first"; and what became of this person's branches
    said.extend(crate::trunk::session_start(&root, &cfg, &session, env));
    said.push(on_here_line(env, crate::harness::current()));
    // once a day on the free version: what Cork did, shown to the person;
    // and a handoff this session picked up, which the person should know of
    let picked_line = picked.first().map(|first| {
        let spec: String = text(first, "message").lines().next().unwrap_or("").chars().take(90).collect();
        let more = if picked.len() > 1 { format!(" (and {} more)", picked.len() - 1) } else { String::new() };
        format!("Cork: this session picked up a handoff{more}: {spec}")
    });
    let summary = [crate::machine::daily_summary(&cfg, env), picked_line].into_iter().flatten().reduce(|a, b| format!("{a}\n{b}"));
    // one emission in whatever shape this harness reads back
    match summary {
        Some(line) if crate::harness::current() == "claude" => {
            // Claude Code shows a systemMessage to the person directly
            let mut out = json!({"systemMessage": line});
            crate::harness::note_injected(&line);
            if !said.is_empty() {
                crate::harness::note_injected(&said.join("\n"));
                out["hookSpecificOutput"] = json!({"hookEventName": "SessionStart", "additionalContext": said.join("\n")});
            }
            println!("{out}");
        }
        other => {
            if let Some(line) = other {
                said.push(format!("{line} Tell the user this in one line."));
            }
            let rendered = crate::harness::render_context("SessionStart", &said.join("\n"));
            if !rendered.is_empty() {
                println!("{rendered}");
            }
        }
    }
    // what already fails before the first write, so later failures can be
    // told apart from it — detached, the session does not wait
    crate::check::spawn_baseline(&root, hook_input, env);
    0
}

/// `index <root>`: the detached tail of a session-start index — no output,
/// long budget.
pub fn index_command(root: &str, env: &Env) -> i32 {
    if root.is_empty() {
        return 0;
    }
    let root = PathBuf::from(root);
    let cfg = config::config(Some(&root), env);
    if cfg.usable() {
        let _ = index_repo(&root, &cfg, env, Duration::from_secs(600));
    }
    0
}

pub fn run(stdin_data: &str, env: &Env) -> i32 {
    let code = run_event(stdin_data, env);
    // everything this event put into the context is carried from now on
    let settle = |env: &Env| {
        let chars = crate::harness::take_injected();
        if chars > 0 {
            if let Ok(input) = serde_json::from_str::<Value>(stdin_data) {
                crate::prompt::note_injected(&text(&input, "session_id"), chars, env);
            }
        }
    };
    // a verdict nothing else carried: emit it alone, in the event's own shape
    let leftover = crate::check::leftover();
    if !leftover.is_empty() {
        let event = serde_json::from_str::<Value>(stdin_data)
            .map(crate::harness::normalize)
            .ok()
            .map(|v| text(&v, "hook_event_name"))
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| "PostToolUse".to_string());
        // ConfigChange reads nothing back, so emitting here would lose the
        // verdict rather than defer it; render_context already returns empty
        // for any event this harness cannot inject into.
        if event != "ConfigChange" {
            let rendered = crate::harness::render_context(&event, &leftover);
            if !rendered.is_empty() {
                println!("{rendered}");
            }
        }
    }
    settle(env);
    code
}

/// The file as it was before an Edit/Write, when the harness hands it over
/// (Claude Code's `originalFile`; a Write that created the file had none).
fn original_file(hook_input: &Value) -> Option<Vec<u8>> {
    let response = hook_input.get("tool_response")?;
    match response.get("originalFile") {
        Some(Value::String(body)) => Some(body.clone().into_bytes()),
        _ if text(response, "type") == "create" => Some(Vec::new()),
        _ => None,
    }
}

fn run_event(stdin_data: &str, env: &Env) -> i32 {
    let started = Instant::now();
    let Ok(hook_input) = serde_json::from_str::<Value>(stdin_data) else { return 0 };
    let hook_input = crate::harness::normalize(hook_input);
    // the permission mode it runs in, which a message it sends to another
    // Claude Code session attests (peer.rs)
    crate::peer::note_mode(&text(&hook_input, "session_id"), &text(&hook_input, "permission_mode"), env);
    if let Ok(mut slot) = HELD_FOR.lock() {
        *slot = Some((env.clone(), text(&hook_input, "session_id")));
    }
    // ConfigChange must return before this: Claude Code discards
    // additionalContext on that event, so a verdict moved into the pending
    // slot here would be emitted into a channel that throws it away — lost,
    // not deferred.
    if text(&hook_input, "hook_event_name") == "ConfigChange" {
        return run_config_change(&hook_input, env);
    }
    // likewise SessionEnd: nothing printed here reaches anyone
    if text(&hook_input, "hook_event_name") == "SessionEnd" {
        return run_session_end(&hook_input, env, started);
    }
    // and StopFailure: a turn that ended on an API error (a usage limit),
    // sent by Claude Code instead of Stop; nothing it prints is read
    if text(&hook_input, "hook_event_name") == "StopFailure" {
        return run_stop(&hook_input, env, started);
    }
    // and SubagentStop: Claude Code shows the subagent's caller nothing of it
    if text(&hook_input, "hook_event_name") == "SubagentStop" {
        return run_subagent_stop(&hook_input, env, started);
    }
    // a call that failed changed nothing: the agent is running, and that is
    // all it reports (Claude Code sends no PostToolUse for it, so a session
    // whose commands failed looked like no agent at all)
    let failed = text(&hook_input, "hook_event_name") == "PostToolUseFailure";
    if failed {
        FAILED_CALL.store(true, std::sync::atomic::Ordering::Relaxed);
    } else {
        crate::check::deliver_pending(&hook_input, env);
    }
    if text(&hook_input, "hook_event_name") == "PostToolBatch" {
        return crate::check::run_post_tool_batch(&hook_input, env);
    }
    if text(&hook_input, "hook_event_name") == "UserPromptSubmit" {
        // a turn starts: the session is busy until its Stop (pings.rs)
        crate::pings::turn_started(&text(&hook_input, "session_id"), env);
        // a release reaches a machine at work at its next prompt, at most
        // one check an hour (selfupdate::RETRY_S), not only at session starts
        if crate::machine::installed(env) {
            crate::selfupdate::spawn_if_due(&crate::machine::update_server(env), env);
        }
        // a signed-in machine still local moves over at the next prompt, not
        // only at the next session
        let moved = crate::machine::plan_check(env);
        // a session already open when Cork was installed hears it now, once
        let notes: Vec<String> = moved.into_iter().chain(crate::machine::welcome_once(&hook_input, env)).collect();
        let moved = (!notes.is_empty()).then(|| notes.join("\n\n"));
        let code = crate::prompt::run_user_prompt(&hook_input, env);
        // plain text joins the briefing; a JSON harness gets one object per
        // event, so it hears at its next session start instead
        if let Some(line) = moved.filter(|_| !matches!(crate::harness::current(), "cursor" | "hermes")) {
            let rendered = crate::harness::render_context("UserPromptSubmit", &line);
            if !rendered.is_empty() {
                println!("{rendered}");
            }
        }
        return code;
    }
    if text(&hook_input, "hook_event_name") == "SessionStart" {
        return run_session_start(&hook_input, env);
    }
    if text(&hook_input, "hook_event_name") == "Stop" {
        let code = run_stop(&hook_input, env, started);
        // Cursor's chat cannot be reached from outside; its agent hears a
        // teammate's message as it stops, as the next thing to do
        if crate::harness::current() == "cursor" {
            if let Some(said) = crate::listen::followup(&hook_input, env) {
                crate::harness::note_injected(&said);
                println!("{}", json!({"followup_message": said}));
            }
        }
        return code;
    }
    let tool = text(&hook_input, "tool_name");
    // a pull request the command just opened gets Cork's badge, detached
    if !failed && SHELL_TOOLS.contains(&tool.as_str()) && text(&hook_input, "hook_event_name") == "PostToolUse" {
        crate::prbadge::after_shell(&hook_input, env);
    }
    if !WRITE_TOOLS.contains(&tool.as_str()) && !READ_TOOLS.contains(&tool.as_str()) {
        return 0;
    }
    let cwd = {
        let from_input = text(&hook_input, "cwd");
        if from_input.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(from_input)
        }
    };
    let tool_input = hook_input.get("tool_input").cloned().unwrap_or(Value::Null);
    let session_id = text(&hook_input, "session_id");

    // locate the repo root: the edited file's directory is the strongest hint,
    // then the cwd, then the harness-provided project dir. Paths are reported
    // relative to this root so a drifted cwd cannot fragment a file's identity.
    let mut is_write = !failed && WRITE_TOOLS.contains(&tool.as_str());
    let mut targets: Vec<PathBuf> = Vec::new();
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    // the repo the SESSION works in (its folder, else its project)
    let session_root = config::find_repo_root(&[
        Some(cwd.clone()),
        (!project_dir.is_empty()).then(|| PathBuf::from(project_dir)),
    ]);
    let hint = if is_write {
        targets = {
            let direct = {
                let file_path = text(&tool_input, "file_path");
                if file_path.is_empty() { text(&tool_input, "notebook_path") } else { file_path }
            };
            if !direct.is_empty() {
                vec![PathBuf::from(direct)]
            } else {
                let command = text(&tool_input, "command");
                if command.contains("*** ") {
                    shell::abs_targets(&shell::apply_patch_paths(&command), &cwd)
                } else {
                    Vec::new()
                }
            }
        };
        targets.first().and_then(|t| t.parent().map(Path::to_path_buf))
    } else {
        let read_path = {
            let file_path = text(&tool_input, "file_path");
            if file_path.is_empty() { text(&tool_input, "path") } else { file_path }
        };
        (!read_path.is_empty())
            .then(|| PathBuf::from(&read_path).parent().map(Path::to_path_buf))
            .flatten()
            // a command that works somewhere else (`cd ~/code/app && …`, a
            // file named by its full path) is that folder's repo's activity:
            // what lets a chat started in a project folder be seen at all.
            // Only for a session that works in no repo: a command of a
            // session in a repo is that repo's, whatever paths it names.
            // Study 9: agents in their own clones named paths inside another
            // checkout (Cork's landing command carries CORK_HOME), and
            // were reported into, and heard from, that other repo.
            .or_else(|| {
                (SHELL_TOOLS.contains(&tool.as_str()) && session_root.is_none())
                    .then(|| shell::dir_hint(&text(&tool_input, "command"), &cwd, &config::get(env, "HOME")))
                    .flatten()
            })
    };

    let root = config::find_repo_root(&[
        hint,
        Some(cwd.clone()),
        (!project_dir.is_empty()).then(|| PathBuf::from(project_dir)),
    ])
    .unwrap_or_else(|| {
        if project_dir.is_empty() { cwd.clone() } else { PathBuf::from(project_dir) }
    });

    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }

    // The pull runs on EVERY hook fire, not only writes: reads are the
    // majority of an agent's tool calls, and a session that only read used to
    // drift arbitrarily far behind with no signal. Cost is bounded — the
    // network fetch is throttled and the local fast-forward is microseconds.
    let sync_note = git::sync_note_with(&root, git::auto_pull_enabled(&root, &config::get(env, "CORK_AUTO_PULL")), crate::land::lands(&root, env));
    // commits that arrived from elsewhere become ops on the protocol (detached)
    crate::proto::maybe_ingest(&root, &cfg, env, &session_id);
    if crate::proto::enabled(&root, &cfg, env) {
        if let Ok(mut slot) = NOTICES_FOR.lock() {
            *slot = Some((env.clone(), session_id.clone()));
        }
    }
    // the folder's branch moved since this session's last step, and not by
    // this step: said once, with whatever else this call has to say
    let step_command = if SHELL_TOOLS.contains(&tool.as_str()) { text(&tool_input, "command") } else { String::new() };
    let switched = crate::switches::observe(&root, &session_id, &step_command, env);
    // trunk mode: a branch this step made is reminded of once, never held
    let made = if failed || step_command.is_empty() { String::new() } else { crate::trunk::after_command(&root, &cfg, &session_id, &step_command, env) };
    let switched = crate::prompt::join_notes(&[&switched, &made]);
    if !switched.is_empty() {
        if let Ok(mut slot) = SWITCHED.lock() {
            *slot = switched;
        }
    }

    let transcript_path = text(&hook_input, "transcript_path");
    let mut meta = transcript::turn_meta_in(&transcript_path, env);
    // the message that made this call replayed the context once, however
    // many tool calls it made; a subagent's message replayed its own
    let worker = text(&hook_input, "agent_id");
    let message = meta.remove("turn_id").and_then(|id| id.as_str().map(str::to_string)).unwrap_or_default();
    crate::prompt::note_call(&session_id, !worker.is_empty(), &message, env);
    // what the message that made this call cost, for the dashboard's line
    // only (`step`): a subagent's call reads its own transcript
    let step = if worker.is_empty() {
        transcript::step_of(&meta, &message)
    } else {
        let own = transcript::subagent_transcript(&text(&hook_input, "agent_transcript_path"), &transcript_path, &worker);
        let own_meta = transcript::turn_meta(&own);
        transcript::step_of(&own_meta, own_meta.get("turn_id").and_then(Value::as_str).unwrap_or(""))
    };
    // the usage is billed from the transcript at Stop, every message once;
    // a call carries only the context it was made in, which prices a saving
    for key in transcript::USAGE_KEYS {
        meta.remove(key);
    }
    if let Some(step) = step {
        meta.insert("step".into(), step);
    }
    // a long turn bills what it has spent so far every few minutes, so the
    // agent-hours meter moves mid-turn (a subagent's transcript is billed
    // by its own SubagentStop and the sweep)
    if worker.is_empty() && transcript::bill_due(&transcript_path, env) {
        let usage = transcript::take_unbilled(&transcript_path, env);
        if !usage.is_empty() {
            let bill = json!({"repo_id": cfg.repo_id, "session": session_id, "agent": user_agent(), "action": "usage", "usage": usage});
            // a short timeout: this rides a tool call the agent is waiting on;
            // what does not land now is owed and goes with the next bill
            if bill_landed(&http::post(&cfg.server, "/presence", &cfg.token, &user_agent(), &bill, Duration::from_millis(800))) {
                transcript::billed(&transcript_path, &usage, env);
            }
        }
    }
    // what the harness calls this conversation, for the roster
    let title = transcript::title(&transcript_path, env);
    if !title.is_empty() {
        meta.insert("title".into(), json!(title));
    }
    // and what it is doing now: the item its todo list has in progress, else
    // what its latest prompt asked. Its work is labelled by that, never by a
    // title generated for an earlier task
    if tool == "TodoWrite" {
        transcript::note_task(&session_id, &transcript::todo_in_progress(&tool_input), env);
    }
    let task = transcript::current_task(&session_id, env);
    if !task.is_empty() {
        meta.insert("task".into(), json!(task));
    }
    // the session's stated intent rides on the protocol ops it makes, unless
    // the workspace keeps titles (and tasks) off its servers
    let intent = if task.is_empty() { &title } else { &task };
    if !intent.is_empty() {
        if crate::privacy::switches(&cfg.server, &cfg.repo_id).session_titles {
            crate::proto::note_intent(env, &session_id, intent);
        } else {
            crate::proto::forget_intent(env, &session_id);
        }
    }
    // and what Claude Code calls it (its ListAgents' name, which a message
    // `--to` takes), and the harness it runs in
    let cc_name = transcript::session_name(&session_id, false, env);
    if !cc_name.is_empty() {
        meta.insert("cc_name".into(), json!(cc_name));
    }
    meta.insert("harness".into(), json!(crate::harness::current()));
    // what another session posted into this one through Claude Code, as its
    // transcript shows it arrived: Cork holds its own copy until told
    let peer_seen = transcript::peer_seen(&session_id, &transcript_path, env);
    if !peer_seen.is_empty() {
        meta.insert("peer_seen".into(), json!(peer_seen));
    }
    // Which conversation made this edit. Claude Code hands every hook a
    // session_id, and a subagent has its own — so forwarding it is what lets
    // the ledger tell one agent's spend from another's when several work
    // under one login. Without it every agent on a machine collapses into a
    // single row and the per-agent economics are unmeasurable.
    let session = text(&hook_input, "session_id");
    if !session.is_empty() {
        meta.insert("session".into(), json!(session));
    }
    // a tool call made inside a subagent carries the parent's session_id and
    // its own agent_id: the worker. Forwarded so the server can count how
    // many hands one conversation has on the repo right now.
    if !worker.is_empty() {
        meta.insert("worker".into(), json!(worker));
        let worker_type = text(&hook_input, "agent_type");
        if !worker_type.is_empty() {
            meta.insert("worker_type".into(), json!(worker_type));
        }
        // and the name it was started with, the dashboard's label for it
        let worker_name = transcript::worker_name(&transcript_path, &worker);
        if !worker_name.is_empty() {
            meta.insert("worker_name".into(), json!(worker_name));
        }
    }
    let branch = git::branch(&root);
    if !branch.is_empty() {
        meta.insert("branch".into(), json!(branch));
    }
    // where its pushes land, when they land through Cork (branches.rs)
    let (_, target) = crate::branches::of(&root, env);
    if !target.is_empty() {
        meta.insert("target".into(), json!(target));
    }
    add_ide(&mut meta, &root, env);
    let machine = machine_id(env);
    if !machine.is_empty() {
        meta.insert("machine".into(), json!(machine));
        // which working copy this is: two sessions in one checkout share
        // every change on disk; two checkouts only share what is pushed
        let checkout = sha256_hex(format!("{machine}\n{}", root.display()).as_bytes());
        meta.insert("checkout".into(), json!(checkout.get(..16).unwrap_or("")));
    }
    // the git origin as a repo id: when it disagrees with the committed config
    // the server treats it as a rename and hands back preferred_repo_id
    let origin = git::origin_repo_id(&root);
    if !origin.is_empty() {
        meta.insert("origin".into(), json!(origin));
    }

    // a shell command that actually changed files is a WRITE, whatever the
    // harness labelled it: what changed while it ran, against the gate's
    // snapshot (shellrun.rs). Without one, what it names, and the clock's
    // guess at what it never named only while no other session works in
    // this checkout: in a shared one that guess took a teammate's edit.
    let mut befores: std::collections::HashMap<PathBuf, Vec<u8>> = std::collections::HashMap::new();
    let mut journal_targets: Vec<PathBuf> = Vec::new();
    if !is_write && SHELL_TOOLS.contains(&tool.as_str()) {
        let command = text(&tool_input, "command");
        let call = crate::shellrun::call_key(&text(&hook_input, "tool_use_id"), &command);
        let proven = if session_id.is_empty() {
            None
        } else {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::shellrun::finish(&root, &session_id, &call, env)))
                .ok()
                .flatten()
        };
        let shell_targets: Vec<PathBuf> = match proven {
            Some(proven) => proven.files.into_iter().map(|(rel, before)| {
                let target = root.join(&rel);
                if let Some(bytes) = before {
                    befores.insert(target.clone(), bytes);
                }
                target
            }).collect(),
            None if command.trim().is_empty() || shell::read_only(&command) => Vec::new(),
            None => {
                let since = crate::journal::now_s() - shell::SHELL_WRITE_WINDOW_S;
                let alone = session_id.is_empty() || !crate::shellrun::others_at_work(&root, &session_id, since, env);
                shell::shell_write_targets_with(&command, &cwd, &root, alone)
                    .into_iter()
                    .filter(|t| {
                        alone || config::rel_path(t, &root).is_some_and(|rel| !crate::shellrun::other_wrote(&root, &rel, &session_id, since, env))
                    })
                    .collect()
            }
        };
        if !shell_targets.is_empty() {
            is_write = true;
            // every file is journaled; the server hears the first few now
            // and is owed the rest, which go with the next run's budget
            journal_targets = shell_targets.clone();
            targets = shell_targets;
            if targets.len() > shell::MAX_SHELL_WRITE_FILES {
                for rest in &targets[shell::MAX_SHELL_WRITE_FILES..] {
                    if let Some(rel) = config::rel_path(rest, &root).filter(|rel| !is_collide_artifact(rel) && !is_build_output(rel)) {
                        crate::prompt::note_touch(&session_id, "written", &rel, env);
                        crate::spool::remember(env, &cfg.server, &cfg.repo_id, &root, &rel, &meta);
                    }
                }
                targets.truncate(shell::MAX_SHELL_WRITE_FILES);
            }
        }
    }
    if journal_targets.is_empty() {
        journal_targets = targets.clone();
    }

    // the first call in a repo this session was not briefed on: the small
    // briefing rides in on this call's result (touch.rs). A call in the
    // session's own repo, or any later call, is a local file read. It has
    // its own short allowance, not what the call's budget has left: a slow
    // git fetch before it left none, and the repo was never briefed.
    if !failed && !session_id.is_empty() {
        let touched: Vec<String> = if is_write {
            targets.iter().filter_map(|t| config::rel_path(t, &root)).collect()
        } else {
            let read_path = {
                let file_path = text(&tool_input, "file_path");
                if file_path.is_empty() { text(&tool_input, "path") } else { file_path }
            };
            (!read_path.is_empty()).then(|| config::rel_path(Path::new(&read_path), &root)).flatten().into_iter().collect()
        };
        let touched: Vec<String> = touched.into_iter().filter(|rel| rel != "." && !is_collide_artifact(rel)).collect();
        let said = crate::touch::note(&cfg, &session_id, &touched, is_write, crate::touch::TIMEOUT, env);
        if !said.is_empty() {
            if let Ok(mut slot) = FIRST_TOUCH.lock() {
                *slot = said;
            }
        }
    }

    // which lines this session wrote, journaled on this machine only, so a
    // commit in a shared checkout can take this session's hunks and no one
    // else's (journal.rs). Local and bounded; a failure records nothing.
    // A helper's lines are its own: to its lead and its siblings, a
    // teammate's (lines.rs)
    if is_write && !session_id.is_empty() {
        let worker_name = if worker.is_empty() { String::new() } else { transcript::worker_name(&transcript_path, &worker) };
        let original = (targets.len() == 1 && matches!(tool.as_str(), "Edit" | "Write" | "MultiEdit"))
            .then(|| original_file(&hook_input))
            .flatten();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for target in &journal_targets {
                let Some(rel) = config::rel_path(target, &root) else { continue };
                if is_collide_artifact(&rel) || is_build_output(&rel) {
                    continue;
                }
                let before = befores.get(target).cloned().or_else(|| original.clone());
                crate::journal::record(&root, target, &rel, &session_id, &worker, &worker_name, &title, before, env);
            }
        }));
    }

    // local work is not Cork's to spend: git (the fetch, and every spawn
    // that reads the branch, the origin or the ignore rules, 50-150 ms each
    // on Windows) and the disk are discounted, so the budget bounds the
    // network alone and a slow machine can never starve the post that follows
    let local = std::cell::Cell::new(started.elapsed());
    let budget_left = || {
        TOTAL_BUDGET
            .checked_sub(started.elapsed().saturating_sub(local.get()))
            .unwrap_or_default()
    };

    if is_write {
        // the protocol's ops for this write go out beside the report: the
        // map keeps learning from /report, the protocol is additive
        let protocol = WRITE_TOOLS.contains(&tool.as_str()) && !session_id.is_empty() && crate::proto::enabled(&root, &cfg, env);
        let proto_ctx = crate::proto::Ctx { root: &root, cfg: &cfg, env, session: &session_id };
        let proto_targets = targets.clone();
        return std::thread::scope(|scope| {
            let refused = protocol.then(|| scope.spawn(|| crate::proto::report(&proto_ctx, &proto_targets)));
            let mut context = String::new();
            let agents_version = agents_block_version(&root);
            let digest = settings_digest(&root);
            let payload_for = |rel: &str, content: &str, meta: &Map<String, Value>| -> Value {
                let mut payload = meta.clone();
                payload.insert("repo_id".into(), json!(cfg.repo_id));
                payload.insert("path".into(), json!(rel));
                payload.extend(cfg.source_fields(rel, content));
                payload.insert("hook_version".into(), json!(HOOK_VERSION));
                payload.insert("agents_version".into(), json!(agents_version));
                payload.insert("settings_digest".into(), json!(digest));
                Value::Object(payload)
            };
            // reports an earlier run could not deliver go first, as the files
            // are now, with a third of this run's budget at most
            local.set(started.elapsed());
            let resend_budget = budget_left() / 3;
            // the files this run reports live are owed no longer
            let covered: Vec<String> = targets.iter().filter_map(|t| config::rel_path(t, &root)).collect();
            crate::spool::resend(env, &cfg.server, &cfg.repo_id, &cfg.token, &user_agent(), resend_budget, &root, &covered,
                &|rel, content, meta| (!is_collide_artifact(rel)).then(|| payload_for(rel, content, meta)));
            for target in &targets {
                let Some(rel) = config::rel_path(target, &root) else { continue };
                if is_collide_artifact(&rel) {
                    continue;
                }
                // before note_touch as well: a scratch file is not the session's
                // work either, and counting it would flatter the hit rate
                let git_started = Instant::now();
                let ignored = is_build_output(&rel) || git::is_ignored(&root, &rel);
                local.set(local.get() + git_started.elapsed());
                if ignored {
                    continue;
                }
                crate::prompt::note_touch(&session_id, "written", &rel, env);
                // a subagent's write saved nothing in the parent's context
                if let (true, Ok(meta)) = (worker.is_empty(), std::fs::metadata(target)) {
                    crate::prompt::note_written_avoided(&session_id, &rel, meta.len(), env);
                }
                match std::fs::metadata(target) {
                    Ok(meta) if meta.len() > MAX_FILE_BYTES => continue,
                    Err(_) => continue, // a deleted file (apply_patch delete) or unreadable
                    _ => {}
                }
                let Ok(bytes) = std::fs::read(target) else { continue };
                let content = text_of_source(&bytes);
                if budget_left().is_zero() {
                    break;
                }
                let payload = payload_for(&rel, &content, &meta);
                let response = match http::post(&cfg.server, "/report", &cfg.token, &user_agent(), &payload, budget_left()) {
                    Ok(response) => {
                        crate::spool::forget(env, &cfg.server, &cfg.repo_id, &root, &rel);
                        response
                    }
                    // the server did not get it: owed, and sent again next run
                    Err(()) => {
                        crate::spool::remember(env, &cfg.server, &cfg.repo_id, &root, &rel, &meta);
                        Value::Object(Default::default())
                    }
                };
                // a lint finding stands only where this file really uses the
                // old name or a call that no longer fits
                let response = biting(&response, &content);

                // a certain interface change: its dependents' tests run detached
                // and grade the change on the graph; nothing of it reaches the agent
                if let Some(plan) = response.get("verify").filter(|p| p.get("tests").and_then(Value::as_array).is_some_and(|t| !t.is_empty())) {
                    crate::verify::spawn(&root, &session_id, plan);
                }
                let fix_note =
                    apply_mechanical(&response, &root, target, &rel, &content, &cfg, &meta, budget_left());
                // a change the pre-write gate or the protocol's push already
                // told this session about (with the lines, arity-checked) is
                // not said again by the name-only lint
                let mut told_facts = crate::deps::Told::load(env, &crate::proto::agent_of(&session_id, &worker));
                let told = told_facts.told_names();
                // and a finding this session already heard for this file is not
                // said again on its next edit of it: each fact once
                let fresh = once_per_session(&strip_told(&response, &told), &rel, &root, &mut told_facts);
                let mut lint_note = lint_context(&fresh, !fix_note.is_empty(), &root, crate::land::lands(&root, env));
                // a break Cork named earlier in this file, that this write
                // fixed: a catch, counted (pings.rs). A finding that names no
                // line the change makes wrong (a renamed parameter, same
                // arity) is no break, and counts nothing (v104's test)
                let pending = crate::pings::pending_break(env, &session_id, &rel);
                if !pending.is_empty() {
                    let fixes: Vec<crate::fixlines::Fix> = pending.iter().filter_map(crate::fixlines::from_finding).collect();
                    if crate::fixlines::render_lines(&rel, &content, &fixes).is_empty() {
                        let by = pending.iter().map(|f| text(f, "by")).find(|b| !b.is_empty()).unwrap_or_default();
                        crate::pings::caught(env, &root, &crate::pings::catch_line(&rel, &by, "a call a teammate's change broke, fixed"));
                        crate::pings::clear_break(env, &session_id, &rel);
                    }
                }
                // the lines of this file each finding makes wrong (fixlines.rs)
                if !lint_note.is_empty() {
                    let findings: Vec<Value> = fresh.get("findings").and_then(Value::as_array).cloned().unwrap_or_default();
                    let fixes: Vec<crate::fixlines::Fix> = findings.iter().filter_map(crate::fixlines::from_finding).collect();
                    let lines = crate::fixlines::render_lines(&rel, &content, &fixes);
                    if !lines.is_empty() {
                        lint_note = format!("{lint_note}\n{lines}");
                        crate::pings::note_break(env, &session_id, &rel, &findings);
                    }
                }
                let rename_note = adopt_preferred_repo_id(&response, &root, &cfg.repo_id);
                let awareness = if context.is_empty() { awareness_context(&response) } else { context.clone() };
                let pieces = [
                    fix_note,
                    lint_note,
                    rename_note,
                    setup_note(&response, &root),
                    sync_note.clone(),
                    artifacts_note(&response, &root, &cfg.server, env),
                    retired_note(&response, &root),
                    ask_why_note(&response),
                    dependents_note(&response, &crate::proto::agent_of(&session_id, &worker), env),
                    prefetch_note(&response, &session_id, env),
                    awareness,
                    crate::machine::inbox(&response),
                ];
                let mut kept: Vec<String> = Vec::new();
                for piece in pieces.into_iter().filter(|p| !p.is_empty()) {
                    if !kept.contains(&piece) {
                        kept.push(piece);
                    }
                }
                if !kept.is_empty() {
                    context = kept.join("\n");
                }
            }
            // a test run that also left files behind (`pytest 2>&1 | tail` just
            // after an edit reads as a shell write): its verdict still counts,
            // and it goes after the writes it judged
            if SHELL_TOOLS.contains(&tool.as_str()) && !budget_left().is_zero() {
                let command = text(&tool_input, "command");
                let failed = FAILED_CALL.load(std::sync::atomic::Ordering::Relaxed);
                if let Some(ok) = test_verdict(&command, &shell_output(&hook_input), failed) {
                    let mut payload = meta.clone();
                    payload.insert("repo_id".into(), json!(cfg.repo_id));
                    payload.insert("path".into(), json!(shown_command(command.trim())));
                    payload.insert("action".into(), json!("running"));
                    payload.insert("tests_ok".into(), json!(ok));
                    let _ = http::post(&cfg.server, "/presence", &cfg.token, &user_agent(), &Value::Object(payload), budget_left());
                }
            }
            // additionalContext lands in the agent's session: awareness arrives
            // with the write, not at the next voluntary check; what the protocol
            // refused leads
            let refused = refused.and_then(|h| h.join().ok()).unwrap_or_default();
            emit(&crate::prompt::join_notes(&[&refused, &context]))
        });
    }

    // presence only: where the agent is looking, or what it is RUNNING.
    // Commands are presence too, but scoped to THIS repo — a command that cd's
    // into another repo is that repo's activity, not ours.
    if tool == "Bash" {
        let command = text(&tool_input, "command").trim().to_string();
        // a plain push went through (a landing promotes its own): the
        // session's ops on what it pushed are proven
        if !FAILED_CALL.load(std::sync::atomic::Ordering::Relaxed) && command.contains("git push") {
            crate::proto::promote_after_push(&root, &cfg, env, &session_id);
        }
        if command.is_empty()
            || shell::command_leaves_repo(&command, &cwd, &root, config::get(env, "HOME"))
            || budget_left().is_zero()
        {
            return emit(&sync_note);
        }
        // git's own trouble, as counts: a conflict, a rejected push, a revert
        // (the waste report's baseline; posts only when there is one)
        crate::gitsignal::observe(
            &command, &shell_output(&hook_input), FAILED_CALL.load(std::sync::atomic::Ordering::Relaxed), &cfg, &session_id,
            budget_left(),
        );
        // the map's answer to the name the agent just searched for
        let sync_note = crate::prompt::join_notes(&[&sync_note, &crate::prompt::grep_companion(&command, &cfg, &root, budget_left(), &session_id, !worker.is_empty(), env)]);
        // what the command DISPLAYED joins the map, the way a Read does:
        // one file as a single observation, several as a batch
        let shown: Vec<Value> = shell::shell_read_targets(&command, &cwd, &root)
            .iter()
            .filter_map(|full| {
                let rel = config::rel_path(full, &root)?;
                if is_collide_artifact(&rel) {
                    return None;
                }
                let content = source_of(full)?;
                crate::prompt::note_touch(&session_id, "read", &rel, env);
                let mut file = cfg.source_fields(&rel, &content);
                file.insert("path".into(), json!(rel));
                Some(Value::Object(file))
            })
            .collect();
        if !shown.is_empty() {
            let mut payload = meta.clone();
            payload.insert("repo_id".into(), json!(cfg.repo_id));
            if shown.len() == 1 {
                if let Some(file) = shown[0].as_object() {
                    payload.extend(file.clone());
                }
            } else {
                payload.insert("files".into(), json!(shown));
            }
            let _ = http::post(
                &cfg.server, "/observe", &cfg.token, &user_agent(), &Value::Object(payload),
                budget_left(),
            );
            return emit(&sync_note);
        }
        let mut payload = meta.clone();
        payload.insert("repo_id".into(), json!(cfg.repo_id));
        payload.insert("path".into(), json!(shown_command(&command)));
        payload.insert("action".into(), json!("running"));
        // the repo's tests ran here: their verdict is how Cork knows a
        // task's work passed, with no commit involved
        let failed = FAILED_CALL.load(std::sync::atomic::Ordering::Relaxed);
        if let Some(ok) = test_verdict(&command, &shell_output(&hook_input), failed) {
            payload.insert("tests_ok".into(), json!(ok));
        }
        let answer = http::post(
            &cfg.server,
            "/presence",
            &cfg.token,
            &user_agent(),
            &Value::Object(payload),
            budget_left(),
        );
        let inbox = answer.map(|a| crate::machine::inbox(&a)).unwrap_or_default();
        return emit(&crate::prompt::join_notes(&[&sync_note, &inbox]));
    }

    let read_path = {
        let file_path = text(&tool_input, "file_path");
        if file_path.is_empty() { text(&tool_input, "path") } else { file_path }
    };
    let (focus, action) = if !read_path.is_empty() {
        // a source file inside the repo joins the map with its content —
        // the same in-memory parse-and-discard as a write; presence rides
        // along server-side. Anything else stays a path-only presence ping.
        if let Some(inside) = config::rel_path(Path::new(&read_path), &root) {
            if is_map_source(&inside) && !budget_left().is_zero() {
                crate::prompt::note_touch(&session_id, "read", &inside, env);
                if let Some(content) = source_of(&root.join(&inside)) {
                    // what it read, the session hears about as it changes
                    crate::proto::follow_read(env, &session_id, &root, &inside, &content);
                    let mut payload = meta.clone();
                    payload.insert("repo_id".into(), json!(cfg.repo_id));
                    payload.insert("path".into(), json!(inside));
                    payload.extend(cfg.source_fields(&inside, &content));
                    let answer = http::post(
                        &cfg.server, "/observe", &cfg.token, &user_agent(),
                        &Value::Object(payload), budget_left(),
                    );
                    let (note, inbox) = answer
                        .map(|a| (crate::prompt::recent_write_note(&a, &inside, crate::land::lands(&root, env)), crate::machine::inbox(&a)))
                        .unwrap_or_default();
                    return emit(&crate::prompt::join_notes(&[&sync_note, &note, &inbox]));
                }
            }
        }
        let labelled = config::rel_path(Path::new(&read_path), &root)
            .or_else(|| external_label(&read_path, env));
        let Some(focus) = labelled else { return emit(&sync_note) };
        (focus, "reading")
    } else {
        let pattern = take_chars(&text(&tool_input, "pattern"), 200);
        if pattern.is_empty() {
            return emit(&sync_note);
        }
        (pattern, "searching")
    };
    if budget_left().is_zero() {
        return emit(&sync_note);
    }
    let mut payload = meta.clone();
    payload.insert("repo_id".into(), json!(cfg.repo_id));
    payload.insert("path".into(), json!(focus));
    payload.insert("action".into(), json!(action));
    let answer = http::post(
        &cfg.server,
        "/presence",
        &cfg.token,
        &user_agent(),
        &Value::Object(payload),
        budget_left(),
    );
    let inbox = answer.map(|a| crate::machine::inbox(&a)).unwrap_or_default();
    emit(&crate::prompt::join_notes(&[&sync_note, &inbox]))
}

/// `add-credentials [path]`: mechanically MERGE a credentials fragment into
/// ~/.collide/credentials.json — never overwrite, so one machine keeps every
/// workspace's tokens.
pub fn add_credentials(source: &str, env: &Env) -> i32 {
    let raw = if source.is_empty() {
        let mut buffer = String::new();
        use std::io::Read;
        let _ = std::io::stdin().read_to_string(&mut buffer);
        buffer
    } else {
        match std::fs::read_to_string(source) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("add-credentials failed: {error}");
                return 1;
            }
        }
    };
    let fragment: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("add-credentials failed: {error}");
            return 1;
        }
    };
    let Some(fragment) = fragment.as_object().filter(|m| !m.is_empty()) else {
        eprintln!("add-credentials: fragment must be a non-empty JSON object");
        return 1;
    };
    let target = config::credentials_path(env);
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // missing or corrupt: start fresh, keeping only the fragment
    let existing = config::load_json(&target);
    let existing = existing.as_object().cloned().unwrap_or_default();
    let mut merged = existing.clone();
    for (key, value) in fragment {
        merged.insert(key.clone(), value.clone());
    }
    let body = match serde_json::to_string_pretty(&Value::Object(merged)) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("add-credentials failed: {error}");
            return 1;
        }
    };
    if let Err(error) = std::fs::write(&target, format!("{body}\n")) {
        eprintln!("add-credentials failed: {error}");
        return 1;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600));
    }
    let added = fragment
        .iter()
        .filter(|(key, value)| existing.get(*key) != Some(*value))
        .count();
    println!("merged {added} credential key(s) into {}", target.display());
    0
}

const INSTALL_NOTICE: &str = "Cork: this machine has no usable hook credential for {server} \
yet, so NOTHING you edit is being reported and every teammate's collision check goes stale \
against your work. Before any other task: call the Cork MCP tool `setup` (repo_id {repo_id}) \
and run the one install command it returns, in a shell at this repo's root. It installs Cork \
once for every repo on this machine, signs the machine in, and retires this repo's old per-repo \
setup (commit what it stages). Tell the human when it is done.";

/// `check-credentials`: print a session-start notice when this machine cannot
/// actually report, silent when it can. Resolves the credential exactly the
/// way the hook does, so what it reports is what the hook will do. Run by
/// the old per-repo setup's session-start command; the argument it passes
/// (the program that ran it) is no longer needed.
pub fn check_credentials(_runner: &str, env: &Env) -> i32 {
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let Some(root) = config::find_repo_root(&[
        std::env::current_dir().ok(),
        (!project_dir.is_empty()).then(|| PathBuf::from(project_dir)),
    ]) else {
        return 0; // not a Cork repo: nothing to say
    };
    let cfg = config::config(Some(&root), env);
    if cfg.server.is_empty() || cfg.repo_id.is_empty() {
        return 0;
    }
    // once per session start: a binary older than the server's hook
    // version fetches its replacement in the background
    crate::selfupdate::spawn_if_due(&cfg.server, env);
    if cfg.token.is_empty() {
        println!("{}", INSTALL_NOTICE.replace("{server}", &cfg.server).replace("{repo_id}", &cfg.repo_id));
    }
    0 // never disturb a session start
}

/// What a shell command printed, as one text: the tool response's streams
/// and a failed call's error.
fn shell_output(hook_input: &Value) -> String {
    let mut out = String::new();
    match hook_input.get("tool_response") {
        Some(Value::String(s)) => out.push_str(s),
        Some(Value::Object(map)) => {
            for key in ["stdout", "stderr", "output", "error"] {
                if let Some(s) = map.get(key).and_then(Value::as_str) {
                    out.push_str(s);
                    out.push('\n');
                }
            }
        }
        _ => {}
    }
    if let Some(s) = hook_input.get("error").and_then(Value::as_str) {
        out.push_str(s);
    }
    out
}

/// A test run's verdict from what the shell printed: Some(true) when the
/// repo's tests ran and passed, Some(false) when they ran and failed, None
/// when the command was no test run or printed nothing a verdict rests on.
/// Twin of report_hook.py's `_test_verdict`.
pub fn test_verdict(command: &str, output: &str, failed: bool) -> Option<bool> {
    let runs = Regex::new(
        r"(?:^|[\s;&|(])(?:pytest|py\.test|python3? -m (?:pytest|unittest)|npm (?:run )?test|yarn test|pnpm (?:run )?test|npx (?:jest|vitest)|jest|vitest|cargo test|go test|rspec|mvn test|gradle test|\./gradlew test|phpunit|dotnet test|mix test)\b",
    )
    .expect("static regex");
    if !runs.is_match(command) {
        return None;
    }
    if failed {
        return Some(false);
    }
    let fails = Regex::new(
        r"(?m)\b[1-9]\d* (?:failed|errors?)\b|^FAILED\b|^FAIL\b|test result: FAILED|Traceback \(most recent call last\)|panicked at",
    )
    .expect("static regex");
    if fails.is_match(output) {
        return Some(false);
    }
    let passes = Regex::new(r"(?m)\b\d+ passed\b|test result: ok\.|^ok\s+\S|Tests:\s+\d+ passed|\bOK \(\d+ tests?\)|^OK$")
        .expect("static regex");
    passes.is_match(output).then_some(true)
}

#[cfg(test)]
mod verdict_tests {
    use super::test_verdict;

    #[test]
    fn a_test_run_s_verdict_is_read_from_what_it_printed() {
        assert_eq!(test_verdict("python3 -m pytest -q 2>&1 | tail -30", "......\n14 passed in 0.02s\n", false), Some(true));
        assert_eq!(test_verdict("cd x && pytest -q", "1 failed, 13 passed in 0.03s", false), Some(false));
        assert_eq!(test_verdict("cargo test", "test result: ok. 5 passed; 0 failed; 0 ignored", false), Some(true));
        assert_eq!(test_verdict("npm test", "", true), Some(false));
        assert_eq!(test_verdict("go test ./...", "ok  \texample.com/m\t0.01s", false), Some(true));
        assert_eq!(test_verdict("git status", "14 passed", false), None);
        assert_eq!(test_verdict("pytest -q", "no tests ran in 0.01s", false), None);
    }
}

#[cfg(test)]
mod add_repo_url_tests {
    use super::*;

    #[test]
    fn the_add_repo_page_is_on_the_site_and_names_the_app_to_go_back_to() {
        let env = Env::new();
        assert_eq!(
            add_repo_url("https://mcp.corkcli.com", "github.com/lithometric/juice", "Visual Studio Code", &env),
            "https://corkcli.com/add-repo?repo=github.com%2Flithometric%2Fjuice&from=Visual%20Studio%20Code"
        );
        assert_eq!(
            add_repo_url("https://mcp.corkcli.com", "github.com/acme/api#dev", "", &env),
            "https://corkcli.com/add-repo?repo=github.com%2Facme%2Fapi%23dev"
        );
        // a server with no known site opens nothing
        assert_eq!(add_repo_url("http://127.0.0.1:8000", "github.com/acme/api", "", &env), "");
    }

    #[test]
    fn only_a_person_at_a_new_session_is_asked_where_a_new_repo_goes() {
        let new = json!({"new_repo": true});
        assert!(asks_where(&new, false, false, false));
        // headless (claude -p, codex exec, CI): nobody to ask
        assert!(!asks_where(&new, false, false, true));
        assert!(!asks_where(&new, false, true, false));
        assert!(!asks_where(&new, true, false, false));
        assert!(!asks_where(&json!({"ok": true}), false, false, false));
    }

    #[test]
    fn codex_exec_and_claude_p_count_as_headless() {
        let mut env = Env::new();
        env.insert("CLAUDE_CODE_ENTRYPOINT".into(), "sdk-cli".into());
        assert!(crate::harness::headless(&json!({}), &env));
        let attended = Env::new();
        assert!(!crate::harness::headless(&json!({"source": "startup"}), &attended));
    }
}

#[cfg(test)]
mod settings_digest_tests {
    use super::*;

    /// The digest's exact value for a fixed settings file. Hooks already in
    /// the field report this value, so a drift fails here rather than
    /// reporting every correct install as stale.
    #[test]
    fn digest_is_stable_for_the_same_settings() {
        let dir = std::env::temp_dir().join("collide-settings-digest-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(
            dir.join(".claude").join("settings.json"),
            r#"{"hooks": {"PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "sh -c \"$CLAUDE_PROJECT_DIR/.collide/report_hook.py\" # collide"}]}, {"matcher": "Read", "hooks": [{"type": "command", "command": "npm run lint"}]}]}, "other": ["collide-gate-hook --harness codex # collide"]}"#,
        )
        .unwrap();
        assert_eq!(settings_digest(&dir), "47fb8f9ef655");

        // no settings file: silence, never a wrong answer
        assert_eq!(settings_digest(&dir.join("nowhere")), "");
    }

    /// Somebody else's hooks are not ours, so merging Cork into a settings
    /// file that already had hooks must not read as drift.
    #[test]
    fn foreign_hooks_do_not_enter_the_digest() {
        let dir = std::env::temp_dir().join("collide-settings-digest-foreign");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(
            dir.join(".claude").join("settings.json"),
            r#"{"hooks": {"PostToolUse": [{"hooks": [{"command": "npm run lint"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(settings_digest(&dir), "");
    }
}

#[cfg(test)]
mod dashboard_tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn the_dashboard_is_the_site_behind_the_mcp_host() {
        assert_eq!(dashboard_url("https://mcp.corkcli.com", &env(&[])), "https://corkcli.com/dashboard");
        assert_eq!(dashboard_url("https://api.example.io/", &env(&[])), "https://example.io/dashboard");
        assert_eq!(dashboard_url("http://localhost:8000", &env(&[])), "");
        assert_eq!(
            dashboard_url("http://localhost:8000", &env(&[("CORK_DASHBOARD_URL", "http://localhost:3000/")])),
            "http://localhost:3000/dashboard"
        );
    }

    #[test]
    fn no_browser_in_ci_over_ssh_or_when_turned_off() {
        let desktop = [("DISPLAY", ":0")];
        assert!(may_open_browser(&env(&desktop)));
        for (key, value) in [("CI", "true"), ("SSH_CONNECTION", "1 2 3 4"), ("CORK_NO_BROWSER", "1"), ("PYTEST_CURRENT_TEST", "x")] {
            assert!(!may_open_browser(&env(&[desktop[0], (key, value)])), "{key}");
        }
        assert!(may_open_browser(&env(&[desktop[0], ("CORK_NO_BROWSER", "0")])));
    }
}

#[cfg(test)]
mod redact_tests {
    use super::*;

    #[test]
    fn secrets_never_leave_in_a_command() {
        let cases = [
            ("STRIPE_KEY=sk_live_abcdef1234567890 npm run seed", "STRIPE_KEY=[redacted] npm run seed"),
            ("curl -H \"Authorization: Bearer ghp_0123456789abcdefghijABCDEFGHIJ\" https://x", "curl -H \"Authorization: Bearer [redacted]\" https://x"),
            ("git clone https://kevin:hunter22@github.com/a/b", "git clone https://kevin:[redacted]@github.com/a/b"),
            ("psql --password s3cret -h db", "psql --password [redacted] -h db"),
            ("npm test", "npm test"),
            ("git log --format=%an -n 3", "git log --format=%an -n 3"),
            ("export OPENAI_API_KEY='abc123'; python x.py", "export OPENAI_API_KEY=[redacted]; python x.py"),
            ("curl 'https://api.x.com/v1?id=4&token=abcdef'", "curl 'https://api.x.com/v1?id=4&token=[redacted]'"),
            ("python3 -m tests.check", "python3 -m tests.check"),
        ];
        for (raw, want) in cases {
            assert_eq!(redact_command(raw), want, "{raw}");
        }
    }
}


/// Hand a "this command has started" note to a detached copy of this
/// program (`started <cwd> <session> <command> <transcript> <worker>
/// <worker_type>`): the gate returns at once. A subagent's command carries
/// the subagent (`worker`, the hook input's agent_id) and its type.
pub fn spawn_started(cwd: &Path, session: &str, command: &str, transcript: &str, worker: &str, worker_type: &str) {
    let command = command.trim();
    if command.is_empty() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut child = std::process::Command::new(exe);
    child
        .arg("started")
        .arg(cwd)
        .arg(session)
        .arg(shown_command(command))
        .arg(transcript)
        .arg(worker)
        .arg(worker_type)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        child.process_group(0);
    }
    let _ = child.spawn();
}

/// `started <cwd> <session> <command>`: tell Cork the agent is running
/// this command now. Silent, short budget, nothing printed.
pub fn started_command(args: &[String], env: &Env) -> i32 {
    let (Some(cwd), Some(session), Some(command)) = (args.get(1), args.get(2), args.get(3)) else { return 0 };
    let Some(root) = config::find_repo_root(&[Some(PathBuf::from(cwd))]) else { return 0 };
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let mut payload = json!({"repo_id": cfg.repo_id, "session": session, "path": command, "action": "running", "started": true});
    // the model, so a new repo's dashboard knows whose agent this is from
    // its first command, not only once one finishes
    let meta = args.get(4).map(|t| transcript::turn_meta_in(t, env)).unwrap_or_default();
    if let Some(model) = meta.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
        payload["model"] = json!(model);
        if let Some(map) = payload.as_object_mut() {
            transcript::copy_mode(&meta, map);
        }
    }
    let branch = git::branch(&root);
    if !branch.is_empty() {
        payload["branch"] = json!(branch);
    }
    if let Some(map) = payload.as_object_mut() {
        add_ide(map, &root, env);
    }
    add_started_worker(&mut payload, args);
    let _ = http::post(&cfg.server, "/presence", &cfg.token, &user_agent(), &payload, Duration::from_secs(5));
    0
}

/// A command a subagent started names it, as every other post does:
/// `worker` (its agent_id), `worker_type`, and `worker_name` (what it was
/// started with, from the transcript) when known.
fn add_started_worker(payload: &mut Value, args: &[String]) {
    let worker = args.get(5).map(|w| w.trim()).unwrap_or("");
    if worker.is_empty() {
        return;
    }
    payload["worker"] = json!(worker);
    let worker_type = args.get(6).map(|t| t.trim()).unwrap_or("");
    if !worker_type.is_empty() {
        payload["worker_type"] = json!(worker_type);
    }
    let name = transcript::worker_name(args.get(4).map(String::as_str).unwrap_or(""), worker);
    if !name.is_empty() {
        payload["worker_name"] = json!(name);
    }
}

#[cfg(test)]
mod started_worker_tests {
    use super::*;

    #[test]
    fn a_subagents_command_names_the_subagent() {
        let args: Vec<String> = ["started", "/repo", "s1", "npm test", "", "agent-7", "Explore"].iter().map(|s| s.to_string()).collect();
        let mut payload = json!({"action": "running"});
        add_started_worker(&mut payload, &args);
        assert_eq!(payload["worker"], "agent-7");
        assert_eq!(payload["worker_type"], "Explore");
        // the main agent's command: no worker fields
        let args: Vec<String> = ["started", "/repo", "s1", "npm test", "", "", ""].iter().map(|s| s.to_string()).collect();
        let mut payload = json!({"action": "running"});
        add_started_worker(&mut payload, &args);
        assert!(payload.get("worker").is_none() && payload.get("worker_type").is_none());
    }

    #[test]
    fn the_subagents_name_comes_from_its_transcript() {
        let dir = std::env::temp_dir().join(format!("collide-started-worker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let transcript = dir.join("sess.jsonl");
        let sub = dir.join("sess").join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(&transcript, "").unwrap();
        std::fs::write(sub.join("agent-agent-7.meta.json"), r#"{"description": "Run the billing tests"}"#).unwrap();
        let args: Vec<String> = ["started", "/repo", "s1", "npm test", &transcript.to_string_lossy(), "agent-7", "general-purpose"]
            .iter().map(|s| s.to_string()).collect();
        let mut payload = json!({});
        add_started_worker(&mut payload, &args);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(payload["worker_name"], "Run the billing tests");
        assert_eq!(payload["worker_type"], "general-purpose");
    }
}

#[cfg(test)]
mod index_choice_tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git").args(args).current_dir(dir)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn the_bulk_index_skips_vendored_code_and_starts_a_huge_repo_from_its_recent_work() {
        let dir = std::env::temp_dir().join(format!("collide-index-choice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@t"]);
        git(&dir, &["config", "user.name", "t"]);
        let write = |rel: &str| {
            let full = dir.join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, format!("def f():\n    return '{rel}'\n")).unwrap();
        };
        for rel in ["app/a.py", "app/b.py", "vendor/lib.py", "crm/packages/x.py", "crm/packages/y.py",
                    "web/app.min.js", "proto/msg_pb2.py", "big/one.py", "big/two.py"] {
            write(rel);
        }
        std::fs::write(dir.join(".gitattributes"), "big/** linguist-vendored\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "seed"]);
        // the work since: app/b.py, then crm/packages/x.py
        std::fs::write(dir.join("app/b.py"), "def g():\n    return 2\n").unwrap();
        git(&dir, &["commit", "-qam", "b"]);
        std::fs::write(dir.join("crm/packages/x.py"), "def h():\n    return 3\n").unwrap();
        git(&dir, &["commit", "-qam", "x"]);

        let env = Env::new();
        let all = index_candidates(&dir, &env);
        assert_eq!(all, vec!["app/a.py", "app/b.py", "crm/packages/x.py", "crm/packages/y.py"], "vendored, generated and linguist-vendored files are skipped");

        let mut capped = Env::new();
        capped.insert("CORK_INDEX_MAX_FILES".into(), "2".into());
        assert_eq!(index_candidates(&dir, &capped), vec!["app/b.py", "crm/packages/x.py"], "past the cap, the recent work first");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod told_lint_tests {
    use super::*;

    /// What the pre-write gate or the protocol's push already said is not
    /// said again by the report's name-only lint; anything else still is.
    #[test]
    fn the_lint_leaves_out_what_the_session_was_told() {
        let response = json!({"findings": [
            {"rule": "signature_drift", "symbol": "compute_tax", "lines": [1, 6], "fix": {"class": "judgment", "prompt": "verify"}},
            {"rule": "stale_name", "symbol": "rate_for", "renamed_to": "rate_of", "lines": [2], "fix": {"class": "mechanical"}},
            {"rule": "removed_symbol_use", "symbol": "Cart.total", "lines": [9], "fix": {"class": "judgment", "prompt": "find it"}},
        ]});
        let told: std::collections::BTreeSet<(String, String)> =
            [("compute_tax".to_string(), "sig".to_string()), ("total".to_string(), "del".to_string())].into();
        let kept = strip_told(&response, &told);
        let rules: Vec<String> = kept["findings"].as_array().unwrap().iter().map(|f| text(f, "rule")).collect();
        assert_eq!(rules, vec!["stale_name".to_string()]);
        assert_eq!(strip_told(&response, &Default::default()), response, "nothing told: the lint as it was");
    }

    /// Lattice test, round 2 (v104): `pickLabels(anchor, opts)` had become
    /// `pickLabels(anchor, opts)` with renamed parameters 14 hours before,
    /// and every call already fit; the lint said "changed its signature" on
    /// each edit. A finding stands only where the file bears it out.
    #[test]
    fn a_finding_the_file_does_not_bear_out_is_dropped() {
        let content = "import { pickLabels, openMenu } from '../menu.js';\n\
pickLabels(anchor, { current, onPick });\nopenMenu(el, items);\nconst cur = issue();\n";
        let response = json!({"findings": [
            {"rule": "signature_drift", "symbol": "pickLabels", "signature": "export function pickLabels(anchor, opts)", "lines": [1, 2]},
            {"rule": "signature_drift", "symbol": "openMenu", "signature": "export function openMenu(anchor, items, opts = {})", "lines": [3]},
            {"rule": "signature_drift", "symbol": "openMenu", "signature": "", "lines": [3]},
            {"rule": "stale_name", "symbol": "cur", "renamed_to": "current", "lines": [4]},
            {"rule": "stale_name", "symbol": "pickPriority", "renamed_to": "choosePriority", "lines": [9]},
            {"rule": "signature_drift", "symbol": "pickEstimate", "signature": "export function pickEstimate(a, b, c)", "lines": [2]},
        ]});
        let kept = biting(&response, content);
        let kept: Vec<(String, String)> =
            kept["findings"].as_array().unwrap().iter().map(|f| (text(f, "rule"), text(f, "symbol"))).collect();
        // `openMenu(el, items)` fits 2-3 params; `pickLabels(anchor, {...})` fits 2;
        // no signature to check against; `cur` is used; `pickPriority` and
        // `pickEstimate` are not in the file
        assert_eq!(kept, vec![("stale_name".to_string(), "cur".to_string())]);
        // a call that no longer fits still stands
        let tight = json!({"findings": [{"rule": "signature_drift", "symbol": "openMenu", "signature": "function openMenu(anchor, items, opts, more)", "lines": [3]}]});
        assert_eq!(biting(&tight, content)["findings"].as_array().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod pr_badge_tests {
    use super::*;

    /// A harness whose hooks cannot add the badge (Cursor, Hermes) asks the
    /// agent to end its pull requests with the line, unless the person said
    /// `cork badge off` or set `CORK_PR_BADGE=0`; where the hooks add it
    /// (Claude Code, Codex) the agent is not asked.
    #[test]
    fn the_badge_line_is_asked_for_only_where_the_hooks_cannot_add_it() {
        let home = std::env::temp_dir().join(format!("cork-pr-badge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        env.insert("CORK_HOME".into(), home.to_string_lossy().to_string());
        let asked = |env: &Env| on_here_line(env, "cursor").contains(crate::machine::PR_BADGE_LINE);
        let line = on_here_line(&env, "cursor");
        assert!(line.starts_with("Cork is on here.") && !line.contains('\n'), "{line}");
        assert!(line.ends_with("after any other attribution lines: [![coordinated by cork](https://corkcli.com/badge.svg)](https://corkcli.com)"), "{line}");
        assert!(asked(&env) && on_here_line(&env, "hermes").contains(crate::machine::PR_BADGE_LINE));
        for covered in ["claude", "codex"] {
            let line = on_here_line(&env, covered);
            assert!(line.starts_with("Cork is on here.") && !line.contains("pull request"), "{covered}: {line}");
        }
        // `cork badge off`, kept in machine.json beside what is there
        crate::machine::save_settings(&env, &json!({"mode": "local"}));
        assert_eq!(crate::machine::badge(&["off".to_string()], &env), 0);
        assert!(!asked(&env));
        assert_eq!(crate::machine::settings(&env)["mode"], "local");
        assert_eq!(crate::machine::badge(&["on".to_string()], &env), 0);
        assert!(asked(&env));
        // the environment switch, whatever the file says
        env.insert("CORK_PR_BADGE".into(), "0".into());
        assert!(!asked(&env));
        assert!(on_here_line(&env, "cursor").starts_with("Cork is on here."));
        let _ = std::fs::remove_dir_all(&home);
    }
}
