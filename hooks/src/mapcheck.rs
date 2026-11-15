//! Every map fact a hook prints is checked against this checkout's copy
//! first.
//!
//! Several checkouts of one repo report into one map, which holds each file
//! as the checkout that last reported it had it. An agent in another
//! checkout was handed `fn date` at src/api.rs:91-97, with a signature from
//! an older version of the file, and told to build on the briefing instead
//! of reading. So the server sends, beside each briefing, every symbol line
//! it printed with where it came from (`checks`), and the hook checks each
//! against a parse of the file here, with the parser the server runs: a
//! line that matches stays, a symbol that moved or changed is printed as
//! this copy has it, a symbol this copy does not have is dropped, a module
//! whose file is not here loses its block, and a module whose copy does not
//! parse right now loses its lines and says why. A module's "complete" mark
//! stays only when every line held. The grep companion's answers are held
//! to the same check (`local_fact`). Local and cheap: one parse per file,
//! no network.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

/// The docstring length a briefing's symbol line carries (brief.rs
/// MAX_SYMBOL_DOC_CHARS): a line rewritten here reads like the server's.
const DOC_CHARS: usize = 200;
/// Names on a constants line (brief.rs MAX_VALUE_NAMES).
const VALUE_NAMES: usize = 8;

/// One symbol as this checkout's copy has it.
#[derive(Debug, Clone, PartialEq)]
pub struct Local {
    pub signature: String,
    pub span: (u32, u32),
    pub doc: String,
}

/// A file's symbols here, by name: None when the file is not here or no
/// parser knows it, `Err` when it does not parse cleanly right now.
type Parsed = Option<Result<HashMap<String, Local>, ()>>;

fn parse(root: &Path, rel: &str) -> Parsed {
    let content = crate::report::source_of(&root.join(rel))?;
    let parsed = cork_core::engine::parse(rel, &content)?;
    if parsed.status != "clean" {
        return Some(Err(()));
    }
    Some(Ok(parsed
        .symbols
        .into_iter()
        .map(|s| (s.name.clone(), Local { signature: s.signature, span: s.span, doc: s.doc }))
        .collect()))
}

/// The symbol `name` of `rel` as this copy has it, or None (not here, or
/// the file does not parse now): what a "Cork map" answer prints instead of
/// the map's entry, or drops.
pub fn local_fact(root: &Path, rel: &str, name: &str) -> Option<Local> {
    match parse(root, rel) {
        Some(Ok(mut symbols)) => symbols.remove(name),
        _ => None,
    }
}

/// `  signature [a-b] — doc`, as brief.rs `symbol_line` prints it.
pub fn symbol_line(name: &str, local: &Local) -> String {
    let signature = if local.signature.is_empty() { name } else { local.signature.as_str() };
    let span = if local.span.0 > 0 && local.span.1 >= local.span.0 { format!(" [{}-{}]", local.span.0, local.span.1) } else { String::new() };
    if local.doc.is_empty() {
        format!("  {signature}{span}")
    } else {
        format!("  {signature}{span} — {}", clip(&local.doc, DOC_CHARS))
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    let cut = head.rfind(' ').filter(|&i| i > max / 2).unwrap_or(head.len());
    format!("{}…", head[..cut].trim_end())
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// What checking a briefing did, for the hook's tally and its tests.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Checked {
    pub text: String,
    pub kept: usize,
    pub rewritten: usize,
    pub dropped: usize,
}

/// A module's header line: the path alone, or the path and ` — ` meta.
fn is_header(line: &str, path: &str) -> bool {
    line == path || line.starts_with(&format!("{path} — "))
}

/// The header without its "complete" mark (` · complete · unchanged…`, or
/// the whole ` — complete · …` when it was the only meta).
fn unmark(header: &str) -> String {
    for lead in [" · complete · ", " — complete · "] {
        if let Some(at) = header.find(lead) {
            return header[..at].to_string();
        }
    }
    header.to_string()
}

/// The briefing's text with each checked line held to this checkout's copy
/// (see the module doc). Lines the server did not list are left as they are.
pub fn apply(root: &Path, brief: &Value) -> Checked {
    let mut lines: Vec<Option<String>> = text(brief, "text").lines().map(|l| Some(l.to_string())).collect();
    let checks: Vec<Value> = brief.get("checks").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Checked::default();
    if checks.is_empty() {
        out.text = text(brief, "text");
        return out;
    }
    let mut files: HashMap<String, Parsed> = HashMap::new();
    let mut parsed = |rel: &str| -> Parsed { files.entry(rel.to_string()).or_insert_with(|| parse(root, rel)).clone() };
    // where a path's block starts and ends: its header, then its indented lines
    let block = |lines: &[Option<String>], path: &str| -> Option<(usize, usize)> {
        let start = lines.iter().position(|l| l.as_deref().is_some_and(|l| is_header(l, path)))?;
        let mut end = start + 1;
        while end < lines.len() && lines[end].as_deref().is_none_or(|l| l.starts_with("  ")) {
            end += 1;
        }
        Some((start, end))
    };
    // each path's verdict once: missing, not parsing, or its symbols
    let mut touched: Vec<String> = Vec::new();
    for check in &checks {
        let kind = text(check, "kind");
        let line = text(check, "line");
        if kind == "spans" {
            // `name [a-b]` refs anywhere in the text (an earlier agent's work)
            let Some(at) = lines.iter().position(|l| l.as_deref() == Some(line.as_str())) else { continue };
            let mut fixed = line.clone();
            for pair in check.get("refs").and_then(Value::as_array).into_iter().flatten() {
                let (path, name) = (pair.get(0).and_then(Value::as_str).unwrap_or(""), pair.get(1).and_then(Value::as_str).unwrap_or(""));
                let Ok(re) = regex::Regex::new(&format!(r"\b{} \[\d+-\d+\]", regex::escape(name))) else { continue };
                let local = match parsed(path) {
                    Some(Ok(symbols)) => symbols.get(name).cloned(),
                    _ => None,
                };
                let now = match local {
                    Some(l) if l.span.0 > 0 => format!("{name} [{}-{}]", l.span.0, l.span.1),
                    _ => name.to_string(),
                };
                fixed = re.replace(&fixed, regex::NoExpand(&now)).into_owned();
            }
            if fixed == line {
                out.kept += 1;
            } else {
                out.rewritten += 1;
                lines[at] = Some(fixed);
            }
            continue;
        }
        let path = text(check, "path");
        let Some((start, end)) = block(&lines, &path) else { continue };
        let Some(at) = (start + 1..end).find(|&i| lines[i].as_deref() == Some(line.as_str())) else { continue };
        if !touched.contains(&path) {
            touched.push(path.clone());
        }
        match parsed(&path) {
            // not in this checkout: nothing in the block is about this copy
            None => {
                out.dropped += 1;
                lines[at] = None;
            }
            // mid-edit, or broken: nothing here can be checked right now
            Some(Err(())) => {
                out.dropped += 1;
                lines[at] = None;
            }
            Some(Ok(symbols)) if kind == "names" => {
                let names: Vec<String> = check
                    .get("names")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter(|n| symbols.contains_key(*n))
                    .map(str::to_string)
                    .collect();
                let listed = check.get("names").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
                if names.len() == listed {
                    out.kept += 1;
                } else if names.is_empty() {
                    out.dropped += 1;
                    lines[at] = None;
                } else {
                    out.rewritten += 1;
                    let head = line.split_once(": ").map(|(head, _)| head).unwrap_or("  constants");
                    let shown: Vec<&str> = names.iter().take(VALUE_NAMES).map(String::as_str).collect();
                    lines[at] = Some(format!("{head}: {}", shown.join(", ")));
                }
            }
            Some(Ok(symbols)) => {
                let name = text(check, "name");
                match symbols.get(&name) {
                    None => {
                        out.dropped += 1;
                        lines[at] = None;
                    }
                    Some(local) => {
                        let mine = symbol_line(&name, local);
                        let span = check.get("span").and_then(Value::as_array).map(|s| s.iter().filter_map(Value::as_u64).collect::<Vec<_>>());
                        let same_span = span.as_deref() == Some(&[local.span.0 as u64, local.span.1 as u64][..]);
                        let same_signature = text(check, "signature").split_whitespace().eq(local.signature.split_whitespace());
                        if same_span && same_signature {
                            out.kept += 1;
                        } else {
                            out.rewritten += 1;
                            lines[at] = Some(mine);
                        }
                    }
                }
            }
        }
    }
    // the blocks: a file not here goes whole, one that does not parse says
    // so, and a block that changed (or a file with symbols it did not list)
    // loses its "complete" mark
    for path in touched {
        let Some((start, end)) = block(&lines, &path) else { continue };
        let Some(header) = lines[start].clone() else { continue };
        match parsed(&path) {
            None => {
                for line in lines.iter_mut().take(end).skip(start) {
                    *line = None;
                }
            }
            Some(Err(())) => {
                lines[start] = Some(format!("{} · your copy does not parse right now: Read it", unmark(&header)));
            }
            Some(Ok(symbols)) => {
                let listed: Vec<&Value> = checks.iter().filter(|c| text(c, "path") == path).collect();
                let mut names: Vec<String> = listed.iter().map(|c| text(c, "name")).filter(|n| !n.is_empty()).collect();
                for c in &listed {
                    names.extend(c.get("names").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string));
                }
                let all_held = (start + 1..end).all(|i| lines[i].is_some())
                    && listed.iter().all(|c| lines.iter().flatten().any(|l| *l == text(c, "line")));
                let none_more = symbols.keys().all(|k| names.contains(k));
                if !(all_held && none_more) {
                    lines[start] = Some(unmark(&header));
                }
            }
        }
    }
    out.text = lines.into_iter().flatten().collect::<Vec<_>>().join("\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const API: &str = "//! The HTTP API.\n\nuse std::sync::Arc;\n\n/// Parses a date: YYYY-MM-DD.\npub fn parse_date(v: &str) -> Option<String> {\n    Some(v.to_string())\n}\n\n/// Creates an issue.\npub fn create_issue(me: Me, state: State, body: Bytes) -> Json {\n    todo!()\n}\n\npub const LIMIT: usize = 50;\n";

    fn root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cork-mapcheck-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/api.rs"), API).unwrap();
        dir
    }

    fn check(name: &str, line: &str, signature: &str, span: [u64; 2]) -> Value {
        json!({"kind": "symbol", "path": "src/api.rs", "name": name, "line": line, "signature": signature, "span": span})
    }

    #[test]
    fn a_map_entry_from_another_checkout_is_dropped_or_rewritten_and_a_current_one_kept() {
        let root = root("stale");
        let current = "  pub fn parse_date(v: &str) -> Option<String> [6-8] — Parses a date: YYYY-MM-DD.";
        // the map's version of the file had `date` (gone here) and an older create_issue
        let gone = "  fn date(v: &Value, field: &str) -> Result<String> [91-97]";
        let older = "  pub async fn create_issue(State(app): State<Arc<AppState>>, Json(body): Json<Map>) -> Json [194-238]";
        let text = format!("Cork briefing — scoped to your prompt: 1 of 1 module(s).\nsrc/api.rs — kevin, 2h ago · complete · unchanged\n  The HTTP API.\n{current}\n{gone}\n{older}\n  constants: LIMIT, MAX_ROWS\nSay so to the user.");
        let brief = json!({"text": text, "checks": [
            check("parse_date", current, "pub fn parse_date(v: &str) -> Option<String>", [6, 8]),
            check("date", gone, "fn date(v: &Value, field: &str) -> Result<String>", [91, 97]),
            check("create_issue", older, "pub async fn create_issue(State(app): State<Arc<AppState>>, Json(body): Json<Map>) -> Json", [194, 238]),
            {"kind": "names", "path": "src/api.rs", "names": ["LIMIT", "MAX_ROWS"], "line": "  constants: LIMIT, MAX_ROWS"},
        ]});
        let out = apply(&root, &brief);
        assert_eq!((out.kept, out.rewritten, out.dropped), (1, 2, 1), "{}", out.text);
        assert!(out.text.contains(current), "{}", out.text);
        assert!(!out.text.contains("fn date("), "a symbol this copy does not have is never printed: {}", out.text);
        assert!(out.text.contains("  pub fn create_issue(me: Me, state: State, body: Bytes) -> Json [11-13] — Creates an issue."), "{}", out.text);
        assert!(out.text.contains("  constants: LIMIT\n"), "{}", out.text);
        // what changed under it is no longer "complete · unchanged"
        assert!(out.text.contains("\nsrc/api.rs — kevin, 2h ago\n"), "{}", out.text);
        assert!(out.text.ends_with("Say so to the user."));
    }

    #[test]
    fn a_block_that_holds_keeps_its_mark_and_a_file_not_here_loses_its_block() {
        let root = root("held");
        let lines = [
            "  pub fn parse_date(v: &str) -> Option<String> [6-8] — Parses a date: YYYY-MM-DD.",
            "  pub fn create_issue(me: Me, state: State, body: Bytes) -> Json [11-13] — Creates an issue.",
        ];
        let text = format!("src/api.rs — kevin, 2h ago · complete · unchanged\n{}\n{}\n  constants: LIMIT\nsrc/gone.rs — ann, 1h ago\n  pub fn gone() [1-3]\nScope — every dependent is listed here.", lines[0], lines[1]);
        let brief = json!({"text": text, "checks": [
            check("parse_date", lines[0], "pub fn parse_date(v: &str) -> Option<String>", [6, 8]),
            check("create_issue", lines[1], "pub fn create_issue(me: Me, state: State, body: Bytes) -> Json", [11, 13]),
            {"kind": "names", "path": "src/api.rs", "names": ["LIMIT"], "line": "  constants: LIMIT"},
            {"kind": "symbol", "path": "src/gone.rs", "name": "gone", "line": "  pub fn gone() [1-3]", "signature": "pub fn gone()", "span": [1, 3]},
        ]});
        let out = apply(&root, &brief);
        assert!(out.text.starts_with("src/api.rs — kevin, 2h ago · complete · unchanged\n"), "{}", out.text);
        assert!(!out.text.contains("src/gone.rs"), "{}", out.text);
        assert_eq!(out.text.lines().last(), Some("Scope — every dependent is listed here."));
        // and a copy mid-edit (it does not parse) is not vouched for
        std::fs::write(root.join("src/api.rs"), "pub fn parse_date(v: &str) -> Option<String> {\n").unwrap();
        let broken = apply(&root, &brief);
        assert!(broken.text.contains("src/api.rs — kevin, 2h ago · your copy does not parse right now: Read it"), "{}", broken.text);
        assert!(!broken.text.contains("create_issue"), "{}", broken.text);
    }

    #[test]
    fn ranges_in_an_earlier_agents_work_are_held_to_this_copy() {
        let root = root("spans");
        let line = "  and changed src/api.rs (create_issue [194-238], date [91-97]).";
        let brief = json!({"text": format!("An agent did similar work here (3h ago): it read src/db.rs\n{line}"), "checks": [
            {"kind": "spans", "line": line, "refs": [["src/api.rs", "create_issue"], ["src/api.rs", "date"]]},
        ]});
        let out = apply(&root, &brief);
        assert!(out.text.ends_with("  and changed src/api.rs (create_issue [11-13], date)."), "{}", out.text);
    }

    #[test]
    fn a_grep_answer_is_this_copys_symbol_or_none() {
        let root = root("fact");
        let fact = local_fact(&root, "src/api.rs", "create_issue").unwrap();
        assert_eq!((fact.signature.as_str(), fact.span), ("pub fn create_issue(me: Me, state: State, body: Bytes) -> Json", (11, 13)));
        assert_eq!(local_fact(&root, "src/api.rs", "date"), None);
        assert_eq!(local_fact(&root, "src/missing.rs", "x"), None);
    }
}
