//! Outline first: a whole-file Read of a big file Cork has mapped is
//! declined once, with the file's outline in its place.
//!
//! Agents read every file whole, briefing or not, believing the Edit tool
//! needs the whole file read first. It does not: Edit accepts a partial
//! Read. So when an agent Reads a file of OUTLINE_MIN_LINES or more with no
//! offset or limit, and the file is mapped in full and unchanged since Cork
//! indexed it (its content hash is the one this checkout's index sent), the
//! read is declined once, its reason the outline: every symbol with its line
//! range and the first line of its docstring, parsed here with the parser
//! the server runs. The agent Reads the range it needs, or Reads the file
//! again, which always goes through, as does any ranged Read.
//!
//! It runs before every Read, so it is local and cheap: the index state the
//! hooks already keep and a parse of the file, no network. Anything unsure
//! (no config, no index entry, an unclean parse, an outline barely smaller
//! than the file) lets the Read through. Claude Code only: Codex reads
//! through its shell, and Cursor's Read input is not one we can tell a
//! ranged read in.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{self, Env};

/// Whether a whole-file Read is outlined first when nothing says otherwise
/// (`cork reads full|outline`, `CORK_READS=full|outline`).
pub const OUTLINE_FIRST: bool = true;
/// A file shorter than this is cheaper read than outlined.
const OUTLINE_MIN_LINES: usize = 300;
/// The outline never grows past this, nor past this share of the file.
const MAX_OUTLINE_CHARS: usize = 6_000;
const MAX_OUTLINE_SHARE: f64 = 0.4;
/// The symbols must span at least this share of the file's lines: a file
/// of loose top-level code has no outline worth reading.
const MIN_COVERAGE: f64 = 0.5;
const MAX_SIGNATURE_CHARS: usize = 140;
const MAX_DOC_CHARS: usize = 90;
/// A session's record of what it was told is dropped after this.
const KEEP_S: u64 = 2 * 86_400;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Whether whole-file Reads are outlined first on this machine.
pub fn outline_first_on(env: &Env) -> bool {
    match config::get(env, "CORK_READS").trim() {
        "full" => return false,
        "outline" => return true,
        _ => {}
    }
    match crate::machine::settings(env).get("reads").and_then(Value::as_str) {
        Some("full") => false,
        Some("outline") => true,
        _ => OUTLINE_FIRST,
    }
}

/// The reason a PreToolUse Read is declined with (the file's outline), or
/// None to let it through.
pub fn outline_first(stdin_data: &str, env: &Env) -> Option<String> {
    if crate::harness::current() != "claude" {
        return None;
    }
    let input: Value = serde_json::from_str(stdin_data).ok()?;
    if text(&input, "tool_name") != "Read" || text(&input, "hook_event_name") != "PreToolUse" {
        return None;
    }
    let tool_input = input.get("tool_input")?;
    // a ranged read (or a PDF's pages) is what this asks for: always through
    if ["offset", "limit", "pages"].iter().any(|k| tool_input.get(*k).is_some_and(|v| !v.is_null())) {
        return None;
    }
    let session = text(&input, "session_id");
    let file = PathBuf::from(text(tool_input, "file_path"));
    if session.is_empty() || !file.is_absolute() || !outline_first_on(env) {
        return None;
    }
    let root = config::find_repo_root(&[file.parent().map(Path::to_path_buf)])?;
    let rel = config::rel_path(&file, &root)?;
    if !crate::report::is_map_source(&rel) {
        return None;
    }
    // the second whole read of a file in a session (or one of its
    // subagents) is the agent saying it wants all of it
    let agent = format!("{session}/{}", text(&input, "agent_id"));
    if told(&agent, &rel, env) {
        return None;
    }
    let content = crate::report::source_of(&file)?;
    let lines = content.lines().count();
    if lines < OUTLINE_MIN_LINES {
        return None;
    }
    // mapped, and unchanged since: this checkout's index sent this content
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return None;
    }
    let state = config::load_json(&crate::report::index_state_path(&root, env, &cfg.server, &cfg.repo_id));
    let sent = state.get("files").and_then(|f| f.get(&rel)).and_then(Value::as_str)?;
    if sent != crate::report::sha256_hex(content.as_bytes()) {
        return None;
    }
    let reason = outline(&rel, &content)?;
    note_told(&agent, &rel, env);
    crate::prompt::note_injected(&session, reason.len(), env);
    Some(reason)
}

/// The decline's text: the file's symbols by line range with the first
/// line of each docstring, and what to do next. None when the parse is not
/// clean, its symbols cover too little of the file, or the outline would
/// not be much smaller than the file.
pub fn outline(rel: &str, content: &str) -> Option<String> {
    let parsed = cork_core::engine::parse(rel, content)?;
    if parsed.status != "clean" || parsed.symbols.is_empty() {
        return None;
    }
    let total = content.lines().count();
    let mut covered = vec![false; total + 1];
    for symbol in &parsed.symbols {
        let (from, to) = (symbol.span.0 as usize, (symbol.span.1 as usize).min(total));
        for line in from.max(1)..=to {
            covered[line] = true;
        }
    }
    if (covered.iter().filter(|c| **c).count() as f64) < MIN_COVERAGE * total as f64 {
        return None;
    }
    let mut symbols: Vec<&cork_core::engine::Symbol> = parsed.symbols.iter().collect();
    symbols.sort_by_key(|s| s.span.0);
    let mut out = vec![format!(
        "Cork: {rel} is {total} lines, mapped in full and unchanged since Cork indexed it, so here is its outline instead \
of the whole file (line ranges):"
    )];
    let doc = first_line(&parsed.doc, MAX_DOC_CHARS);
    if !doc.is_empty() {
        out.push(format!("  {doc}"));
    }
    let mut used: usize = out.iter().map(|l| l.len() + 1).sum();
    let mut left = 0usize;
    for symbol in &symbols {
        let signature = clip(&symbol.signature.split_whitespace().collect::<Vec<_>>().join(" "), MAX_SIGNATURE_CHARS);
        let signature = if signature.is_empty() { symbol.name.clone() } else { signature };
        let doc = first_line(&symbol.doc, MAX_DOC_CHARS);
        let line = if doc.is_empty() {
            format!("  {}-{} {signature}", symbol.span.0, symbol.span.1)
        } else {
            format!("  {}-{} {signature} — {doc}", symbol.span.0, symbol.span.1)
        };
        if used + line.len() + 1 > MAX_OUTLINE_CHARS {
            left += 1;
            continue;
        }
        used += line.len() + 1;
        out.push(line);
    }
    if left > 0 {
        out.push(format!("  … {left} more symbol(s)"));
    }
    out.push(
        "Read only the range you need (Read with offset = its first line, limit = its line count): the Edit tool accepts \
a partial read. To read the whole file anyway, Read it again: that read goes through."
            .to_string(),
    );
    let reason = out.join("\n");
    if reason.len() as f64 > MAX_OUTLINE_SHARE * content.len() as f64 {
        return None;
    }
    Some(reason)
}

/// A docstring's first sentence or line, clipped.
fn first_line(doc: &str, max: usize) -> String {
    let line = doc.trim().lines().next().unwrap_or("").trim();
    let sentence = match line.find(". ") {
        Some(at) => &line[..=at],
        None => line,
    };
    clip(sentence, max)
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}…", text.chars().take(max - 1).collect::<String>().trim_end())
}

/// Where a session's outlined files are listed, one path a line.
fn told_path(agent: &str, env: &Env) -> PathBuf {
    let digest = crate::report::sha256_hex(agent.as_bytes());
    config::data_dir(env).join("outlined").join(format!("{}.txt", &digest[..16]))
}

fn told(agent: &str, rel: &str, env: &Env) -> bool {
    std::fs::read_to_string(told_path(agent, env)).is_ok_and(|listed| listed.lines().any(|l| l == rel))
}

/// Appended, one short line: parallel Reads of one session never lose each
/// other's. A new session's file sweeps the ones past KEEP_S.
fn note_told(agent: &str, rel: &str, env: &Env) {
    let path = told_path(agent, env);
    let Some(dir) = path.parent() else { return };
    let _ = std::fs::create_dir_all(dir);
    if !path.exists() {
        sweep(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(format!("{rel}\n").as_bytes());
    }
}

fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let old = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|m| m.elapsed().ok());
        if old.is_some_and(|age| age.as_secs() > KEEP_S) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `cork reads full|outline|status`: whether an agent's whole-file Read of a
/// big mapped file gets the outline first.
pub fn command(args: &[String], env: &Env) -> i32 {
    let mode = match args.first().map(String::as_str).unwrap_or("status") {
        "full" => "full",
        "outline" => "outline",
        "status" => {
            println!(
                "Cork {}.",
                if outline_first_on(env) {
                    "outlines first: an agent's whole-file Read of a big file Cork has mapped gets the outline, and its second Read gets the file (`cork reads full` turns it off)"
                } else {
                    "lets every Read through whole (`cork reads outline` gives big mapped files their outline first)"
                }
            );
            return 0;
        }
        _ => {
            println!(
                "usage: cork reads full|outline|status\n\nOutline first (on unless switched off): an agent's first whole-file Read of a \
big file Cork has mapped is answered with the file's outline, by line range; it then Reads the range it needs, or the \
file again."
            );
            return 2;
        }
    };
    let mut machine = crate::machine::settings(env);
    machine["reads"] = serde_json::json!(mode);
    if !crate::machine::save_settings(env, &machine) {
        println!("Cork: could not save the setting.");
        return 1;
    }
    println!(
        "{}",
        if mode == "full" {
            "Cork lets every Read through whole now."
        } else {
            "Cork outlines big mapped files first now: the second Read of a file gets all of it."
        }
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A repo set up for Cork with one big mapped Python file, and the env
    /// its hooks would run in.
    fn repo(tag: &str, lines: usize) -> (PathBuf, PathBuf, Env) {
        let base = std::env::temp_dir().join(format!("cork-reads-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("repo");
        std::fs::create_dir_all(root.join(".cork")).unwrap();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::create_dir_all(base.join("home")).unwrap();
        std::fs::write(root.join(".cork/config.json"), json!({"server_url": "http://127.0.0.1:9", "repo_id": "github.com/acme/app"}).to_string()).unwrap();
        let mut source = String::from("\"\"\"Billing: charges and refunds.\"\"\"\n\n");
        let mut i = 0;
        while source.lines().count() < lines {
            source.push_str(&format!(
                "\n\ndef charge_{i}(user, cents: int) -> int:\n    \"\"\"Charges user {i}. Then logs it.\"\"\"\n    total = cents\n{}    return total\n",
                "    total += 1\n    total -= 1\n".repeat(6)
            ));
            i += 1;
        }
        let file = root.join("pkg/billing.py");
        std::fs::write(&file, &source).unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), base.join("home").to_string_lossy().into_owned());
        env.insert("CORK_TOKEN".into(), "cat_test".into());
        // the session start's index sent this content
        let cfg = config::config(Some(&root), &env);
        let state = crate::report::index_state_path(&root, &env, &cfg.server, &cfg.repo_id);
        crate::report::save_json(&state, &json!({"files": {"pkg/billing.py": crate::report::sha256_hex(source.as_bytes())}}));
        (root, file, env)
    }

    fn read(file: &Path, session: &str, extra: Value) -> String {
        let mut tool_input = json!({"file_path": file.to_string_lossy()});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            tool_input[k] = v;
        }
        json!({"hook_event_name": "PreToolUse", "tool_name": "Read", "session_id": session, "tool_input": tool_input}).to_string()
    }

    #[test]
    fn a_first_whole_read_of_a_big_mapped_file_gets_its_outline_and_the_second_goes_through() {
        let (_root, file, env) = repo("first", 400);
        let reason = outline_first(&read(&file, "s1", json!({})), &env).expect("declined with the outline");
        assert!(reason.starts_with("Cork: pkg/billing.py is "), "{reason}");
        assert!(reason.contains("\n  Billing: charges and refunds.\n"), "{reason}");
        assert!(reason.contains("\n  5-20 def charge_0(user, cents: int) -> int — Charges user 0.\n"), "{reason}");
        assert!(reason.ends_with("To read the whole file anyway, Read it again: that read goes through."), "{reason}");
        // the second whole read of the same file in the session goes through
        assert_eq!(outline_first(&read(&file, "s1", json!({})), &env), None);
        // another session is told once too
        assert!(outline_first(&read(&file, "s2", json!({})), &env).is_some());
    }

    #[test]
    fn ranged_reads_changed_unmapped_and_small_files_go_through() {
        let (root, file, env) = repo("through", 400);
        for ranged in [json!({"offset": 4, "limit": 7}), json!({"limit": 50}), json!({"offset": 100})] {
            assert_eq!(outline_first(&read(&file, "s1", ranged.clone()), &env), None, "{ranged}");
        }
        // changed since the index sent it: the map may be behind
        let mut changed = std::fs::read_to_string(&file).unwrap();
        changed.push_str("\n\ndef late():\n    return 1\n");
        std::fs::write(&file, &changed).unwrap();
        assert_eq!(outline_first(&read(&file, "s1", json!({})), &env), None);
        // a file the index never sent
        let other = root.join("pkg/other.py");
        std::fs::copy(&file, &other).unwrap();
        assert_eq!(outline_first(&read(&other, "s1", json!({})), &env), None);
        // a small file is cheaper read than outlined
        let (_root, small, env) = repo("small", 120);
        assert_eq!(outline_first(&read(&small, "s1", json!({})), &env), None);
    }

    #[test]
    fn switched_off_every_read_goes_through() {
        let (_root, file, mut env) = repo("off", 400);
        env.insert("CORK_READS".into(), "full".into());
        assert_eq!(outline_first(&read(&file, "s1", json!({})), &env), None);
        env.remove("CORK_READS");
        let mut machine = crate::machine::settings(&env);
        machine["reads"] = json!("full");
        assert!(crate::machine::save_settings(&env, &machine));
        assert!(!outline_first_on(&env));
        assert_eq!(outline_first(&read(&file, "s1", json!({})), &env), None);
        // and the env var wins over the machine's setting, both ways
        env.insert("CORK_READS".into(), "outline".into());
        assert!(outline_first(&read(&file, "s1", json!({})), &env).is_some());
    }

    #[test]
    fn loose_code_has_no_outline_worth_giving() {
        let body: String = (0..400).map(|i| format!("x_{i} = {i}\n")).collect();
        assert_eq!(outline("pkg/settings.py", &body), None);
    }
}
