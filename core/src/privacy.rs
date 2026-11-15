//! What leaves a machine, made smaller: absolute paths become repo-relative
//! (or `~`), and a shell command becomes its action and its repo-relative
//! targets (`pytest tests/test_api.py`, `git push`). Shared by the hooks,
//! which apply it before anything is sent, and the server, which applies it
//! again to whatever an older hook still sends.
//!
//! No regex: the server and the hooks both link this, and a command is
//! tokenized the way a shell would split it, quotes included.

/// A shortened command is never longer than this.
pub const SHORT_MAX: usize = 160;
/// At most this many arguments of one program are kept.
const MAX_ARGS: usize = 6;
/// An argument longer than this is a payload, not a target.
const MAX_ARG: usize = 60;
const ELLIPSIS: &str = "\u{2026}";

/// `text` with every absolute path under one of `roots` made relative to it
/// (the root itself becomes `.`), then the home directory written as `~`.
/// Roots are tried longest first, so a worktree inside the repo wins over
/// the repo. A path that merely shares a prefix (`/repo2` for `/repo`) is
/// left alone.
pub fn strip_paths(text: &str, roots: &[String], home: &str) -> String {
    let mut sorted: Vec<&str> = roots.iter().map(|r| r.trim_end_matches(['/', '\\'])).filter(|r| r.len() > 1).collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.len()));
    sorted.dedup();
    let mut out = text.to_string();
    for root in sorted {
        if out.contains(root) {
            out = replace_prefix(&out, root, "", ".");
        }
    }
    let home = home.trim_end_matches(['/', '\\']);
    if home.len() > 1 && out.contains(home) {
        out = replace_prefix(&out, home, "~/", "~");
    }
    out
}

/// Every occurrence of `prefix` that is a whole path component: followed by
/// a separator it becomes `with_sep` (the separator dropped), followed by
/// anything that ends a path it becomes `bare`. Anything else (`/repo2`) is
/// kept as it was.
fn replace_prefix(text: &str, prefix: &str, with_sep: &str, bare: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        let (before, after) = rest.split_at(at);
        let tail = &after[prefix.len()..];
        // a path starts the text or follows something that is not a path
        let starts = before.chars().next_back().is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '\\' | '~')));
        let next = tail.chars().next();
        out.push_str(before);
        match next {
            Some('/') | Some('\\') if starts => {
                out.push_str(with_sep);
                rest = &tail[1..];
            }
            None if starts => {
                out.push_str(bare);
                rest = tail;
            }
            Some(c) if starts && !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')) => {
                out.push_str(bare);
                rest = tail;
            }
            _ => {
                out.push_str(prefix);
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A home directory the server cannot know but can recognise:
/// `/Users/<name>/`, `/home/<name>/`, `C:\Users\<name>\` become `~/`. For
/// what an older hook still sends unstripped.
pub fn strip_home_like(text: &str) -> String {
    let mut out = text.to_string();
    for (marker, sep) in [("/Users/", '/'), ("/home/", '/'), (":\\Users\\", '\\')] {
        let mut from = 0;
        while let Some(found) = out[from..].find(marker) {
            let at = from + found;
            // the whole `C:` of a Windows path goes too
            let start = if sep == '\\' { at.saturating_sub(1) } else { at };
            // a path starts the text or follows something that is not a path
            // (`api.x.com/Users/7` is a URL's path, not a home)
            let starts = out[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '\\')));
            let name_end = out[at + marker.len()..].find(sep).filter(|n| *n > 0);
            match (starts, name_end) {
                (true, Some(n)) => {
                    out.replace_range(start..at + marker.len() + n + 1, "~/");
                    from = start + 2;
                }
                _ => from = at + marker.len(),
            }
        }
    }
    out
}

/// A token that is a credential by its shape: a known prefix, a JWT, or a
/// long run of mixed letters and digits with no separators a path or a
/// name would have.
pub fn looks_secret(token: &str) -> bool {
    const PREFIXES: [&str; 14] = [
        "sk-", "sk_live_", "sk_test_", "rk_live_", "rk_test_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "xox", "cat_", "AKIA",
    ];
    let t = token.trim_matches(|c| c == '"' || c == '\'');
    if PREFIXES.iter().any(|p| t.starts_with(p) && t.len() >= p.len() + 8) || (t.starts_with("AIza") && t.len() >= 30) {
        return true;
    }
    if t.starts_with("eyJ") && t.matches('.').count() == 2 && t.len() > 30 {
        return true;
    }
    if t.len() >= 24 && !t.contains('/') && !t.contains('.') {
        let digits = t.chars().filter(char::is_ascii_digit).count();
        let letters = t.chars().filter(char::is_ascii_alphabetic).count();
        let other = t.chars().filter(|c| !c.is_ascii_alphanumeric() && !matches!(c, '_' | '-' | '+' | '=')).count();
        return other == 0 && digits >= 4 && letters >= 4;
    }
    false
}

/// A shortened command: what the agent is doing (`running`, or
/// `searching` for a grep) and the target in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Short {
    pub action: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    /// `&&`, `||`, `;`, `&`, `|`
    Op(&'static str),
}

/// Heredoc bodies are input, never the command: the lines between a `<<WORD`
/// and its `WORD` are dropped, and what remains is one line.
fn without_heredocs(command: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut until: Option<String> = None;
    for line in command.lines() {
        if let Some(end) = &until {
            if line.trim() == end {
                until = None;
            }
            continue;
        }
        kept.push(line);
        if let Some(at) = line.find("<<") {
            let rest = line[at + 2..].trim_start_matches(['-', '<']).trim_start();
            let word: String = rest
                .trim_start_matches(['\'', '"'])
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            // `<<<` is a here-string: no body follows
            if !word.is_empty() && !line[at..].starts_with("<<<") {
                until = Some(word);
            }
        }
    }
    kept.join(" ; ")
}

fn lex(command: &str) -> Vec<Tok> {
    let chars: Vec<char> = command.chars().collect();
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut i = 0;
    let flush = |word: &mut String, in_word: &mut bool, out: &mut Vec<Tok>| {
        if *in_word {
            out.push(Tok::Word(std::mem::take(word)));
            *in_word = false;
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '\\' if i + 1 < chars.len() => {
                in_word = true;
                word.push(chars[i + 1]);
                i += 1;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                // a substitution is one word, however many spaces it holds
                in_word = true;
                let mut depth = 0;
                while i < chars.len() {
                    match chars[i] {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                word.push(')');
                                break;
                            }
                        }
                        _ => {}
                    }
                    word.push(chars[i]);
                    i += 1;
                }
            }
            c if c.is_whitespace() => flush(&mut word, &mut in_word, &mut out),
            '&' | '|' | ';' => {
                // `2>&1`'s ampersand belongs to the redirect
                if c == '&' && word.ends_with('>') {
                    word.push(c);
                    i += 1;
                    continue;
                }
                flush(&mut word, &mut in_word, &mut out);
                let next = chars.get(i + 1).copied();
                let op = match (c, next) {
                    ('&', Some('&')) => { i += 1; "&&" }
                    ('|', Some('|')) => { i += 1; "||" }
                    ('|', Some('&')) => { i += 1; "|" }
                    ('&', Some('>')) => {
                        // `&>file`: a redirect
                        word.push('&');
                        in_word = true;
                        i += 1;
                        continue;
                    }
                    ('&', _) => "&",
                    ('|', _) => "|",
                    _ => ";",
                };
                out.push(Tok::Op(op));
            }
            '(' | ')' => flush(&mut word, &mut in_word, &mut out),
            _ => {
                in_word = true;
                word.push(c);
            }
        }
        i += 1;
    }
    flush(&mut word, &mut in_word, &mut out);
    out
}

/// `>file`, `2>&1`, `<in`, `<<EOF`, `&>log`: a redirect, and whether its
/// target is the next word.
fn redirect(word: &str) -> Option<bool> {
    let body = word.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&');
    if !(body.starts_with('>') || body.starts_with('<')) {
        return None;
    }
    // `2>&1`, `>&-`: a duplication, no target; `>out.log`: attached
    Some(body.trim_start_matches(['>', '<']).is_empty())
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else { return false };
    !name.is_empty() && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn program_name(word: &str) -> String {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    base.trim_end_matches(".exe").to_string()
}

const WRAPPERS: [&str; 9] = ["sudo", "env", "time", "nohup", "exec", "command", "builtin", "nice", "stdbuf"];
const INTERPRETERS: [&str; 14] = ["python", "python3", "python2", "node", "ruby", "perl", "bash", "sh", "zsh", "deno", "bun", "php", "pwsh", "powershell"];
const INLINE_FLAGS: [&str; 7] = ["-c", "-e", "--eval", "-p", "--print", "-r", "-Command"];
const SEARCHERS: [&str; 6] = ["grep", "egrep", "fgrep", "rg", "ag", "ack"];
const SECRET_NAMES: [&str; 8] = ["token", "secret", "password", "passwd", "key", "auth", "credential", "pass"];

/// A URL without its credentials, query or fragment.
fn bare_url(arg: &str) -> String {
    let Some((scheme, rest)) = arg.split_once("://") else { return arg.to_string() };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (host_part, path) = match rest.find('/') {
        Some(n) => (&rest[..n], &rest[n..]),
        None => (rest, ""),
    };
    let host = host_part.rsplit('@').next().unwrap_or(host_part);
    format!("{scheme}://{host}{path}")
}

/// One argument as it may travel, or None to drop it.
fn shown_arg(arg: &str) -> String {
    if arg.is_empty() {
        return String::new();
    }
    if arg.contains(char::is_whitespace) || arg.contains('\n') {
        return ELLIPSIS.to_string(); // a message, a script, a query: payload
    }
    if let Some((name, value)) = arg.strip_prefix('-').and_then(|a| a.split_once('=')) {
        let lowered = name.to_ascii_lowercase();
        if SECRET_NAMES.iter().any(|s| lowered.contains(s)) || looks_secret(value) || value.len() > 40 {
            return format!("-{name}={ELLIPSIS}");
        }
        if value.contains("://") {
            return format!("-{name}={}", bare_url(value));
        }
        return arg.to_string();
    }
    if looks_secret(arg) {
        return ELLIPSIS.to_string();
    }
    // `-e NAME=value`: the name may travel, the value is payload
    if is_assignment(arg) {
        let name = arg.split_once('=').map(|(n, _)| n).unwrap_or(arg);
        return format!("{name}={ELLIPSIS}");
    }
    if arg.contains("://") {
        let url = bare_url(arg);
        return if url.chars().count() > MAX_ARG { ELLIPSIS.to_string() } else { url };
    }
    if arg.starts_with('/') || (arg.len() > 2 && arg.as_bytes()[1] == b':' && arg.contains('\\')) {
        // an absolute path outside the repo and home: only its name
        let name = arg.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or("");
        return if name.is_empty() { "/".to_string() } else { format!("{ELLIPSIS}/{name}") };
    }
    if arg.chars().count() > MAX_ARG {
        return ELLIPSIS.to_string();
    }
    arg.to_string()
}

/// The pattern a grep searches for: `-e PAT`, or its first operand.
fn search_pattern(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-e" || a == "--regexp" {
            return args.get(i + 1).cloned();
        }
        if let Some(p) = a.strip_prefix("--regexp=") {
            return Some(p.to_string());
        }
        if a == "--" {
            return args.get(i + 1).cloned();
        }
        if a.starts_with('-') {
            // flags that take a value: skip it
            if matches!(a.as_str(), "-A" | "-B" | "-C" | "-m" | "-g" | "--glob" | "-t" | "--type" | "--include" | "--exclude" | "-f" | "--max-count") {
                i += 1;
            }
            i += 1;
            continue;
        }
        return Some(a.clone());
    }
    None
}

/// One simple command (no operators) as it is shown; `first`: the head of a
/// pipeline, whose arguments travel (a later stage shows its name only).
fn shown_simple(words: &[String], first: bool) -> Option<(String, Option<String>)> {
    // redirects and their targets go
    let mut kept: Vec<String> = Vec::new();
    let mut skip_next = false;
    for w in words.iter().cloned() {
        if skip_next {
            skip_next = false;
            continue;
        }
        match redirect(&w) {
            Some(takes_next) => skip_next = takes_next,
            None => kept.push(w),
        }
    }
    // leading assignments and wrappers
    let mut start = 0;
    let mut wrapped = false;
    while start < kept.len() {
        let w = kept[start].as_str();
        if WRAPPERS.contains(&w) {
            wrapped = true;
        } else if !(is_assignment(w) || (wrapped && w.starts_with('-'))) {
            break;
        }
        start += 1;
    }
    let words = &kept[start..];
    let program_word = words.first()?;
    let program = program_name(program_word);
    if program.is_empty() {
        return None;
    }
    let args: Vec<String> = words[1..].to_vec();
    if !first {
        return Some((program, None));
    }
    // a search: its pattern is the target
    let git_grep = program == "git" && args.first().map(String::as_str) == Some("grep");
    if SEARCHERS.contains(&program.as_str()) || git_grep {
        let rest = if git_grep { &args[1..] } else { &args[..] };
        if let Some(pattern) = search_pattern(rest) {
            let pattern: String = pattern.chars().take(80).collect();
            if !looks_secret(&pattern) && !pattern.is_empty() {
                return Some((program, Some(pattern)));
            }
        }
    }
    let interpreter = INTERPRETERS.contains(&program.as_str()) || program.starts_with("python3.");
    let mut out = vec![program.clone()];
    let mut shown = 0;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if interpreter && INLINE_FLAGS.contains(&a.as_str()) {
            out.push(a.clone());
            out.push(ELLIPSIS.to_string());
            break; // the script and its arguments: payload
        }
        // a flag whose value is a credential, by its name
        let lowered = a.to_ascii_lowercase();
        if a.starts_with('-') && !a.contains('=') && SECRET_NAMES.iter().any(|s| lowered.contains(s)) {
            out.push(a.clone());
            out.push(ELLIPSIS.to_string());
            i += 2;
            continue;
        }
        if shown >= MAX_ARGS {
            out.push(ELLIPSIS.to_string());
            break;
        }
        let s = shown_arg(a);
        if !s.is_empty() {
            out.push(s);
            shown += 1;
        }
        i += 1;
    }
    out.dedup_by(|b, a| a == ELLIPSIS && b == ELLIPSIS);
    Some((out.join(" "), None))
}

/// A command as the briefing and the dashboard show it: the programs it
/// runs, with the first program's flags and repo-relative targets; no
/// environment, no secrets, no inline scripts, no heredoc bodies, nothing
/// after a pipe but program names. A search (`grep -rn compute_tax src/`)
/// is `searching` its pattern. Shortening a shortened command changes
/// nothing.
pub fn shorten_command(command: &str) -> Short {
    let flat = without_heredocs(command);
    let tokens = lex(&flat);
    // pipelines joined by && || ; &, each a list of simple commands
    let mut pipelines: Vec<(Vec<Vec<String>>, &'static str)> = vec![(vec![Vec::new()], "")];
    for tok in tokens {
        match tok {
            Tok::Word(w) => pipelines.last_mut().unwrap().0.last_mut().unwrap().push(w),
            Tok::Op("|") => pipelines.last_mut().unwrap().0.push(Vec::new()),
            Tok::Op(op) => pipelines.push((vec![Vec::new()], op)),
        }
    }
    let mut parts: Vec<(String, &'static str)> = Vec::new();
    let mut search: Option<String> = None;
    let mut navigation: Vec<String> = Vec::new();
    for (pipeline, joined_by) in &pipelines {
        let mut stages: Vec<String> = Vec::new();
        for (n, simple) in pipeline.iter().enumerate() {
            let Some((shown, pattern)) = shown_simple(simple, n == 0) else { continue };
            let name = shown.split(' ').next().unwrap_or("").to_string();
            // moving around and setting variables is not the work
            if n == 0 && pipeline.len() == 1 && matches!(name.as_str(), "cd" | "pushd" | "popd" | "export" | "set" | "unset" | "source" | ".") {
                navigation.push(name);
                continue;
            }
            if let Some(p) = pattern {
                if search.is_none() && parts.is_empty() && stages.is_empty() {
                    search = Some(p);
                }
                stages.push(name);
                continue;
            }
            stages.push(shown);
        }
        if !stages.is_empty() {
            parts.push((stages.join(" | "), joined_by));
        }
    }
    if let Some(pattern) = search.filter(|_| parts.len() <= 1) {
        return Short { action: "searching", text: format!("\"{pattern}\"") };
    }
    let mut text = String::new();
    for (i, (shown, op)) in parts.iter().enumerate() {
        if i > 0 {
            match *op {
                "" | ";" => text.push_str("; "),
                op => text.push_str(&format!(" {op} ")),
            }
        }
        text.push_str(shown);
    }
    if text.is_empty() {
        text = navigation.first().cloned().unwrap_or_default();
    }
    if text.chars().count() > SHORT_MAX {
        text = format!("{}{ELLIPSIS}", text.chars().take(SHORT_MAX - 1).collect::<String>());
    }
    Short { action: "running", text }
}

/// A gist is never longer than this.
pub const GIST_MAX: usize = 60;
/// Programs whose first operand says what they do (`cargo test`, `git push`).
const SUBCOMMANDED: [&str; 36] = [
    "cargo", "git", "npm", "pnpm", "yarn", "bun", "deno", "go", "docker", "kubectl", "gh", "uv", "pip", "pip3", "poetry",
    "railway", "brew", "rustup", "terraform", "make", "just", "dotnet", "mvn", "gradle", "swift", "helm", "fly", "flyctl",
    "vercel", "wrangler", "firebase", "gcloud", "aws", "systemctl", "collide", "next",
];
/// Programs that run another one named next (`npx vitest`).
const RUNNERS: [&str; 4] = ["npx", "bunx", "pnpx", "uvx"];

/// What an agent is doing in a few words, for a line told to other agents:
/// the action and the program (`running cargo test`, `running pytest`,
/// `running npm run build`, `searching`). Never a target, a flag, a pattern
/// or a payload, and never more than `GIST_MAX` characters. A raw command
/// and a shortened one give the same gist.
pub fn gist(action: &str, command: &str) -> String {
    if action == "searching" {
        return "searching".to_string();
    }
    let short = shorten_command(command);
    if short.action == "searching" {
        return "searching".to_string();
    }
    // the first program of the first pipeline
    let first = short.text.split([';', '|', '&']).map(str::trim).find(|part| !part.is_empty()).unwrap_or("");
    let words: Vec<&str> = first.split_whitespace().filter(|w| *w != ELLIPSIS).collect();
    let Some(&head) = words.first() else { return "running a command".to_string() };
    fn word_like(w: &&str) -> bool {
        !w.starts_with('-') && w.chars().count() <= 24 && w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
    }
    let (mut program, mut rest) = (head, &words[1..]);
    let interpreter = INTERPRETERS.contains(&program) || program.starts_with("python3.");
    if interpreter && rest.first() == Some(&"-m") && rest.get(1).is_some_and(|w| word_like(&w)) {
        program = rest[1];
        rest = &rest[2..];
    } else if RUNNERS.contains(&program) && rest.first().is_some_and(|w| word_like(&w)) {
        program = rest[0];
        rest = &rest[1..];
    }
    let mut out = format!("running {program}");
    if SUBCOMMANDED.contains(&program) {
        if let Some(sub) = rest.first().filter(|w| word_like(w)) {
            out.push(' ');
            out.push_str(sub);
            if matches!(program, "npm" | "pnpm" | "yarn" | "bun") && *sub == "run" {
                if let Some(script) = rest.get(1).filter(|w| word_like(w)) {
                    out.push(' ');
                    out.push_str(script);
                }
            }
        }
    }
    if out.chars().count() > GIST_MAX {
        out = format!("{}{ELLIPSIS}", out.chars().take(GIST_MAX - 1).collect::<String>());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(c: &str) -> String {
        shorten_command(c).text
    }

    #[test]
    fn paths_become_repo_relative_and_home_becomes_a_tilde() {
        let roots = vec!["/Users/kim/code/app".to_string(), "/Users/kim/code/app/.claude/worktrees/x".to_string()];
        let home = "/Users/kim";
        assert_eq!(strip_paths("cd /Users/kim/code/app && pytest /Users/kim/code/app/tests/a.py", &roots, home), "cd . && pytest tests/a.py");
        assert_eq!(strip_paths("/Users/kim/code/app/.claude/worktrees/x/src/m.rs", &roots, home), "src/m.rs");
        assert_eq!(strip_paths("cat /Users/kim/.zshrc", &roots, home), "cat ~/.zshrc");
        assert_eq!(strip_paths("ls /Users/kim/code/app2/x", &roots, home), "ls ~/code/app2/x", "a sibling is not the repo");
        assert_eq!(strip_paths("error at /Users/kim/code/app/src/a.py:12", &roots, home), "error at src/a.py:12");
        assert_eq!(strip_paths("\"/Users/kim/code/app\"", &roots, home), "\".\"");
        assert_eq!(strip_paths("nothing here", &roots, home), "nothing here");
        assert_eq!(strip_paths("/Users/kimberly/x", &roots, home), "/Users/kimberly/x", "another user's home is not this one");
    }

    #[test]
    fn a_home_the_server_cannot_know_is_still_recognised() {
        assert_eq!(strip_home_like("cd /Users/kim/code/app && ls"), "cd ~/code/app && ls");
        assert_eq!(strip_home_like("/home/dev/x and /home/ops/y"), "~/x and ~/y");
        assert_eq!(strip_home_like("C:\\Users\\kim\\repo\\a.py"), "~/repo\\a.py");
        assert_eq!(strip_home_like("pytest tests/x.py"), "pytest tests/x.py");
        assert_eq!(strip_home_like("curl https://api.x.com/Users/7/a"), "curl https://api.x.com/Users/7/a");
    }

    #[test]
    fn commands_shorten_to_the_action_and_its_targets() {
        let cases = [
            ("pytest tests/test_api.py", "pytest tests/test_api.py"),
            ("cd server && CORK_NATIVE_INSTALL=0 .venv/bin/python -m pytest -q tests/test_api.py 2>&1 | tail -30", "python -m pytest -q tests/test_api.py | tail"),
            ("git push", "git push"),
            ("git push origin HEAD:dev 2>&1 | tail -5", "git push origin HEAD:dev | tail"),
            ("STRIPE_KEY=sk_live_abcdef1234567890 npm run seed", "npm run seed"),
            ("export OPENAI_API_KEY='abc123'; python x.py", "python x.py"),
            ("curl -H \"Authorization: Bearer ghp_0123456789abcdefghijABCDEFGHIJ\" https://api.x.com/v1/items?token=abc", "curl -H \u{2026} https://api.x.com/v1/items"),
            ("git clone https://kevin:hunter22@github.com/a/b", "git clone https://github.com/a/b"),
            ("psql --password s3cret -h db", "psql --password \u{2026} -h db"),
            ("mysql --password=s3cret -u root", "mysql --password=\u{2026} -u root"),
            ("python3 -c 'import os; print(os.environ[\"SECRET\"])'", "python3 -c \u{2026}"),
            ("node -e \"require('fs').writeFileSync('x', 1)\"", "node -e \u{2026}"),
            ("cat > notes.md <<'EOF'\nthe secret plan\nline two\nEOF\ngit add notes.md", "cat; git add notes.md"),
            ("git commit -m \"fix the leak in billing\"", "git commit -m \u{2026}"),
            ("cargo test --release -- brief::", "cargo test --release -- brief::"),
            ("ls /private/var/folders/xy/T/scratch", "ls \u{2026}/scratch"),
            ("docker run -e AWS_SECRET_ACCESS_KEY=AKIAABCDEFGHIJKLMNOP img", "docker run -e AWS_SECRET_ACCESS_KEY=\u{2026} img"),
            ("sudo -E env FOO=1 make install", "make install"),
            ("npm test", "npm test"),
            ("echo $(cat /etc/hosts) | wc -l", "echo \u{2026} | wc"),
            ("cd /tmp", "cd"),
        ];
        for (raw, want) in cases {
            assert_eq!(short(raw), want, "{raw}");
        }
    }

    #[test]
    fn a_search_is_its_pattern() {
        assert_eq!(shorten_command("grep -rn compute_tax src/"), Short { action: "searching", text: "\"compute_tax\"".into() });
        assert_eq!(shorten_command("rg -n 'def compute_tax' --type py"), Short { action: "searching", text: "\"def compute_tax\"".into() });
        assert_eq!(shorten_command("git grep -e rate_for -- '*.py'"), Short { action: "searching", text: "\"rate_for\"".into() });
        assert_eq!(shorten_command("cd app && grep -n total cart.py | head"), Short { action: "searching", text: "\"total\"".into() });
        // grep after a pipe is a filter, not the search
        assert_eq!(shorten_command("cargo test 2>&1 | grep FAILED").text, "cargo test | grep");
    }

    #[test]
    fn shortening_twice_changes_nothing() {
        for raw in ["cd x && FOO=1 python -m pytest -q tests/a.py 2>&1 | tail -3", "python3 -c 'print(1)'", "git commit -m \"x y\"",
                    "curl https://a:b@h.io/p?q=1", "ls /tmp/a/b", "git push", "make -j8 all && ./run.sh --flag"] {
            let once = short(raw);
            assert_eq!(short(&once), once, "{raw}");
        }
    }

    #[test]
    fn a_long_command_is_capped() {
        let long = format!("make {}", (0..80).map(|i| format!("t{i}")).collect::<Vec<_>>().join(" && make "));
        assert!(short(&long).chars().count() <= SHORT_MAX);
    }

    #[test]
    fn credentials_are_recognised_by_their_shape() {
        for s in ["sk-abcdefghijklmnopqrstu", "ghp_0123456789abcdefghij", "AKIAABCDEFGHIJKLMNOP", "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijk",
                  "a8f3k29dk3m4n5b6v7c8x9z0q1w2"] {
            assert!(looks_secret(s), "{s}");
        }
        for s in ["tests/test_api.py", "compute_tax", "--release", "HEAD:dev", "server/cork-hooks/src/report.rs", "abcdefghijklmnopqrstuvwxyz"] {
            assert!(!looks_secret(s), "{s}");
        }
    }

    #[test]
    fn a_gist_is_the_action_and_the_program() {
        let cases = [
            ("cargo test --release -- brief::", "running cargo test"),
            ("cd server && CORK_NATIVE_INSTALL=0 .venv/bin/python -m pytest -q tests/test_api.py 2>&1 | tail -30", "running pytest"),
            ("git push origin HEAD:dev 2>&1 | tail -5", "running git push"),
            ("npm run build -- --webpack", "running npm run build"),
            ("npx tsc --noEmit", "running tsc"),
            ("python3 -c 'import os; print(1)'", "running python3"),
            ("STRIPE_KEY=sk_live_abcdef1234567890 make -j8 test", "running make"),
            ("make test", "running make test"),
            ("ls /private/var/folders/xy/T/scratch", "running ls"),
            ("git commit -m \"fix the leak in billing\"", "running git commit"),
            ("", "running a command"),
        ];
        for (raw, want) in cases {
            assert_eq!(gist("running", raw), want, "{raw}");
            // what an older server stored, already shortened, gives the same
            assert_eq!(gist("running", &short(raw)), want, "{raw} shortened");
        }
        assert_eq!(gist("running", "grep -rn compute_tax src/"), "searching");
        assert_eq!(gist("searching", "\"compute_tax\""), "searching");
        let long = format!("{} --flag", "x".repeat(200));
        assert!(gist("running", &long).chars().count() <= GIST_MAX);
    }
}
