//! Deciding which files a tool call touched.
//!
//! Harnesses hand shell calls over as one opaque command string with no
//! file_path, and some steer agents to edit with `sed`, heredocs and inline
//! scripts. Treating those as reads let an agent rewrite a whole repo while
//! the workspace tree stood still, so the command's tokens are matched
//! against files the filesystem says just changed.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use fancy_regex::Regex as FancyRegex;
use regex::Regex;

use crate::config::{absolute, normpath, rel_path};

pub const MAX_SHELL_GATE_FILES: usize = 5;
pub const MAX_SHELL_WRITE_FILES: usize = 5;
pub const SHELL_WRITE_WINDOW_S: f64 = 20.0;

pub const PATCH_MARKERS: [&str; 4] = [
    "*** Add File:",
    "*** Update File:",
    "*** Delete File:",
    "*** Move to:",
];

/// Shell idioms that can write a file. Deliberately over-inclusive: this only
/// decides whether it is worth stat-ing anything. `2>&1` and `>&2` are
/// excluded so redirecting diagnostics doesn't look like an edit.
fn write_idiom() -> &'static FancyRegex {
    static RE: OnceLock<FancyRegex> = OnceLock::new();
    RE.get_or_init(|| {
        FancyRegex::new(concat!(
            r"(?<![0-9&])>>?(?!&)",
            r"|\bsed\b[^|;]*\s-[A-Za-z]*i",
            r"|\b(?:tee|cp|mv|touch|install|dd|truncate|patch|rsync|ln)\b",
            r"|\bgit\s+(?:apply|checkout|restore|stash|mv|rm|clean)\b",
            r"|\b(?:python[0-9.]*|py|node|deno|bun|perl|ruby|php)\b[^|;]*\s-[ce]\b",
            r"|\b(?:python[0-9.]*|py)\s+-m\b",
            r"|<<-?\s*['\x22]?\w+",
            // a program that was RUN: a script writes files that appear
            // nowhere on its command line, so the window and git's view of
            // what changed decide
            r"|(?:^|[;&|(]\s*)(?:\w+=[^\s;&|]*\s+)*(?:(?:sudo|env|time|exec|nice)\s+)?",
            r"(?:python[0-9.]*|py|node|deno|bun|perl|ruby|php|bash|sh|zsh|make|npm|npx|pnpm|yarn|cargo|go)\s+[^\s|;&<>]+",
            r"|(?:^|[;&|(]\s*)\.{1,2}/\S+",
        ))
        .expect("write idiom pattern")
    })
}

/// Anything in the command that could name a file: a run containing a path
/// separator, or a bare `name.ext`. Quotes fall outside the class on purpose,
/// which is what finds the path inside `python -c "open('src/a.py','w')"`.
fn path_token() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[\w.~@+-]*(?:[/\\][\w.~@+-]+)+|[\w~@+-]+\.[A-Za-z0-9_]{1,12}")
            .expect("path token pattern")
    })
}

fn cd_re() -> &'static FancyRegex {
    static RE: OnceLock<FancyRegex> = OnceLock::new();
    RE.get_or_init(|| FancyRegex::new(r"\b(?:cd|pushd)\s+(?!-)([^\s;&|]+)").expect("cd pattern"))
}

fn git_c_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bgit\s+-C\s+([^\s;&|]+)").expect("git -C pattern"))
}

pub fn has_write_idiom(command: &str) -> bool {
    write_idiom().is_match(command).unwrap_or(false)
}

fn tokens(command: &str) -> Vec<String> {
    path_token()
        .find_iter(command)
        .map(|m| m.as_str().to_string())
        .collect()
}

fn expanduser(path: &str, home: &str) -> String {
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", home.trim_end_matches('/'), rest)
    } else {
        path.to_string()
    }
}

/// True when a shell command's work belongs to a DIFFERENT repo: it cd/pushd-es
/// outside the root, or aims git elsewhere with `git -C`. Conservative by
/// design — a target that cannot be resolved to a concrete in-repo path (an
/// unexpanded `$VAR`) counts as leaving.
pub fn command_leaves_repo(command: &str, cwd: &Path, root: &Path, home: &str) -> bool {
    let mut targets: Vec<String> = Vec::new();
    for caps in cd_re().captures_iter(command).flatten() {
        if let Some(m) = caps.get(1) {
            targets.push(m.as_str().to_string());
        }
    }
    for caps in git_c_re().captures_iter(command) {
        if let Some(m) = caps.get(1) {
            targets.push(m.as_str().to_string());
        }
    }
    for target in targets {
        let target = target.trim().trim_matches(|c| c == '\'' || c == '"');
        if target.is_empty() || target == "-" {
            continue;
        }
        if target.contains('$') {
            return true;
        }
        let expanded = expanduser(target, home);
        let resolved = if Path::new(&expanded).is_absolute() {
            PathBuf::from(&expanded)
        } else {
            cwd.join(&expanded)
        };
        if rel_path(&normpath(&resolved), root).is_none() {
            return true;
        }
    }
    false
}

fn mtime_secs(path: &Path) -> Option<f64> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let modified = meta.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs_f64())
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Absolute paths a shell command just wrote: every token naming a real file
/// inside the repo whose mtime lands in this command's window. A wrong guess
/// costs nothing — a file merely read has an old mtime and drops out.
///
/// Only the fallback now: a command the gate saw run is judged by what
/// changed while it ran (shellrun.rs), never by the clock.
pub fn shell_write_targets(command: &str, cwd: &Path, root: &Path) -> Vec<PathBuf> {
    shell_write_targets_with(command, cwd, root, true)
}

/// `shell_write_targets`, with the files the command never named left out
/// when `unnamed` is false: in a checkout another session is working in,
/// a dirty file with a fresh mtime is as likely theirs as this command's.
pub fn shell_write_targets_with(command: &str, cwd: &Path, root: &Path, unnamed: bool) -> Vec<PathBuf> {
    if command.is_empty() || !has_write_idiom(command) {
        return Vec::new();
    }
    let cutoff = now_secs() - SHELL_WRITE_WINDOW_S;
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for token in tokens(command) {
        if token.is_empty() || token.starts_with('-') {
            continue;
        }
        for base in [cwd, root] {
            let candidate = if Path::new(&token).is_absolute() {
                PathBuf::from(&token)
            } else {
                base.join(&token)
            };
            let candidate = normpath(&candidate);
            if seen.contains(&candidate) {
                continue;
            }
            seen.push(candidate.clone());
            // a directory is no write target: `cd repo && pytest 2>&1` after
            // pytest made its cache there is a test run, not a write
            if rel_path(&candidate, root).is_none() || !candidate.is_file() {
                continue;
            }
            match mtime_secs(&candidate) {
                Some(mtime) if mtime >= cutoff => {
                    out.push(candidate);
                    if out.len() >= MAX_SHELL_WRITE_FILES {
                        return out;
                    }
                }
                _ => continue,
            }
        }
    }
    // what the command never named — see `recently_written_in_repo`
    if !unnamed {
        return out;
    }
    for candidate in recently_written_in_repo(root, cutoff) {
        if seen.contains(&candidate) {
            continue;
        }
        seen.push(candidate.clone());
        out.push(candidate);
        if out.len() >= MAX_SHELL_WRITE_FILES {
            break;
        }
    }
    out
}

/// Files under the repo written inside this command's window that the
/// command never named, sorted by path: git's view of what is modified or
/// new (ignored files excluded), stat-ed against the window.
///
/// `python3 fix.py` names only the script; the files it wrote appear nowhere
/// on its command line, so the token scan cannot see them. Without this a
/// scripted edit went unreported, the server's baseline for the file went
/// days stale, and the next report of it read as a rename that never
/// happened. Bounded and fail-open: no git, no repo, or too slow means
/// nothing extra, never a wrong answer. Mirrors `_recently_written_in_repo`
/// in report_hook.py, sorted the same way (by the path's text).
pub fn recently_written_in_repo(root: &Path, cutoff: f64) -> Vec<PathBuf> {
    let Some(listing) = crate::git::git(
        root,
        &["ls-files", "-m", "-o", "--exclude-standard", "-z"],
        std::time::Duration::from_millis(250),
    ) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for rel in listing.split('\0') {
        if rel.is_empty() {
            continue;
        }
        let candidate = normpath(&root.join(rel));
        if !candidate.is_file() {
            continue;
        }
        if mtime_secs(&candidate).is_some_and(|mtime| mtime >= cutoff) {
            out.push(candidate);
        }
    }
    out.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
    out
}

/// Programs that never change a file on their own: a command made only of
/// these (with no redirect, no in-place edit) needs no snapshot before it
/// runs. Anything else may write: a test runner, a build, `tsx script.ts`.
const READ_ONLY_PROGRAMS: [&str; 62] = [
    "ls", "cat", "head", "tail", "grep", "egrep", "fgrep", "rg", "ag", "ack", "fd", "wc", "pwd", "echo",
    "printf", "which", "type", "whereis", "stat", "file", "du", "df", "tree", "less", "more", "sort",
    "uniq", "cut", "tr", "diff", "cmp", "date", "whoami", "uname", "hostname", "id", "true", "false",
    "sleep", "test", "[", "jq", "column", "nl", "od", "xxd", "hexdump", "strings", "basename", "dirname",
    "realpath", "readlink", "printenv", "ps", "lsof", "cd", "pushd", "popd", "export", "sed", "awk", "find",
];

/// git's subcommands that leave the working tree as it was.
const GIT_READS: [&str; 26] = [
    "status", "log", "diff", "show", "rev-parse", "rev-list", "ls-files", "ls-tree", "blame", "describe",
    "shortlog", "grep", "remote", "cat-file", "merge-base", "name-rev", "for-each-ref", "show-ref",
    "count-objects", "help", "version", "fetch", "reflog", "config", "branch", "tag",
];

/// git's subcommand, past `-C dir`, `-c key=value` and other options.
fn git_subcommand(segment: &str) -> String {
    let mut words = segment.split_whitespace().skip_while(|w| program_of(w) != "git").skip(1);
    while let Some(word) = words.next() {
        if word == "-C" || word == "-c" {
            words.next();
        } else if !word.starts_with('-') {
            return word.to_string();
        }
    }
    String::new()
}

/// Whether a command surely writes no file: every part of it is a program
/// that only reads, with no redirect, in-place edit or heredoc script. Only
/// decides what the gate may skip, so anything unsure is a write.
pub fn read_only(command: &str) -> bool {
    let split = split_shell(command);
    if split.bodies.iter().any(|(owner, _)| {
        let program = split.segments.get(*owner).map(|s| program_of(s)).unwrap_or_default();
        is_shell(&program) || is_interpreter(&program)
    }) {
        return false;
    }
    split.segments.iter().filter(|s| !s.is_empty()).all(|segment| {
        if shell_write().is_match(segment).unwrap_or(true) {
            return false;
        }
        let program = program_of(segment);
        match program.as_str() {
            "git" => {
                let sub = git_subcommand(segment);
                let words: Vec<&str> = segment.split_whitespace().collect();
                GIT_READS.contains(&sub.as_str())
                    && !(sub == "branch" && words.iter().any(|w| matches!(*w, "-d" | "-D" | "-m" | "-M" | "-c" | "-C" | "--delete" | "--move")))
                    && !(sub == "config" && !words.iter().any(|w| w.starts_with("--get") || matches!(*w, "-l" | "--list")))
                    && !(sub == "reflog" && words.iter().any(|w| matches!(*w, "expire" | "delete")))
                    && !(sub == "remote" && words.iter().any(|w| matches!(*w, "add" | "remove" | "rm" | "rename" | "set-url" | "prune")))
            }
            "find" => !segment.split_whitespace().any(|w| matches!(w, "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fprint" | "-fprintf" | "-fls")),
            "awk" => !segment.contains("system("),
            "" => true,
            p => READ_ONLY_PROGRAMS.contains(&p),
        }
    })
}

/// Whether a command names `rel` (a repo path): on its line, or in the text
/// of a script it runs (`tsx scripts/add-note.ts` whose source says
/// `CLAUDE.md`). When two sessions' commands ran at once and a file
/// changed, the one that names it wrote it; when neither does, nobody can
/// say.
pub fn names_path(command: &str, cwd: &Path, root: &Path, rel: &str) -> bool {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    if base.is_empty() {
        return false;
    }
    let mentions = |text: &str| mentions_name(text, rel) || mentions_name(text, base);
    if mentions(command) {
        return true;
    }
    let mut read = 0;
    for token in tokens(command) {
        if read >= 4 {
            break;
        }
        let candidate = normpath(&if Path::new(&token).is_absolute() { PathBuf::from(&token) } else { cwd.join(&token) });
        if rel_path(&candidate, root).is_none() || !candidate.is_file() {
            continue;
        }
        let small = std::fs::metadata(&candidate).is_ok_and(|m| m.len() <= 256 * 1024);
        if !small {
            continue;
        }
        read += 1;
        if std::fs::read(&candidate).is_ok_and(|bytes| mentions(&String::from_utf8_lossy(&bytes))) {
            return true;
        }
    }
    false
}

/// `name` in `text` as a whole name: `a.py` is not in `data.py`, nor
/// `CLAUDE.md` in `CLAUDE.md.bak`; `src/a.py` and `"a.py"` hold it.
fn mentions_name(text: &str, name: &str) -> bool {
    let part = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    text.match_indices(name).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + name.len()..].chars().next();
        !before.is_some_and(part) && !after.is_some_and(|c| part(c) && !(c == '.' && text[at + name.len() + 1..].chars().next().map_or(true, |n| !part(n))))
    })
}

/// Shell commands that DISPLAY a file: what they show joins the map too.
const READ_CMDS: [&str; 8] = ["cat", "head", "tail", "bat", "less", "more", "nl", "sed"];
const MAX_SHELL_READ_FILES: usize = 4;

/// Absolute paths of repo source files a shell command displayed, so they
/// join the map the way a Read does. Only plain display idioms — cat, head,
/// tail, sed -n and friends — and only tokens naming an existing source file
/// inside the repo; a command that also writes is the write path's business.
/// Capped at four: a `cat` over a glob is an index, not a read. Mirrors
/// `_shell_read_targets` in report_hook.py.
/// Where a shell command works, when it says so: the first `cd`/`pushd` or
/// `git -C` target, else the first absolute path it names that exists (its
/// folder, for a file). `None` for a command that names no place: it works
/// in the session's folder. Lets a chat started outside a repo (a project
/// folder) have its commands in a repo counted as that repo's activity.
pub fn dir_hint(command: &str, cwd: &Path, home: &str) -> Option<PathBuf> {
    if command.trim().is_empty() {
        return None;
    }
    let mut named: Vec<String> = Vec::new();
    for caps in cd_re().captures_iter(command).flatten() {
        if let Some(m) = caps.get(1) {
            named.push(m.as_str().to_string());
        }
    }
    for caps in git_c_re().captures_iter(command) {
        if let Some(m) = caps.get(1) {
            named.push(m.as_str().to_string());
        }
    }
    for target in named {
        let target = target.trim().trim_matches(|c| c == '\'' || c == '"');
        if target.is_empty() || target == "-" || target.contains('$') {
            continue;
        }
        let expanded = expanduser(target, home);
        let resolved = if Path::new(&expanded).is_absolute() { PathBuf::from(&expanded) } else { cwd.join(&expanded) };
        let resolved = normpath(&resolved);
        if resolved.is_dir() {
            return Some(resolved);
        }
    }
    for found in path_token().find_iter(command) {
        // a setting's value (`CORK_HOME=/elsewhere cork land`) is
        // where a program keeps its state, not where the command works
        let before = &command[..found.start()];
        if before.ends_with('=') || before.ends_with("='") || before.ends_with("=\"") {
            continue;
        }
        // nor is Cork's own program, wherever its home is
        if found.as_str().contains("/.cork/") || found.as_str().contains("/.collide/") {
            continue;
        }
        let expanded = expanduser(found.as_str(), home);
        if !Path::new(&expanded).is_absolute() {
            continue;
        }
        let path = normpath(Path::new(&expanded));
        if path.is_dir() {
            return Some(path);
        }
        if path.is_file() {
            return path.parent().map(Path::to_path_buf);
        }
    }
    None
}

pub fn shell_read_targets(command: &str, cwd: &Path, root: &Path) -> Vec<PathBuf> {
    if command.is_empty() || has_write_idiom(command) {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = Vec::new();
    let joined = command.replace("||", "\u{1}").replace("&&", "\u{1}");
    for segment in joined.split(|c| c == ';' || c == '|' || c == '\u{1}') {
        let mut words = segment.split_whitespace().peekable();
        while words.peek().is_some_and(|w| w.contains('=') && !w.starts_with('-')) {
            words.next(); // leading VAR=value assignments
        }
        let Some(first) = words.next() else { continue };
        let base = Path::new(first).file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !READ_CMDS.contains(&base) {
            continue;
        }
        let rest: Vec<&str> = words.collect();
        if base == "sed"
            && (rest.iter().any(|a| a.starts_with("-i")) || !rest.iter().any(|a| a.starts_with("-n")))
        {
            continue;
        }
        for token in tokens(segment) {
            if token.is_empty() || token.starts_with('-') || token.starts_with('~') {
                continue;
            }
            let candidate = if Path::new(&token).is_absolute() {
                PathBuf::from(&token)
            } else {
                cwd.join(&token)
            };
            let candidate = normpath(&candidate);
            let Some(rel) = rel_path(&candidate, root) else { continue };
            if crate::report::is_map_source(&rel) && candidate.is_file() && !out.contains(&candidate) {
                out.push(candidate);
                if out.len() >= MAX_SHELL_READ_FILES {
                    return out;
                }
            }
        }
    }
    out
}

/// A command's simple commands, split on `;`, `&&`, `||`, `|` and newlines
/// outside quotes, so a separator inside `python -c "a; b"` stays put. A
/// heredoc's body is not shell: it is left out (see `split_shell`).
pub fn shell_segments(command: &str) -> Vec<String> {
    split_shell(command).segments.into_iter().filter(|s| !s.is_empty()).collect()
}

/// A command as the shell reads it: its simple commands (trimmed; an empty
/// one stays, so a body's index holds) and each heredoc's body beside the
/// index of the segment that opened it.
struct Split {
    segments: Vec<String>,
    bodies: Vec<(usize, String)>,
}

/// A heredoc's delimiter at `chars[at..]` (just past `<<`): (delimiter,
/// strip leading tabs, index past it). `<<<` (a here-string) is none.
fn heredoc_marker(chars: &[char], at: usize) -> Option<(String, bool, usize)> {
    let mut i = at;
    if chars.get(i) == Some(&'<') {
        return None;
    }
    let strip = chars.get(i) == Some(&'-');
    if strip {
        i += 1;
    }
    while chars.get(i).is_some_and(|c| *c == ' ' || *c == '\t') {
        i += 1;
    }
    let mut word = String::new();
    match chars.get(i) {
        Some(q @ ('\'' | '"')) => {
            let q = *q;
            i += 1;
            while let Some(c) = chars.get(i) {
                i += 1;
                if *c == q {
                    break;
                }
                word.push(*c);
            }
        }
        _ => {
            while let Some(c) = chars.get(i) {
                if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')') {
                    break;
                }
                if *c != '\\' && *c != '\'' && *c != '"' {
                    word.push(*c);
                }
                i += 1;
            }
        }
    }
    (!word.is_empty()).then_some((word, strip, i))
}

/// Split a command the way the shell reads it. Before this, a heredoc's body
/// was read as more shell: an apostrophe in its text ("Alice's", in a Python
/// string) opened a quote that ran on into the next commands, a read of
/// `analysis.json` landed in one segment with a `sed -i`, and the gate held
/// that read as a write (Study 9).
fn split_shell(command: &str) -> Split {
    let chars: Vec<char> = command.chars().collect();
    let mut segments: Vec<String> = Vec::new();
    let mut bodies: Vec<(usize, String)> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    // heredocs opened on the line being read: (delimiter, strip tabs, segment)
    let mut pending: Vec<(String, bool, usize)> = Vec::new();
    let mut i = 0;
    let push = |segments: &mut Vec<String>, current: &mut String| segments.push(std::mem::take(current).trim().to_string());
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            current.push(c);
            if c == '\\' && q == '"' {
                if let Some(next) = chars.get(i + 1) {
                    current.push(*next);
                    i += 1;
                }
            } else if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                current.push(c);
            }
            '<' if chars.get(i + 1) == Some(&'<') => {
                if let Some((word, strip, end)) = heredoc_marker(&chars, i + 2) {
                    pending.push((word, strip, segments.len()));
                    current.extend(&chars[i..end]);
                    i = end;
                    continue;
                }
                current.push(c);
            }
            ';' => push(&mut segments, &mut current),
            '\n' => {
                push(&mut segments, &mut current);
                // the bodies of the heredocs this line opened, in order
                for (word, strip, owner) in std::mem::take(&mut pending) {
                    let mut body = String::new();
                    i += 1;
                    while i < chars.len() {
                        let end = chars[i..].iter().position(|c| *c == '\n').map(|p| i + p).unwrap_or(chars.len());
                        let line: String = chars[i..end].iter().collect();
                        let bare = if strip { line.trim_start_matches('\t') } else { line.as_str() };
                        i = end;
                        if bare.trim_end() == word {
                            break;
                        }
                        body.push_str(&line);
                        body.push('\n');
                        i += 1; // past the newline
                    }
                    // `i` now sits on the newline ending the delimiter line
                    bodies.push((owner, body));
                }
            }
            '|' | '&' if chars.get(i + 1) == Some(&c) => {
                i += 1;
                push(&mut segments, &mut current);
            }
            '|' => push(&mut segments, &mut current),
            _ => current.push(c),
        }
        i += 1;
    }
    push(&mut segments, &mut current);
    Split { segments, bodies }
}

/// Shell's own ways to write a file: a redirect that writes, an in-place
/// `sed`, a program whose job is writing files, or git rewriting the tree.
fn shell_write() -> &'static FancyRegex {
    static RE: OnceLock<FancyRegex> = OnceLock::new();
    RE.get_or_init(|| {
        FancyRegex::new(concat!(
            r"(?<![0-9&<])>>?(?!&)",
            r"|\bsed\b[^|;]*\s-[A-Za-z]*i",
            r"|\b(?:tee|cp|mv|touch|install|dd|truncate|patch|rsync|ln)\b",
            r"|\bgit\s+(?:apply|checkout|restore|stash|mv|rm|clean)\b",
        ))
        .expect("shell write pattern")
    })
}

/// A script (Python, Node, Ruby, Perl, PHP) that writes or moves a file:
/// an `open` with a writing mode, a `write_text`, a `writeFileSync`, a
/// rename. `json.load(open('analysis.json'))` reads.
fn script_writes(script: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r#"open\([^()]*,\s*(?:mode\s*=\s*)?['"](?:[rwa]\+|[wax])b?\+?['"]"#,
            r#"|,\s*['"](?:w|a|x|wb|ab|w\+|a\+|r\+)['"]\s*[,)]|\.open\(\s*['"](?:[wax]|r\+)"#,
            r#"|['"]>>?['"]"#,
            r"|\.write_(?:text|bytes)\(|\bwrite(?:File|FileSync)\(|\bappendFile(?:Sync)?\(",
            r"|\bos\.(?:remove|unlink|rename|replace|truncate)\(|\bshutil\.(?:copy\w*|move|rmtree)\(",
            r"|\.(?:unlink|rename|touch)\(|\b(?:File|IO)\.(?:write|open|rename|delete)\(",
            r"|\bfs\.(?:rm|unlink|rename|copyFile|truncate)\w*\(|\bunlink\s*\(|\brename\s*\(",
        ))
        .expect("script write pattern")
    })
    .is_match(script)
}

/// A segment's program, past `VAR=value` settings and `sudo`/`env`/`time`.
fn program_of(segment: &str) -> String {
    segment
        .split_whitespace()
        .find(|w| {
            let name = w.split('=').next().unwrap_or("");
            let setting = w.contains('=') && !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            !setting && !matches!(*w, "sudo" | "env" | "time" | "command" | "exec" | "nice")
        })
        .map(|w| w.rsplit('/').next().unwrap_or(w).to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_interpreter(program: &str) -> bool {
    let base = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    matches!(base, "python" | "py" | "node" | "deno" | "bun" | "perl" | "ruby" | "php")
}

fn is_shell(program: &str) -> bool {
    matches!(program, "bash" | "sh" | "zsh" | "dash" | "ksh")
}

/// The paths in one simple command that it may write, before it runs. The
/// shell's own writes name their targets; an inline script (`python -c`)
/// names them only when it writes at all; a program that is merely run
/// (`python3 analyze.py`, `./build.sh`, `python3 - <<EOF`) writes files that
/// appear nowhere on its line, so none of its words is taken for one.
fn segment_write_tokens(segment: &str) -> Vec<String> {
    if shell_write().is_match(segment).unwrap_or(false) {
        let named = tokens(segment);
        // `cp a b` (and install, rsync, ln) writes only its last path; the
        // sources are read. The staging demo's agent ran `cp daybook.py /tmp/x`
        // to diff it and was blocked on daybook.py. mv removes its sources,
        // so they stay gated.
        if copies_only_to_last(segment) && named.len() > 1 {
            return named[named.len() - 1..].to_vec();
        }
        return named;
    }
    let inline = Regex::new(r"\s-[ce]\s").map(|re| re.is_match(segment)).unwrap_or(false);
    if inline && is_interpreter(&program_of(segment)) && script_writes(segment) {
        return tokens(segment);
    }
    Vec::new()
}

/// Every path a simple command names (only its destination for a copy).
fn touch_tokens(segment: &str) -> Vec<String> {
    let named = tokens(segment);
    if copies_only_to_last(segment) && named.len() > 1 {
        return named[named.len() - 1..].to_vec();
    }
    named
}

/// A simple command that copies (cp, install, rsync, ln) and runs nothing else
/// that writes: only its last path is written.
fn copies_only_to_last(segment: &str) -> bool {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let Some(first) = words.iter().find(|w| !w.contains('=')) else { return false };
    let base = first.rsplit('/').next().unwrap_or(first);
    matches!(base, "cp" | "install" | "rsync" | "ln")
        && !segment.contains('>')
        && !segment.contains("<<")
}

/// A part of a command the gate never holds: it edits no one's code, it
/// moves files about or resolves a conflict. git itself (`git checkout
/// --ours/--theirs`, `git add`, `git rebase --continue/--abort`, `git merge`,
/// `git restore`, ...), `mv`, `cp`, `install`, `rsync`, `ln`, and an append
/// (`echo ... >> file`), which adds lines and changes none. Study 8 (second
/// study): an agent resolving its own rebase conflict was blocked on `git
/// checkout --theirs`, `mv` and `echo >>`, fifteen times in all. What these
/// do is still reported after it happens; a teammate's symbols they touch
/// reach the session as deltas.
pub fn moves_without_editing(segment: &str) -> bool {
    let words: Vec<&str> = segment.split_whitespace().collect();
    // `VAR=value` settings and `sudo`/`env`/`time` come first; the program is the word after
    let Some(at) = words.iter().position(|w| {
        let name = w.split('=').next().unwrap_or("");
        let setting = w.contains('=') && !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        !setting && !matches!(*w, "sudo" | "env" | "time" | "command" | "exec")
    }) else {
        return false;
    };
    let program = words[at].rsplit('/').next().unwrap_or(words[at]);
    let redirect_over = {
        // a `>` that truncates (not `>>`, not `2>&1`, not `>&2`)
        let bytes = segment.as_bytes();
        (0..bytes.len()).any(|i| {
            bytes[i] == b'>'
                && bytes.get(i + 1) != Some(&b'>')
                && (i == 0 || bytes[i - 1] != b'>')
                && bytes.get(i + 1) != Some(&b'&')
                && !(i > 0 && (bytes[i - 1] == b'2' || bytes[i - 1] == b'&'))
        })
    };
    match program {
        "git" => !redirect_over,
        "mv" | "cp" | "install" | "rsync" | "ln" => !redirect_over && !segment.contains("<<"),
        // an append adds lines and edits none
        _ => segment.contains(">>") && !redirect_over && !has_inplace_or_script(segment),
    }
}

fn has_inplace_or_script(segment: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\bsed\b[^|;]*\s-[A-Za-z]*i|\b(?:python[0-9.]*|py|node|perl|ruby|php)\b[^|;]*\s-[ce]\b|\btee\b|\btruncate\b|\bdd\b")
            .expect("in-place pattern")
    })
    .is_match(segment)
}

/// PreToolUse runs BEFORE the command, so unlike the reporter this cannot lean
/// on mtime — there is no evidence yet, only the command text. It gates the
/// conservative subset: tokens naming a file that already exists, in a part of
/// the command that itself writes. `cat a.py && python -m pytest` reads a.py;
/// only the part with the write idiom can name what is written. Anything it
/// misses is simply not blocked, which is the right direction for a gate whose
/// whole contract is to fail open.
pub fn shell_gate_paths(command: &str, cwd: &Path) -> Vec<PathBuf> {
    shell_paths(command, cwd, true)
}

/// Every existing file a command may write, moves and appends included:
/// what the journal copies before the command runs, so the lines it adds
/// are attributed to the session (`shell_gate_paths` leaves those out of the
/// gate only).
pub fn shell_touch_paths(command: &str, cwd: &Path) -> Vec<PathBuf> {
    shell_paths(command, cwd, false)
}

fn shell_paths(command: &str, cwd: &Path, gated_only: bool) -> Vec<PathBuf> {
    if command.is_empty() || !has_write_idiom(command) {
        return Vec::new();
    }
    let split = split_shell(command);
    let mut named: Vec<String> = Vec::new();
    for segment in &split.segments {
        if segment.is_empty() || !has_write_idiom(segment) || (gated_only && moves_without_editing(segment)) {
            continue;
        }
        // what the journal copies first stays broad (a run script's
        // arguments too); the gate holds only what the command writes
        named.extend(if gated_only { segment_write_tokens(segment) } else { touch_tokens(segment) });
    }
    // a heredoc fed to a shell is more shell; fed to Python, Node and the
    // like it is a script, whose paths count only when it writes; fed to
    // `cat > file` it is the file's text, never a path to write
    for (owner, body) in &split.bodies {
        let program = split.segments.get(*owner).map(|s| program_of(s)).unwrap_or_default();
        if is_shell(&program) {
            named.extend(shell_paths(body, cwd, gated_only).into_iter().map(|p| p.to_string_lossy().into_owned()));
        } else if is_interpreter(&program) && script_writes(body) {
            named.extend(tokens(body));
        }
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for token in named {
        if token.is_empty() || token.starts_with('-') {
            continue;
        }
        let candidate = if Path::new(&token).is_absolute() { PathBuf::from(&token) } else { cwd.join(&token) };
        let candidate = normpath(&candidate);
        if seen.contains(&candidate) {
            continue;
        }
        seen.push(candidate.clone());
        if candidate.is_file() {
            out.push(candidate);
            if out.len() >= MAX_SHELL_GATE_FILES {
                return out;
            }
        }
    }
    out
}

/// Repo-relative paths a Codex `apply_patch` touches, read from its headers —
/// Codex passes only the patch text, never a file_path.
pub fn apply_patch_paths(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in command.lines() {
        let stripped = line.trim();
        for marker in PATCH_MARKERS {
            if let Some(rest) = stripped.strip_prefix(marker) {
                let path = rest.trim();
                if !path.is_empty() && !out.iter().any(|p| p == path) {
                    out.push(path.to_string());
                }
                break;
            }
        }
    }
    out
}

pub fn abs_targets(paths: &[String], cwd: &Path) -> Vec<PathBuf> {
    paths.iter().map(|p| absolute(&cwd.join(p))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command the gate held in Study 9, verbatim but for its folders and
    /// a name: a Python heredoc that rewrites run9.py (with "Alice's" in a
    /// string), a
    /// `sed -i` on pilot.sh, and a second heredoc that only READS
    /// analysis.json. The apostrophe used to open a quote that ran into the
    /// next commands, so analysis.json shared a segment with the `sed -i`
    /// and was held as a write ("Another session of yours is changing
    /// test-repos/study9/analysis.json").
    const STUDY9_COMMAND: &str = r##"cd DIR && python3 - <<'EOF'
p='run9.py'; s=open(p).read()
old='''def harness_of(i):
    return "codex" if i % 4 == 3 else "claude"'''
new='''# Codex was planned for every 4th worker; Alice's ChatGPT account hit its Codex usage limit
# during the first pilot ("try again at Nov 15th, 2026"), so the study runs Claude Code only
# unless STUDY9_CODEX=1.
USE_CODEX = os.environ.get("STUDY9_CODEX") == "1"


def harness_of(i):
    return "codex" if USE_CODEX and i % 4 == 3 else "claude"'''
assert old in s; s=s.replace(old,new); open(p,'w').write(s)
EOF
/Users/alice/Documents/GitHub/collide/server/.venv/bin/python -c "import ast;ast.parse(open('run9.py').read())" && sed -i '' 's/collide p2 --window/collide p3 --window/; s/control p2 --window/control p3 --window/' pilot.sh && /usr/bin/grep run9 pilot.sh; python3 - <<'EOF'
import json
d=json.load(open('DIR/analysis.json'))
for r in d['runs']:
    if r['dir']=='n24-collide-p2':
        print('collide cost:', {k:v for k,v in r['collide_cost'].items() if k in ('carried','reaction','stall','tool_calls','tool_call_count','share_of_claude_tokens')})
        print('notices per claude task:', r['notice_counts_per_claude_task'])
        print('health:', r['health']); print('staging:', {k:r['staging'][k] for k in ('tokens','usd','tokens_match_provider','usd_diff','repo_listed','repo_listed_after')}, r['staging_changed'], r['staging_commit'])
    if r['dir']=='n24-control-p2': print('control health:', r['health'])
EOF"##;

    #[test]
    fn a_heredoc_that_only_reads_is_never_held_as_a_write() {
        let dir = std::env::temp_dir().join(format!("collide-study9-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["run9.py", "pilot.sh", "analysis.json"] {
            std::fs::write(dir.join(name), "x\n").unwrap();
        }
        let command = STUDY9_COMMAND.replace("DIR", &dir.to_string_lossy());
        let gated = shell_gate_paths(&command, &dir);
        assert!(!gated.contains(&dir.join("analysis.json")), "a read is not a write: {gated:?}");
        // what it does write is still held: the sed -i, and the heredoc that opens run9.py 'w'
        assert!(gated.contains(&dir.join("pilot.sh")), "{gated:?}");
        assert!(gated.contains(&dir.join("run9.py")), "{gated:?}");
        // the heredoc bodies are not shell: three commands before the second body, none inside it
        let segments = shell_segments(&command);
        assert!(segments.iter().all(|s| !s.contains("analysis.json")), "{segments:?}");
        assert!(segments.iter().any(|s| s.starts_with("sed -i")), "{segments:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_script_that_is_only_run_or_only_reads_names_no_write() {
        let dir = std::env::temp_dir().join(format!("collide-shell-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["analyze.py", "data.json", "out.json", "notes.md"] {
            std::fs::write(dir.join(name), "x\n").unwrap();
        }
        for read in ["python3 analyze.py data.json", "python -c \"import json; print(json.load(open('data.json')))\"",
                     "node -e \"console.log(require('fs').readFileSync('data.json','utf8'))\"",
                     "python3 - <<'EOF'\nprint(open('data.json').read())\nEOF", "cat <<EOF\ndata.json > out.json\nEOF"] {
            assert!(shell_gate_paths(read, &dir).is_empty(), "{read}: {:?}", shell_gate_paths(read, &dir));
        }
        assert_eq!(shell_gate_paths("python3 - <<'EOF'\nimport json\njson.dump({}, open('out.json', 'w'))\nEOF", &dir), vec![dir.join("out.json")]);
        assert_eq!(shell_gate_paths("bash <<'EOF'\necho hi > notes.md\nEOF", &dir), vec![dir.join("notes.md")]);
        assert_eq!(shell_gate_paths("cat > notes.md <<'EOF'\nAlice's data.json; out.json\nEOF", &dir), vec![dir.join("notes.md")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where a command works is named by `cd`, `git -C` or a path it acts
    /// on, never by a setting's value or Cork's own program: the landing
    /// command (`CORK_HOME='/x/home' /x/home/.collide/bin/collide land`)
    /// pointed into another checkout (Study 9).
    #[test]
    fn a_settings_value_or_collides_own_program_is_not_where_a_command_works() {
        let base = std::env::temp_dir().join(format!("collide-hint-{}", std::process::id()));
        let home = base.join("home");
        let app = base.join("app");
        std::fs::create_dir_all(home.join(".collide").join("bin")).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(home.join(".collide").join("bin").join("collide"), "").unwrap();
        let cwd = Path::new("/");
        let landing = format!("COLLIDE_HOME='{h}' '{h}/.collide/bin/collide' land --session s1", h = home.display());
        assert_eq!(dir_hint(&landing, cwd, "/nowhere"), None);
        assert_eq!(dir_hint(&format!("COLLIDE_HOME={} make", home.display()), cwd, "/nowhere"), None);
        assert_eq!(dir_hint(&format!("ls {}", app.display()), cwd, "/nowhere"), Some(app.clone()));
        assert_eq!(dir_hint(&format!("cd {} && make", app.display()), cwd, "/nowhere"), Some(app.clone()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Another repo's activity is not reported here: a cd/pushd out of the
    /// root, `git -C` at another tree, or a target that cannot be resolved.
    #[test]
    fn a_command_that_works_in_another_repo_is_not_ours() {
        let root = Path::new("/work/collide");
        let leaves = |command: &str| command_leaves_repo(command, root, root, "/home/me");
        // plain commands, and a cd to a subdirectory of the repo, stay ours
        assert!(!leaves("pytest -q"));
        assert!(!leaves("cd server && pytest"));
        assert!(!leaves("git -C . log"));
        // a relative hop out, an absolute other repo, and home belong elsewhere
        assert!(leaves("cd ../otherrepo && git log"));
        assert!(leaves("pushd /work/otherrepo && make"));
        assert!(leaves("cd ~ && ls"));
        assert!(leaves("git -C /work/otherrepo status"));
        // an unexpanded variable cannot be confirmed in-repo
        assert!(leaves("cd $OTHER_REPO && npm test"));
    }

    #[test]
    fn the_gate_only_gates_the_part_of_a_command_that_writes() {
        let dir = std::env::temp_dir().join(format!("collide-shell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fmt.py"), "x\n").unwrap();
        std::fs::write(dir.join("notes.md"), "x\n").unwrap();
        let read_then_test = "cat fmt.py && echo '--- a; b ---' && find . -o -iname x -print && python -m pytest -q";
        assert!(shell_gate_paths(read_then_test, &dir).is_empty());
        assert_eq!(shell_gate_paths("cat fmt.py | tee notes.md", &dir), vec![dir.join("notes.md")]);
        // a copy reads its source: only the destination is written
        assert!(shell_gate_paths("cp fmt.py /tmp/check.py && diff /tmp/check.py fmt.py", &dir).is_empty());
        // moving files about, resolving a conflict and appending edit no one's
        // code: never held (Study 8, second study: an agent resolving its own
        // rebase conflict was blocked on each of these)
        for free in ["cp /tmp/x notes.md", "mv /tmp/resolved.py fmt.py && echo MOVED_OK", "git checkout --theirs -- fmt.py && echo OK",
                     "git add fmt.py && git rebase --continue", "git merge --abort", "echo \"# test\" >> fmt.py && echo WROTE_OK"] {
            assert!(shell_gate_paths(free, &dir).is_empty(), "{free}");
        }
        // an overwrite still is a write
        assert_eq!(shell_gate_paths("echo x > fmt.py", &dir), vec![dir.join("fmt.py")]);
        assert_eq!(shell_gate_paths("sed -i '' 's/a/b/' fmt.py >> notes.md", &dir), vec![dir.join("fmt.py"), dir.join("notes.md")]);
        assert_eq!(shell_gate_paths("python -c \"x=1; open('fmt.py','w')\"", &dir), vec![dir.join("fmt.py")]);
        assert_eq!(shell_gate_paths("cat > notes.md <<'EOF'\nhi; there\nEOF", &dir), vec![dir.join("notes.md")]);
        assert_eq!(shell_segments("a 2>&1 | b && c || d; e"), vec!["a 2>&1", "b", "c", "d", "e"]);
        assert_eq!(shell_segments("python -c \"a; b | c\" && d"), vec!["python -c \"a; b | c\"", "d"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_command_made_wholly_of_reads_skips_the_snapshot() {
        for reads in [
            "git status --short && git log --oneline -5",
            "ls -la src | head",
            "grep -rn draft src | sort | uniq -c",
            "cd src && cat worker.ts",
            "git -C ../other diff HEAD~1 --stat",
            "find . -name '*.ts' | wc -l",
            "sed -n '1,40p' src/worker.ts",
            "git branch --show-current",
            "",
        ] {
            assert!(read_only(reads), "{reads}");
        }
        for writes in [
            "npm test",
            "tsx scripts/add-doc-note.ts",
            "npx tsx scripts/x.ts",
            "uv run python fix.py",
            "make gen",
            "echo hi > notes.txt",
            "cat a.txt | tee b.txt",
            "sed -i '' 's/a/b/' x.ts",
            "find . -name '*.orig' -delete",
            "git checkout -- src",
            "git stash",
            "git branch -D old",
            "git config user.name x",
            "grep -l x src | xargs rm",
            "./scripts/gen.sh",
            "bash <<'EOF'\necho hi\nEOF",
            "awk 'BEGIN { system(\"rm x\") }'",
            "curl -o out.json https://example.com",
        ] {
            assert!(!read_only(writes), "{writes}");
        }
    }

    #[test]
    fn a_command_names_a_file_on_its_line_or_in_the_script_it_runs() {
        let dir = std::env::temp_dir().join(format!("cork-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/note.ts"), "fs.appendFileSync('CLAUDE.md', 'x')").unwrap();
        std::fs::write(dir.join("scripts/gen.ts"), "fs.writeFileSync(['gen', 'erated.ts'].join(''))").unwrap();
        assert!(names_path("tsx scripts/note.ts", &dir, &dir, "CLAUDE.md"));
        assert!(names_path("cd scripts && tsx note.ts", &dir.join("scripts"), &dir, "CLAUDE.md"));
        assert!(names_path("sed -i '' s/a/b/ src/worker.ts", &dir, &dir, "src/worker.ts"));
        assert!(!names_path("tsx scripts/gen.ts", &dir, &dir, "src/generated.ts"));
        assert!(!names_path("npm test", &dir, &dir, "CLAUDE.md"));
        // a whole name only: a.py is not data.py, CLAUDE.md is not CLAUDE.md.bak
        assert!(!names_path("python data.py", &dir, &dir, "a.py"));
        assert!(!names_path("cp CLAUDE.md.bak /tmp", &dir, &dir, "CLAUDE.md"));
        assert!(names_path("edit \"CLAUDE.md\".", &dir, &dir, "CLAUDE.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
