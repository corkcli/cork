//! The repo check, run after each batch of writes — the Rust twin of the
//! Python hook's `--check` path, byte-for-byte in what it says.
//!
//! The check (`verify` in .collide/config.json) runs DETACHED: the batch
//! hook spawns `collide-hook check …` and returns, waiting only as long as
//! the check is known to take (capped at a second) so a fast check lands in
//! the same step. The verdict is written into the session's state file
//! (~/.collide/check/<session>.json, shared with the Python hook) and
//! delivered inside the next hook event of any kind. Classified, never raw:
//! newly failing (yours, or another agent's recent write), already failing
//! before you started, alternating, same as before, pending a file you have
//! not written yet, or flaky in this repo (flaky.rs: a quick failing check
//! runs once more on the same tree, and what flips is shared with the team).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::config::{self, Env};
use crate::flaky;
use crate::http;
use crate::report::{save_json, text, user_agent, MAP_EXTS, WRITE_TOOLS};

pub const CHECK_MAX_S: f64 = 300.0;
const CHECK_GRACE_CAP_S: f64 = 1.0;
const CHECK_TAIL_CHARS: usize = 2000;
const CHECK_KEY_LINES: usize = 6;
const CHECK_RECENT_S: f64 = 600.0;

/// A delivered verdict waiting to ride the event's own output.
static PENDING: OnceLock<Mutex<String>> = OnceLock::new();

fn pending() -> &'static Mutex<String> {
    PENDING.get_or_init(|| Mutex::new(String::new()))
}

/// Prepend the pending verdict (once) to what an event is about to emit.
pub fn with_pending(context: &str) -> String {
    let mut slot = pending().lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_empty() {
        return context.to_string();
    }
    let out = if context.is_empty() { slot.clone() } else { format!("{slot}\n{context}") };
    slot.clear();
    out
}

/// Whatever is still pending after the event ran (it emitted nothing).
pub fn leftover() -> String {
    let mut slot = pending().lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *slot)
}

const DELTAS_BUDGET: u64 = 1_500;
const DELTAS_TIMEOUT: Duration = Duration::from_secs(3);

/// What other agents changed in this session's working set since the last
/// event: a few lines, prepended to whatever the event emits.
fn fetch_deltas(hook_input: &Value, env: &Env) -> String {
    let session = text(hook_input, "session_id");
    if session.is_empty() {
        return String::new();
    }
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(from_input) }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project_dir.is_empty()).then(|| PathBuf::from(project_dir));
    let Some(root) = config::find_repo_root(&[Some(cwd), project]) else { return String::new() };
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return String::new();
    }
    // each agent of the session (its lead, each helper) reads the deltas on
    // its own: how far it has read, and what it was told. Shared, one
    // helper's read moved the session past a change the others never heard
    let worker = text(hook_input, "agent_id");
    let since_key = if worker.is_empty() { "deltas_ts".to_string() } else { format!("deltas_ts:{worker}") };
    let state_file = state_path(&session, env);
    let since = load_state(&state_file).get(&since_key).and_then(Value::as_f64).unwrap_or(0.0);
    // a teammate's interface change is said once per agent: what the
    // pre-write gate or the protocol's push already said stays out, and
    // what the deltas say joins the set
    let mut told = crate::deps::Told::load(env, &crate::proto::agent_of(&session, &worker));
    let mut ids = told.ids();
    // a helper: what the session's daemon pushed to the session is said
    // already, whichever of its agents read it
    if !worker.is_empty() {
        let pushed = crate::deps::Told::load(env, &session).pushed_ids();
        ids.extend(pushed.into_iter().filter(|f| !told.knows(f)));
    }
    let told_ids: Vec<&String> = ids.iter().rev().filter(|f| !f.starts_with("lint:")).take(200).collect();
    let mut payload = json!({"repo_id": cfg.repo_id, "session": session, "since_ts": since, "budget": DELTAS_BUDGET, "told": told_ids});
    // a session that lands its pushes hears "landing will rebase", never "pull now"
    if crate::land::lands(&root, env) {
        payload["lands"] = json!(true);
    }
    // a helper's call: the lines are that helper's, on the heads-up log too
    if !worker.is_empty() {
        payload["worker"] = json!(worker);
    }
    let Ok(answer) = http::post(&cfg.server, "/deltas", &cfg.token, &user_agent(), &payload, DELTAS_TIMEOUT) else {
        return String::new();
    };
    let facts: Vec<&str> = answer.get("facts").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    if !facts.is_empty() {
        for fact in facts {
            told.note(fact, "", false);
        }
        told.save();
    }
    if let Some(ts) = answer.get("ts").and_then(Value::as_f64) {
        let mut state = load_state(&state_file);
        state.insert(since_key, json!(ts));
        save_json(&state_file, &Value::Object(state));
    }
    let lines: Vec<&str> = answer
        .get("lines")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut said = lines.join("\n");
    // the lines of the caller's own files each rename or new signature makes
    // wrong, read here from disk (fixlines.rs)
    if let Some(fixes) = answer.get("fixes").and_then(Value::as_array).filter(|f| !f.is_empty()) {
        let fix = crate::fixlines::for_deltas(&root, fixes);
        if !fix.is_empty() && !said.is_empty() {
            said = format!("{said}\n{fix}");
        }
    }
    said
}

/// Called first on every event: stash the pending verdict and the deltas so
/// the event's own output carries them (or the run wrapper emits them alone).
pub fn deliver_pending(hook_input: &Value, env: &Env) {
    let event = text(hook_input, "hook_event_name");
    let deltas = if matches!(event.as_str(), "PostToolUse" | "PostToolBatch" | "UserPromptSubmit") {
        fetch_deltas(hook_input, env)
    } else {
        String::new()
    };
    if event == "PostToolBatch" {
        // the batch handler takes the verdict itself, after its own spawn
        *pending().lock().unwrap_or_else(|e| e.into_inner()) = deltas;
        return;
    }
    let verdict = take_result(&text(hook_input, "session_id"), env);
    let joined = crate::prompt::join_notes(&[&deltas, &verdict]);
    if !joined.is_empty() {
        *pending().lock().unwrap_or_else(|e| e.into_inner()) = joined;
    }
}

pub fn verify_command(root: &Path) -> String {
    let cfg = config::repo_config(&root);
    cfg.get("verify").and_then(Value::as_str).unwrap_or("").trim().to_string()
}

pub(crate) fn home(env: &Env) -> String {
    let cork_home = config::get(env, "CORK_HOME");
    let home = config::get(env, "HOME");
    // Windows sets USERPROFILE, not HOME, unless the hook runs under Git
    // Bash; a harness that runs it directly (Codex, Cursor) must still find
    // the credential. Python's expanduser reads it the same way.
    let profile = config::get(env, "USERPROFILE");
    if !cork_home.is_empty() {
        cork_home.to_string()
    } else if !home.is_empty() {
        home.to_string()
    } else if !profile.is_empty() {
        profile.to_string()
    } else {
        "~".to_string()
    }
}

/// The one line a resumed session needs before anything else: whether the
/// repo's check was passing when the context was lost, and on what.
pub fn resume_line(root: &Path, session: &str, env: &Env) -> String {
    let state = load_state(&state_path(session, env));
    let last = match state.get("last").filter(|l| l.as_object().map(|o| !o.is_empty()).unwrap_or(false)) {
        Some(l) => l.clone(),
        None => state.get("baseline").cloned().unwrap_or(Value::Null),
    };
    if !last.is_object() || last.get("ok").is_none() {
        return String::new();
    }
    let command = verify_command(root);
    let where_ = if command.is_empty() { String::new() } else { format!(" (`{command}`)") };
    if last.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return format!("Cork: resuming this session — last check passed{where_}.");
    }
    let failing: Vec<String> = last
        .get("failing")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect())
        .unwrap_or_default();
    let mut names: String = failing.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if failing.len() > 5 {
        names.push_str(&format!(" +{} more", failing.len() - 5));
    }
    let named = if names.is_empty() { String::new() } else { format!(": {names}") };
    if last.get("flaky_only").and_then(Value::as_bool).unwrap_or(false) {
        return format!("Cork: resuming this session — last check failed only on tests flaky in this repo{named}{where_}: not this session's change.");
    }
    format!("Cork: resuming this session — last check FAILED{named}{where_}; fix or re-run it before new work.")
}

pub fn state_path(session: &str, env: &Env) -> PathBuf {
    let tag: String = if session.is_empty() { "nosession".to_string() } else { session.chars().take(8).collect() };
    config::data_dir(env).join("check").join(format!("{tag}.json"))
}

fn load_state(path: &Path) -> Map<String, Value> {
    config::load_json(path).as_object().cloned().unwrap_or_default()
}

pub fn batch_written(hook_input: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for call in hook_input.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
        if !WRITE_TOOLS.contains(&text(call, "tool_name").as_str()) {
            continue;
        }
        let input = call.get("tool_input").cloned().unwrap_or(Value::Null);
        let mut path = text(&input, "file_path");
        if path.is_empty() {
            path = text(&input, "notebook_path");
        }
        if !path.is_empty() && !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

pub fn spawn_check(root: &Path, session: &str, run_id: &str, paths: &[String], _env: &Env) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = Command::new(exe);
    command
        .arg("check")
        .arg(root)
        .arg(session)
        .arg(run_id)
        .arg(json!(paths).to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// PostToolBatch: spawn the check for the source files this batch wrote;
/// wait only as long as the check is known to take (capped), then emit
/// whatever verdict is ready — this batch's or an earlier one's.
pub fn run_post_tool_batch(hook_input: &Value, env: &Env) -> i32 {
    let session = text(hook_input, "session_id");
    let written = batch_written(hook_input);
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(from_input) }
    };
    let first_dir = written.first().map(|p| PathBuf::from(p).parent().map(Path::to_path_buf).unwrap_or_default());
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project_dir.is_empty()).then(|| PathBuf::from(project_dir));
    let root = config::find_repo_root(&[first_dir, Some(cwd.clone()), project]);
    let state_file = state_path(&session, env);
    let mut state = load_state(&state_file);
    if let Some(root) = root.as_ref() {
        if !verify_command(root).is_empty() {
            let abs_root = config::absolute(root);
            let mut rel: Vec<String> = Vec::new();
            for path in &written {
                let abs = config::absolute(&cwd.join(path));
                let Ok(r) = abs.strip_prefix(&abs_root) else { continue };
                let r = r.to_string_lossy().replace('\\', "/");
                let ext = Path::new(&r).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                if MAP_EXTS.contains(&ext.as_str()) {
                    rel.push(r);
                }
            }
            if !rel.is_empty() {
                let run_id = format!("{:.6}", now_s());
                state.insert("run_id".into(), json!(run_id));
                state.insert("root".into(), json!(abs_root.to_string_lossy()));
                save_json(&state_file, &Value::Object(state.clone()));
                spawn_check(root, &session, &run_id, &rel, env);
                let last = state.get("last_elapsed").and_then(Value::as_f64).unwrap_or(0.0);
                let grace = if last > 0.0 { (last * 1.5).min(CHECK_GRACE_CAP_S) } else { 0.0 };
                let deadline = Instant::now() + Duration::from_secs_f64(grace);
                while Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                    let fresh = load_state(&state_file);
                    if fresh.get("result").and_then(|r| r.get("run_id")).and_then(Value::as_str) == Some(run_id.as_str()) {
                        break;
                    }
                }
            }
        }
    }
    let note = with_pending(&take_result(&session, env));
    let rendered = crate::harness::render_context("PostToolBatch", &note);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    0
}

/// The undelivered verdict for this session, if any; delivered once.
pub fn take_result(session: &str, env: &Env) -> String {
    let path = state_path(session, env);
    let mut state = load_state(&path);
    let Some(result) = state.get("result").and_then(Value::as_object).cloned() else { return String::new() };
    let text_out = result.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    if text_out.is_empty() || result.get("delivered").and_then(Value::as_bool).unwrap_or(false) {
        return String::new();
    }
    let mut result = result;
    result.insert("delivered".into(), json!(true));
    state.insert("result".into(), Value::Object(result));
    save_json(&path, &Value::Object(state));
    text_out
}

fn now_s() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn regexes(patterns: &[&str]) -> Vec<Regex> {
    patterns.iter().filter_map(|p| Regex::new(p).ok()).collect()
}

fn failing_id_res() -> Vec<Regex> {
    regexes(&[
        r"(?m)^FAILED (\S+)", r"(?m)^ERROR (\S+)", r"(?m)^(?:FAIL|ERROR): (\S+)",
        r"(?m)^test (\S+) \.\.\. FAILED", r"(?m)^--- FAIL: (\S+)", r"(?m)^\s*[✕×✗] (.+)$",
    ])
}

/// The failing tests a run names, in order; the catch-all `check` when it
/// failed without naming any.
fn failing_ids(output: &str, ok: bool) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    if ok {
        return ids;
    }
    for rx in failing_id_res() {
        for m in rx.captures_iter(output) {
            let v = m.get(1).map(|g| g.as_str().trim().to_string()).unwrap_or_default();
            if !v.is_empty() && !ids.contains(&v) {
                ids.push(v);
            }
        }
    }
    if ids.is_empty() {
        ids.push("check".to_string());
    }
    ids
}

fn file_ref_res() -> Vec<Regex> {
    regexes(&[
        r"((?:[\w.-]+/)*[\w.-]+\.(?:py|pyi|ts|tsx|js|jsx|mjs|cjs|go|rs|java|cs|rb|c|h|cc|cpp|hpp|php)):(\d+)",
        r#"File "([^"]+)", line (\d+)"#,
    ])
}

fn missing_module_res() -> Vec<Regex> {
    regexes(&[
        r"No module named '([\w.]+)'", r"cannot import name '\w+' from '([\w.]+)'",
        r"Cannot find module '([^']+)'", r"unresolved import `([\w:]+)`",
    ])
}

fn symbol_err_res() -> Vec<Regex> {
    regexes(&[
        r"'(\w+)' object has no attribute '\w+'", r"name '(\w+)' is not defined",
        r"module '[\w.]+' has no attribute '(\w+)'", r"cannot import name '(\w+)'",
    ])
}

fn key_line_re() -> Regex {
    Regex::new(r"Error|error\b|FAIL|assert|Exception|panicked").expect("static regex")
}

fn module_on_disk(root: &Path, module: &str) -> bool {
    let base = module.replace('.', "/").replace("::", "/");
    for cand in [
        format!("{base}.py"), format!("{base}/__init__.py"), format!("{base}.js"), format!("{base}.ts"),
        format!("{base}.rs"), format!("{base}/mod.rs"), base.clone(),
    ] {
        if root.join(cand).exists() {
            return true;
        }
    }
    false
}

fn age_text(secs: f64) -> String {
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

/// `(ok, output, elapsed, timed_out)` — stdout then stderr, as the Python
/// hook concatenates them, so both hooks see the same text for a run.
pub(crate) fn run_verify(root: &Path, command: &str, cap_s: f64) -> (bool, String, f64, bool) {
    let started = Instant::now();
    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) => return (false, format!("could not run `{command}`: {err}"), started.elapsed().as_secs_f64(), false),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout { let _ = s.read_to_end(&mut buf); }
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr { let _ = s.read_to_end(&mut buf); }
        buf
    });
    let deadline = started + Duration::from_secs_f64(cap_s);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let out = out_thread.join().unwrap_or_default();
    let err = err_thread.join().unwrap_or_default();
    let elapsed = started.elapsed().as_secs_f64();
    match status {
        None => (false, String::new(), elapsed, true),
        Some(status) => {
            let mut output = String::from_utf8_lossy(&out).into_owned();
            output.push_str(&String::from_utf8_lossy(&err));
            (status.success(), output, elapsed, false)
        }
    }
}

fn str_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn tail_chars(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit { text.to_string() } else { text.chars().skip(count - limit).collect() }
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn take_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit { text.to_string() } else { text.chars().take(limit).collect() }
}

/// The verdict text, and whether the run failed ONLY on tests known to be
/// flaky in this repo (`known`: the team's registry and this machine's own
/// evidence). Mutates state (last, history, flags).
#[allow(clippy::too_many_arguments)]
fn classify(
    root: &Path, command: &str, ok: bool, output: &str, elapsed: f64, written: &[String],
    state: &mut Map<String, Value>, attribution: &Value, known: &BTreeMap<String, flaky::Evidence>,
) -> (String, bool) {
    let tail = tail_chars(output, CHECK_TAIL_CHARS);
    let ids = failing_ids(output, ok);
    let norm = collapse(&tail);
    let sig = {
        let mut h = Sha256::new();
        h.update((if ok { "ok" } else { "fail" }).as_bytes());
        h.update(norm.as_bytes());
        format!("{:x}", h.finalize()).chars().take(12).collect::<String>()
    };
    let baseline = state.get("baseline").cloned().filter(|b| b.is_object());
    let prev = state.get("last").cloned().filter(|l| l.is_object()).or_else(|| baseline.clone());
    let prev_failing = str_list(prev.as_ref().and_then(|p| p.get("failing")));
    let base_failing: BTreeSet<String> = str_list(baseline.as_ref().and_then(|b| b.get("failing"))).into_iter().collect();
    let mut flaky: BTreeSet<String> = str_list(state.get("flaky")).into_iter().collect();
    flaky.extend(known.keys().cloned());
    // the real failures first; a flaky test failing is not the writer's
    let flaky_failing: Vec<String> = ids.iter().filter(|i| flaky.contains(*i)).cloned().collect();
    let real_failing: Vec<String> = ids.iter().filter(|i| !flaky.contains(*i)).cloned().collect();
    let flaky_only = !ok && real_failing.is_empty() && !flaky_failing.is_empty();
    let mut history: BTreeMap<String, Vec<bool>> = state
        .get("history")
        .and_then(Value::as_object)
        .map(|h| {
            h.iter()
                .map(|(k, v)| (k.clone(), v.as_array().map(|a| a.iter().filter_map(Value::as_bool).collect()).unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default();
    let mut touched: BTreeSet<String> = prev_failing.iter().cloned().collect();
    touched.extend(ids.iter().cloned());
    for i in &touched {
        let h = history.entry(i.clone()).or_default();
        h.push(ids.contains(i));
        if h.len() > 8 {
            let drop = h.len() - 8;
            h.drain(0..drop);
        }
    }
    state.insert("history".into(), json!(history));

    let newly: Vec<String> = ids.iter().filter(|i| !prev_failing.contains(i) && !base_failing.contains(*i) && !flaky.contains(*i)).cloned().collect();
    let preexisting: Vec<String> = ids.iter().filter(|i| base_failing.contains(*i) && !flaky.contains(*i)).cloned().collect();
    let fixed: Vec<String> = prev_failing.iter().filter(|i| !ids.contains(i) && !flaky.contains(*i)).cloned().collect();
    let same = !ok && state.get("last").and_then(|l| l.get("sig")).and_then(Value::as_str) == Some(sig.as_str());
    let said: BTreeSet<String> = str_list(state.get("oscillation_said")).into_iter().collect();
    let mut flips: Vec<String> = Vec::new();
    for i in &ids {
        let h = history.get(i).cloned().unwrap_or_default();
        let changes = h.windows(2).filter(|w| w[0] != w[1]).count();
        if changes >= 2 && !said.contains(i) && !flaky.contains(i) {
            flips.push(i.clone());
        }
    }
    let mut missing: Vec<String> = Vec::new();
    for rx in missing_module_res() {
        for m in rx.captures_iter(output) {
            let module = m.get(1).map(|g| g.as_str().to_string()).unwrap_or_default();
            if !missing.contains(&module) && !module_on_disk(root, &module) {
                missing.push(module);
            }
        }
    }
    let where_ = if written.is_empty() { "your writes".to_string() } else { written.join(", ") };
    let head = format!("Cork check (`{command}`, {elapsed:.1}s) after your write to {where_}: ");
    let mut lines: Vec<String> = Vec::new();
    if ok {
        lines.push(format!("{head}PASSED — marked verified."));
        if !fixed.is_empty() {
            lines.push(format!("  now passing: {}", fixed.join(", ")));
        }
    } else if flaky_only {
        // said plainly, so the agent does not spend turns chasing it
        lines.push(format!("{head}FAILED"));
        let verb = if flaky_failing.len() == 1 { "failed, which is" } else { "failed, which are" };
        lines.push(format!(
            "  only {} {verb} flaky in this repo ({}): not your change",
            flaky::backticked(&flaky_failing),
            flaky::evidence_text(&flaky_failing, known)
        ));
        if !fixed.is_empty() {
            lines.push(format!("  now passing: {}", fixed.join(", ")));
        }
    } else {
        lines.push(format!("{head}FAILED"));
        if same {
            lines.push("  same failure as after your previous write: it did not address this.".to_string());
        } else if !newly.is_empty() {
            lines.push(format!("  newly failing ({}): {}", newly.len(), newly.join(", ")));
        }
        let key_re = key_line_re();
        let key: Vec<String> = tail
            .lines()
            .filter(|ln| key_re.is_match(ln))
            // a flaky test's own lines are not the failure to read
            .filter(|ln| !flaky_failing.iter().any(|f| ln.contains(f.as_str())))
            .map(|ln| take_chars(ln.trim(), 160))
            .collect();
        if !same {
            let start = key.len().saturating_sub(CHECK_KEY_LINES);
            for ln in &key[start..] {
                lines.push(format!("    {ln}"));
            }
        }
        for module in &missing {
            lines.push(format!("  likely pending: module {module} is not on disk yet — probably fixed by your next file"));
        }
        if let Some(files) = attribution.get("files").and_then(Value::as_object) {
            for (path, who) in files {
                let Some(who) = who.as_object() else { continue };
                if who.get("mine").and_then(Value::as_bool).unwrap_or(false) || written.contains(path) {
                    continue;
                }
                let age = who.get("age_s").and_then(Value::as_f64).unwrap_or(1e9);
                if age <= CHECK_RECENT_S {
                    let agent = crate::prompt::agent_suffix(who.get("agent").and_then(Value::as_str).unwrap_or(""));
                    // named as the reader knows them: `nickname (email)`, from the server
                    let user = who.get("who").and_then(Value::as_str).filter(|u| !u.is_empty())
                        .or_else(|| who.get("user").and_then(Value::as_str).filter(|u| !u.is_empty()))
                        .unwrap_or("a teammate");
                    let whom = if who.get("same_user").and_then(Value::as_bool).unwrap_or(false) { "another session of yours" } else { user };
                    lines.push(format!(
                        "  introduced by {whom}{agent} writing {path} {}, not by you — leave it to them; re-check after their next write",
                        age_text(who.get("age_s").and_then(Value::as_f64).unwrap_or(0.0))
                    ));
                }
            }
        }
        if let Some(facts) = attribution.get("facts").and_then(Value::as_object) {
            for (name, fact) in facts {
                let Some(fact) = fact.as_object() else { continue };
                // the map could not tell which one: both named, neither as the fact
                if let Some(paths) = fact.get("ambiguous").and_then(Value::as_array) {
                    let paths: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();
                    lines.push(format!("  fact: `{name}` is defined in more than one file ({})", paths.join(", ")));
                    continue;
                }
                let signature = fact.get("signature").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(name);
                let doc = fact.get("doc").and_then(Value::as_str).unwrap_or("");
                let path = fact.get("path").and_then(Value::as_str).unwrap_or("");
                let doc_part = if doc.is_empty() { String::new() } else { format!(" — {doc}") };
                lines.push(format!("  fact: {signature}{doc_part}   ({path})"));
            }
        }
        if !flips.is_empty() {
            lines.push(format!(
                "  alternating: {} have failed, passed, failed across your last writes — the constraints conflict; read both before writing again",
                flips.join(", ")
            ));
            let mut all: BTreeSet<String> = said.clone();
            all.extend(flips.iter().cloned());
            state.insert("oscillation_said".into(), json!(all.into_iter().collect::<Vec<_>>()));
        }
        if !preexisting.is_empty() && !state.get("reported_preexisting").and_then(Value::as_bool).unwrap_or(false) {
            lines.push(format!("  already failing before you started (not yours): {}", preexisting.join(", ")));
            state.insert("reported_preexisting".into(), json!(true));
        }
        if !flaky_failing.is_empty() {
            lines.push(format!(
                "  also failed, flaky in this repo (not your change): {} ({})",
                flaky::backticked(&flaky_failing),
                flaky::evidence_text(&flaky_failing, known)
            ));
        }
        if !fixed.is_empty() {
            lines.push(format!("  now passing: {}", fixed.join(", ")));
        }
    }
    let mut last = json!({"ok": ok, "failing": ids, "sig": sig});
    if flaky_only {
        last["flaky_only"] = json!(true);
    }
    state.insert("last".into(), last);
    state.insert("last_elapsed".into(), json!((elapsed * 1000.0).round() / 1000.0));
    (lines.join("\n"), flaky_only)
}

/// `check <root> <session> <run_id> <paths-json>`: the detached run.
/// Writes the verdict into the session state unless a newer run superseded it.
pub fn check_command(root: &str, session: &str, run_id: &str, paths_json: &str, env: &Env) -> i32 {
    if root.is_empty() {
        return 0;
    }
    let root = PathBuf::from(root);
    let command = verify_command(&root);
    if command.is_empty() {
        return 0;
    }
    let written: Vec<String> = serde_json::from_str::<Value>(paths_json)
        .ok()
        .map(|v| str_list(Some(&v)))
        .unwrap_or_default();
    let state_file = state_path(session, env);
    let fp_before = flaky::fingerprint(&root);
    let (ok, output, elapsed, timed_out) = run_verify(&root, &command, CHECK_MAX_S);
    let state = load_state(&state_file);
    let current = state.get("run_id").and_then(Value::as_str).unwrap_or("");
    if run_id != "baseline" && !current.is_empty() && current != run_id {
        return 0; // a later batch's check will speak instead
    }
    let cfg = config::config(Some(&root), env);
    let known = if timed_out {
        BTreeMap::new()
    } else {
        learn_flaky(&root, &command, run_id, ok, &output, elapsed, fp_before.as_deref(), &state, &cfg, env)
    };
    // the rerun took time: a newer batch may have started meanwhile
    let mut state = load_state(&state_file);
    let current = state.get("run_id").and_then(Value::as_str).unwrap_or("");
    if run_id != "baseline" && !current.is_empty() && current != run_id {
        return 0;
    }
    if run_id == "baseline" {
        let mut ok = ok;
        if !timed_out {
            // a flaky test failing at session start is not "already failing"
            let ids: Vec<String> = failing_ids(&output, ok).into_iter().filter(|i| !known.contains_key(i)).collect();
            ok = ok || ids.is_empty();
            state.insert("baseline".into(), json!({"ok": ok, "failing": ids}));
            state.insert("last_elapsed".into(), json!((elapsed * 1000.0).round() / 1000.0));
        }
        save_json(&state_file, &Value::Object(state));
        if ok && !timed_out {
            // a passing baseline verifies the whole indexed tree: the first
            // agent's briefing then says "verified by …" like the fourth's
            if cfg.usable() {
                let paths = crate::report::tracked_sources(&root);
                let _ = http::post(&cfg.server, "/verified", &cfg.token, &user_agent(),
                    &json!({"repo_id": cfg.repo_id, "paths": paths, "command": command, "ok": true, "baseline": true}),
                    Duration::from_secs(10));
            }
        }
        return 0;
    }
    if timed_out {
        if !state.get("over_budget_said").and_then(Value::as_bool).unwrap_or(false) {
            state.insert("result".into(), json!({"run_id": run_id, "delivered": false,
                "text": format!("Cork check (`{command}`) ran over {}s after your write; not run again this session — run it yourself.", CHECK_MAX_S as i64)}));
            state.insert("over_budget_said".into(), json!(true));
        }
        save_json(&state_file, &Value::Object(state));
        return 0;
    }
    let mut attribution = json!({});
    if !ok {
        let mut files: Vec<String> = Vec::new();
        for rx in file_ref_res() {
            for m in rx.captures_iter(&output) {
                let mut p = m.get(1).map(|g| g.as_str().to_string()).unwrap_or_default();
                if Path::new(&p).is_absolute() {
                    match Path::new(&p).strip_prefix(&config::absolute(&root)) {
                        Ok(rel) => p = rel.to_string_lossy().into_owned(),
                        Err(_) => continue,
                    }
                }
                if !p.starts_with("..") && !files.contains(&p) && !written.contains(&p) {
                    files.push(p);
                }
            }
        }
        let mut symbols: Vec<String> = Vec::new();
        for rx in symbol_err_res() {
            for m in rx.captures_iter(&output) {
                let s = m.get(1).map(|g| g.as_str().to_string()).unwrap_or_default();
                if !symbols.contains(&s) {
                    symbols.push(s);
                }
            }
        }
        if cfg.usable() && (!files.is_empty() || !symbols.is_empty()) {
            files.truncate(40);
            symbols.truncate(20);
            if let Ok(answer) = http::post(&cfg.server, "/attribute", &cfg.token, &user_agent(),
                &json!({"repo_id": cfg.repo_id, "paths": files, "symbols": symbols, "session": session}), Duration::from_secs(5)) {
                attribution = answer;
            }
        }
    }
    let (verdict, flaky_only) = classify(&root, &command, ok, &output, elapsed, &written, &mut state, &attribution, &known);
    // the tree this run saw: the next run on the same tree is a rerun
    if let (Some(fp), Some(last)) = (fp_before.as_ref(), state.get_mut("last").and_then(Value::as_object_mut)) {
        last.insert("fp".into(), json!(fp));
    }
    state.insert("result".into(), json!({"run_id": run_id, "delivered": false, "text": verdict}));
    save_json(&state_file, &Value::Object(state));
    if cfg.usable() && !written.is_empty() {
        // a failure only on the repo's flaky tests does not mark the writes failed
        let _ = http::post(&cfg.server, "/verified", &cfg.token, &user_agent(),
            &json!({"repo_id": cfg.repo_id, "paths": written, "command": command, "ok": ok || flaky_only}), Duration::from_secs(5));
    }
    0
}

/// What this run says about flaky tests, and every test known to be flaky
/// in this repo afterwards (the team's registry and this machine's own).
///
/// Evidence comes only from runs on unchanged code: a failing check that
/// names a failure nothing explains yet (not already failing, not known
/// flaky, not a module still to be written) and is quick is run once more
/// on the same tree; a test that fails and then passes is flaky. Two checks
/// in a row on the same tree count the same way. What is learned goes to
/// the server as names and counts (flaky.rs).
#[allow(clippy::too_many_arguments)]
fn learn_flaky(
    root: &Path, command: &str, run_id: &str, ok: bool, output: &str, elapsed: f64, fp_before: Option<&str>,
    state: &Map<String, Value>, cfg: &config::Config, env: &Env,
) -> BTreeMap<String, flaky::Evidence> {
    let ids = failing_ids(output, ok);
    let last = state.get("last").filter(|l| l.is_object());
    let last_fp = if run_id == "baseline" { None } else { last.and_then(|l| l.get("fp")).and_then(Value::as_str) };
    let same_tree_as_last = fp_before.is_some() && last_fp == fp_before;
    if ok && !same_tree_as_last {
        flaky::flush(env, cfg);
        return BTreeMap::new(); // a pass needs no names; counts still owed go now
    }
    let mut known = flaky::known(env, cfg);
    let known_set: BTreeSet<String> = known.keys().cloned().collect();
    let base_failing: BTreeSet<String> = if run_id == "baseline" {
        BTreeSet::new()
    } else {
        str_list(state.get("baseline").and_then(|b| b.get("failing"))).into_iter().collect()
    };
    let unexplained = ids.iter().any(|i| flaky::clean_id(i) == *i && !known.contains_key(i) && !base_failing.contains(i));
    let pending_module = missing_module_res().iter().any(|rx| {
        rx.captures_iter(output).any(|m| m.get(1).map(|g| !module_on_disk(root, g.as_str())).unwrap_or(false))
    });
    let rerun_allowed = config::repo_config(&root)
        .get("flaky_rerun")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut seen = BTreeMap::new();
    if !ok && unexplained && !pending_module && rerun_allowed && elapsed <= flaky::RERUN_MAX_S && fp_before.is_some() {
        let (ok2, output2, _, timed_out2) = run_verify(root, command, CHECK_MAX_S);
        if !timed_out2 && flaky::fingerprint(root).as_deref() == fp_before {
            seen = flaky::observe(&ids, &failing_ids(&output2, ok2), &known_set);
        }
    } else if same_tree_as_last {
        let before = str_list(last.and_then(|l| l.get("failing")));
        seen = flaky::observe(&before, &ids, &known_set);
    }
    flaky::record(env, cfg, &seen);
    flaky::flush(env, cfg);
    if !seen.is_empty() {
        known = flaky::known(env, cfg);
    }
    known
}

/// SessionStart: what already fails before the first write, detached.
pub fn spawn_baseline(root: &Path, hook_input: &Value, env: &Env) {
    if verify_command(root).is_empty() {
        return;
    }
    let session = text(hook_input, "session_id");
    let state_file = state_path(&session, env);
    let mut state = load_state(&state_file);
    state.insert("root".into(), json!(config::absolute(root).to_string_lossy()));
    save_json(&state_file, &Value::Object(state));
    spawn_check(root, &session, "baseline", &[], env);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(list: &[(&str, f64, f64)]) -> BTreeMap<String, flaky::Evidence> {
        list.iter().map(|(id, runs, fails)| (id.to_string(), flaky::Evidence { runs: *runs, fails: *fails })).collect()
    }

    #[test]
    fn a_failure_only_on_a_flaky_test_is_not_the_writers() {
        let mut state = Map::new();
        let output = "FAILED tests/a.py::test_x - AssertionError: assert 0 == 1\n";
        let (text, flaky_only) = classify(Path::new("."), "pytest", false, output, 1.0, &["src/b.py".to_string()],
            &mut state, &json!({}), &known(&[("tests/a.py::test_x", 20.0, 4.0)]));
        assert!(flaky_only);
        assert!(text.contains("only `tests/a.py::test_x` failed, which is flaky in this repo (failed 4/20 runs on unchanged code): not your change"), "{text}");
        assert!(!text.contains("newly failing") && !text.contains("AssertionError"), "nothing to chase: {text}");
        assert_eq!(state["last"]["flaky_only"], json!(true));
    }

    #[test]
    fn real_failures_come_first_and_the_flaky_one_is_named_after() {
        let mut state = Map::new();
        let output = "FAILED tests/a.py::test_x - boom\nFAILED tests/a.py::test_real - assert 1 == 2\n";
        let (text, flaky_only) = classify(Path::new("."), "pytest", false, output, 1.0, &[], &mut state, &json!({}),
            &known(&[("tests/a.py::test_x", 9.0, 2.0)]));
        assert!(!flaky_only);
        let real = text.find("newly failing (1): tests/a.py::test_real").expect(&text);
        let flaky = text
            .find("also failed, flaky in this repo (not your change): `tests/a.py::test_x` (failed 2/9 runs on unchanged code)")
            .expect(&text);
        assert!(real < flaky, "{text}");
        assert!(!text.contains("boom"), "the flaky test's own lines are left out: {text}");
    }

    #[test]
    fn a_flaky_test_flipping_is_not_called_a_conflict() {
        let mut state = Map::new();
        let k = known(&[("t::x", 4.0, 2.0)]);
        for out in ["FAILED t::x\n", "", "FAILED t::x\n"] {
            let (text, _) = classify(Path::new("."), "pytest", out.is_empty(), out, 1.0, &[], &mut state, &json!({}), &k);
            assert!(!text.contains("alternating") && !text.contains("now passing: t::x"), "{text}");
        }
    }
}
