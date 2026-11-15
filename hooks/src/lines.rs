//! Whose lines a write changes.
//!
//! Lattice test, round 2 (v104): two sessions in one checkout added an
//! issue `kind` and a `start_date` to the same SQL statements and the same
//! struct. The gate stopped B four times for edits that only added lines
//! beside A's (each went through unchanged on retry), and said nothing when
//! B rewrote the INSERT and UPDATE lines A had written a minute before. A
//! lease says who is editing a symbol, not whose lines a write touches. The
//! write journal does (journal.rs: each session's added lines, by content,
//! on this machine), so the gate asks it:
//!
//! - a write that changes or removes lines another agent wrote in the last
//!   `LIVE_S` is told whose lines and which. Another agent is another
//!   session, or another helper of this one (or its lead): two helpers of
//!   one session in one checkout overwrote each other's lines unseen while
//!   the journal held them as one session's. It is stopped once only
//!   when it would overwrite them unseen: a shell command or a patch. The
//!   harness's own Edit and Write refuse a file changed since it was read,
//!   so through them the agent has read those lines;
//! - a write that only adds lines, beside theirs or anywhere, goes in, with
//!   at most one line said.
//!
//! A shell command's write is predicted from its text: an append adds, a
//! `>` or `tee` replaces the file, a `sed -i`/`perl -pi` substitution or
//! deletion changes the lines its pattern matches. Anything else (a script)
//! cannot be told, and is said as a line, never stopped.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

use crate::config::{self, Env};
use crate::journal::{self, Entry};

/// A teammate's line counts as live work this long after they wrote it.
pub const LIVE_S: f64 = 1800.0;
const MAX_SAMPLE: usize = 70;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// One other agent's lines that a write would change.
#[derive(Debug, Clone, PartialEq)]
pub struct Theirs {
    pub session: String,
    /// The helper of `session` that wrote them ("" for the session itself).
    pub worker: String,
    /// Who wrote them, as the agent reads it (`whose`).
    pub who: String,
    pub title: String,
    /// When the newest of those lines was written.
    pub at: f64,
    /// 1-based, in the file as it is now.
    pub lines: Vec<usize>,
    /// The first of them, trimmed and clipped.
    pub sample: String,
}

/// A line worth naming: not blank, not a lone brace or `);`.
fn trivial(line: &[u8]) -> bool {
    String::from_utf8_lossy(line).chars().filter(|c| c.is_alphanumeric()).count() < 3
}

/// The writing agent of `entry` is the reader (`session`, and its helper
/// `worker`, "" for the session itself).
fn same_agent(entry: &Entry, session: &str, worker: &str) -> bool {
    journal::same_session(crate::proto::agent_session(&entry.session), crate::proto::agent_session(session)) && entry.worker == worker
}

/// One line's writer: (session, helper, the helper's name, title, when).
pub type Writer = (String, String, String, String, f64);

/// The journal entries for these files, oldest first: only their lines of
/// the log are parsed.
fn entries_for(root: &Path, env: &Env, rels: &[String]) -> Vec<Entry> {
    let Ok(body) = std::fs::read_to_string(journal::journal_dir(root, env).join("log.jsonl")) else { return Vec::new() };
    let wanted: Vec<String> = rels.iter().map(|r| format!("\"p\":{}", Value::String(r.clone()))).collect();
    body.lines()
        .filter(|line| wanted.iter().any(|w| line.contains(w.as_str())))
        .filter_map(journal::parse_entry)
        .collect()
}

/// The lines of `current` (1-based) that another agent wrote in the last
/// `LIVE_S`: for each line, the newest journaled write that added it, when
/// that write is not this agent's (the session `session`, or its helper
/// `worker`).
pub fn live_lines(entries: &[Entry], rel: &str, current: &[u8], session: &str, worker: &str, now: f64) -> BTreeMap<usize, Writer> {
    let mut by_key: HashMap<u64, &Entry> = HashMap::new();
    for entry in entries.iter().filter(|e| e.path == rel && !e.binary) {
        for (_, added) in &entry.hunks {
            for key in added {
                by_key.insert(*key, entry);
            }
        }
    }
    let lines = journal::split_lines(current);
    let mut out = BTreeMap::new();
    for (i, key) in journal::keys(&lines).iter().enumerate() {
        let Some(entry) = by_key.get(key) else { continue };
        if trivial(lines[i]) || same_agent(entry, session, worker) || now - entry.t > LIVE_S {
            continue;
        }
        out.insert(i + 1, (entry.session.clone(), entry.worker.clone(), entry.worker_name.clone(), entry.title.clone(), entry.t));
    }
    out
}

/// The lines of `current` (1-based) that `after` changes or removes; empty
/// when the write only adds lines.
pub fn changed_lines(current: &[u8], after: &[u8]) -> BTreeSet<usize> {
    let (old, new) = (journal::split_lines(current), journal::split_lines(after));
    journal::diff(&journal::keys(&old), &journal::keys(&new), 400)
        .iter()
        .flat_map(|h| (h.os + 1)..=(h.os + h.ol))
        .collect()
}

/// `live` lines among `changed`, by the agent that wrote them. `reader` is
/// (the reader's session, its Claude Code name): a writer of the same
/// session is named as its lead or its helper.
pub fn theirs_among(live: &BTreeMap<usize, Writer>, changed: &BTreeSet<usize>, current: &[u8], reader: (&str, &str)) -> Vec<Theirs> {
    let lines = journal::split_lines(current);
    let mut by_agent: BTreeMap<(String, String), Theirs> = BTreeMap::new();
    for n in changed {
        let Some((session, worker, worker_name, title, at)) = live.get(n) else { continue };
        let entry = by_agent.entry((session.clone(), worker.clone())).or_insert_with(|| Theirs {
            session: session.clone(),
            worker: worker.clone(),
            who: whose(session, worker, worker_name, title, reader),
            title: title.clone(),
            at: *at,
            lines: Vec::new(),
            sample: String::from_utf8_lossy(lines.get(n - 1).copied().unwrap_or_default()).trim().chars().take(MAX_SAMPLE).collect(),
        });
        entry.at = entry.at.max(*at);
        entry.lines.push(*n);
    }
    by_agent.into_values().collect()
}

// ------------------------------------------------------------ a shell command's write

/// What a shell command does to one file, as far as its text says.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellWrite {
    /// `>>`, `tee -a`, `sed -i '$a…'`: lines added, none changed.
    Appends,
    /// `>`, `tee`: the whole file replaced.
    Replaces,
    /// A substitution or deletion: the lines its pattern matches (1-based).
    Lines(BTreeSet<usize>),
    /// A script or anything else: cannot be told.
    Unknown,
}

/// Whether `word` names `target` (as given, relative to `cwd`).
fn names(word: &str, target: &Path, cwd: &Path) -> bool {
    let word = word.trim_matches(['"', '\'']);
    if word.is_empty() || word.starts_with('-') {
        return false;
    }
    let path = if Path::new(word).is_absolute() { PathBuf::from(word) } else { cwd.join(word) };
    config::normpath(&path) == config::normpath(target)
}

/// A basic (or, with `-E`/`-r`, extended) sed regex as the regex crate reads it.
fn sed_regex(pattern: &str, extended: bool) -> Option<Regex> {
    if extended {
        return Regex::new(pattern).ok();
    }
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(e @ ('(' | ')' | '{' | '}' | '+' | '?' | '|')) => out.push(e),
                Some(e) => {
                    out.push('\\');
                    out.push(e);
                }
                None => out.push_str("\\\\"),
            },
            '(' | ')' | '{' | '}' | '+' | '?' | '|' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    Regex::new(&out).ok()
}

/// The lines one sed (or `perl -pe`) script changes in `lines`, or None when
/// the script is not one this can read: `s<d>pat<d>rep<d>`, `/pat/d`, `/pat/s…`,
/// appends and inserts (`a`, `i`, `$a`), each optionally after an address.
fn sed_changes(script: &str, lines: &[String], extended: bool) -> Option<BTreeSet<usize>> {
    let mut changed = BTreeSet::new();
    for command in script.split([';', '\n']).map(str::trim).filter(|c| !c.is_empty()) {
        // an address: `/pat/`, `$`, a line number
        let (address, rest): (Option<Regex>, &str) = if let Some(after) = command.strip_prefix('/') {
            let end = after.find('/')?;
            (Some(sed_regex(&after[..end], extended)?), after[end + 1..].trim_start())
        } else {
            let digits = command.trim_start_matches(|c: char| c.is_ascii_digit() || c == '$' || c == ',');
            (None, digits)
        };
        let addressed = |line: &str| address.as_ref().map(|a| a.is_match(line)).unwrap_or(true);
        let mut letters = rest.chars();
        match letters.next() {
            Some('a' | 'i') => continue, // adds lines
            Some('d') if address.is_some() => {
                changed.extend(lines.iter().enumerate().filter(|(_, l)| addressed(l)).map(|(i, _)| i + 1));
            }
            Some('s') => {
                let delimiter = letters.next()?;
                let body: String = letters.collect();
                let pattern = body.split(delimiter).next()?;
                let re = sed_regex(pattern, extended)?;
                changed.extend(lines.iter().enumerate().filter(|(_, l)| addressed(l) && re.is_match(l)).map(|(i, _)| i + 1));
            }
            _ => return None,
        }
    }
    Some(changed)
}

/// What `command` does to `target`, whose lines are `current`.
pub fn shell_write(command: &str, target: &Path, cwd: &Path, current: &[u8]) -> ShellWrite {
    let lines: Vec<String> = String::from_utf8_lossy(current).lines().map(str::to_string).collect();
    let mut verdict: Option<ShellWrite> = None;
    for segment in crate::shell::shell_segments(command) {
        let words = crate::sharedtree::words(&segment);
        let Some(program) = words.iter().find(|w| !w.contains('=')).cloned() else { continue };
        let program = program.rsplit('/').next().unwrap_or("").to_string();
        // redirections first: `> file`, `>file`, `>> file`
        let mut redirect: Option<bool> = None;
        for (i, word) in words.iter().enumerate() {
            for (op, append) in [(">>", true), (">", false)] {
                if let Some(rest) = word.strip_prefix(op).filter(|_| !word.starts_with("2>") && !word.starts_with("&>")) {
                    let named = if rest.is_empty() { words.get(i + 1).map(|w| names(w, target, cwd)).unwrap_or(false) } else { names(rest, target, cwd) };
                    if named && !(op == ">" && word.starts_with(">>")) {
                        redirect = Some(redirect.unwrap_or(true) && append);
                    }
                }
            }
        }
        let this = match (program.as_str(), redirect) {
            (_, Some(true)) => Some(ShellWrite::Appends),
            (_, Some(false)) => Some(ShellWrite::Replaces),
            ("tee", None) if words.iter().any(|w| names(w, target, cwd)) => {
                Some(if words.iter().any(|w| w == "-a" || w == "--append") { ShellWrite::Appends } else { ShellWrite::Replaces })
            }
            ("sed" | "gsed" | "perl", None) if words.iter().any(|w| names(w, target, cwd)) => {
                let inplace = words.iter().any(|w| w.starts_with("-i") || w.starts_with("--in-place") || (program == "perl" && w.starts_with('-') && w.contains('i') && w.contains('p')));
                if !inplace {
                    None
                } else {
                    let extended = program == "perl" || words.iter().any(|w| w == "-E" || w == "-r" || w == "--regexp-extended");
                    // the script: after -e, else the first word that is not a flag or the file
                    let script = words
                        .iter()
                        .position(|w| w == "-e" || (program == "perl" && w.starts_with('-') && w.ends_with('e')))
                        .and_then(|i| words.get(i + 1).cloned())
                        .or_else(|| words.iter().skip(1).find(|w| !w.starts_with('-') && !names(w, target, cwd) && !w.is_empty() && **w != "''").cloned());
                    Some(match script.and_then(|s| sed_changes(&s, &lines, extended)) {
                        Some(changed) => ShellWrite::Lines(changed),
                        None => ShellWrite::Unknown,
                    })
                }
            }
            _ => None,
        };
        let Some(this) = this else { continue };
        // several writes to one file: the widest says
        verdict = Some(match (verdict, this) {
            (None, w) => w,
            (Some(ShellWrite::Replaces), _) | (_, ShellWrite::Replaces) => ShellWrite::Replaces,
            (Some(ShellWrite::Unknown), _) | (_, ShellWrite::Unknown) => ShellWrite::Unknown,
            (Some(ShellWrite::Lines(mut a)), ShellWrite::Lines(b)) => {
                a.extend(b);
                ShellWrite::Lines(a)
            }
            (Some(ShellWrite::Lines(a)), ShellWrite::Appends) | (Some(ShellWrite::Appends), ShellWrite::Lines(a)) => ShellWrite::Lines(a),
            (Some(ShellWrite::Appends), ShellWrite::Appends) => ShellWrite::Appends,
        });
    }
    verdict.unwrap_or(ShellWrite::Unknown)
}

// ------------------------------------------------------------ the gate's check

/// What the gate learns of one write from the journal.
#[derive(Debug, Default, Clone)]
pub struct Check {
    /// Other sessions' live lines the write changes, per file.
    pub rewrites: Vec<(String, Theirs)>,
    /// Lines the write may change that cannot be told (a script), per file,
    /// where other sessions have live lines.
    pub unknown: Vec<(String, Theirs)>,
    /// Every target only gains lines.
    pub adds_only: bool,
    /// Sessions with live lines in these files on this machine: whose
    /// lines the journal can speak for.
    pub here: BTreeSet<String>,
    /// The agent has read the files as they are: the harness's own Edit and
    /// Write refuse a file changed since it was read.
    pub seen: bool,
}

/// The journal's word on a write about to happen, by the session `session`
/// or its helper `worker`.
#[allow(clippy::too_many_arguments)]
pub fn check(root: &Path, targets: &[PathBuf], tool: &str, tool_input: &Value, cwd: &Path, session: &str, worker: &str, env: &Env) -> Check {
    let mut out = Check { adds_only: true, seen: matches!(tool, "Edit" | "MultiEdit" | "Write" | "NotebookEdit"), ..Check::default() };
    let rels: Vec<String> = targets.iter().filter_map(|t| config::rel_path(t, root)).collect();
    let entries = entries_for(root, env, &rels);
    let now = journal::now_s();
    let command = text(tool_input, "command");
    // a writer of this session is named as its lead or its helper
    let lead = std::cell::OnceCell::new();
    let reader = |lead: &std::cell::OnceCell<String>| (session.to_string(), lead.get_or_init(|| crate::transcript::session_name(session, false, env)).clone());
    for target in targets {
        let Some(rel) = config::rel_path(target, root) else { continue };
        let current = std::fs::read(target).unwrap_or_default();
        let live = live_lines(&entries, &rel, &current, session, worker, now);
        // the protocol's lease holder is a session: a sibling helper holds none
        out.here.extend(live.values().map(|(s, _, _, _, _)| crate::proto::agent_session(s).to_string()));
        let (me, name) = if live.is_empty() { (String::new(), String::new()) } else { reader(&lead) };
        let changed: Option<BTreeSet<usize>> = if crate::gate::is_shell_tool(tool) && !command.contains("*** ") {
            match shell_write(&command, target, cwd, &current) {
                ShellWrite::Appends => Some(BTreeSet::new()),
                ShellWrite::Replaces => Some((1..=journal::split_lines(&current).len()).collect()),
                ShellWrite::Lines(lines) => Some(lines),
                ShellWrite::Unknown => None,
            }
        } else {
            let as_text = String::from_utf8_lossy(&current).into_owned();
            let exists = !current.is_empty() || target.exists();
            crate::proto::predict_after(exists.then_some(as_text.as_str()), tool, tool_input, target, cwd)
                .map(|after| changed_lines(&current, after.unwrap_or_default().as_bytes()))
        };
        match changed {
            Some(changed) => {
                if !changed.is_empty() {
                    out.adds_only = false;
                }
                out.rewrites.extend(theirs_among(&live, &changed, &current, (&me, &name)).into_iter().map(|t| (rel.clone(), t)));
            }
            None => {
                out.adds_only = false;
                let all: BTreeSet<usize> = live.keys().copied().collect();
                out.unknown.extend(theirs_among(&live, &all, &current, (&me, &name)).into_iter().map(|t| (rel.clone(), t)));
            }
        }
    }
    out
}

// ------------------------------------------------------------ the words

/// `3, 7-9`: line numbers as ranges.
pub fn ranges(lines: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = lines[i];
        let mut end = start;
        while i + 1 < lines.len() && lines[i + 1] == end + 1 {
            i += 1;
            end = lines[i];
        }
        out.push(if start == end { format!("{start}") } else { format!("{start}-{end}") });
        i += 1;
    }
    out.join(", ")
}

/// Who wrote them, as the agent reads it: the journal is this machine's, so
/// another session of the same person, or another agent of the reader's own
/// session (`reader`: its id and Claude Code name): its lead, or one of its
/// helpers, `cork-cloud-19's helper repo-names`.
fn whose(session: &str, worker: &str, worker_name: &str, title: &str, reader: (&str, &str)) -> String {
    let (me, lead) = reader;
    if !me.is_empty() && journal::same_session(crate::proto::agent_session(session), crate::proto::agent_session(me)) {
        let lead = if lead.is_empty() { "your session".to_string() } else { lead.to_string() };
        if worker.is_empty() {
            return if lead == "your session" { "your lead".to_string() } else { format!("{lead} (your lead)") };
        }
        let name = if worker_name.is_empty() { worker.chars().take(24).collect::<String>() } else { worker_name.to_string() };
        return if lead == "your session" { format!("your session's helper {name}") } else { format!("{lead}'s helper {name}") };
    }
    if title.is_empty() {
        format!("another session of yours ({})", session.chars().take(8).collect::<String>())
    } else {
        format!("another session of yours (\"{title}\")")
    }
}

fn line_word(lines: &[usize]) -> &'static str {
    if lines.len() == 1 { "line" } else { "lines" }
}

/// The notice for a write that changes a teammate's lines it has read.
pub fn notice(rel: &str, theirs: &Theirs, now: f64) -> String {
    format!(
        "Cork: this write changes {} {} of {rel}, which {} wrote {} (`{}`). Keep what they added in your new lines; landing merges the rest.",
        line_word(&theirs.lines),
        ranges(&theirs.lines),
        theirs.who,
        crate::prompt::age_text(now - theirs.at),
        theirs.sample,
    )
}

/// The block for a write that would overwrite a teammate's lines unseen.
pub fn block(rel: &str, theirs: &Theirs, now: f64) -> String {
    format!(
        "this command would overwrite {} {} of {rel}, which {} wrote {} (`{}`), and you have not read them. Read those lines, \
keep what they added, then run it again: it goes through.",
        line_word(&theirs.lines),
        ranges(&theirs.lines),
        theirs.who,
        crate::prompt::age_text(now - theirs.at),
        theirs.sample,
    )
}

/// The line for a command whose effect on a teammate's lines cannot be told.
pub fn unknown(rel: &str, theirs: &Theirs, now: f64) -> String {
    format!(
        "Cork: this command edits {rel}, where {} wrote {} {} {} (`{}`). Keep their lines as they are.",
        theirs.who,
        line_word(&theirs.lines),
        ranges(&theirs.lines),
        crate::prompt::age_text(now - theirs.at),
        theirs.sample,
    )
}

/// The line for a lease's holder when the write only adds lines.
pub fn beside(who: &str, symbol: &str) -> String {
    format!("Cork: {who} is editing {symbol} right now too; your write only adds lines beside theirs, so it goes in. Landing merges both.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session: &str, title: &str, path: &str, t: f64, added: &[&str]) -> Entry {
        let keys = journal::keys(&added.iter().map(|l| l.as_bytes()).collect::<Vec<_>>());
        Entry {
            t, session: session.into(), worker: String::new(), worker_name: String::new(), title: title.into(), path: path.into(),
            after: String::new(), binary: false, hunks: vec![(Vec::new(), keys)],
        }
    }

    #[test]
    fn a_sed_substitution_changes_only_the_lines_it_matches() {
        let lines: Vec<String> = ["let a = 1;", "let kind = ?15;", "let b = 2;"].iter().map(|s| s.to_string()).collect();
        assert_eq!(sed_changes("s/kind = ?15/kind = ?15, start = ?16/", &lines, false), Some(BTreeSet::from([2])));
        assert_eq!(sed_changes("/let b/d", &lines, false), Some(BTreeSet::from([3])));
        assert_eq!(sed_changes("$a\\new line", &lines, false), Some(BTreeSet::new()));
        assert_eq!(sed_changes("y/abc/xyz/", &lines, false), None);
    }

    #[test]
    fn a_shell_command_says_what_it_does_to_a_file() {
        let cwd = Path::new("/r");
        let target = Path::new("/r/API.md");
        let current = b"# API\nkind: one of bug, feature\n";
        assert_eq!(shell_write("echo '- start_date' >> API.md", target, cwd, current), ShellWrite::Appends);
        assert_eq!(shell_write("cat new.md > API.md", target, cwd, current), ShellWrite::Replaces);
        assert_eq!(shell_write("printf x | tee API.md", target, cwd, current), ShellWrite::Replaces);
        assert_eq!(shell_write("printf x | tee -a API.md", target, cwd, current), ShellWrite::Appends);
        assert_eq!(shell_write("sed -i '' 's/bug, feature/bug, feature, chore/' API.md", target, cwd, current), ShellWrite::Lines(BTreeSet::from([2])));
        assert_eq!(shell_write("perl -pi -e 's/^# API/# The API/' API.md", target, cwd, current), ShellWrite::Lines(BTreeSet::from([1])));
        assert_eq!(shell_write("python3 fix.py API.md", target, cwd, current), ShellWrite::Unknown);
    }

    #[test]
    fn only_another_sessions_fresh_lines_count_and_braces_never() {
        let current = b"fn a() {\n    let kind = 1;\n}\nfn b() {}\n";
        let now = 10_000.0;
        let entries = vec![
            entry("sess-a", "Kind field", "src/db.rs", now - 60.0, &["    let kind = 1;\n", "}\n"]),
            entry("sess-old", "", "src/db.rs", now - 4000.0, &["fn b() {}\n"]),
        ];
        let live = live_lines(&entries, "src/db.rs", current, "sess-b", "", now);
        assert_eq!(live.keys().copied().collect::<Vec<_>>(), vec![2], "{live:?}");
        assert!(live_lines(&entries, "src/db.rs", current, "sess-a", "", now).is_empty(), "its own lines are not news");
        assert_eq!(ranges(&[3, 4, 5, 9, 11, 12]), "3-5, 9, 11-12");
    }
}
