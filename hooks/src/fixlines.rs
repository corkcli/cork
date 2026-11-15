//! A block that carries its fix: the lines of THIS write that a teammate's
//! rename, removal or new signature makes wrong, and what each becomes.
//!
//! A block that only says "alice renamed `amount_due`" sends the agent off to
//! re-read the file and find its own uses of the old name: a read, a search,
//! another try. The hook already holds the write (the tool's `new_string` or
//! `content`, or the file as the write will leave it), so it can name them:
//!
//! ```text
//! In this write (total.py):
//!   line 12: `amount_due(cart)` → `checkout_total(cart)`
//!   line 30: `amount_due(cart)` passes 1 arg; it now takes 2 (amount, region)
//! ```
//!
//! Computed here, on this machine, from the facts the gate returns (the
//! symbol, its new name or signature): no file text goes anywhere. Capped at
//! MAX_LINES; a line the write itself adds or rewrites comes first.

use std::collections::BTreeSet;

use regex::Regex;
use serde_json::Value;

pub const MAX_LINES: usize = 5;
const MAX_SNIPPET: usize = 60;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// A teammate's change, as the fix needs it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Fix {
    pub symbol: String,
    /// The new name, for a rename.
    pub new_name: String,
    /// The new signature, for a re-signature (and with a rename, when known).
    pub signature: String,
    pub removed: bool,
}

impl Fix {
    /// From a gate event (`{symbol, kind, new_name, signature_after, detail}`),
    /// as the server's /gate returns them.
    pub fn from_event(event: &Value) -> Option<Fix> {
        let detail = event.get("detail").cloned().unwrap_or(Value::Null);
        let pick = |own: &str, key: &str| {
            let given = text(event, own);
            if given.is_empty() { text(&detail, key) } else { given }
        };
        let said = detail.as_str().and_then(|d| d.strip_prefix("signature changed to: ")).unwrap_or("").to_string();
        let signature = Some(pick("signature_after", "signature")).filter(|s| !s.is_empty()).unwrap_or(said);
        let fix = Fix {
            symbol: text(event, "symbol"),
            new_name: pick("new_name", "new_name"),
            signature,
            removed: text(event, "kind") == "removed",
        };
        let kind = text(event, "kind");
        let known = matches!(kind.as_str(), "renamed" | "removed" | "signature" | "modified");
        (known && !fix.symbol.is_empty() && (fix.removed || !fix.new_name.is_empty() || !fix.signature.is_empty())).then_some(fix)
    }

    /// From the deps change the protocol's ops make (deps::summarize).
    pub fn from_change(symbol: &str, change: &crate::deps::Change) -> Option<Fix> {
        let fix = Fix {
            symbol: symbol.to_string(),
            new_name: change.new_name.clone(),
            signature: change.new_signature.clone(),
            removed: change.deleted,
        };
        (!fix.symbol.is_empty() && (fix.removed || !fix.new_name.is_empty() || !fix.signature.is_empty())).then_some(fix)
    }
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= MAX_SNIPPET {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(MAX_SNIPPET - 1).collect::<String>())
    }
}

/// The parameter names of a signature, without `self`: `(amount, region)`.
pub fn param_names(signature: &str, name: &str) -> Vec<String> {
    let bare = cork_core::local::bare_name(name);
    let from = signature.find(bare).map(|i| i + bare.len()).unwrap_or(0);
    let Some(open) = signature[from..].find('(').map(|i| from + i) else { return Vec::new() };
    let Some(params) = crate::deps::split_top(&signature[open + 1..]) else { return Vec::new() };
    params
        .iter()
        .filter_map(|p| {
            let p = p.trim().trim_start_matches(['*', '&', '.']).trim_start_matches("mut ").trim();
            let head = p.split([':', '=']).next().unwrap_or("").trim().trim_end_matches('?');
            (!head.is_empty() && !matches!(head, "self" | "cls" | "this" | "/" | "")).then(|| head.to_string())
        })
        .collect()
}

fn takes(signature: &str, name: &str) -> Option<(crate::deps::Arity, String)> {
    let arity = crate::deps::arity(signature, name)?;
    let n = match arity.max {
        Some(max) if max == arity.min => format!("{max}"),
        Some(max) => format!("{}-{max}", arity.min),
        None => format!("at least {}", arity.min),
    };
    let names = param_names(signature, name);
    let named = if names.is_empty() { String::new() } else { format!(" ({})", names.join(", ")) };
    Some((arity, format!("it now takes {n}{named}")))
}

/// `name(args)` as it is written on this line, when it closes there.
fn call_text(line: &str, at: usize, name_len: usize) -> String {
    let rest = &line[at + name_len..];
    let open = rest.len() - rest.trim_start().len();
    let mut depth = 0i32;
    for (i, c) in rest.char_indices().skip(open) {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    let whole = &line[at..at + name_len + i + 1];
                    return if whole.chars().count() <= MAX_SNIPPET { whole.to_string() } else { format!("{}(…)", &line[at..at + name_len]) };
                }
            }
            _ => {}
        }
    }
    format!("{}(…)", &line[at..at + name_len])
}

/// The lines of `after` (the file as this write leaves it) that `fix` makes
/// wrong, each as what to change; the write's own lines (`new_lines`) first.
pub fn lines_for(after: &str, new_lines: &BTreeSet<usize>, fix: &Fix) -> Vec<String> {
    let bare = cork_core::local::bare_name(&fix.symbol).to_string();
    if bare.is_empty() {
        return Vec::new();
    }
    let Ok(word) = Regex::new(&format!(r"(?:^|[^\w$]){}\b", regex::escape(&bare))) else { return Vec::new() };
    let arity = if fix.removed || !fix.new_name.is_empty() { None } else { takes(&fix.signature, &bare) };
    if !fix.removed && fix.new_name.is_empty() && arity.is_none() {
        return Vec::new();
    }
    let all: Vec<&str> = after.split('\n').collect();
    let mut mine: Vec<(usize, String)> = Vec::new();
    let mut rest: Vec<(usize, String)> = Vec::new();
    for (index, line) in all.iter().enumerate() {
        let Some(found) = word.find(line) else { continue };
        let at = found.end() - bare.len();
        let number = index + 1;
        let lead = line[..at].trim_end();
        let last_word = lead.rsplit(|c: char| c.is_whitespace()).next().unwrap_or("");
        let declares = matches!(last_word, "def" | "fn" | "function" | "func" | "class" | "struct" | "enum" | "trait" | "type" | "const" | "let");
        let after_name = &line[at + bare.len()..];
        let calls = after_name.trim_start().starts_with('(');
        let said = if !fix.new_name.is_empty() {
            let new = &fix.new_name;
            if declares || !calls {
                let shown = clip(line);
                let renamed = word.replace(&shown, |c: &regex::Captures| c[0].replacen(bare.as_str(), new, 1)).to_string();
                format!("line {number}: `{shown}` → `{renamed}`")
            } else {
                let old = call_text(line, at, bare.len());
                format!("line {number}: `{old}` → `{}`", old.replacen(bare.as_str(), new, 1))
            }
        } else if fix.removed {
            if declares {
                format!("line {number} still declares `{bare}`, which they removed")
            } else if calls {
                format!("line {number}: `{}` calls `{bare}`, which they removed", call_text(line, at, bare.len()))
            } else {
                format!("line {number}: `{}` names `{bare}`, which they removed", clip(line))
            }
        } else {
            let Some((arity, now_takes)) = &arity else { continue };
            if declares {
                let compact = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
                if compact(line).starts_with(&compact(&fix.signature)) {
                    continue;
                }
                format!("line {number}: `{}` → `{}`", clip(line), clip(&fix.signature))
            } else if calls {
                // the arguments, counted from the whole text: a call may span lines
                let offset: usize = all[..index].iter().map(|l| l.len() + 1).sum::<usize>() + at + bare.len();
                let paren = offset + after[offset..].find('(').unwrap_or(0);
                match crate::deps::count_args(&after[paren + 1..]) {
                    Some(n) if !arity.fits(n) => format!(
                        "line {number}: `{}` passes {n} arg{}; {now_takes}",
                        call_text(line, at, bare.len()),
                        if n == 1 { "" } else { "s" }
                    ),
                    _ => continue,
                }
            } else {
                continue;
            }
        };
        if new_lines.contains(&number) { mine.push((number, said)) } else { rest.push((number, said)) }
    }
    let total = mine.len() + rest.len();
    let mut out: Vec<String> = mine.into_iter().chain(rest).take(MAX_LINES).map(|(_, s)| s).collect();
    if total > MAX_LINES {
        out.push(format!("(+{} more)", total - MAX_LINES));
    }
    out
}

/// The fix lines for several changes to one file, said under one heading,
/// at most MAX_LINES in all; "" when there is nothing to change.
pub fn render(rel: &str, after: &str, new_lines: &BTreeSet<usize>, fixes: &[Fix]) -> String {
    render_under(&format!("In this write ({rel}):"), after, new_lines, fixes)
}

/// A semantic lint finding (`stale_name`, `removed_symbol_use`,
/// `signature_drift`) as the fix it asks for.
pub fn from_finding(finding: &Value) -> Option<Fix> {
    let fix = Fix {
        symbol: text(finding, "symbol"),
        new_name: if text(finding, "rule") == "stale_name" { text(finding, "renamed_to") } else { String::new() },
        signature: if text(finding, "rule") == "signature_drift" { text(finding, "signature") } else { String::new() },
        removed: text(finding, "rule") == "removed_symbol_use",
    };
    (!fix.symbol.is_empty() && (fix.removed || !fix.new_name.is_empty() || !fix.signature.is_empty())).then_some(fix)
}

/// The fix lines a change notice or a semantic alert carries for the
/// caller's own file, as it is on disk now (hooks v78: renames reached
/// callers as notices and alerts, never as gate blocks, so the lines never
/// appeared). Read here, on this machine; no file text is sent anywhere.
pub fn for_file(root: &std::path::Path, rel: &str, fixes: &[Fix]) -> String {
    if fixes.is_empty() || rel.is_empty() {
        return String::new();
    }
    let path = root.join(rel);
    if std::fs::metadata(&path).map(|m| m.len() > 512 * 1024).unwrap_or(true) {
        return String::new();
    }
    let Ok(content) = std::fs::read_to_string(&path) else { return String::new() };
    render_under(&format!("In {rel}:"), &content, &BTreeSet::new(), fixes)
}

/// The deltas' `fixes` (`{symbol, kind, new_name, signature_after, in}`),
/// each caller file's lines under its own heading, MAX_LINES in all.
pub fn for_deltas(root: &std::path::Path, fixes: &[Value]) -> String {
    let mut by_file: Vec<(String, Vec<Fix>)> = Vec::new();
    for raw in fixes {
        let Some(fix) = Fix::from_event(raw) else { continue };
        let file = text(raw, "in");
        match by_file.iter_mut().find(|(f, _)| *f == file) {
            Some((_, list)) => list.push(fix),
            None => by_file.push((file, vec![fix])),
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut left = MAX_LINES;
    for (file, list) in by_file {
        if left == 0 {
            break;
        }
        let said = for_file(root, &file, &list);
        let mut lines = said.lines();
        let Some(head) = lines.next() else { continue };
        let body: Vec<&str> = lines.filter(|l| !l.trim_start().starts_with("(+")).take(left).collect();
        if body.is_empty() {
            continue;
        }
        left -= body.len();
        out.push(head.to_string());
        out.extend(body.into_iter().map(str::to_string));
    }
    out.join("\n")
}

/// `for_file` over content the caller already holds (the file just reported).
pub fn render_lines(rel: &str, content: &str, fixes: &[Fix]) -> String {
    if fixes.is_empty() {
        return String::new();
    }
    render_under(&format!("In {rel}:"), content, &BTreeSet::new(), fixes)
}

fn render_under(heading: &str, after: &str, new_lines: &BTreeSet<usize>, fixes: &[Fix]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for fix in fixes {
        for line in lines_for(after, new_lines, fix) {
            if lines.len() >= MAX_LINES && !line.starts_with("(+") {
                break;
            }
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = vec![heading.to_string()];
    out.extend(lines.into_iter().take(MAX_LINES + 1).map(|l| format!("  {l}")));
    out.join("\n")
}

/// The file as this write leaves it, and the lines it adds or rewrites;
/// `None` when it cannot be told (a shell command, an edit that no longer
/// applies, a deletion).
pub fn pending(tool: &str, tool_input: &Value, target: &std::path::Path, cwd: &std::path::Path) -> Option<(String, BTreeSet<usize>)> {
    if std::fs::metadata(target).is_ok_and(|m| m.len() > 512 * 1024) {
        return None;
    }
    let current = std::fs::read(target).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
    let after = crate::proto::predict_after(current.as_deref(), tool, tool_input, target, cwd)??;
    let new_lines = crate::deps::new_lines(current.as_deref(), &after);
    Some((after, new_lines))
}

/// A block reason with the fix for `events` (gate events) appended, when the
/// write can be read; the reason as it was otherwise.
pub fn with_fix(reason: String, rel: &str, events: &[Value], write: Option<&(String, BTreeSet<usize>)>) -> String {
    let Some((after, new_lines)) = write else { return reason };
    let fixes: Vec<Fix> = events.iter().filter_map(Fix::from_event).collect();
    let fix = render(rel, after, new_lines, &fixes);
    if fix.is_empty() { reason } else { format!("{reason}\n{fix}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CART: &str = "from total import amount_due\n\n\ndef pay(cart):\n    due = amount_due(cart)\n    return due\n\n\ndef refund(cart, region):\n    return -amount_due(cart, region)\n";

    #[test]
    fn a_rename_names_each_line_and_what_it_becomes() {
        let fix = Fix::from_event(&json!({"symbol": "amount_due", "kind": "renamed", "new_name": "checkout_total"})).unwrap();
        let new_lines: BTreeSet<usize> = [10].into_iter().collect();
        let lines = lines_for(CART, &new_lines, &fix);
        assert_eq!(lines, vec![
            "line 10: `amount_due(cart, region)` → `checkout_total(cart, region)`".to_string(),
            "line 1: `from total import amount_due` → `from total import checkout_total`".to_string(),
            "line 5: `amount_due(cart)` → `checkout_total(cart)`".to_string(),
        ], "the write's own line first");
    }

    #[test]
    fn a_new_signature_names_the_calls_that_no_longer_fit() {
        let fix = Fix::from_event(&json!({"symbol": "amount_due", "kind": "signature",
            "signature_after": "def amount_due(amount, region)"})).unwrap();
        let lines = lines_for(CART, &BTreeSet::new(), &fix);
        assert_eq!(lines, vec!["line 5: `amount_due(cart)` passes 1 arg; it now takes 2 (amount, region)".to_string()],
            "line 10 already passes 2");
        // the declaration itself, in the file that declares it
        let decl = "def amount_due(lines):\n    return sum(lines)\n";
        let lines = lines_for(decl, &BTreeSet::new(), &fix);
        assert_eq!(lines, vec!["line 1: `def amount_due(lines):` → `def amount_due(amount, region)`".to_string()]);
        // older markers carry it in `detail`
        let old = Fix::from_event(&json!({"symbol": "amount_due", "kind": "signature", "detail": "signature changed to: def amount_due(a, b)"})).unwrap();
        assert_eq!(old.signature, "def amount_due(a, b)");
    }

    #[test]
    fn a_removal_and_the_cap() {
        let fix = Fix::from_event(&json!({"symbol": "amount_due", "kind": "removed"})).unwrap();
        let lines = lines_for(CART, &BTreeSet::new(), &fix);
        assert_eq!(lines[1], "line 5: `amount_due(cart)` calls `amount_due`, which they removed");
        let many: String = (0..9).map(|i| format!("x{i} = amount_due({i})\n")).collect();
        let lines = lines_for(&many, &BTreeSet::new(), &fix);
        assert_eq!(lines.len(), MAX_LINES + 1);
        assert_eq!(lines.last().unwrap(), "(+4 more)");
        let said = render("cart.py", &many, &BTreeSet::new(), &[fix]);
        assert!(said.starts_with("In this write (cart.py):\n  line 1:") && said.lines().count() == MAX_LINES + 2, "{said}");
        // a body edit, or nothing to name, says nothing
        assert!(Fix::from_event(&json!({"symbol": "f", "kind": "modified"})).is_none());
        assert_eq!(render("cart.py", "x = 1\n", &BTreeSet::new(), &[Fix { symbol: "f".into(), removed: true, ..Fix::default() }]), "");
    }

    /// Scaling study 2: renames reached callers as change notices and lint
    /// alerts, never as gate blocks, so no fix line appeared. Both now carry
    /// the caller's own lines, read from its file on this machine.
    #[test]
    fn a_change_notice_and_a_lint_alert_name_the_callers_lines() {
        let root = std::env::temp_dir().join(format!("collide-fixlines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("shop")).unwrap();
        std::fs::write(root.join("shop/cart.py"), CART).unwrap();
        // the deltas' fixes: a rename and a new signature, in the caller's file
        let said = for_deltas(&root, &[
            json!({"symbol": "amount_due", "kind": "renamed", "new_name": "checkout_total", "in": "shop/cart.py"}),
            json!({"symbol": "ghost", "kind": "removed", "in": "shop/missing.py"}),
        ]);
        assert!(said.starts_with("In shop/cart.py:\n  line 1: `from total import amount_due` → `from total import checkout_total`"), "{said}");
        assert!(said.contains("  line 5: `amount_due(cart)` → `checkout_total(cart)`"), "{said}");
        assert!(said.lines().count() <= MAX_LINES + 1, "{said}");
        // a lint finding
        let fix = from_finding(&json!({"rule": "signature_drift", "symbol": "amount_due", "signature": "def amount_due(amount, region)", "lines": [5]})).unwrap();
        let said = for_file(&root, "shop/cart.py", &[fix]);
        assert_eq!(said, "In shop/cart.py:\n  line 5: `amount_due(cart)` passes 1 arg; it now takes 2 (amount, region)");
        // the cap holds across files
        let many: String = (0..9).map(|i| format!("x{i} = amount_due({i})\n")).collect();
        std::fs::write(root.join("shop/many.py"), many).unwrap();
        let said = for_deltas(&root, &[
            json!({"symbol": "amount_due", "kind": "removed", "in": "shop/many.py"}),
            json!({"symbol": "amount_due", "kind": "removed", "in": "shop/cart.py"}),
        ]);
        assert_eq!(said.lines().filter(|l| l.starts_with("  line")).count(), MAX_LINES, "{said}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
