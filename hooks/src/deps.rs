//! Pre-write semantic break alerts: what a write CALLS, against what
//! teammates did to it.
//!
//! The gate already stops a write onto a symbol someone else changed. This
//! is the other half: a write to `app.py` that calls `compute_tax`, which a
//! teammate just re-signatured in `tax.py`. Merge-time tools find that after
//! both sides landed; here the caller hears it before its write goes in.
//!
//! All of it runs on this machine, from the file as the write will leave it:
//!
//! - `deps_of` parses the file (cork-core, the server's own parser), takes
//!   its imports, finds each imported module on disk and the declaration the
//!   file calls in it, and the call sites with how many arguments each
//!   passes. The key is the callee's protocol key; the base is this copy's
//!   content hash of it.
//! - The protocol answers a kind-less `check` on those keys with the ops this
//!   copy missed (SPEC.md §9): a signature change, a rename, a delete.
//! - `judge` says what that means for THIS file: the calls that no longer fit
//!   the new signature (a call that already fits says nothing), or the calls
//!   to a name that is gone. It blocks only when the write itself adds or
//!   rewrites a call to a removed or renamed name; everything else is a
//!   line in the agent's context.
//! - `Told` keeps each fact to once per session, across this gate and the
//!   pushed deltas (protod.rs, check.rs): the same change in another file is
//!   one short line naming that file's calls, never the story again.
//!
//! Limits, said plainly: names resolve through imports only (Python,
//! JavaScript/TypeScript, Rust `use`), never through dynamic dispatch, a
//! method on an object, `getattr`, or a re-export chain; arity is counted
//! from the text, and a splat (`*args`, `...rest`) is never judged.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::config::{self, Env};

const MAX_MODULES: usize = 12;
const MAX_DEPS: usize = 40;
const MAX_CALLS: usize = 20;
const MAX_FILE_BYTES: u64 = 512 * 1024;
/// A fact is said once per session; after a day it may be said again.
const TOLD_FOR_S: f64 = 86_400.0;
const MAX_TOLD: usize = 300;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// One call to a dependency: its 1-based line and how many arguments it
/// passes (`None`: a splat, or a call whose parentheses never close).
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub line: usize,
    pub args: Option<usize>,
}

/// A declaration in another file that this file calls.
#[derive(Debug, Clone, PartialEq)]
pub struct Dep {
    /// `code:tax.py::compute_tax`
    pub key: String,
    /// `tax.py`
    pub path: String,
    /// The declaration's name there: `compute_tax`.
    pub name: String,
    /// How this file calls it: `compute_tax`, an alias, or `tax.compute_tax`.
    pub called: String,
    /// This copy's content hash of the declaration (the check's base).
    pub base_hash: String,
    /// This copy's signature: what this file's calls were written against.
    pub signature: String,
    pub calls: Vec<Call>,
    /// Lines that import it by name.
    pub imports: Vec<usize>,
}

// ------------------------------------------------------------ finding what a file calls

fn parent(rel: &str) -> &str {
    rel.rfind('/').map(|i| &rel[..i]).unwrap_or("")
}

fn join(base: &str, rest: &str) -> String {
    if base.is_empty() {
        rest.to_string()
    } else if rest.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rest}")
    }
}

/// `a/./b/../c` → `a/c`; `None` when it climbs out of the repo.
fn normpath(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn probe(root: &Path, stem: &str, exts: &[&str]) -> Option<String> {
    let stem = normpath(stem)?;
    exts.iter().map(|e| format!("{stem}{e}")).find(|c| !c.is_empty() && root.join(c).is_file())
}

/// The repo file a module string names, from the file `from_rel`, by
/// looking on disk; `None` for a package outside the repo or a language
/// this does not resolve.
pub fn resolve_module(root: &Path, from_rel: &str, language: &str, module: &str) -> Option<String> {
    let module = module.trim();
    if module.is_empty() {
        return None;
    }
    let here = parent(from_rel);
    match language {
        "python" => {
            const PY: [&str; 2] = [".py", "/__init__.py"];
            let dots = module.len() - module.trim_start_matches('.').len();
            let rest = module[dots..].replace('.', "/");
            if dots > 0 {
                let mut base = here.to_string();
                for _ in 1..dots {
                    base = parent(&base).to_string();
                }
                return probe(root, &join(&base, &rest), &PY);
            }
            let mut bases: Vec<String> = vec![String::new(), "src".into()];
            let mut dir = here.to_string();
            while !dir.is_empty() {
                bases.push(dir.clone());
                dir = parent(&dir).to_string();
            }
            bases.iter().find_map(|b| probe(root, &join(b, &rest), &PY))
        }
        "typescript" | "tsx" | "javascript" => {
            const JS: [&str; 10] = ["", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", "/index.ts", "/index.tsx", "/index.js"];
            let stem = if module.starts_with('.') {
                join(here, module)
            } else if let Some(rest) = module.strip_prefix("@/").or_else(|| module.strip_prefix("~/")) {
                join("src", rest)
            } else {
                return None; // a package
            };
            let stem = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"].iter().find_map(|e| stem.strip_suffix(e)).map(str::to_string).unwrap_or(stem);
            probe(root, &stem, &JS)
        }
        "rust" => {
            let mut parts: Vec<&str> = module.split("::").filter(|p| !p.is_empty()).collect();
            let base = match parts.first().copied() {
                Some("crate") => {
                    parts.remove(0);
                    // the crate this file belongs to: the nearest `src`
                    let mut dir = here.to_string();
                    while !dir.is_empty() && !dir.ends_with("src") {
                        dir = parent(&dir).to_string();
                    }
                    if dir.is_empty() { "src".to_string() } else { dir }
                }
                Some("super") => {
                    let mut base = parent(here).to_string();
                    parts.remove(0);
                    while parts.first() == Some(&"super") {
                        base = parent(&base).to_string();
                        parts.remove(0);
                    }
                    base
                }
                Some("self") => {
                    parts.remove(0);
                    here.to_string()
                }
                _ => return None,
            };
            // `use crate::tax::compute_tax`: the module is `tax`, or the path names it whole
            (0..=parts.len().min(1)).find_map(|drop| {
                let take = parts.len().checked_sub(drop)?;
                if take == 0 {
                    return None;
                }
                probe(root, &join(&base, &parts[..take].join("/")), &[".rs", "/mod.rs"])
            })
        }
        _ => None,
    }
}

/// Where `called(` appears in `content`, with the arguments each passes.
/// A declaration of the same name (`def compute_tax(`) is not a call.
pub fn calls_of(content: &str, called: &str) -> Vec<Call> {
    let Ok(pattern) = Regex::new(&format!(r"(?m)(?:^|[^\w.$])({})[ \t]*\(", regex::escape(called))) else { return Vec::new() };
    let mut out = Vec::new();
    for caps in pattern.captures_iter(content) {
        let (Some(whole), Some(name)) = (caps.get(0), caps.get(1)) else { continue };
        let paren = whole.end() - 1;
        let line_start = content[..name.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let lead = content[line_start..name.start()].trim();
        let last_word = lead.rsplit(char::is_whitespace).next().unwrap_or("");
        if matches!(last_word, "def" | "function" | "fn" | "func" | "class") {
            continue;
        }
        let line = content[..name.start()].matches('\n').count() + 1;
        out.push(Call { line, args: count_args(&content[paren + 1..]) });
        if out.len() >= MAX_CALLS {
            break;
        }
    }
    out
}

/// The top-level, comma-separated pieces of an argument or parameter list
/// that starts just after its `(`; `None` when it never closes.
pub(crate) fn split_top(rest: &str) -> Option<Vec<String>> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut pieces: Vec<String> = vec![String::new()];
    for (i, c) in rest.chars().enumerate() {
        if i > 4000 {
            return None;
        }
        if let Some(q) = quote {
            pieces.last_mut()?.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' | '`' => {
                quote = Some(c);
                pieces.last_mut()?.push(c);
            }
            '(' | '[' | '{' => {
                depth += 1;
                pieces.last_mut()?.push(c);
            }
            ')' | ']' | '}' if depth > 0 => {
                depth -= 1;
                pieces.last_mut()?.push(c);
            }
            ')' => {
                return Some(pieces.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect());
            }
            ',' if depth == 0 => pieces.push(String::new()),
            _ => pieces.last_mut()?.push(c),
        }
    }
    None
}

/// How many arguments a call passes; `None` for a splat it cannot count.
pub fn count_args(rest: &str) -> Option<usize> {
    let args = split_top(rest)?;
    if args.iter().any(|a| a.starts_with('*') || a.starts_with("...")) {
        return None;
    }
    Some(args.len())
}

/// What a signature accepts: at least `min` arguments, at most `max`
/// (`None`: any number). `None` when the text has no parameter list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Arity {
    pub min: usize,
    pub max: Option<usize>,
}

impl Arity {
    pub fn fits(&self, n: usize) -> bool {
        n >= self.min && self.max.is_none_or(|m| n <= m)
    }
}

pub fn arity(signature: &str, name: &str) -> Option<Arity> {
    let bare = cork_core::local::bare_name(name);
    let from = signature.find(bare).map(|i| i + bare.len()).unwrap_or(0);
    let open = from + signature[from..].find('(')?;
    let params = split_top(&signature[open + 1..])?;
    let (mut min, mut total, mut variadic) = (0usize, 0usize, false);
    for (i, p) in params.iter().enumerate() {
        let p = p.trim();
        let head = p.split([':', '=']).next().unwrap_or("").trim();
        if i == 0 && matches!(head, "self" | "cls" | "&self" | "&mut self" | "mut self" | "this") {
            continue;
        }
        if matches!(p, "*" | "/") {
            continue;
        }
        if p.starts_with('*') || p.starts_with("...") {
            variadic = true;
            continue;
        }
        total += 1;
        if !(p.contains('=') || head.ends_with('?')) {
            min += 1;
        }
    }
    Some(Arity { min, max: (!variadic).then_some(total) })
}

fn import_lines(content: &str, name: &str) -> Vec<usize> {
    let Ok(word) = Regex::new(&format!(r"\b{}\b", regex::escape(name))) else { return Vec::new() };
    content
        .lines()
        .enumerate()
        .filter(|(_, l)| (l.contains("import") || l.trim_start().starts_with("use ")) && word.is_match(l))
        .map(|(i, _)| i + 1)
        .take(5)
        .collect()
}

/// `tax.<name>(` for a module imported as `tax`.
fn qualified_names(content: &str, alias: &str) -> BTreeSet<String> {
    let Ok(pattern) = Regex::new(&format!(r"(?m)(?:^|[^\w.$]){}\.([A-Za-z_]\w*)[ \t]*\(", regex::escape(alias))) else { return BTreeSet::new() };
    pattern.captures_iter(content).filter_map(|c| c.get(1).map(|m| m.as_str().to_string())).collect()
}

/// The declarations in other files this file calls, as it will be after
/// the write: resolved through its imports, on this copy.
pub fn deps_of(root: &Path, rel: &str, content: &str) -> Vec<Dep> {
    let Some(parsed) = cork_core::engine::parse(rel, content) else { return Vec::new() };
    let own: BTreeSet<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
    let mut modules: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut files: BTreeMap<String, Vec<cork_core::local::LocalSymbol>> = BTreeMap::new();
    let mut out: Vec<Dep> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for import in &parsed.imports {
        if !modules.contains_key(&import.module) && modules.len() >= MAX_MODULES {
            continue;
        }
        let path = modules
            .entry(import.module.clone())
            .or_insert_with(|| resolve_module(root, rel, parsed.language, &import.module).filter(|p| p != rel))
            .clone();
        let Some(path) = path else { continue };
        if !files.contains_key(&path) {
            let full = root.join(&path);
            let symbols = std::fs::metadata(&full)
                .ok()
                .filter(|m| m.len() <= MAX_FILE_BYTES)
                .and_then(|_| std::fs::read_to_string(&full).ok())
                .and_then(|c| cork_core::local::symbols(&path, &c))
                .filter(|p| p.clean)
                .map(|p| p.symbols)
                .unwrap_or_default();
            files.insert(path.clone(), symbols);
        }
        let symbols = &files[&path];
        for (local, original) in &import.names {
            if local == "*" || local.is_empty() {
                continue;
            }
            // a module: `tax.compute_tax(...)`; a name: `compute_tax(...)`
            let wanted: Vec<(String, String)> = if original == "*" {
                qualified_names(content, local).into_iter().map(|n| (n.clone(), format!("{local}.{n}"))).collect()
            } else if own.contains(local.as_str()) {
                Vec::new() // this file declares the name itself
            } else {
                vec![(original.clone(), local.clone())]
            };
            for (name, called) in wanted {
                let Some(sym) = symbols.iter().find(|s| s.name == name) else { continue };
                let key = crate::proto::sym_key(&path, &sym.name);
                if !seen.insert(format!("{key}|{called}")) {
                    continue;
                }
                let calls = calls_of(content, &called);
                let imports = if original == "*" { Vec::new() } else { import_lines(content, original) };
                if calls.is_empty() && imports.is_empty() {
                    continue;
                }
                out.push(Dep {
                    key,
                    path: path.clone(),
                    name: sym.name.clone(),
                    called,
                    base_hash: sym.hash.clone(),
                    signature: sym.signature.clone(),
                    calls,
                    imports,
                });
                if out.len() >= MAX_DEPS {
                    return out;
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------ what changed, and what it breaks here

/// A teammate's change to a dependency, from the ops this copy missed.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// `alice@x.com (claude)`
    pub who: String,
    pub intent: String,
    /// The signature the change started from, when the author sent it.
    pub old_signature: String,
    pub new_signature: String,
    pub new_name: String,
    pub new_path: String,
    pub deleted: bool,
    /// Pushed by its author (a promoted op): on origin, where a pull gets
    /// it. A draft is only in their checkout: said as what to write against,
    /// and never a block, since no pull or re-read could clear it.
    pub pushed: bool,
}

impl Change {
    fn kind(&self) -> &'static str {
        if self.deleted {
            "del"
        } else if !self.new_name.is_empty() || !self.new_path.is_empty() {
            "ren"
        } else {
            "sig"
        }
    }
}

fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The fact's id, the same from every channel: `tax.py::compute_tax|sig|<new>`.
pub fn fact_id(key: &str, kind: &str, new: &str) -> String {
    let symbol = key.strip_prefix("code:").unwrap_or(key);
    format!("{symbol}|{kind}|{}", compact(new))
}

impl Change {
    pub fn fact(&self, key: &str) -> String {
        let new = match self.kind() {
            "ren" => if self.new_name.is_empty() { self.new_path.as_str() } else { self.new_name.as_str() },
            "sig" => self.new_signature.as_str(),
            _ => "",
        };
        fact_id(key, self.kind(), new)
    }
}

/// The breaking part of `theirs` (ops as the server sends them), in order:
/// the last author's name, where the subject went, its newest signature.
/// `None` when nothing in it breaks a caller.
pub fn summarize(theirs: &[Value], who_of: impl Fn(&Value) -> String) -> Option<Change> {
    let mut change = Change {
        who: String::new(),
        intent: String::new(),
        old_signature: String::new(),
        new_signature: String::new(),
        new_name: String::new(),
        new_path: String::new(),
        deleted: false,
        pushed: false,
    };
    let mut breaking = false;
    for op in theirs {
        let public = op.get("public").cloned().unwrap_or(Value::Null);
        match text(op, "kind").as_str() {
            "signature_change" => change.new_signature = text(&public, "new_signature"),
            "rename" => change.new_name = text(&public, "new_name"),
            "move" => change.new_path = text(&public, "new_path"),
            "delete" => change.deleted = true,
            "insert" => change.deleted = false,
            _ => continue,
        }
        if !breaking && change.old_signature.is_empty() {
            change.old_signature = text(&public, "old_signature");
        }
        breaking = true;
        change.who = who_of(op.get("author").unwrap_or(&Value::Null));
        change.pushed |= op.get("layer").and_then(Value::as_str) == Some("promoted");
        let said = text(&public, "intent");
        let intent = if said.is_empty() { text(op, "intent_id") } else { said };
        if !intent.is_empty() {
            change.intent = intent;
        }
    }
    // a signature change that only re-wrote what the copy already has is no change
    breaking.then_some(change)
}

/// What a change means for one file: the lines to name, whether the write
/// must be stopped, and the words.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub fact: String,
    pub file: String,
    pub lines: Vec<usize>,
    pub block: bool,
    /// The whole fact: who, what, why, and this file's lines.
    pub full: String,
    /// The fact already told: this file's lines only.
    pub short: String,
}

fn lines_text(lines: &[usize]) -> String {
    let shown: Vec<String> = lines.iter().take(8).map(|l| l.to_string()).collect();
    let more = if lines.len() > 8 { format!(" (+{} more)", lines.len() - 8) } else { String::new() };
    format!("{} {}{more}", if lines.len() == 1 { "line" } else { "lines" }, shown.join(", "))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `dep` (this copy, `file` as the write leaves it) against `change`.
/// `new_lines` are the lines of the file the write adds or rewrites; a call
/// to a removed or renamed name on one of them is the only thing that
/// blocks. A call that already fits the new signature is not named, and a
/// file with none left says nothing at all.
pub fn judge(dep: &Dep, change: &Change, file: &str, new_lines: &BTreeSet<usize>) -> Option<Finding> {
    let old_sig = if change.old_signature.is_empty() { dep.signature.as_str() } else { change.old_signature.as_str() };
    let why = if change.intent.is_empty() { String::new() } else { format!(" (their intent: {})", change.intent) };
    let name = cork_core::local::bare_name(&dep.name).to_string();
    let direct = dep.called == name || dep.called.ends_with(&format!(".{name}"));
    let (lines, block, head, tail) = if change.deleted || !change.new_name.is_empty() || !change.new_path.is_empty() {
        // the name is gone from where this file finds it
        let mut lines: Vec<usize> = dep.imports.clone();
        if direct {
            lines.extend(dep.calls.iter().map(|c| c.line));
        }
        lines.sort_unstable();
        lines.dedup();
        if lines.is_empty() {
            return None;
        }
        // only a pushed change blocks: a pull and a re-read clear it. One only
        // in their checkout is what to write against (Study 8's phantom block)
        let block = change.pushed && lines.iter().any(|l| new_lines.contains(l));
        let head = if change.deleted {
            format!("Δ {} deleted {} from {}{why}.", change.who, dep.name, dep.path)
        } else if !change.new_name.is_empty() {
            let sig = if change.new_signature.is_empty() { String::new() } else { format!(", now `{}`", change.new_signature) };
            format!("Δ {} renamed {} -> {} in {}{sig}{why}.", change.who, dep.name, change.new_name, dep.path)
        } else {
            format!("Δ {} moved {} from {} to {}{why}.", change.who, dep.name, dep.path, change.new_path)
        };
        let to = if change.deleted { "its replacement".to_string() } else if change.new_name.is_empty() { change.new_path.clone() } else { format!("`{}`", change.new_name) };
        // the edit itself, for a rename: `compute_tax(…)` → `tax_of(…)`
        let edit = if !change.deleted && !change.new_name.is_empty() && !dep.calls.is_empty() {
            let renamed = match dep.called.rsplit_once('.') {
                Some((head, _)) => format!("{head}.{}", change.new_name),
                None => change.new_name.clone(),
            };
            format!(" (`{}(…)` → `{renamed}(…)`)", dep.called)
        } else {
            String::new()
        };
        let tail = if change.pushed {
            format!("{file} still uses `{}` on {}: write it against {to}{edit}.", dep.called, lines_text(&lines))
        } else {
            format!("{file} still uses `{}` on {}: not pushed yet, write it against {to}{edit}; landing will rebase onto it.", dep.called, lines_text(&lines))
        };
        (lines.clone(), block, head, tail)
    } else {
        let new_sig = change.new_signature.as_str();
        if new_sig.is_empty() || compact(new_sig) == compact(&dep.signature) {
            return None; // this copy already has it
        }
        let stale: Vec<&Call> = match arity(new_sig, &dep.name) {
            // a call whose arguments cannot be counted is not judged
            Some(a) => dep.calls.iter().filter(|c| c.args.is_some_and(|n| !a.fits(n))).collect(),
            None => dep.calls.iter().collect(),
        };
        if stale.is_empty() {
            return None; // every call already fits
        }
        let lines: Vec<usize> = stale.iter().map(|c| c.line).collect();
        let passes: Vec<String> = stale
            .iter()
            .take(4)
            .map(|c| match c.args {
                Some(n) => format!("line {} passes {n} arg{}", c.line, plural(n)),
                None => format!("line {}", c.line),
            })
            .collect();
        let head = format!("Δ {} changed {} in {}: `{old_sig}` -> `{new_sig}`{why}.", change.who, dep.name, dep.path);
        let later = if change.pushed { "" } else { " Not pushed yet: landing will rebase onto it." };
        // what it takes now, by name: what each of those calls must pass
        let now_takes = match arity(new_sig, &dep.name) {
            Some(a) => {
                let n = match a.max {
                    Some(max) if max == a.min => max.to_string(),
                    Some(max) => format!("{}-{max}", a.min),
                    None => format!("at least {}", a.min),
                };
                let names = crate::fixlines::param_names(new_sig, &dep.name);
                let named = if names.is_empty() { String::new() } else { format!(" ({})", names.join(", ")) };
                format!("; it now takes {n}{named}")
            }
            None => String::new(),
        };
        (lines, false, head, format!("Your calls in {file} do not fit it yet ({}{now_takes}): update them.{later}", passes.join(", ")))
    };
    let fact = change.fact(&dep.key);
    let short = format!("Δ {} in {} changed (said above): {tail}", dep.name, dep.path);
    Some(Finding { fact, file: file.to_string(), lines, block, full: format!("{head} {tail}"), short })
}

/// A change pushed to the session, against the files on disk that call its
/// key (`rows`: `{path, called}` as the session recorded them): what each
/// still has to fix. Nothing blocks here; the write has not started.
pub fn findings_on_disk(root: &Path, key: &str, rows: &[Value], change: &Change) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    for row in rows {
        let path = text(row, "path");
        if path.is_empty() || !done.insert(path.clone()) {
            continue;
        }
        let full = root.join(&path);
        if std::fs::metadata(&full).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&full) else { continue };
        for dep in deps_of(root, &path, &content).iter().filter(|d| d.key == key) {
            if let Some(f) = judge(dep, change, &path, &BTreeSet::new()) {
                out.push(f);
            }
        }
    }
    out
}

/// The lines of `after` that `before` does not have (a write's own lines).
pub fn new_lines(before: Option<&str>, after: &str) -> BTreeSet<usize> {
    let old: BTreeSet<&str> = before.unwrap_or("").lines().map(str::trim).collect();
    after.lines().enumerate().filter(|(_, l)| !old.contains(l.trim())).map(|(i, _)| i + 1).collect()
}

// ------------------------------------------------------------ said once per session

/// What this session was already told: fact → the files named and whether
/// a write was stopped over it. One file per session, beside its spool.
pub struct Told {
    path: std::path::PathBuf,
    facts: Map<String, Value>,
    /// The session's daemon is telling: what it notes came to the whole
    /// session through its spool (`pushed_ids`).
    push: bool,
}

impl Told {
    pub fn load(env: &Env, session: &str) -> Told {
        let path = crate::proto::session_dir(env, session).join("told.json");
        let cutoff = now_s() - TOLD_FOR_S;
        let facts = config::load_json(&path)
            .get("facts")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, v)| v.get("ts").and_then(Value::as_f64).unwrap_or(0.0) >= cutoff)
            .collect();
        Told { path, facts, push: false }
    }

    /// The session's list as its protocol daemon notes into it: a pushed
    /// delta reaches the session's spool, which whichever of its agents runs
    /// next reads.
    pub fn load_for_push(env: &Env, session: &str) -> Told {
        Told { push: true, ..Told::load(env, session) }
    }

    /// The facts the session's daemon told: another session's changes, said
    /// to the session as a whole. A helper's deltas leave them out too, as
    /// its lead's do.
    pub fn pushed_ids(&self) -> Vec<String> {
        self.facts.iter().filter(|(_, v)| v.get("push").and_then(Value::as_bool) == Some(true)).map(|(k, _)| k.clone()).collect()
    }

    pub fn knows(&self, fact: &str) -> bool {
        self.facts.contains_key(fact)
    }

    pub fn knows_file(&self, fact: &str, file: &str) -> bool {
        self.facts.get(fact).and_then(|v| v.get("files")).and_then(Value::as_array).is_some_and(|f| f.iter().any(|x| x.as_str() == Some(file)))
    }

    pub fn blocked(&self, fact: &str) -> bool {
        self.facts.get(fact).and_then(|v| v.get("block")).and_then(Value::as_bool) == Some(true)
    }

    pub fn note(&mut self, fact: &str, file: &str, block: bool) {
        let row = self.facts.entry(fact.to_string()).or_insert_with(|| json!({"files": [], "block": false}));
        row["ts"] = json!(now_s());
        if self.push {
            row["push"] = json!(true);
        }
        if block {
            row["block"] = json!(true);
        }
        if !file.is_empty() {
            if let Some(files) = row.get_mut("files").and_then(Value::as_array_mut) {
                if !files.iter().any(|x| x.as_str() == Some(file)) {
                    files.push(json!(file));
                }
            }
        }
    }

    /// Every fact told, newest last: what the server's deltas leave out.
    pub fn ids(&self) -> Vec<String> {
        let mut rows: Vec<(&String, f64)> = self.facts.iter().map(|(k, v)| (k, v.get("ts").and_then(Value::as_f64).unwrap_or(0.0))).collect();
        rows.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        rows.into_iter().map(|(k, _)| k.clone()).collect()
    }

    /// `(bare symbol, sig|ren|del)` of every fact told: what the report's
    /// name-only lint leaves out.
    pub fn told_names(&self) -> BTreeSet<(String, String)> {
        self.facts
            .keys()
            .filter_map(|f| {
                let mut parts = f.split('|');
                let symbol = parts.next()?.rsplit("::").next()?;
                let kind = parts.next()?;
                Some((cork_core::local::bare_name(symbol).to_string(), kind.to_string()))
            })
            .collect()
    }

    /// The `path::symbol` of every fact told.
    pub fn symbols(&self) -> Vec<String> {
        let mut out: Vec<String> = self.facts.keys().filter_map(|f| f.split('|').next().map(str::to_string)).collect();
        out.dedup();
        out
    }

    pub fn save(&mut self) {
        if self.facts.len() > MAX_TOLD {
            let mut rows: Vec<(String, Value)> = std::mem::take(&mut self.facts).into_iter().collect();
            rows.sort_by(|a, b| {
                let ts = |v: &Value| v.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
                ts(&b.1).partial_cmp(&ts(&a.1)).unwrap_or(std::cmp::Ordering::Equal)
            });
            rows.truncate(MAX_TOLD);
            self.facts = rows.into_iter().collect();
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, json!({"facts": self.facts}).to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// The gate's words for its findings: a block reason (the first finding
/// that blocks and was never blocked on), and the lines for the agent's
/// context, each fact once per session. Marks what it says as told.
pub fn speak(findings: Vec<Finding>, told: &mut Told) -> (Option<String>, String) {
    let (block, lines) = tell(findings, told);
    let notice = if lines.is_empty() {
        String::new()
    } else {
        format!(
            "Cork (before this write): a teammate changed code this file calls. Fix the calls in this write and say so to the user:\n{}",
            lines.join("\n")
        )
    };
    (block, notice)
}

/// `speak` without the heading: the block, and each line to say.
pub fn tell(findings: Vec<Finding>, told: &mut Told) -> (Option<String>, Vec<String>) {
    let mut block: Option<String> = None;
    let mut lines: Vec<String> = Vec::new();
    for f in findings {
        if f.block && !told.blocked(&f.fact) && block.is_none() {
            block = Some(format!(
                "{} This write adds a call to a name that no longer exists in their copy. Write it against their change \
(or message them first), then write again.",
                f.full
            ));
            told.note(&f.fact, &f.file, true);
            continue;
        }
        if told.knows_file(&f.fact, &f.file) {
            continue;
        }
        let said = if told.knows(&f.fact) { f.short.clone() } else { f.full.clone() };
        told.note(&f.fact, &f.file, false);
        if !lines.contains(&said) {
            lines.push(said);
        }
    }
    (block, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAX: &str = "def compute_tax(amount):\n    return round(amount * 0.2, 2)\n\n\ndef rate_for(region):\n    return 0.2\n";
    const APP: &str = "from tax import compute_tax, rate_for\nimport tax\n\n\ndef checkout(cart):\n    total = sum(cart)\n    return total + compute_tax(total)\n\n\ndef refund(x):\n    return -compute_tax(\n        x,\n    ) + tax.rate_for('eu')\n";

    fn repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("deps-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(dir.join("tax.py"), TAX).unwrap();
        dir
    }

    #[test]
    fn calls_and_arity_are_counted_from_the_text() {
        let calls = calls_of(APP, "compute_tax");
        assert_eq!(calls, vec![Call { line: 7, args: Some(1) }, Call { line: 11, args: Some(1) }]);
        assert!(calls_of("def compute_tax(a):\n    pass\n", "compute_tax").is_empty(), "a declaration is not a call");
        assert_eq!(calls_of("x = f(*args)\ny = f(a, (b, c), [d, e], 'f,g')\nz = f()\n", "f"),
            vec![Call { line: 1, args: None }, Call { line: 2, args: Some(4) }, Call { line: 3, args: Some(0) }]);
        assert!(calls_of("obj.f(1)\nfoo_f(2)\n", "f").is_empty(), "a method or a longer name is another function");
        assert_eq!(calls_of("compute_tax(1)\n", "compute_tax"), vec![Call { line: 1, args: Some(1) }]);

        let a = arity("def compute_tax(amount, region, *, rounding=2)", "compute_tax").unwrap();
        assert_eq!(a, Arity { min: 2, max: Some(3) });
        assert!(!a.fits(1) && a.fits(2) && a.fits(3) && !a.fits(4));
        assert_eq!(arity("def f(self, a, *rest, **kw)", "f").unwrap(), Arity { min: 1, max: None });
        assert_eq!(arity("function computeTax(amount: number, region?: string)", "computeTax").unwrap(), Arity { min: 1, max: Some(2) });
        assert_eq!(arity("pub fn tax(&self, a: Money, f: impl Fn(u8, u8) -> u8) -> Money", "tax").unwrap(), Arity { min: 2, max: Some(2) });
        assert!(arity("class Order", "Order").is_none());
    }

    #[test]
    fn a_file_names_what_it_calls_in_other_files() {
        let root = repo("resolve");
        std::fs::write(root.join("pkg/util.py"), "def helper():\n    pass\n").unwrap();
        std::fs::write(root.join("pkg/web.ts"), "export function computeTax(a: number) { return a }\n").unwrap();
        let deps = deps_of(&root, "app.py", APP);
        let got: Vec<(&str, &str, Vec<usize>)> = deps.iter().map(|d| (d.key.as_str(), d.called.as_str(), d.calls.iter().map(|c| c.line).collect())).collect();
        assert_eq!(got, vec![
            ("code:tax.py::compute_tax", "compute_tax", vec![7, 11]),
            ("code:tax.py::rate_for", "rate_for", vec![]),
            ("code:tax.py::rate_for", "tax.rate_for", vec![13]),
        ]);
        assert_eq!(deps[0].signature, "def compute_tax(amount)");
        assert_eq!(deps[0].imports, vec![1]);
        let cork_core::local::LocalParse { symbols, .. } = cork_core::local::symbols("tax.py", TAX).unwrap();
        assert_eq!(deps[0].base_hash, symbols[0].hash, "the base is this copy's content");
        // relative and package imports, and TypeScript
        let rel = deps_of(&root, "pkg/a.py", "from .util import helper\n\ndef go():\n    helper()\n");
        assert_eq!(rel[0].key, "code:pkg/util.py::helper");
        let ts = deps_of(&root, "pkg/app.ts", "import { computeTax } from './web';\n\nexport function go() { return computeTax(1, 2); }\n");
        assert_eq!(ts[0].key, "code:pkg/web.ts::computeTax");
        assert_eq!(ts[0].calls, vec![Call { line: 3, args: Some(2) }]);
        // a package outside the repo, and a name the file declares itself
        assert!(deps_of(&root, "b.py", "import os\n\ndef f():\n    os.getcwd()\n").is_empty());
        assert!(deps_of(&root, "c.py", "from tax import compute_tax\n\ndef compute_tax(a):\n    return a\n").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pushed (promoted) change, as a teammate's landed op.
    fn change(kind: &str, public: Value) -> Change {
        change_in(kind, public, "promoted")
    }

    fn change_in(kind: &str, public: Value, layer: &str) -> Change {
        let op = json!({"kind": kind, "public": public, "author": {"person": "alice@x.com", "agent_id": "claude"}, "layer": layer});
        summarize(&[op], |a| format!("{} ({})", text(a, "person"), text(a, "agent_id"))).unwrap()
    }

    /// Study 8's phantom block: a rename only in its author's checkout was
    /// a block no pull could clear. Unpushed, it is what to write against,
    /// never a block and never "pull".
    #[test]
    fn an_unpushed_change_is_what_to_write_against_never_a_block() {
        let root = repo("unpushed");
        let deps = deps_of(&root, "app.py", APP);
        let dep = &deps[0];
        let adds = new_lines(Some(APP), &APP.replace("return total + compute_tax(total)", "return total + compute_tax(total) + 1"));
        let ren = change_in("rename", json!({"new_name": "tax_of"}), "draft");
        let f = judge(dep, &ren, "app.py", &adds).unwrap();
        assert!(!f.block, "a draft never blocks: {f:?}");
        assert!(f.full.contains("not pushed yet, write it against `tax_of` (`compute_tax(…)` → `tax_of(…)`); landing will rebase onto it"), "{}", f.full);
        assert!(!f.full.to_lowercase().contains("pull"), "{}", f.full);
        let sig = change_in("signature_change", json!({"new_signature": "def compute_tax(amount, region)"}), "draft");
        let f = judge(dep, &sig, "app.py", &BTreeSet::new()).unwrap();
        assert!(f.full.ends_with("update them. Not pushed yet: landing will rebase onto it."), "{}", f.full);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_change_is_judged_against_this_files_calls() {
        let root = repo("judge");
        let deps = deps_of(&root, "app.py", APP);
        let dep = &deps[0];
        let none = BTreeSet::new();
        let sig = change("signature_change", json!({"new_signature": "def compute_tax(amount, region)", "old_signature": "def compute_tax(amount)",
            "intent": "regional rates"}));
        let f = judge(dep, &sig, "app.py", &none).unwrap();
        assert_eq!(f.full, "Δ alice@x.com (claude) changed compute_tax in tax.py: `def compute_tax(amount)` -> `def compute_tax(amount, region)` \
(their intent: regional rates). Your calls in app.py do not fit it yet (line 7 passes 1 arg, line 11 passes 1 arg; it now takes 2 \
(amount, region)): update them.");
        assert!(!f.block, "a signature change never blocks");
        assert_eq!(f.fact, "tax.py::compute_tax|sig|defcompute_tax(amount,region)");

        // an already-updated caller hears nothing
        let updated = APP.replace("compute_tax(total)", "compute_tax(total, 'eu')").replace("x,\n    )", "x, 'eu',\n    )");
        let deps2 = deps_of(&root, "app.py", &updated);
        assert!(judge(&deps2[0], &sig, "app.py", &none).is_none());
        // and a new optional parameter breaks nothing
        let optional = change("signature_change", json!({"new_signature": "def compute_tax(amount, region='us')"}));
        assert!(judge(dep, &optional, "app.py", &none).is_none());

        // a rename blocks only when the write itself adds a call to the old name
        let ren = change("rename", json!({"new_name": "tax_of"}));
        let f = judge(dep, &ren, "app.py", &none).unwrap();
        assert!(!f.block && f.lines == vec![1, 7, 11], "{f:?}");
        assert!(f.full.contains("renamed compute_tax -> tax_of in tax.py") && f.full.contains("app.py still uses `compute_tax` on lines 1, 7, 11"), "{}", f.full);
        assert!(f.full.ends_with("write it against `tax_of` (`compute_tax(…)` → `tax_of(…)`)."), "the edit itself: {}", f.full);
        let adds = new_lines(Some(APP), &APP.replace("return total + compute_tax(total)", "return total + compute_tax(total) + 1"));
        assert!(judge(dep, &ren, "app.py", &adds).unwrap().block);
        let del = change("delete", json!({}));
        assert!(judge(dep, &del, "app.py", &none).unwrap().full.starts_with("Δ alice@x.com (claude) deleted compute_tax from tax.py."));
        // a body edit is not a change a caller hears about
        let body = json!({"kind": "modify_body", "author": {}});
        assert!(summarize(&[body], |_| String::new()).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn each_fact_is_said_once_per_session() {
        let home = std::env::temp_dir().join(format!("deps-told-{}", std::process::id()));
        let mut env = Env::default();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        let finding = |file: &str, block: bool| Finding {
            fact: "tax.py::compute_tax|ren|tax_of".into(), file: file.into(), lines: vec![3], block,
            full: format!("FULL {file}"), short: format!("SHORT {file}"),
        };
        let mut told = Told::load(&env, "s1");
        let (stop, said) = speak(vec![finding("app.py", true)], &mut told);
        assert_eq!(stop.as_deref().map(|s| s.starts_with("FULL app.py")), Some(true));
        assert_eq!(said, "");
        told.save();
        // blocked once: the retry goes in, and nothing is said again for that file
        let mut told = Told::load(&env, "s1");
        assert_eq!(speak(vec![finding("app.py", true)], &mut told), (None, String::new()));
        // another file: its lines, not the story again
        let (_, said) = speak(vec![finding("cli.py", false)], &mut told);
        assert!(said.ends_with("\nSHORT cli.py"), "{said}");
        assert_eq!(speak(vec![finding("cli.py", false)], &mut told).1, "");
        assert_eq!(told.symbols(), vec!["tax.py::compute_tax".to_string()]);
        // another session hears it whole
        let mut other = Told::load(&env, "s2");
        assert!(speak(vec![finding("cli.py", false)], &mut other).1.ends_with("\nFULL cli.py"));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Each agent of a session keeps its own list: a helper hears once what
    /// its sibling or its lead was told, and the session's daemon's pushes,
    /// read by whichever agent ran next, are the session's and stay said.
    #[test]
    fn each_agent_of_a_session_keeps_its_own_list_but_the_daemons_pushes_are_shared() {
        let home = std::env::temp_dir().join(format!("deps-told-agents-{}", std::process::id()));
        let mut env = Env::default();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        let (lead, helper) = ("4f1c2d3e-aaaa-bbbb-cccc-0123456789ab", "aplugin-release-c854ef15933f0799");
        let mut told = Told::load(&env, &crate::proto::agent_of(lead, helper));
        told.note("api.py::handle|sig|handle(request)", "", false);
        told.save();
        assert!(!Told::load(&env, lead).knows("api.py::handle|sig|handle(request)"), "the lead was not told it");
        assert!(Told::load(&env, &crate::proto::agent_of(lead, helper)).knows("api.py::handle|sig|handle(request)"));
        assert!(!Told::load(&env, &crate::proto::agent_of(lead, "asibling-0000000000000000")).knows("api.py::handle|sig|handle(request)"));
        // a helper's folder never runs into a sibling's, however long their ids
        assert_ne!(
            crate::proto::session_dir(&env, &crate::proto::agent_of(lead, "ageneral-purpose-1111111111111111")),
            crate::proto::session_dir(&env, &crate::proto::agent_of(lead, "ageneral-purpose-2222222222222222")),
        );
        // the daemon's note is the session's
        let mut pushed = Told::load_for_push(&env, lead);
        pushed.note("db.rs::open|ren|connect", "", false);
        pushed.save();
        let lead_list = Told::load(&env, lead);
        assert_eq!(lead_list.pushed_ids(), vec!["db.rs::open|ren|connect".to_string()]);
        let _ = std::fs::remove_dir_all(&home);
    }
}
