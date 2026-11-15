//! PreToolUse: ask Cork's /gate whether this write may proceed.
//!
//! Exit 0 allows, exit 2 blocks with the reason on stderr so the agent can
//! adapt. HARD FAIL-OPEN: the whole thing is budgeted at 200ms and any error,
//! timeout, missing configuration or non-200 response allows the write. The
//! server being down must never stop a write.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::http;
use crate::shell;

pub const TOTAL_BUDGET: Duration = Duration::from_millis(200);

const GATE_TOOLS: [&str; 7] = [
    "Edit", "Write", "MultiEdit", "apply_patch", "Bash", "shell", "run_terminal_cmd",
];
const SHELL_TOOLS: [&str; 3] = ["Bash", "shell", "run_terminal_cmd"];

/// Whether a tool runs shell commands.
pub fn is_shell_tool(tool: &str) -> bool {
    SHELL_TOOLS.contains(&tool)
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Absolute paths this write touches: one for Claude Edit/Write, possibly
/// several for a Codex apply_patch, or the likely targets of a shell command.
fn edit_targets(tool_input: &Value, cwd: &Path, tool: &str) -> Vec<PathBuf> {
    let direct = {
        let file_path = text(tool_input, "file_path");
        if file_path.is_empty() { text(tool_input, "notebook_path") } else { file_path }
    };
    if !direct.is_empty() {
        return vec![PathBuf::from(direct)];
    }
    let command = text(tool_input, "command");
    if command.contains("*** ") {
        return shell::abs_targets(&shell::apply_patch_paths(&command), cwd);
    }
    if SHELL_TOOLS.contains(&tool) && !command.is_empty() {
        return shell::shell_gate_paths(&command, cwd);
    }
    Vec::new()
}

/// A file about to be CREATED whose name starts with a number, in a folder
/// that already numbers its files (a migration, an ADR): the highest number
/// already there, so the server can claim this one or steer to the next.
/// `None` for everything else, which is almost every write.
fn numbered_new_file(target: &Path) -> Option<i64> {
    if target.exists() {
        return None;
    }
    let leading = |name: &str| -> Option<i64> {
        let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
        (!digits.is_empty() && digits.len() <= 8).then(|| digits.parse().ok()).flatten()
    };
    leading(target.file_name()?.to_str()?)?;
    let siblings = std::fs::read_dir(target.parent()?).ok()?;
    siblings
        .flatten()
        .filter_map(|entry| leading(entry.file_name().to_str()?))
        .max()
}

/// Returns (exit_code, stderr_text). Never panics; every failure allows.
pub fn run(stdin_data: &str, env: &Env) -> (i32, String) {
    let (code, stderr_text, _) = run_with_notice(stdin_data, env);
    (code, stderr_text)
}

/// `run`, and the lines an allowed write carries into the agent's context:
/// a teammate changed a function this write calls (see deps.rs).
pub fn run_with_notice(stdin_data: &str, env: &Env) -> (i32, String, String) {
    let mut notice = String::new();
    let (code, stderr_text) = run_inner(stdin_data, env, &mut notice);
    if code != 0 {
        notice.clear();
    }
    (code, stderr_text, notice)
}

fn run_inner(stdin_data: &str, env: &Env, notice: &mut String) -> (i32, String) {
    let started = Instant::now();
    let Ok(hook_input) = serde_json::from_str::<Value>(stdin_data).map(crate::harness::normalize) else {
        return (0, String::new());
    };
    let tool = text(&hook_input, "tool_name");
    if !GATE_TOOLS.contains(&tool.as_str()) {
        return (0, String::new());
    }
    let tool_input = hook_input.get("tool_input").cloned().unwrap_or(Value::Null);
    let cwd = {
        let from_input = text(&hook_input, "cwd");
        if from_input.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(from_input)
        }
    };
    // a shell command is about to run: say so, in a process of its own, so
    // the dashboard shows it running for as long as it runs and the command
    // never waits on the network
    if SHELL_TOOLS.contains(&tool.as_str()) && config::get(env, crate::machine::MACHINE_FLAG) == "1" {
        crate::report::spawn_started(
            &cwd, &text(&hook_input, "session_id"), &text(&tool_input, "command"), &text(&hook_input, "transcript_path"),
            &text(&hook_input, "agent_id"), &text(&hook_input, "agent_type"),
        );
    }
    // which folder this session works in, and what it stages through git
    crate::sharedtree::note(&hook_input, &cwd, env);
    // a manual pull mid-task, where landing would rebase for it: it runs,
    // with one short line, once a session (never a block)
    if SHELL_TOOLS.contains(&tool.as_str()) {
        if let Some(line) = manual_pull_note(&text(&tool_input, "command"), &cwd, &text(&hook_input, "session_id"), env) {
            *notice = line;
        }
    }
    // a shell command that may write: the tree just before it runs, so the
    // report credits it with what it changed and nothing a teammate in this
    // checkout did meanwhile (shellrun.rs)
    if SHELL_TOOLS.contains(&tool.as_str()) {
        let command = text(&tool_input, "command");
        let session = text(&hook_input, "session_id");
        if !session.is_empty() && !command.trim().is_empty() && !command.contains("*** ") && !shell::read_only(&command) {
            if let Some(root) = crate::shellrun::root_for(&cwd, env) {
                let call = crate::shellrun::call_key(&text(&hook_input, "tool_use_id"), &command);
                let background = tool_input.get("run_in_background").and_then(Value::as_bool).unwrap_or(false);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::shellrun::begin(&root, &session, &call, &command, &cwd, background, env);
                }));
            }
        }
    }
    let mut targets = edit_targets(&tool_input, &cwd, &tool);
    // what the journal copies first: a shell command's moves and appends too,
    // which the gate itself never holds
    let shell = SHELL_TOOLS.contains(&tool.as_str()) && !text(&tool_input, "command").contains("*** ");
    let mut touched = if shell { shell::shell_touch_paths(&text(&tool_input, "command"), &cwd) } else { targets.clone() };
    if touched.is_empty() {
        return (0, String::new());
    }

    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project_dir.is_empty()).then(|| PathBuf::from(&project_dir));
    // a shell command belongs to the repo the session works in, and a path
    // it names outside that repo is not that repo's activity, nor another
    // repo's (Study 9: agents in their own clones were gated against, and
    // reported into, the checkout their commands' paths pointed into)
    let session_root = if shell { config::find_repo_root(&[Some(cwd.clone()), project.clone()]) } else { None };
    if let Some(own) = &session_root {
        touched.retain(|t| config::rel_path(t, own).is_some());
        targets.retain(|t| config::rel_path(t, own).is_some());
        if touched.is_empty() {
            return (0, String::new());
        }
    }
    let root = session_root
        .or_else(|| config::find_repo_root(&[touched[0].parent().map(Path::to_path_buf), Some(cwd.clone()), project]))
        .unwrap_or_else(|| {
            if project_dir.is_empty() { cwd.clone() } else { PathBuf::from(&project_dir) }
        });

    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return (0, String::new());
    }

    // the protocol's check runs beside the server's /gate: what the write
    // changes, symbol by symbol, against what teammates did since this copy
    let session = text(&hook_input, "session_id");
    // the agent writing: the session, or one of its helpers. What it was
    // told and what holds it are its own (proto::agent_of); a sibling
    // helper's lines are a teammate's (lines.rs)
    let worker = text(&hook_input, "agent_id");
    let agent = crate::proto::agent_of(&session, &worker);
    // the file as it is now: the exact "before" of this write for the
    // journal (journal.rs), where the harness hands over no copy of its own
    if !session.is_empty() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for target in &touched {
                crate::journal::note_before(&root, target, &session, &worker, env);
            }
        }));
    }
    if targets.is_empty() {
        return (0, String::new());
    }
    let rels: Vec<String> = targets.iter().filter_map(|t| config::rel_path(t, &root)).collect();
    // whose lines this write changes, from this machine's journal (lines.rs)
    let lines = if session.is_empty() {
        crate::lines::Check { adds_only: true, ..Default::default() }
    } else {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::lines::check(&root, &targets, &tool, &tool_input, &cwd, &session, &worker, env)
        }))
        .unwrap_or_default()
    };
    let protocol = !session.is_empty() && crate::proto::enabled(&root, &cfg, env);
    let ctx = crate::proto::Ctx { root: &root, cfg: &cfg, env, session: &session };
    let lands = crate::land::lands(&root, env);
    std::thread::scope(|scope| {
        let checked = protocol.then(|| scope.spawn(|| crate::proto::gate(&ctx, &tool, &tool_input, &cwd, &targets, started, TOTAL_BUDGET)));
        let write = Write { tool: &tool, input: &tool_input, cwd: &cwd, lands };
        let mut apart = serde_json::Map::new();
        let mut creating = serde_json::Map::new();
        let (code, reason, server_notice) =
            http_gate(&hook_input, &targets, &root, &cfg, env, &write, started, &mut apart, &mut creating);
        if code != 0 {
            return (code, reason);
        }
        let mut said = checked.and_then(|h| h.join().ok()).unwrap_or_default();
        let path = rels.first().cloned().unwrap_or_default();
        // the protocol's block is about a session on another branch: never a
        // hold, said once as a line (branches.rs)
        if said.block.is_some() {
            if let Some(on) = apart.get(&said.block_by).and_then(Value::as_str) {
                said.block = None;
                let fact = format!("branch|{}|{path}", said.block_by);
                let mut told = crate::deps::Told::load(env, &agent);
                if !told.knows(&fact) {
                    told.note(&fact, &path, false);
                    told.save();
                    let who = if said.block_who.is_empty() { "a teammate".to_string() } else { said.block_who.clone() };
                    let line = branch_line(&who, &path, on);
                    said.notice = [said.notice.clone(), line].into_iter().filter(|n| !n.is_empty()).collect::<Vec<_>>().join("\n");
                }
            }
        }
        // the protocol's block is a lease on code a teammate is CREATING
        // (not on origin yet) that this write creates too: never a hold, its
        // interface so far. A write to code that is already in the file is
        // no copy of theirs, and is judged by its lines below (v104: "build
        // against its interface" was said of an edit to an existing function)
        if said.block.is_some() && said.lease {
            let rel = crate::proto::key_path(&said.block_key);
            let symbol = crate::proto::key_symbol(&said.block_key);
            if being_created(&root, &rel, &symbol) && !in_file_now(&root, &rel, &symbol) {
                said.block = None;
                let fact = format!("creating|{}|{rel}", said.block_by);
                let mut told = crate::deps::Told::load(env, &agent);
                if !told.knows(&fact) {
                    told.note(&fact, &rel, false);
                    told.save();
                    let who = if said.block_who.is_empty() { "a teammate".to_string() } else { said.block_who.clone() };
                    let line = creating_line(&who, &rel, &symbol, creating.get(&rel));
                    said.notice = [said.notice.clone(), line].into_iter().filter(|n| !n.is_empty()).collect::<Vec<_>>().join("\n");
                }
            }
        }
        // a lease is who is editing a symbol, not whose lines this write
        // touches: the journal says that for the sessions on this machine,
        // and a write that only adds lines leaves theirs alone wherever
        // they are (v104: four blocks on pure additions, each passed on retry)
        if said.block.is_some() && said.lease {
            let holder = crate::proto::agent_session(&said.block_by).to_string();
            if !lease_holds(&lines, &holder) {
                said.block = None;
                let fact = format!("beside|{holder}|{}", said.block_key);
                let mut told = crate::deps::Told::load(env, &agent);
                if lines.adds_only && !told.knows(&fact) {
                    told.note(&fact, &path, false);
                    told.save();
                    let who = if said.block_who.is_empty() { "a teammate".to_string() } else { said.block_who.clone() };
                    let line = crate::lines::beside(&who, &crate::proto::key_symbol(&said.block_key));
                    said.notice = [said.notice.clone(), line].into_iter().filter(|n| !n.is_empty()).collect::<Vec<_>>().join("\n");
                }
            }
        }
        // the session still on the code, addressed by the name Claude Code
        // gives it when it is one on this machine, not by the account it shares
        if said.lease {
            let name = crate::transcript::name_of(&said.block_by, env);
            if let (false, Some(reason)) = (name.is_empty(), said.block.as_mut()) {
                *reason = address_by_name(reason, &name);
            }
        }
        // a teammate's live lines this write changes: said with whose and
        // which, and stopped once only when they would go unseen
        let (line_block, line_notes) = judge_lines(&lines, env, &agent);
        if let Some(reason) = line_block {
            if crate::hold::may_hold(env, &agent, &root, &path, &reason) {
                return (2, held(&path, &reason, env, &agent, Some((&root, ""))));
            }
        }
        match said.block {
            Some(reason) if crate::hold::may_hold(env, &agent, &root, &path, &reason) => {
                // a teammate's pushed change this copy lacks, or a call it
                // breaks, is a catch; a lease that still stands (its holder
                // works elsewhere, the line check above speaks for the lines
                // of anyone here) only says who is on the code
                let catch = !said.lease;
                (2, held(&path, &reason, env, &agent, catch.then_some((root.as_path(), said.block_who.as_str()))))
            }
            _ => {
                // through: a hold on these files, if any, is spent
                crate::hold::passed(env, &agent, &rels);
                *notice = [server_notice, said.notice, line_notes].into_iter().filter(|n| !n.is_empty()).collect::<Vec<_>>().join("\n");
                (code, reason)
            }
        }
    })
}

/// Whether a teammate's lease on what this write touches still stops it:
/// not when the journal speaks for the holder's lines here (the line check
/// decides), nor when the write only adds lines.
fn lease_holds(lines: &crate::lines::Check, holder: &str) -> bool {
    let known_here = lines.here.iter().any(|s| crate::journal::same_session(s, holder));
    !(known_here || lines.adds_only)
}

/// What the journal's check makes of a write: a block (once per teammate's
/// lines, only when they would go unseen), and the lines to say. Each fact
/// once a session, so the retry of a stopped write goes through.
fn judge_lines(lines: &crate::lines::Check, env: &Env, session: &str) -> (Option<String>, String) {
    if session.is_empty() || (lines.rewrites.is_empty() && lines.unknown.is_empty()) {
        return (None, String::new());
    }
    let now = crate::journal::now_s();
    let mut told = crate::deps::Told::load(env, session);
    let mut block: Option<String> = None;
    let mut said: Vec<String> = Vec::new();
    for (rel, theirs) in &lines.rewrites {
        let fact = format!("lines:{rel}:{}|{}", crate::proto::agent_of(&theirs.session, &theirs.worker), crate::lines::ranges(&theirs.lines));
        if told.knows(&fact) {
            continue;
        }
        if !lines.seen && block.is_none() {
            told.note(&fact, rel, true);
            block = Some(crate::lines::block(rel, theirs, now));
        } else {
            told.note(&fact, rel, false);
            said.push(crate::lines::notice(rel, theirs, now));
        }
    }
    for (rel, theirs) in &lines.unknown {
        let fact = format!("lines:{rel}:{}|unknown", crate::proto::agent_of(&theirs.session, &theirs.worker));
        if !told.knows(&fact) {
            told.note(&fact, rel, false);
            said.push(crate::lines::unknown(rel, theirs, now));
        }
    }
    told.save();
    (block, said.join("\n"))
}

/// Whether `symbol` is declared in `rel` as the file is now: a write to it
/// changes code that exists, it creates no copy of anyone's.
fn in_file_now(root: &Path, rel: &str, symbol: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(root.join(rel)) else { return false };
    cork_core::local::symbols(rel, &content).is_some_and(|parsed| parsed.symbols.iter().any(|s| s.name == symbol))
}

/// The words of a held write. A real catch (the write would have
/// overwritten someone's lines, or broken a call; `caught`: the checkout
/// it happened in, and whose change it met when Cork knows) is counted and
/// pinged, the month's count said once a session (pings.rs); a heads-up
/// that someone is on the code is not a catch, and counts nothing.
fn held(path: &str, reason: &str, env: &Env, session: &str, caught: Option<(&Path, &str)>) -> String {
    let tell = if let Some((root, who)) = caught {
        let n = crate::pings::caught(env, root, &crate::pings::catch_line(path, who, reason));
        crate::pings::tell_line(&crate::pings::count_once(env, session, n))
    } else {
        "Tell the user who is on it and what you will do instead.".to_string()
    };
    format!("Cork blocked this write ({path}): {reason}\n{tell}\n")
}

/// A block's `cork message --to <them>` aimed at a session's name: an
/// account names every session of the person, which is no one in particular
/// when it is the reader's own.
fn address_by_name(reason: &str, name: &str) -> String {
    static TO: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let to = TO.get_or_init(|| regex::Regex::new(r"--to \S+ ").expect("to pattern"));
    to.replace(reason, regex::NoExpand(&format!("--to {name} "))).into_owned()
}

/// Whether a shell command pulls or rebases onto origin by hand: `git pull`,
/// `git rebase origin/...` (not `--continue`/`--abort`/`--skip`), `git merge
/// origin/...`, or a fetch chained into one of those.
pub fn is_manual_pull(command: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"\bgit\s+(?:-C\s+\S+\s+)?(?:pull\b|rebase\s+(?:-\S+\s+)*(?:origin|upstream|@\{u\}|FETCH_HEAD)|merge\s+(?:-\S+\s+)*(?:origin|upstream|@\{u\}|FETCH_HEAD))")
            .expect("pull pattern")
    })
    .is_match(command)
}

/// The note a manual pull gets when this checkout lands its pushes: once a
/// session, never while a rebase or merge is already in progress (the agent
/// is resolving one), never a block. Study 8, second rerun: agents ran about
/// 35 pulls and rebases by hand a round, and those, not landing, made the
/// round's 23 merge conflicts.
fn manual_pull_note(command: &str, cwd: &Path, session: &str, env: &Env) -> Option<String> {
    if session.is_empty() || !is_manual_pull(command) {
        return None;
    }
    let root = config::find_repo_root(&[Some(cwd.to_path_buf())])?;
    if !crate::land::lands(&root, env) || crate::hold::rebase_in_progress(&root) {
        return None;
    }
    let mut told = crate::deps::Told::load(env, session);
    if told.knows("manual-pull") {
        return None;
    }
    told.note("manual-pull", "", false);
    told.save();
    Some(
        "Cork lands your push and rebases for you; a manual pull now can conflict. Carry on and push when done."
            .to_string(),
    )
}

/// The write being gated, as `http_gate` describes it to the server.
struct Write<'a> {
    tool: &'a str,
    input: &'a Value,
    cwd: &'a Path,
    lands: bool,
}

/// A protocol block about a session on another branch, said as a line: the
/// work meets only when the branches merge (branches.rs).
fn branch_line(who: &str, path: &str, on: &str) -> String {
    format!("Cork: {who} is changing code in {path} on branch {on}; this will conflict when the branches merge. No need to stop now.")
}

/// Whether `symbol` in `rel` is code being created: neither origin (as this
/// checkout last fetched it) nor this checkout's last commit has it. A
/// lease on it is a teammate writing new code, which no wait helps with.
fn being_created(root: &Path, rel: &str, symbol: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let has = |symbols: &crate::origin::Symbols| symbols.contains_key(symbol) || (symbol == rel && !symbols.is_empty());
    if let Some(upstream) = crate::origin::upstream_symbols(root, rel) {
        if has(&upstream) {
            return false;
        }
    }
    match crate::git::git(root, &["show", &format!("HEAD:{rel}")], std::time::Duration::from_millis(400)) {
        None => true,
        Some(content) => match cork_core::local::symbols(rel, &content) {
            Some(parsed) => !parsed.symbols.iter().any(|s| s.name == symbol),
            None => false,
        },
    }
}

/// The line for a lease on code a teammate is creating: what the server
/// knows of it (`creating`, from the gate's answer), else its name.
fn creating_line(who: &str, rel: &str, symbol: &str, creating: Option<&Value>) -> String {
    let (names, new_file, iface) = match creating {
        Some(c) => (
            c.get("names").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>(),
            c.get("new_file").and_then(Value::as_bool).unwrap_or(false),
            text(c, "interface"),
        ),
        None => (vec![symbol.to_string()], false, String::new()),
    };
    let names = if names.is_empty() { vec![symbol.to_string()] } else { names };
    let what = if new_file {
        format!("{rel} is new work by {who}")
    } else {
        let shown: Vec<String> = names.iter().take(4).map(|n| format!("`{n}`")).collect();
        format!("{who} is adding {} to {rel}", shown.join(", "))
    };
    let mut line = format!(
        "Cork in flight: {what}, not pushed yet. Build against its interface instead of writing your own copy: \
import it and keep your edit to your own code; landing will rebase onto it once it lands."
    );
    if !iface.is_empty() {
        line.push_str(&format!(" Interface: {iface}"));
    }
    line
}

/// What this write needs changed for the teammate changes in `events`: the
/// block reason with its fix lines (fixlines.rs), read from the write itself.
fn with_fix(reason: String, rel: &str, events: &[Value], target: &Path, write: &Write) -> String {
    let pending = crate::fixlines::pending(write.tool, write.input, target, write.cwd);
    crate::fixlines::with_fix(reason, rel, events, pending.as_ref())
}

/// A server block checked against origin: a teammate's change it says is
/// pushed, which origin (as this checkout last fetched it) does not have, is
/// no block. There is nothing to pull, and a block no pull can clear is the
/// phantom block of Study 8. Returns the line to say instead, or `None` to
/// keep the block (origin has it, or origin cannot be read).
fn not_on_origin(response: &Value, root: &Path, rel: &str) -> Option<String> {
    if response.get("decision").and_then(Value::as_bool) == Some(true) || response.get("claim").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if response.get("stale_copy").and_then(Value::as_bool) == Some(true) {
        if response.get("pushed").and_then(Value::as_bool) != Some(true) {
            return None; // "not pushed" never says pull
        }
        return (crate::origin::file_behind(root, rel) == Some(false)).then(|| {
            format!("Cork: a teammate's change to {rel} is recorded as pushed, but origin (as this checkout last fetched it) \
does not have it yet, so there is nothing to bring in. Carry on; landing rebases onto it once it is there.")
        });
    }
    let events = response.get("collisions").and_then(Value::as_array).filter(|e| !e.is_empty())?;
    let mut lines = Vec::new();
    for event in events {
        if crate::origin::Change::from_event(event).on_origin(root, rel) != Some(false) {
            return None;
        }
        let author = event.get("author").and_then(Value::as_str).unwrap_or("a teammate");
        lines.push(unpushed_words(author, rel, event));
    }
    Some(lines.join("\n"))
}

/// The first of the server's `unpushed` changes (renames, removals, new
/// signatures it knows only from the author's checkout) that origin has
/// and this copy does not, as a block reason, said once per change: the
/// author pushed it without landing it, so a pull and a re-read clear it.
fn pushed_anyway(response: &Value, root: &Path, rel: &str, env: &Env, session: &str, lands: bool) -> Option<(String, Value)> {
    let events = response.get("unpushed").and_then(Value::as_array).filter(|e| !e.is_empty())?;
    for event in events {
        let change = crate::origin::Change::from_event(event);
        if change.on_origin(root, rel) != Some(true) {
            continue;
        }
        let fact = format!("origin|{rel}::{}|{}|{}{}", change.symbol, change.kind, change.new_name, change.signature_after);
        let mut told = crate::deps::Told::load(env, session);
        if session.is_empty() || told.blocked(&fact) {
            continue;
        }
        told.note(&fact, rel, true);
        told.save();
        let author = event.get("author").and_then(Value::as_str).unwrap_or("a teammate");
        let what = match change.kind.as_str() {
            "renamed" => format!("renamed `{}` to `{}`", change.symbol, change.new_name),
            "removed" => format!("removed `{}`", change.symbol),
            _ => format!("changed `{}` to `{}`", change.symbol, change.signature_after),
        };
        let against = if !change.new_name.is_empty() { change.new_name.clone() } else { change.signature_after.clone() };
        let reason = if lands {
            let to = if against.is_empty() { "their change".to_string() } else { format!("`{against}`") };
            format!(
                "{author} {what} in {rel}, and it is on origin; your copy does not have it yet. Write against {to}: \
landing will rebase onto it when you push. Retry the write and it goes through."
            )
        } else {
            format!(
                "{author} {what} in {rel}, and it is on origin; your copy does not have it yet. Pull it (`git pull --rebase`) \
and re-read {rel}: once your copy has it, this write goes through."
            )
        };
        return Some((reason, event.clone()));
    }
    None
}

/// A teammate's change that is not on origin, as a line: what to write against.
fn unpushed_words(author: &str, rel: &str, event: &Value) -> String {
    let symbol = text(event, "symbol");
    let new_name = text(event, "new_name");
    let after = text(event, "signature_after");
    let target = if !new_name.is_empty() { format!("`{new_name}`") } else if !after.is_empty() { format!("`{after}`") } else { String::new() };
    let what = match text(event, "kind").as_str() {
        "renamed" => format!("renamed `{symbol}`"),
        "removed" => format!("removed `{symbol}`"),
        _ => format!("changed `{symbol}`"),
    };
    if target.is_empty() {
        format!("Cork: {author} {what} in {rel}, not on origin yet; landing will rebase onto it.")
    } else {
        format!("Cork: {author} {what} in {rel}, not pushed yet: write against {target}; landing will rebase onto it.")
    }
}

/// An allowed write's notice, into the agent's context: Claude Code reads
/// `additionalContext` from PreToolUse, so it arrives before the write
/// lands. A harness that reads nothing back from PreToolUse gets it with
/// this write's own report instead (the session's spool, proto.rs).
pub fn emit_notice(stdin_data: &str, env: &Env, notice: &str) {
    if crate::harness::current() == "claude" {
        let rendered = crate::harness::render_context("PreToolUse", notice);
        if !rendered.is_empty() {
            println!("{rendered}");
        }
        return;
    }
    let session = serde_json::from_str::<Value>(stdin_data).map(|v| text(&v, "session_id")).unwrap_or_default();
    if !session.is_empty() {
        crate::proto::spool_line(env, &session, "notice", notice);
    }
}

/// Cork's /gate for each target: claims, and certain collisions from
/// the map. Each request says what the write does to the file by name
/// (`write`, so two agents adding their own functions to one file pass) and
/// whether this session lands its pushes. Returns (exit code, block reason,
/// the lines an allowed write carries).
#[allow(clippy::too_many_arguments)]
fn http_gate(
    hook_input: &Value, targets: &[PathBuf], root: &Path, cfg: &config::Config, env: &Env, write: &Write, started: Instant,
    apart: &mut serde_json::Map<String, Value>, creating: &mut serde_json::Map<String, Value>,
) -> (i32, String, String) {
    let mut notices: Vec<String> = Vec::new();
    // gate every file the write touches (a Codex apply_patch can hit several);
    // the first certain collision blocks the whole tool call
    for target in targets {
        let Some(path) = config::rel_path(target, root) else { continue };
        let remaining = TOTAL_BUDGET.checked_sub(started.elapsed()).unwrap_or_default();
        if remaining.is_zero() {
            break;
        }
        let mut payload = json!({"repo_id": cfg.repo_id, "path": path, "session": text(hook_input, "session_id")});
        // a subagent's write carries the parent's session_id and its own
        // agent_id: named, so the server marks the subagent as editing and
        // not the session (whose own focus stands)
        let worker = text(hook_input, "agent_id");
        if !worker.is_empty() {
            payload["worker"] = json!(worker);
            let worker_type = text(hook_input, "agent_type");
            if !worker_type.is_empty() {
                payload["worker_type"] = json!(worker_type);
            }
        }
        if let Some(after) = numbered_new_file(target) {
            payload["claim"] = json!({"after": after});
        }
        if !crate::report::is_collide_artifact(&path)
            && std::fs::metadata(target).map(|m| m.len() <= 512 * 1024).unwrap_or(true)
        {
            if let Some(facts) = crate::proto::write_facts(write.tool, write.input, target, write.cwd, &path) {
                payload["write"] = facts;
            }
        }
        if write.lands {
            payload["lands"] = json!(true);
        }
        // the branch this write lands on: another branch's work never holds it
        if let Some(map) = payload.as_object_mut() {
            crate::branches::add_to(map, root, env);
        }
        let remaining = TOTAL_BUDGET.checked_sub(started.elapsed()).unwrap_or_default();
        if remaining.is_zero() {
            break;
        }
        let Ok(response) = http::post(
            &cfg.server,
            "/gate",
            &cfg.token,
            "collide-gate-hook/1",
            &payload,
            remaining,
        ) else {
            continue;
        };
        let said = text(&response, "notice");
        if !said.is_empty() {
            notices.push(said);
        }
        if let Some(sessions) = response.get("apart").and_then(Value::as_object) {
            apart.extend(sessions.clone());
        }
        if let Some(new_work) = response.get("creating").filter(|c| c.is_object()) {
            creating.insert(path.clone(), new_work.clone());
        }
        // the facts it states join what this session was told: no delta and
        // no lint finding says them again
        let facts: Vec<&str> = response.get("facts").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        // what this agent was told: the session's, or its helper's own
        let session = crate::proto::agent_of(&text(hook_input, "session_id"), &text(hook_input, "agent_id"));
        if !facts.is_empty() && !session.is_empty() {
            let mut told = crate::deps::Told::load(env, &session);
            for fact in facts {
                told.note(fact, "", false);
            }
            told.save();
        }
        let allow = response.get("allow").and_then(Value::as_bool).unwrap_or(true);
        // a change the server only knows as unpushed (its author never
        // landed it) that origin has all the same, pushed some other way:
        // a pull brings it in, and the write is stopped once
        if allow {
            if let Some((reason, event)) = pushed_anyway(&response, root, &path, env, &session, write.lands)
                .filter(|(reason, _)| crate::hold::may_hold(env, &session, root, &path, reason))
            {
                let who = text(&event, "author");
                let reason = with_fix(reason, &path, &[event], target, write);
                return (2, held(&path, &reason, env, &session, Some((root, &who))), String::new());
            }
        }
        if !allow {
            // a block that tells the agent to pull stands only when origin
            // has the change and this copy does not
            if let Some(instead) = not_on_origin(&response, root, &path) {
                notices.push(instead);
                continue;
            }
            let reason = {
                let stated = text(&response, "reason");
                if stated.is_empty() {
                    "Cork: a certain collision touches this path.".to_string()
                } else {
                    stated
                }
            };
            // held once per teammate change, never mid-conflict; the next
            // write goes through (hold.rs)
            if !crate::hold::may_hold(env, &session, root, &path, &reason) {
                continue;
            }
            // the lines of this write the teammate's change makes wrong
            // (held once on the reason as the server said it, so the fix,
            // which varies with the write, never makes it hold again)
            let events: Vec<Value> = ["collisions", "unpushed"].iter()
                .flat_map(|k| response.get(*k).and_then(Value::as_array).cloned().unwrap_or_default())
                .collect();
            let who = events.iter().map(|e| text(e, "author")).find(|a| !a.is_empty()).unwrap_or_default();
            let reason = with_fix(reason, &path, &events, target, write);
            // an owner's standing decision is a rule met, not a collision
            // caught; a teammate's change missing from this copy is one (a
            // live teammate's lines this write would take out are the line
            // check's, in `run_inner`)
            let stale = events.iter().any(|e| e.get("stale_copy").and_then(Value::as_bool) == Some(true))
                || response.get("unpushed").and_then(Value::as_array).is_some_and(|u| !u.is_empty());
            let catch = response.get("decision").and_then(Value::as_bool) != Some(true) && stale;
            return (2, held(&path, &reason, env, &session, catch.then_some((root, who.as_str()))), String::new());
        }
    }
    (0, String::new(), notices.join("\n"))
}

#[cfg(test)]
mod landing_never_says_pull {
    //! With landing on (a `git push` here lands through `cork land`),
    //! no notice the hooks print tells the agent to pull: landing rebases
    //! onto teammates' commits. Study 8 (second study): "Cork git-sync:
    //! origin is N commits ahead … `git pull --rebase`" still fired 23-27
    //! times a round, and agents ran 34 manual pulls, the cause of most of
    //! the round's remaining conflicts. Every template that can name a
    //! teammate's change is rendered here with landing on, and none may say
    //! "pull".

    use super::*;
    use std::process::{Command, Stdio};

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        assert!(ok.success(), "git {args:?}");
    }

    fn says_pull(text: &str) -> bool {
        text.to_lowercase().contains("pull")
    }

    #[test]
    fn no_notice_says_pull_when_landing_is_on() {
        let base = std::env::temp_dir().join(format!("collide-nopull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let origin = base.join("origin.git");
        sh(&base, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        let clone = |name: &str| {
            let dir = base.join(name);
            sh(&base, &["clone", "-q", origin.to_str().unwrap(), dir.to_str().unwrap()]);
            sh(&dir, &["config", "user.email", "t@t"]);
            sh(&dir, &["config", "user.name", "t"]);
            dir
        };
        let seed = clone("seed");
        let total = "def amount_due(lines):\n    return sum(lines)\n\n\ndef helper():\n    return 1\n";
        std::fs::write(seed.join("total.py"), total).unwrap();
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-qm", "seed"]);
        sh(&seed, &["push", "-q", "-u", "origin", "HEAD"]);
        let mine = clone("mine");
        // a teammate renames amount_due and pushes; this checkout changes the same line
        std::fs::write(seed.join("total.py"), total.replace("def amount_due(", "def checkout_total(")).unwrap();
        sh(&seed, &["commit", "-qam", "rename"]);
        sh(&seed, &["push", "-q", "origin", "HEAD"]);
        sh(&mine, &["fetch", "-q", "origin"]);
        std::fs::write(mine.join("total.py"), total.replace("def amount_due(lines):", "def amount_due(lines, region):")).unwrap();

        let mut said: Vec<(String, String)> = Vec::new();
        // git-sync
        let _ = std::fs::remove_file(crate::git::stamp_path(&mine, "sync"));
        said.push(("git-sync".into(), crate::git::sync_note_with(&mine, false, true)));
        // git-sync again within the same landing burst: silent
        let again = crate::git::sync_note_with(&mine, false, true);
        assert!(again.is_empty(), "one git-sync note per landing burst: {again}");
        let _ = std::fs::remove_file(crate::git::stamp_path(&mine, "sync"));
        // the gate: a change on origin the server knows only as unpushed, a
        // block the server says is pushed but origin lacks, an unpushed change
        let home = base.join("home");
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        let rename = json!({"symbol": "amount_due", "kind": "renamed", "new_name": "checkout_total", "author": "Bob (bob@x.com)"});
        said.push(("gate, pushed without landing".into(),
            pushed_anyway(&json!({"unpushed": [rename.clone()]}), &mine, "total.py", &env, "s1", true).map(|r| r.0).unwrap_or_default()));
        said.push(("gate, pushed elsewhere".into(),
            not_on_origin(&json!({"stale_copy": true, "pushed": true}), &base.join("seed"), "total.py").unwrap_or_default()));
        said.push(("gate, unpushed".into(), unpushed_words("Bob (bob@x.com)", "total.py", &rename)));
        // the read note
        let read = json!({"recent_write": {"who": "Bob (bob@x.com)", "agent": "claude", "age_s": 3.0, "in_copy": false, "pushed": true}});
        said.push(("read note".into(), crate::prompt::recent_write_note(&read, "total.py", true)));
        // the protocol's gate: a pushed op this copy lacks, and a draft
        let alice = json!({"session": "a1", "agent_id": "claude", "person": "alice@x.com"});
        for layer in ["promoted", "draft"] {
            let theirs = json!([{"op_id": "a1.1", "key": "code:total.py::amount_due", "version": 1, "kind": "rename",
                "public": {"new_name": "checkout_total"}, "author": alice, "hlc": {"w": 1, "c": 0}, "layer": layer}]);
            let stale = json!({"key": "code:total.py::amount_due", "status": "stale", "would": "conflict", "theirs": theirs,
                "current": {"version": 1, "hash": "h"}});
            let verdict = crate::proto::verdict(&[stale], None, true);
            said.push((format!("protocol gate, {layer}"), verdict.block.unwrap_or_default()));
            said.extend(verdict.notices.into_iter().map(|(_, line)| (format!("protocol notice, {layer}"), line)));
        }
        // the semantic alert at the gate, pushed and unpushed
        let app = "from total import amount_due\n\n\ndef pay(x):\n    return amount_due(x)\n";
        std::fs::write(mine.join("app.py"), app).unwrap();
        let deps = crate::deps::deps_of(&mine, "app.py", app);
        for layer in ["promoted", "draft"] {
            let op = json!({"kind": "rename", "public": {"new_name": "checkout_total"}, "author": alice, "layer": layer});
            let change = crate::deps::summarize(&[op], crate::proto::who).unwrap();
            if let Some(f) = deps.first().and_then(|d| crate::deps::judge(d, &change, "app.py", &std::collections::BTreeSet::new())) {
                said.push((format!("semantic alert, {layer}"), f.full));
            }
        }
        // the lint, for a rename on origin and one nowhere yet
        for renamed_to in ["checkout_total", "never_pushed"] {
            let finding = json!({"findings": [{"rule": "stale_name", "symbol": "amount_due", "renamed_to": renamed_to, "by": "Bob",
                "lines": [1], "fix": {"class": "mechanical"}}]});
            said.push((format!("lint, {renamed_to}"), crate::report::lint_context(&finding, false, &mine, true)));
        }
        // land: a conflict it left stopped, and a push while it still is
        let files = vec!["total.py".to_string()];
        said.push(("land conflict".into(),
            crate::land::conflict_message(1, "origin/main", &["Bob".to_string()], &files, "<<<<<<< ours")));
        said.push(("land, rebase in progress".into(), crate::land::in_progress_message(&files)));
        said.push(("land, rebase in progress, no files".into(), crate::land::in_progress_message(&[])));
        // the protocol's live notices, as the next tool result shows them
        crate::proto::spool_line(&env, "s1", "delta", "Δ amount_due → checkout_total (alice@x.com)");
        said.push(("protocol live header".into(), crate::proto::take_notices(&env, "s1")));
        // v75: a block's fix lines, for this very write
        let after = total.replace("def amount_due(lines):", "def amount_due(lines, region):");
        let new_lines: std::collections::BTreeSet<usize> = [1].into_iter().collect();
        let reason = pushed_anyway(&json!({"unpushed": [rename.clone()]}), &mine, "total.py", &env, "s2", true).map(|r| r.0).unwrap_or_default();
        let fixed = crate::fixlines::with_fix(reason, "total.py", &[rename.clone()], Some(&(after.clone(), new_lines.clone())));
        assert!(fixed.contains("In this write (total.py)"), "{fixed}");
        said.push(("fix lines, rename".into(), fixed));
        for (kind, event) in [("removed", json!({"symbol": "amount_due", "kind": "removed"})),
                              ("signature", json!({"symbol": "amount_due", "kind": "signature", "signature_after": "def amount_due(a, b, c)"}))] {
            said.push((format!("fix lines, {kind}"), crate::fixlines::with_fix(String::new(), "app.py", &[event], Some(&(app.to_string(), Default::default())))));
        }
        // v75: land's merge preview
        let changed = vec![("total.py".to_string(), "amount_due".to_string(), "signature".to_string(), 1)];
        let answer = json!({"conflicts": [{"path": "total.py", "symbol": "amount_due", "who": "Bob (bob@x.com)", "kind": "rename",
            "now": "checkout_total"}], "more": 2});
        said.extend(crate::land::preview_lines(&answer, &changed).into_iter().map(|t| ("merge preview".to_string(), t)));
        // v75: a protocol block from another branch, said as a line
        said.push(("branch line".into(), branch_line("alice@x.com (claude)", "total.py", "feat/x")));
        // the manual-pull note discourages pulling, so it is the one line
        // allowed to name it; it must never read as a step to take
        let note = manual_pull_note("git pull --rebase", &mine, "s9", &env);
        if let Some(n) = &note {
            assert!(n.contains("Carry on and push when done") && !n.contains("Run `git pull"), "{n}");
        }

        for (what, text) in &said {
            assert!(!says_pull(text), "{what} says pull with landing on: {text}");
        }
        let spoken: Vec<&(String, String)> = said.iter().filter(|(_, t)| !t.is_empty()).collect();
        assert!(spoken.len() >= 10, "every template was rendered: {said:?}");
        for what in ["fix lines, rename", "fix lines, removed", "fix lines, signature", "merge preview", "branch line"] {
            assert!(said.iter().any(|(w, t)| w == what && !t.is_empty()), "{what} was not rendered: {said:?}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod creating_never_holds {
    //! Scaling study 2 (v76): b7 was about to use `balance_for` in a credits
    //! module b6 was still writing; the protocol's lease said "is editing
    //! balance_for right now. Wait for them to land it", and b7 ran `rm -f
    //! lanternly/billing/credits.py` and rebuilt around it (25 calls). A lease
    //! on code a teammate is creating is its interface, never a hold.

    use super::*;
    use std::process::{Command, Stdio};

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        assert!(ok.success(), "git {args:?}");
    }

    #[test]
    fn a_lease_on_a_module_being_created_is_its_interface_never_a_wait() {
        let base = std::env::temp_dir().join(format!("collide-creating-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("billing")).unwrap();
        sh(&base, &["init", "-q", "-b", "main"]);
        sh(&base, &["config", "user.email", "t@t"]);
        sh(&base, &["config", "user.name", "t"]);
        std::fs::write(base.join("billing/invoices.py"), "def total_due(invoice):\n    return 1\n").unwrap();
        sh(&base, &["add", "."]);
        sh(&base, &["commit", "-qm", "seed"]);
        // a module nobody committed, and a new name in a committed file: created
        assert!(being_created(&base, "billing/credits.py", "balance_for"));
        assert!(being_created(&base, "billing/invoices.py", "credit_applied"));
        // a function already in the repo: a real edit, the lease stands
        assert!(!being_created(&base, "billing/invoices.py", "total_due"));

        // the gate's answer names what the teammate has written so far
        let creating = json!({"who": "another session of yours", "new_file": true, "names": ["balance_for"],
                              "interface": "def balance_for(member_id: int) -> int"});
        let line = creating_line("another session of yours", "billing/credits.py", "balance_for", Some(&creating));
        assert_eq!(line, "Cork in flight: billing/credits.py is new work by another session of yours, not pushed yet. \
Build against its interface instead of writing your own copy: import it and keep your edit to your own code; landing \
will rebase onto it once it lands. Interface: def balance_for(member_id: int) -> int");
        // without the server's answer, its name
        let line = creating_line("Bob", "billing/invoices.py", "credit_applied", None);
        assert!(line.contains("Bob is adding `credit_applied` to billing/invoices.py") && line.contains("Build against its interface"), "{line}");
        for said in [&line] {
            assert!(!said.to_lowercase().contains("wait") && !said.contains("rm ") && !said.contains("delete"), "{said}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_lease_block_says_retry_never_wait() {
        let alice = json!({"person": "alice@x.com", "agent_id": "claude", "session": "a1"});
        let leased = json!({"key": "code:tax.py::compute_tax", "status": "leased", "holder": alice});
        let said = crate::proto::verdict(&[leased], None, true);
        let block = said.block.expect("a lease on existing code still holds once");
        assert!(!block.to_lowercase().contains("wait"), "{block}");
        assert!(block.contains("retry the write and it goes through"), "{block}");
        assert!(said.block_fact.starts_with("lease|"));
    }
}

#[cfg(test)]
mod line_tests {
    //! Lattice test, round 2 (v104), replayed: A adds an issue `kind` to the
    //! `Issue` struct and to the INSERT and UPDATE statements; B, in the same
    //! checkout, adds a `start_date` beside it. Before: B was stopped four
    //! times on edits that only added lines next to A's (a lease on `Issue`,
    //! `open`, API.md, `mountIssue`), and its edits that rewrote A's INSERT
    //! and UPDATE lines went through with no word. After: the additions go
    //! in with no block, the Edit that rewrites A's lines is told whose and
    //! which, and a `sed -i` that would rewrite them unseen is stopped once.

    use super::*;

    const DB: &str = "pub struct Issue {\n    pub id: i64,\n    pub title: String,\n}\n";
    const API: &str = "fn create_issue() {\n    db.execute(\"INSERT INTO issues (title) VALUES (?1)\", params![title]);\n}\n\
fn update_issue() {\n    db.execute(\"UPDATE issues SET title = ?1 WHERE id = ?2\", params![title, id]);\n}\n";

    fn edit(path: &Path, old: &str, new: &str) -> Value {
        json!({"file_path": path.to_string_lossy(), "old_string": old, "new_string": new})
    }

    /// A writes through the harness: the journal takes its lines.
    fn a_writes(root: &Path, rel: &str, from: &str, to: &str, env: &Env) {
        let target = root.join(rel);
        let before = std::fs::read(&target).unwrap();
        std::fs::write(&target, std::fs::read_to_string(&target).unwrap().replacen(from, to, 1)).unwrap();
        crate::journal::record(root, &target, rel, "aaaa1111-kind", "", "", "Kind field", Some(before), env);
    }

    #[test]
    fn b_adds_beside_as_lines_and_is_told_when_it_rewrites_them() {
        let root = std::env::temp_dir().join(format!("cork-lines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), root.join("home").to_string_lossy().into_owned());
        std::fs::write(root.join("src/db.rs"), DB).unwrap();
        std::fs::write(root.join("src/api.rs"), API).unwrap();
        // A: the struct field and both statements
        a_writes(&root, "src/db.rs", "    pub title: String,\n", "    pub title: String,\n    pub kind: Option<String>,\n", &env);
        a_writes(&root, "src/api.rs", "(title) VALUES (?1)\", params![title]", "(title, kind) VALUES (?1, ?2)\", params![title, kind]", &env);
        a_writes(&root, "src/api.rs", "SET title = ?1 WHERE id = ?2\", params![title, id]", "SET title = ?1, kind = ?3 WHERE id = ?2\", params![title, id, kind]", &env);
        let b = "bbbb2222-start";
        let cwd = root.clone();
        let check = |tool: &str, input: &Value, rel: &str| crate::lines::check(&root, &[root.join(rel)], tool, input, &cwd, b, "", &env);

        // 1. a field added beside A's: only adds, so A's lease on `Issue` stops nothing
        let beside = edit(&root.join("src/db.rs"), "    pub kind: Option<String>,\n", "    pub kind: Option<String>,\n    pub start_date: Option<String>,\n");
        let added = check("Edit", &beside, "src/db.rs");
        assert!(added.adds_only && added.rewrites.is_empty(), "{added:?}");
        assert!(!lease_holds(&added, "aaaa1111-kind"), "a pure addition is never held");
        assert_eq!(judge_lines(&added, &env, b), (None, String::new()));

        // 2. the INSERT line A just wrote, rewritten by an Edit: told whose and which, not stopped
        let insert = edit(&root.join("src/api.rs"), "(title, kind) VALUES (?1, ?2)\", params![title, kind]",
            "(title, kind, start_date) VALUES (?1, ?2, ?3)\", params![title, kind, start_date]");
        let rewrote = check("Edit", &insert, "src/api.rs");
        assert_eq!(rewrote.rewrites.len(), 1, "{rewrote:?}");
        assert_eq!(rewrote.rewrites[0].1.lines, vec![2]);
        assert!(!lease_holds(&rewrote, "aaaa1111-kind"), "the journal speaks for A's lines here");
        let (block, said) = judge_lines(&rewrote, &env, b);
        assert!(block.is_none(), "{block:?}");
        assert!(said.starts_with("Cork: this write changes line 2 of src/api.rs, which another session of yours (\"Kind field\") wrote just now"), "{said}");
        assert!(said.contains("INSERT INTO issues (title, kind)") && said.contains("Keep what they added"), "{said}");
        assert_eq!(judge_lines(&rewrote, &env, b), (None, String::new()), "said once");

        // 3. the UPDATE line, rewritten unseen by sed: stopped once, then through
        let sed = json!({"command": "sed -i '' 's/kind = ?3 WHERE/kind = ?3, start_date = ?4 WHERE/' src/api.rs"});
        let unseen = check("Bash", &sed, "src/api.rs");
        assert_eq!(unseen.rewrites.len(), 1, "{unseen:?}");
        assert_eq!(unseen.rewrites[0].1.lines, vec![5]);
        let (block, _) = judge_lines(&unseen, &env, b);
        let block = block.expect("stopped once");
        assert!(block.starts_with("this command would overwrite line 5 of src/api.rs, which another session of yours (\"Kind field\")"), "{block}");
        assert_eq!(judge_lines(&unseen, &env, b).0, None, "the retry goes through");

        // 4. an append to the file by shell: adds only
        let append = check("Bash", &json!({"command": "echo '// start_date' >> src/api.rs"}), "src/api.rs");
        assert!(append.adds_only && append.rewrites.is_empty(), "{append:?}");

        // "build against its interface" is for a write that creates the
        // name, never for one that changes a function already in the file
        std::fs::write(root.join("src/db.rs"), "pub fn issue_from_row(r: &Row) -> Issue {\n    todo!()\n}\n").unwrap();
        assert!(in_file_now(&root, "src/db.rs", "issue_from_row"));
        assert!(!in_file_now(&root, "src/db.rs", "parse_kind"));

        // A's own edits never come back to it
        let own = crate::lines::check(&root, &[root.join("src/api.rs")], "Edit", &insert, &cwd, "aaaa1111-kind", "", &env);
        assert!(own.rewrites.is_empty(), "{own:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same round with two helpers of ONE session in one checkout: the
    /// journal held their lines as the session's, so B rewrote A's INSERT
    /// line unseen and no one said a word. Each helper is its own agent now:
    /// B is told whose lines (A, by name), a `sed` that would overwrite them
    /// unseen is stopped once, A never hears of its own lines, and the lead
    /// is told of its helper's lines like any teammate's.
    #[test]
    fn a_helpers_lines_are_a_teammates_to_its_sibling_and_its_lead() {
        let root = std::env::temp_dir().join(format!("cork-helper-lines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), root.join("home").to_string_lossy().into_owned());
        std::fs::write(root.join("src/api.rs"), API).unwrap();
        let session = "cccc3333-lead";
        // helper A writes the INSERT line
        let target = root.join("src/api.rs");
        let before = std::fs::read(&target).unwrap();
        std::fs::write(&target, API.replacen("(title) VALUES (?1)\", params![title]", "(title, kind) VALUES (?1, ?2)\", params![title, kind]", 1)).unwrap();
        crate::journal::record(&root, &target, "src/api.rs", session, "agent-a", "repo-names", "Lead", Some(before), &env);
        let cwd = root.clone();
        let check = |worker: &str, tool: &str, input: &Value| crate::lines::check(&root, &[target.clone()], tool, input, &cwd, session, worker, &env);
        let insert = edit(&target, "(title, kind) VALUES (?1, ?2)\", params![title, kind]",
            "(title, kind, start_date) VALUES (?1, ?2, ?3)\", params![title, kind, start_date]");

        // helper B, by Edit: told whose and which
        let b = crate::proto::agent_of(session, "agent-b");
        let rewrote = check("agent-b", "Edit", &insert);
        assert_eq!(rewrote.rewrites.len(), 1, "{rewrote:?}");
        let (block, said) = judge_lines(&rewrote, &env, &b);
        assert!(block.is_none(), "{block:?}");
        assert!(said.starts_with("Cork: this write changes line 2 of src/api.rs, which your session's helper repo-names wrote just now"), "{said}");
        // helper D, by sed, unseen: stopped once, then through
        let d = crate::proto::agent_of(session, "agent-d");
        let sed = json!({"command": "sed -i '' 's/(title, kind) VALUES/(title, kind, start_date) VALUES/' src/api.rs"});
        let unseen = check("agent-d", "Bash", &sed);
        let (block, _) = judge_lines(&unseen, &env, &d);
        assert!(block.expect("stopped once").contains("which your session's helper repo-names wrote"));
        assert_eq!(judge_lines(&unseen, &env, &d).0, None, "the retry goes through");

        // helper A: its own lines are no news
        assert!(check("agent-a", "Edit", &insert).rewrites.is_empty());
        // the lead: its helper's lines are a teammate's, told on its own list
        let lead = check("", "Edit", &insert);
        let (_, said) = judge_lines(&lead, &env, session);
        assert!(said.contains("which your session's helper repo-names wrote"), "{said}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod labels {
    use super::*;

    /// v104's test: "kevin@x.com is editing Issue right now ...
    /// `cork message --to kevin@x.com`" named the account both
    /// sessions shared. The session on the code is addressed by its name.
    #[test]
    fn a_lease_block_is_answered_by_the_holders_session_name() {
        let reason = "kevin@x.com is editing Issue right now (their intent: add an issue kind field), not pushed yet. Keep your \
edit to your own lines of it, then retry the write and it goes through; landing merges both. If you both need the same lines, \
message them (`cork message --to kevin@x.com \"...\"`) and agree who changes what.";
        let said = address_by_name(reason, "cork-linear-test-0a");
        assert!(said.contains("(`cork message --to cork-linear-test-0a \"...\"`)") && !said.contains("--to kevin@x.com"), "{said}");
        assert!(said.contains("(their intent: add an issue kind field)"));
    }
}
