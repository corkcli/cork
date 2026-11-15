//! The write journal: which session wrote which lines of an uncommitted file.
//!
//! Several sessions in one checkout share one git index, and `git add
//! <file>` stages the whole file: one session's commit carried another's
//! half-done edit in the same file. The hooks already see every write a
//! session makes, so each write is journaled here, on this machine only:
//! the file, the session, the content hashes before and after, and the
//! changed lines' hashes, hunk by hunk. Nothing of it is ever sent.
//!
//! `cork commit --mine` diffs the tree against HEAD, gives each hunk to
//! the session whose journal holds its lines, and stages only this
//! session's hunks (a hunk with someone else's lines in it is reported, not
//! guessed at). `cork land` and the commit guard read the same answer.
//!
//! The "before" of a write is, best first: the harness's own copy of the
//! original file (Claude Code's `originalFile`), the copy the gate took just
//! before the tool ran, the last journaled state of the file, HEAD.
//!
//! Lines are matched by content (a hash of the line's bytes), not by
//! position, so the per-write diffs and the commit-time diff need not group
//! lines the same way. Fail open: any error records nothing.
//!
//! Layout, per checkout: `~/.collide/journal/<checkout>/log.jsonl` (one entry
//! per write), `snap/<path>` (the file as the last journaled write left it),
//! `pre/<session>-<path>` (the gate's copy). Entries expire after a week, or
//! once a commit has taken (or a revert undone) every line they name.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::{self, Env};

/// Larger files are not journaled (the reporter skips them too).
pub(crate) const MAX_BYTES: usize = 512 * 1024;
/// Edit distance the hook will trace before calling the change one hunk.
const HOOK_MAX_D: usize = 400;
const CLI_MAX_D: usize = 5000;
/// Changed lines kept per write; beyond it the rest go unclaimed.
const MAX_LINES_KEPT: usize = 5000;
const LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;
const EXPIRE_S: f64 = 7.0 * 86400.0;
/// How recently a session must have written to count as at work.
pub const LIVE_S: f64 = 12.0 * 3600.0;
/// The gate's copy is the before of the write that follows it, not of one later.
const PRE_FRESH_S: f64 = 600.0;
const HOOK_GIT_TIMEOUT: Duration = Duration::from_millis(300);

pub(crate) fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub(crate) fn sha16(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// A line's identity: its exact bytes, terminator included (so a last line
/// that gains a newline is a changed line, and splicing never joins two).
fn line_key(line: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in line {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash & 0xffff_ffff_ffff
}

fn hex(key: u64) -> String {
    format!("{key:012x}")
}

fn unhex(text: &str) -> Option<u64> {
    u64::from_str_radix(text, 16).ok()
}

/// Lines with their terminators; a last line without one is still a line.
pub fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            out.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}

pub(crate) fn keys(lines: &[&[u8]]) -> Vec<u64> {
    lines.iter().map(|l| line_key(l)).collect()
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

// ------------------------------------------------------------ the line diff

/// One run of changed lines: `ol` lines at `os` in the old file became `nl`
/// lines at `ns` in the new one (0-based).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub os: usize,
    pub ol: usize,
    pub ns: usize,
    pub nl: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Del,
    Ins,
}

/// The changed runs between two files' lines. The common head and tail are
/// cut first (an edit is almost always small), then Myers' diff runs on the
/// middle; past `max_d` edits the middle is one hunk.
pub fn diff(a: &[u64], b: &[u64], max_d: usize) -> Vec<Hunk> {
    let mut p = 0;
    while p < a.len() && p < b.len() && a[p] == b[p] {
        p += 1;
    }
    let mut s = 0;
    while s < a.len() - p && s < b.len() - p && a[a.len() - 1 - s] == b[b.len() - 1 - s] {
        s += 1;
    }
    let (am, bm) = (&a[p..a.len() - s], &b[p..b.len() - s]);
    if am.is_empty() && bm.is_empty() {
        return Vec::new();
    }
    let whole = vec![Hunk { os: p, ol: am.len(), ns: p, nl: bm.len() }];
    let Some(ops) = myers(am, bm, max_d) else { return whole };
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    let (mut i, mut j) = (p, p);
    for op in ops {
        match op {
            Op::Eq => {
                if let Some(h) = current.take() {
                    hunks.push(h);
                }
                i += 1;
                j += 1;
            }
            Op::Del => {
                current.get_or_insert(Hunk { os: i, ol: 0, ns: j, nl: 0 }).ol += 1;
                i += 1;
            }
            Op::Ins => {
                current.get_or_insert(Hunk { os: i, ol: 0, ns: j, nl: 0 }).nl += 1;
                j += 1;
            }
        }
    }
    if let Some(h) = current {
        hunks.push(h);
    }
    // a trace that does not add up is never trusted
    if i != a.len() - s || j != b.len() - s {
        return whole;
    }
    hunks
}

fn myers(a: &[u64], b: &[u64], max_d: usize) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    if n == 0 || m == 0 {
        let mut ops = vec![Op::Del; a.len()];
        ops.extend(vec![Op::Ins; b.len()]);
        return Some(ops);
    }
    let max = (n + m) as usize;
    let off = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    // trace[d]: v over k in -d-1..=d+1 as iteration d found it
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=(max.min(max_d) as isize) {
        trace.push(v[(off - d - 1) as usize..=(off + d + 1) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let idx = (off + k) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) { v[idx + 1] } else { v[idx - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m));
            }
            k += 2;
        }
    }
    None
}

fn backtrack(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<Op> {
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..trace.len()).rev() {
        let di = d as isize;
        let get = |k: isize| trace[d][(k + di + 1) as usize];
        let k = x - y;
        let prev_k = if k == -di || (k != di && get(k - 1) < get(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = get(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Eq);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            ops.push(if x == prev_x { Op::Ins } else { Op::Del });
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    ops
}

// ------------------------------------------------------------ storage

fn checkout_key(root: &Path) -> String {
    let real = std::fs::canonicalize(root).unwrap_or_else(|_| config::absolute(root));
    sha16(real.to_string_lossy().as_bytes())
}

pub(crate) fn journal_dir(root: &Path, env: &Env) -> PathBuf {
    crate::config::data_dir(env).join("journal").join(checkout_key(root))
}

pub(crate) fn path_key(rel: &str) -> String {
    sha16(rel.as_bytes())
}

pub(crate) fn session_tag(session: &str) -> String {
    session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(36).collect()
}

/// The gate's copy is the writing agent's: two helpers of one session
/// writing one file each keep their own "before".
fn agent_tag(session: &str, worker: &str) -> String {
    let tag = session_tag(session);
    let worker: String = worker.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(48).collect();
    if tag.is_empty() || worker.is_empty() { tag } else { format!("{tag}-{worker}") }
}

/// Whether two ways of naming a session name the same one: the full id, or
/// the eight-character tag the hooks show.
pub fn same_session(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || (a.len().min(b.len()) >= 8 && (a.starts_with(b) || b.starts_with(a)))
}

/// One journaled write.
#[derive(Clone, Debug)]
pub struct Entry {
    pub t: f64,
    pub session: String,
    /// The helper of the session that wrote it ("" for the session itself),
    /// and the name it was started with: whose lines they are, to the
    /// session's other agents.
    pub worker: String,
    pub worker_name: String,
    pub title: String,
    pub path: String,
    pub after: String,
    pub binary: bool,
    /// (lines removed, lines added), by key
    pub hunks: Vec<(Vec<u64>, Vec<u64>)>,
}

pub(crate) fn parse_entry(line: &str) -> Option<Entry> {
    let v: Value = serde_json::from_str(line).ok()?;
    let keys_of = |v: &Value| -> Vec<u64> {
        v.as_array().map(|a| a.iter().filter_map(|h| h.as_str().and_then(unhex)).collect()).unwrap_or_default()
    };
    let hunks = v.get("h").and_then(Value::as_array).map(|a| {
        a.iter().filter_map(|h| {
            let h = h.as_array()?;
            Some((keys_of(h.get(4)?), keys_of(h.get(5)?)))
        }).collect()
    }).unwrap_or_default();
    Some(Entry {
        t: v.get("t").and_then(Value::as_f64)?,
        session: v.get("s").and_then(Value::as_str)?.to_string(),
        worker: v.get("w").and_then(Value::as_str).unwrap_or("").to_string(),
        worker_name: v.get("wn").and_then(Value::as_str).unwrap_or("").to_string(),
        title: v.get("n").and_then(Value::as_str).unwrap_or("").to_string(),
        path: v.get("p").and_then(Value::as_str)?.to_string(),
        after: v.get("a").and_then(Value::as_str).unwrap_or("").to_string(),
        binary: v.get("bin").and_then(Value::as_bool).unwrap_or(false),
        hunks,
    })
}

/// Every unexpired entry for this checkout, oldest first.
pub fn load(root: &Path, env: &Env) -> Vec<Entry> {
    let body = std::fs::read_to_string(journal_dir(root, env).join("log.jsonl")).unwrap_or_default();
    let cutoff = now_s() - EXPIRE_S;
    body.lines().filter_map(parse_entry).filter(|e| e.t >= cutoff).collect()
}

/// The gate, just before a write: a copy of the file as it is, the exact
/// before of the write that follows (by this agent: the session, or the
/// helper `worker` of it). Cheap and silent.
pub fn note_before(root: &Path, target: &Path, session: &str, worker: &str, env: &Env) {
    let tag = agent_tag(session, worker);
    let Some(rel) = config::rel_path(target, root) else { return };
    if tag.is_empty() || rel == "." {
        return;
    }
    let dir = journal_dir(root, env).join("pre");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let base = dir.join(format!("{tag}-{}", path_key(&rel)));
    let gone = base.with_extension("none");
    match std::fs::metadata(target) {
        Ok(meta) if meta.is_file() && meta.len() as usize <= MAX_BYTES => {
            if let Ok(bytes) = std::fs::read(target) {
                let _ = std::fs::remove_file(&gone);
                let _ = std::fs::write(&base, bytes);
            }
        }
        Err(_) => {
            let _ = std::fs::remove_file(&base);
            let _ = std::fs::write(&gone, b"");
        }
        _ => {}
    }
}

fn fresh(path: &Path) -> bool {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs_f64() <= PRE_FRESH_S)
}

/// The gate's copy for this session and file, taken (it is used once).
fn take_before(dir: &Path, tag: &str, rel: &str) -> Option<Vec<u8>> {
    let base = dir.join("pre").join(format!("{tag}-{}", path_key(rel)));
    let gone = base.with_extension("none");
    let found = if fresh(&gone) {
        Some(Vec::new())
    } else if fresh(&base) {
        std::fs::read(&base).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&base);
    let _ = std::fs::remove_file(&gone);
    found
}

/// The file at HEAD (empty when HEAD has none), as bytes, bounded.
pub(crate) fn head_bytes(root: &Path, rel: &str, timeout: Duration) -> Option<Vec<u8>> {
    let spec = format!("HEAD:{rel}");
    match git_bytes(root, &["cat-file", "-e", &spec], None, &[], timeout) {
        Some(_) => git_bytes(root, &["cat-file", "blob", &spec], None, &[], timeout),
        None => Some(Vec::new()),
    }
}

/// The report hook, after a write: journal what this session changed in
/// `target`, and which of its agents did (`worker`, a helper, named
/// `worker_name`; "" for the session itself). `original` is the harness's
/// own copy of the file before the write, when it gave one. Never fails the
/// write: any error records nothing.
#[allow(clippy::too_many_arguments)]
pub fn record(
    root: &Path, target: &Path, rel: &str, session: &str, worker: &str, worker_name: &str, title: &str,
    original: Option<Vec<u8>>, env: &Env,
) {
    let tag = agent_tag(session, worker);
    if tag.is_empty() || rel.is_empty() || rel == "." {
        return;
    }
    let after = match std::fs::metadata(target) {
        Ok(meta) if meta.is_file() && meta.len() as usize <= MAX_BYTES => match std::fs::read(target) {
            Ok(bytes) => bytes,
            Err(_) => return,
        },
        Ok(_) => return,
        Err(_) => Vec::new(), // removed
    };
    let dir = journal_dir(root, env);
    if std::fs::create_dir_all(dir.join("snap")).is_err() {
        return;
    }
    let snap = dir.join("snap").join(path_key(rel));
    let pre = take_before(&dir, &tag, rel);
    let before = match original.or(pre) {
        Some(bytes) => bytes,
        None => match std::fs::read(&snap) {
            Ok(bytes) => bytes,
            Err(_) => match head_bytes(root, rel, HOOK_GIT_TIMEOUT) {
                Some(bytes) => bytes,
                None => return,
            },
        },
    };
    let _ = std::fs::write(&snap, &after);
    if before == after {
        return;
    }
    let mut entry = json!({"t": (now_s() * 1000.0).round() / 1000.0, "s": session, "p": rel, "b": sha16(&before), "a": sha16(&after)});
    if !title.is_empty() {
        entry["n"] = json!(title.chars().take(80).collect::<String>());
    }
    if !worker.is_empty() {
        entry["w"] = json!(worker);
        if !worker_name.is_empty() {
            entry["wn"] = json!(worker_name.chars().take(80).collect::<String>());
        }
    }
    if is_binary(&before) || is_binary(&after) {
        entry["bin"] = json!(true);
        entry["h"] = json!([]);
    } else {
        let (old, new) = (split_lines(&before), split_lines(&after));
        let (ok, nk) = (keys(&old), keys(&new));
        let mut kept = 0usize;
        let mut hunks: Vec<Value> = Vec::new();
        for h in diff(&ok, &nk, HOOK_MAX_D) {
            if kept >= MAX_LINES_KEPT {
                entry["cut"] = json!(true);
                break;
            }
            let del: Vec<String> = ok[h.os..h.os + h.ol].iter().take(MAX_LINES_KEPT).map(|k| hex(*k)).collect();
            let add: Vec<String> = nk[h.ns..h.ns + h.nl].iter().take(MAX_LINES_KEPT).map(|k| hex(*k)).collect();
            kept += del.len() + add.len();
            hunks.push(json!([h.os, h.ol, h.ns, h.nl, del, add]));
        }
        entry["h"] = json!(hunks);
    }
    let log = dir.join("log.jsonl");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
        let _ = file.write_all(format!("{entry}\n").as_bytes());
    }
    if std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0) > LOG_MAX_BYTES {
        compact(&dir, |_| true);
    }
}

/// Rewrite the log keeping the entries `keep` says to (and never an expired
/// one); past the size bound only the newest half survives. Appends that
/// land while it runs are carried over.
fn compact(dir: &Path, keep: impl Fn(&Entry) -> bool) {
    let lock = dir.join("log.lock");
    if std::fs::OpenOptions::new().write(true).create_new(true).open(&lock).is_err() {
        if fresh_within(&lock, 30.0) {
            return;
        }
        let _ = std::fs::remove_file(&lock);
        if std::fs::OpenOptions::new().write(true).create_new(true).open(&lock).is_err() {
            return;
        }
    }
    let log = dir.join("log.jsonl");
    let body = std::fs::read_to_string(&log).unwrap_or_default();
    let cutoff = now_s() - EXPIRE_S;
    let mut kept: Vec<&str> = body.lines().filter(|l| parse_entry(l).is_some_and(|e| e.t >= cutoff && keep(&e))).collect();
    let size: usize = kept.iter().map(|l| l.len() + 1).sum();
    if size as u64 > LOG_MAX_BYTES {
        kept.drain(..kept.len() / 2);
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    // whatever was appended since the read
    if let Ok(now) = std::fs::read(&log) {
        if now.len() > body.len() {
            out.push_str(&String::from_utf8_lossy(&now[body.len()..]));
        }
    }
    let tmp = dir.join("log.jsonl.tmp");
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, &log);
    }
    // the gate's copies nobody used
    for entry in std::fs::read_dir(dir.join("pre")).into_iter().flatten().flatten() {
        if !fresh_within(&entry.path(), PRE_FRESH_S) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let _ = std::fs::remove_file(&lock);
}

fn fresh_within(path: &Path, seconds: f64) -> bool {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs_f64() <= seconds)
}

// ------------------------------------------------------------ git, as bytes

pub(crate) fn git_bytes(root: &Path, args: &[&str], stdin: Option<&[u8]>, envs: &[(&str, &str)], timeout: Duration) -> Option<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(root).env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped()).stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().ok()?;
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(data);
    }
    let reader = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut out, &mut buf);
            buf
        })
    });
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => return None,
        }
    };
    let out = reader.and_then(|r| r.join().ok()).unwrap_or_default();
    status.success().then_some(out)
}

const CLI_GIT: Duration = Duration::from_secs(30);

fn git_text(root: &Path, args: &[&str]) -> Option<String> {
    git_bytes(root, args, None, &[], CLI_GIT).map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn git_z(root: &Path, args: &[&str]) -> Vec<String> {
    git_text(root, args).unwrap_or_default().split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// (mode, blob) of `rel` in HEAD, or in an index (`index` = GIT_INDEX_FILE).
fn tree_entry(root: &Path, rel: &str, index: Option<&Path>) -> Option<(String, String)> {
    let line = match index {
        None => git_text(root, &["ls-tree", "-z", "HEAD", "--", rel])?,
        Some(file) => {
            let envs = [("GIT_INDEX_FILE", file.to_str()?)];
            String::from_utf8_lossy(&git_bytes(root, &["ls-files", "-s", "-z", "--", rel], None, &envs, CLI_GIT)?).into_owned()
        }
    };
    let head = line.split('\0').next()?.split('\t').next()?;
    let parts: Vec<&str> = head.split_whitespace().collect();
    match (index, parts.as_slice()) {
        (None, [mode, _kind, sha]) => Some((mode.to_string(), sha.to_string())),
        (Some(_), [mode, sha, _stage]) => Some((mode.to_string(), sha.to_string())),
        _ => None,
    }
}

fn blob(root: &Path, sha: &str) -> Option<Vec<u8>> {
    git_bytes(root, &["cat-file", "blob", sha], None, &[], CLI_GIT)
}

fn index_file(root: &Path) -> PathBuf {
    let path = git_text(root, &["rev-parse", "--git-path", "index"]).unwrap_or_default();
    let path = PathBuf::from(path.trim());
    if path.is_absolute() { path } else { root.join(path) }
}

// ------------------------------------------------------------ attribution

/// Whose a hunk is.
#[derive(Clone, Debug, PartialEq)]
pub enum Owner {
    Mine,
    Theirs(String),
    /// the sessions whose lines share it ("" = lines nobody journaled)
    Mixed(Vec<String>),
    Nobody,
}

/// Who claims each line of one file, from the journal.
#[derive(Default)]
struct Claims {
    /// line → session → the sessions whose uncommitted lines it replaced
    add: HashMap<u64, BTreeMap<String, BTreeSet<String>>>,
    del: HashMap<u64, BTreeSet<String>>,
    /// the newest binary write: (content hash, session)
    binary: Option<(String, String)>,
}

fn claims_for(entries: &[&Entry], base: &HashSet<u64>) -> Claims {
    let mut added_by: HashMap<u64, BTreeSet<&str>> = HashMap::new();
    for e in entries {
        for (_, add) in &e.hunks {
            for k in add {
                added_by.entry(*k).or_default().insert(&e.session);
            }
        }
    }
    let mut claims = Claims::default();
    for e in entries {
        if e.binary {
            claims.binary = Some((e.after.clone(), e.session.clone()));
        }
        for (del, add) in &e.hunks {
            // a write that replaced lines another session added (and HEAD
            // never had) builds on that session's work
            let built_on: BTreeSet<String> = del.iter()
                .filter(|k| !base.contains(k))
                .flat_map(|k| added_by.get(k).into_iter().flatten())
                .filter(|s| !same_session(s, &e.session))
                .map(|s| s.to_string())
                .collect();
            for k in add {
                claims.add.entry(*k).or_default().entry(e.session.clone()).or_default().extend(built_on.iter().cloned());
            }
            for k in del {
                claims.del.entry(*k).or_default().insert(e.session.clone());
            }
        }
    }
    claims
}

fn classify(h: &Hunk, base: &[u64], work: &[u64], claims: &Claims, me: &str) -> Owner {
    let mut sets: Vec<BTreeSet<String>> = Vec::new();
    let mut built_on: BTreeSet<String> = BTreeSet::new();
    for k in &base[h.os..h.os + h.ol] {
        sets.push(claims.del.get(k).cloned().unwrap_or_default());
    }
    for k in &work[h.ns..h.ns + h.nl] {
        let by = claims.add.get(k);
        sets.push(by.map(|m| m.keys().cloned().collect()).unwrap_or_default());
        for (s, on) in by.into_iter().flatten() {
            if same_session(s, me) {
                built_on.extend(on.iter().cloned());
            }
        }
    }
    let empties = sets.iter().filter(|s| s.is_empty()).count();
    if empties == sets.len() {
        return Owner::Nobody;
    }
    let owner_of = |s: &str| -> Owner {
        if !same_session(s, me) {
            Owner::Theirs(s.to_string())
        } else if built_on.is_empty() {
            Owner::Mine
        } else {
            Owner::Mixed(std::iter::once(s.to_string()).chain(built_on.iter().cloned()).collect())
        }
    };
    let singles: BTreeSet<&String> = sets.iter().filter(|s| s.len() == 1).flat_map(|s| s.iter()).collect();
    if empties == 0 {
        // the one session every line can be, and that the lines only it
        // wrote name (or, with none, the only one all lines share)
        let mut common: BTreeSet<String> = sets[0].clone();
        for s in &sets[1..] {
            common = common.intersection(s).cloned().collect();
        }
        if common.len() == 1 && singles.len() <= 1 {
            let only = common.into_iter().next().unwrap_or_default();
            if singles.iter().all(|s| **s == only) {
                return owner_of(&only);
            }
        }
    }
    let mut who: Vec<String> = if singles.is_empty() {
        sets.iter().flatten().cloned().collect::<BTreeSet<_>>().into_iter().collect()
    } else {
        singles.into_iter().cloned().collect()
    };
    if empties > 0 {
        who.push(String::new());
    }
    Owner::Mixed(who)
}

/// One file's uncommitted change, hunk by hunk.
pub struct FilePlan {
    pub rel: String,
    base_mode: Option<String>,
    base: Option<Vec<u8>>,
    work: Option<Vec<u8>>,
    work_mode: String,
    pub hunks: Vec<(Hunk, Owner)>,
    /// set when the file is one piece: binary, or a removal
    pub whole: Option<Owner>,
    pub skipped: Option<&'static str>,
}

impl FilePlan {
    fn base_lines(&self) -> Vec<&[u8]> {
        split_lines(self.base.as_deref().unwrap_or_default())
    }
    fn work_lines(&self) -> Vec<&[u8]> {
        split_lines(self.work.as_deref().unwrap_or_default())
    }

    pub fn owners(&self) -> Vec<&Owner> {
        match &self.whole {
            Some(owner) => vec![owner],
            None => self.hunks.iter().map(|(_, o)| o).collect(),
        }
    }

    /// What the index should hold for this file once the hunks `take` picks
    /// are staged: `None` = no change, `Some(None)` = removed.
    fn staged(&self, take: &dyn Fn(&Owner) -> bool) -> Option<Option<(String, Vec<u8>)>> {
        if self.skipped.is_some() {
            return None;
        }
        let mode = self.base_mode.clone().unwrap_or_else(|| self.work_mode.clone());
        if let Some(owner) = &self.whole {
            if !take(owner) {
                return None;
            }
            return Some(self.work.clone().map(|w| (self.work_mode.clone(), w)));
        }
        let picked: Vec<(Hunk, bool)> = self.hunks.iter().map(|(h, o)| (*h, take(o))).collect();
        if !picked.iter().any(|(_, t)| *t) {
            return None;
        }
        if picked.iter().all(|(_, t)| *t) && self.work.is_none() {
            return Some(None);
        }
        let (base, work) = (self.base_lines(), self.work_lines());
        let mut out: Vec<u8> = Vec::new();
        let mut i = 0;
        for (h, t) in picked {
            for line in &base[i..h.os] {
                out.extend_from_slice(line);
            }
            let lines = if t { &work[h.ns..h.ns + h.nl] } else { &base[h.os..h.os + h.ol] };
            for line in lines {
                out.extend_from_slice(line);
            }
            i = h.os + h.ol;
        }
        for line in &base[i..] {
            out.extend_from_slice(line);
        }
        Some(Some((mode, out)))
    }
}

/// The session's state file from the hooks: what it staged or removed
/// through git (`git rm` leaves no write behind).
fn staged_through_git(session: &str, env: &Env) -> BTreeSet<String> {
    let state = config::load_json(&crate::check::state_path(session, env));
    state.get("staged_paths").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn work_mode(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(path).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false) {
            return "100755".into();
        }
    }
    let _ = path;
    "100644".into()
}

/// What the attribution compares the working tree against.
#[derive(Clone, Copy, PartialEq)]
pub enum Base<'a> {
    Head,
    Index(&'a Path),
}

/// One file's plan: its base and working-tree content, diffed and attributed.
fn plan_file(root: &Path, rel: &str, base_kind: Base, entries: &[Entry], me: &str, env: &Env) -> Option<FilePlan> {
    let (base_mode, base) = match base_kind {
        Base::Head => tree_entry(root, rel, None),
        Base::Index(file) => tree_entry(root, rel, Some(file)),
    }
    .map(|(mode, sha)| (Some(mode), blob(root, &sha)))
    .unwrap_or((None, None));
    let abs = root.join(rel);
    let meta = std::fs::symlink_metadata(&abs).ok();
    let mut plan = FilePlan {
        rel: rel.to_string(), base_mode: base_mode.clone(), base: None, work: None,
        work_mode: work_mode(&abs), hunks: Vec::new(), whole: None, skipped: None,
    };
    if base_mode.as_deref().is_some_and(|m| m == "160000" || m == "120000") || meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
        plan.skipped = Some("a link or submodule");
        return Some(plan);
    }
    if meta.as_ref().is_some_and(|m| m.len() > 8 * MAX_BYTES as u64) {
        plan.skipped = Some("too large");
        return Some(plan);
    }
    let work = match &meta {
        Some(m) if m.is_file() => Some(std::fs::read(&abs).ok()?),
        Some(_) => return None,
        None => None,
    };
    if base_mode.is_some() && base.is_none() {
        return None;
    }
    if base == work {
        return None;
    }
    plan.base = base;
    plan.work = work;
    let path_entries: Vec<&Entry> = entries.iter().filter(|e| e.path == rel).collect();
    let (b, w) = (plan.base.clone().unwrap_or_default(), plan.work.clone().unwrap_or_default());
    if is_binary(&b) || is_binary(&w) {
        let latest = path_entries.iter().rev().find(|e| e.binary);
        plan.whole = Some(match latest {
            Some(e) if plan.work.is_some() && e.after == sha16(&w) => {
                if same_session(&e.session, me) { Owner::Mine } else { Owner::Theirs(e.session.clone()) }
            }
            _ => Owner::Nobody,
        });
        return Some(plan);
    }
    let (bl, wl) = (split_lines(&b), split_lines(&w));
    let (bk, wk) = (keys(&bl), keys(&wl));
    let base_set: HashSet<u64> = bk.iter().copied().collect();
    let claims = claims_for(&path_entries, &base_set);
    plan.hunks = diff(&bk, &wk, CLI_MAX_D).into_iter().map(|h| {
        let owner = classify(&h, &bk, &wk, &claims, me);
        (h, owner)
    }).collect();
    // a removal through `git rm` / `rm`: the file is one piece, and it is the
    // session's that staged its removal, when no one else wrote it
    if plan.work.is_none() && plan.hunks.iter().all(|(_, o)| *o == Owner::Nobody) && !me.is_empty() {
        let others_wrote = path_entries.iter().any(|e| !same_session(&e.session, me));
        if !others_wrote && staged_through_git(me, env).contains(rel) {
            plan.whole = Some(Owner::Mine);
        }
    }
    Some(plan)
}

/// Every changed file (tracked and changed against `base`, or untracked),
/// planned.
pub fn plan(root: &Path, base: Base, me: &str, only: Option<&[String]>, env: &Env) -> Vec<FilePlan> {
    let entries = load(root, env);
    let mut paths: BTreeSet<String> = BTreeSet::new();
    match only {
        Some(list) => paths.extend(list.iter().cloned()),
        None => {
            let index_env: Vec<(&str, &str)> = match base {
                Base::Index(file) => vec![("GIT_INDEX_FILE", file.to_str().unwrap_or(""))],
                Base::Head => Vec::new(),
            };
            let diff_args: &[&str] = match base {
                Base::Head => &["diff", "HEAD", "--name-only", "--no-renames", "-z"],
                Base::Index(_) => &["diff", "--name-only", "--no-renames", "-z"],
            };
            let changed = git_bytes(root, diff_args, None, &index_env, CLI_GIT).unwrap_or_default();
            paths.extend(String::from_utf8_lossy(&changed).split('\0').filter(|s| !s.is_empty()).map(str::to_string));
            // an untracked file counts only when a session journaled it
            let journaled: HashSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
            paths.extend(git_z(root, &["ls-files", "--others", "--exclude-standard", "-z"]).into_iter().filter(|p| journaled.contains(p.as_str())));
        }
    }
    paths.iter().filter_map(|rel| plan_file(root, rel, base, &entries, me, env)).collect()
}

/// A session as people know it: its tag and, when the harness named the
/// conversation, that name.
pub fn session_name(session: &str, entries: &[Entry]) -> String {
    if session.is_empty() {
        return "no session (a person's or an unreported edit)".into();
    }
    let tag: String = session.chars().take(8).collect();
    match entries.iter().rev().find(|e| e.session == session && !e.title.is_empty()) {
        Some(e) => format!("session {tag} (\"{}\")", e.title),
        None => format!("session {tag}"),
    }
}

fn last_seen(session: &str, entries: &[Entry]) -> f64 {
    entries.iter().filter(|e| e.session == session).map(|e| e.t).fold(0.0, f64::max)
}

fn line_range(h: &Hunk) -> String {
    if h.nl == 0 {
        format!("{}", h.ns.max(1))
    } else if h.nl == 1 {
        format!("{}", h.ns + 1)
    } else {
        format!("{}-{}", h.ns + 1, h.ns + h.nl)
    }
}

// ------------------------------------------------------------ the commands

struct CommitArgs {
    mine: bool,
    message: Option<String>,
    dry_run: bool,
    include_mixed: bool,
    session: String,
}

fn parse_commit_args(args: &[String]) -> CommitArgs {
    let mut out = CommitArgs { mine: false, message: None, dry_run: false, include_mixed: false, session: String::new() };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mine" => out.mine = true,
            "--dry-run" | "-n" => out.dry_run = true,
            "--include-mixed" => out.include_mixed = true,
            "-m" | "--message" => {
                out.message = args.get(i + 1).cloned();
                i += 1;
            }
            "--session" => {
                out.session = args.get(i + 1).cloned().unwrap_or_default();
                i += 1;
            }
            other => {
                if let Some(m) = other.strip_prefix("--message=") {
                    out.message = Some(m.to_string());
                } else if let Some(s) = other.strip_prefix("--session=") {
                    out.session = s.to_string();
                }
            }
        }
        i += 1;
    }
    out
}

/// `--session`, else the session the hooks stamped on the command, else the
/// one agent at work in an ancestor process.
fn resolve_session(given: &str, env: &Env) -> String {
    if !given.is_empty() {
        return given.to_string();
    }
    crate::machine::running_session(env)
}

fn toplevel(cwd: &Path) -> Option<PathBuf> {
    let top = git_text(cwd, &["rev-parse", "--show-toplevel"])?;
    let top = top.trim();
    (!top.is_empty()).then(|| PathBuf::from(top))
}

/// Write `content` into an index (a blob and its entry), or take the path out.
fn set_entry(root: &Path, index: &Path, rel: &str, content: &Option<(String, Vec<u8>)>) -> bool {
    let envs = [("GIT_INDEX_FILE", index.to_str().unwrap_or(""))];
    match content {
        None => git_bytes(root, &["update-index", "--force-remove", "--", rel], None, &envs, CLI_GIT).is_some(),
        Some((mode, bytes)) => {
            let Some(sha) = git_bytes(root, &["hash-object", "-w", "--no-filters", "--stdin"], Some(bytes), &[], CLI_GIT) else { return false };
            let sha = String::from_utf8_lossy(&sha).trim().to_string();
            let info = format!("{mode},{sha},{rel}");
            git_bytes(root, &["update-index", "--add", "--cacheinfo", &info], None, &envs, CLI_GIT).is_some()
        }
    }
}

/// Counts for the summary.
#[derive(Default)]
struct Tally {
    mine: usize,
    mine_files: Vec<String>,
    theirs: BTreeMap<String, (usize, BTreeSet<String>)>,
    mixed: Vec<String>,
    nobody: usize,
    nobody_files: BTreeSet<String>,
    skipped: Vec<String>,
}

fn tally(plans: &[FilePlan], take: &dyn Fn(&Owner) -> bool) -> Tally {
    let mut t = Tally::default();
    for p in plans {
        if let Some(why) = p.skipped {
            t.skipped.push(format!("{} ({why})", p.rel));
            continue;
        }
        let mut mine_here = false;
        let ranges: Vec<Option<Hunk>> = match &p.whole {
            Some(_) => vec![None],
            None => p.hunks.iter().map(|(h, _)| Some(*h)).collect(),
        };
        for (owner, h) in p.owners().into_iter().zip(ranges) {
            let at = h.map(|h| format!("{}:{}", p.rel, line_range(&h))).unwrap_or_else(|| p.rel.clone());
            match owner {
                o if take(o) => {
                    t.mine += 1;
                    mine_here = true;
                    if matches!(o, Owner::Mixed(_)) {
                        t.mixed.push(at);
                    }
                }
                Owner::Mixed(_) => t.mixed.push(at),
                Owner::Mine => {}
                Owner::Theirs(s) => {
                    let slot = t.theirs.entry(s.clone()).or_default();
                    slot.0 += 1;
                    slot.1.insert(p.rel.clone());
                }
                Owner::Nobody => {
                    t.nobody += 1;
                    t.nobody_files.insert(p.rel.clone());
                }
            }
        }
        if mine_here {
            t.mine_files.push(p.rel.clone());
        }
    }
    t
}

fn list(items: &[String], cap: usize) -> String {
    let shown: Vec<&str> = items.iter().take(cap).map(String::as_str).collect();
    let more = if items.len() > cap { format!(" and {} more", items.len() - cap) } else { String::new() };
    format!("{}{more}", shown.join(", "))
}

fn summary(t: &Tally, entries: &[Entry], include_mixed: bool, verb: &str) -> Vec<String> {
    let mut lines = Vec::new();
    if t.mine == 0 {
        lines.push("Cork: nothing you wrote is left to commit.".to_string());
    } else {
        lines.push(format!("Cork: {verb} {} hunk(s) of yours in {} file(s): {}.", t.mine, t.mine_files.len(), list(&t.mine_files, 6)));
    }
    for (s, (n, files)) in &t.theirs {
        let files: Vec<String> = files.iter().cloned().collect();
        lines.push(format!("Left for {}: {n} hunk(s) in {}.", session_name(s, entries), list(&files, 4)));
    }
    if !t.mixed.is_empty() {
        if include_mixed {
            lines.push(format!("Taken although they mix your lines with others' (--include-mixed): {}.", list(&t.mixed, 6)));
        } else {
            lines.push(format!("Mixed, not staged (your lines and another's in one hunk; --include-mixed takes them): {}.", list(&t.mixed, 6)));
        }
    }
    if t.nobody > 0 {
        let files: Vec<String> = t.nobody_files.iter().cloned().collect();
        lines.push(format!("Unattributed, left alone (no session wrote them: a person's or an unreported edit): {} hunk(s) in {}.", t.nobody, list(&files, 4)));
    }
    if !t.skipped.is_empty() {
        lines.push(format!("Skipped: {}.", list(&t.skipped, 4)));
    }
    lines
}

fn owner_word(owner: &Owner, entries: &[Entry]) -> String {
    match owner {
        Owner::Mine => "yours".into(),
        Owner::Theirs(s) => session_name(s, entries),
        Owner::Mixed(who) => {
            let names: Vec<String> = who.iter().map(|s| if s.is_empty() { "unattributed".to_string() } else { session_name(s, entries) }).collect();
            format!("mixed: {}", names.join(" + "))
        }
        Owner::Nobody => "unattributed".into(),
    }
}

fn details(plans: &[FilePlan], entries: &[Entry], me: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for p in plans {
        if let Some(why) = p.skipped {
            lines.push(format!("  {}: skipped ({why})", p.rel));
            continue;
        }
        if let Some(owner) = &p.whole {
            lines.push(format!("  {}: whole file, {}", p.rel, owner_word(owner, entries)));
            continue;
        }
        for (h, owner) in &p.hunks {
            let mixed_mine = matches!(owner, Owner::Mixed(w) if w.iter().any(|s| same_session(s, me)));
            let word = if mixed_mine { owner_word(owner, entries).replace(&session_name(me, entries), "you") } else { owner_word(owner, entries) };
            lines.push(format!("  {}:{} (+{} -{}): {word}", p.rel, line_range(h), h.nl, h.ol));
        }
    }
    lines
}

/// Drop entries every line of which a commit has taken (or a revert undone):
/// none of their lines is still part of an uncommitted change.
pub fn prune(root: &Path, env: &Env) {
    let entries = load(root, env);
    if entries.is_empty() {
        return;
    }
    let paths: BTreeSet<String> = entries.iter().map(|e| e.path.clone()).collect();
    let mut live: HashMap<String, HashSet<u64>> = HashMap::new();
    for rel in &paths {
        let Some(p) = plan_file(root, rel, Base::Head, &[], "", env) else {
            // clean: its last snapshot is HEAD now, so the snapshot goes
            let _ = std::fs::remove_file(journal_dir(root, env).join("snap").join(path_key(rel)));
            continue;
        };
        let (bk, wk) = (keys(&p.base_lines()), keys(&p.work_lines()));
        let set = live.entry(rel.clone()).or_default();
        if p.whole.is_some() || p.skipped.is_some() {
            set.insert(u64::MAX);
            continue;
        }
        for (h, _) in &p.hunks {
            set.extend(&bk[h.os..h.os + h.ol]);
            set.extend(&wk[h.ns..h.ns + h.nl]);
        }
    }
    compact(&journal_dir(root, env), |e| {
        let Some(set) = live.get(&e.path) else { return false };
        set.contains(&u64::MAX) || e.binary || e.hunks.iter().any(|(d, a)| d.iter().chain(a).any(|k| set.contains(k)))
    });
}

/// `cork commit --mine [-m MSG] [--dry-run] [--include-mixed] [--session ID]`
pub fn commit_command(args: &[String], env: &Env) -> i32 {
    commit_in(&std::env::current_dir().unwrap_or_default(), args, env)
}

pub fn commit_in(cwd: &Path, raw: &[String], env: &Env) -> i32 {
    let args = parse_commit_args(raw);
    if !args.mine {
        println!("usage: cork commit --mine [-m MESSAGE] [--dry-run] [--include-mixed] [--session ID]\n\
Stages (and with -m commits) only the lines this session wrote; other sessions' and unattributed lines stay as they are.");
        return 2;
    }
    let Some(root) = toplevel(cwd) else {
        println!("Cork commit: this is not a git repository.");
        return 1;
    };
    let me = resolve_session(&args.session, env);
    if me.is_empty() {
        println!("Cork commit: cannot tell which session this is. Pass --session <id>.");
        return 1;
    }
    let entries = load(&root, env);
    let committing = args.message.is_some() || args.dry_run;
    let real_index = index_file(&root);
    let base = if committing { Base::Head } else { Base::Index(&real_index) };
    let plans = plan(&root, base, &me, None, env);
    let include = args.include_mixed;
    let take = |o: &Owner| matches!(o, Owner::Mine) || (include && matches!(o, Owner::Mixed(w) if w.iter().any(|s| same_session(s, &me))));
    let t = tally(&plans, &take);
    if args.dry_run {
        let mut out = summary(&t, &entries, include, "would stage");
        out.extend(details(&plans, &entries, &me));
        println!("{}", out.join("\n"));
        return 0;
    }
    if t.mine == 0 {
        println!("{}", summary(&t, &entries, include, "stage").join("\n"));
        return 0;
    }
    let chosen: Vec<(&FilePlan, Option<(String, Vec<u8>)>)> =
        plans.iter().filter_map(|p| p.staged(&take).map(|c| (p, c))).collect();
    let Some(message) = args.message.clone() else {
        // staging only: into the index as it is
        let failed: Vec<String> = chosen.iter().filter(|(p, c)| !set_entry(&root, &real_index, &p.rel, c)).map(|(p, _)| p.rel.clone()).collect();
        let mut out = summary(&t, &entries, include, "staged");
        if !failed.is_empty() {
            out.push(format!("Could not stage: {}.", list(&failed, 6)));
        }
        out.push("Nothing committed: add -m \"...\" to commit them.".into());
        println!("{}", out.join("\n"));
        return if failed.is_empty() { 0 } else { 1 };
    };
    // the commit is built in an index of its own, HEAD plus this session's
    // hunks, so nothing anyone else staged can ride along
    let tmp = root.join(git_text(&root, &["rev-parse", "--git-path", "collide-mine.index"]).unwrap_or_default().trim());
    let _ = std::fs::remove_file(&tmp);
    let tmp_env = [("GIT_INDEX_FILE", tmp.to_str().unwrap_or(""))];
    let has_head = git_text(&root, &["rev-parse", "--verify", "-q", "HEAD"]).is_some();
    let read = if has_head { vec!["read-tree", "HEAD"] } else { vec!["read-tree", "--empty"] };
    if git_bytes(&root, &read, None, &tmp_env, CLI_GIT).is_none() {
        println!("Cork commit: could not prepare the commit; nothing changed.");
        return 1;
    }
    for (p, content) in &chosen {
        if !set_entry(&root, &tmp, &p.rel, content) {
            let _ = std::fs::remove_file(&tmp);
            println!("Cork commit: could not stage {}; nothing changed.", p.rel);
            return 1;
        }
    }
    let status = Command::new("git")
        .args(["commit", "-q", "-m", &message])
        .current_dir(&root)
        .env("GIT_INDEX_FILE", &tmp)
        .stdin(Stdio::null())
        .status();
    let _ = std::fs::remove_file(&tmp);
    if !status.is_ok_and(|s| s.success()) {
        println!("Cork commit: git commit failed; nothing was committed and the index is as it was.");
        return 1;
    }
    // the shared index learns the commit: this session's hunks, on top of
    // whatever else is staged there, so `git status` does not show them as
    // staged backwards
    let files: Vec<String> = chosen.iter().map(|(p, _)| p.rel.clone()).collect();
    for p in plan(&root, Base::Index(&real_index), &me, Some(&files), env) {
        if let Some(content) = p.staged(&take) {
            set_entry(&root, &real_index, &p.rel, &content);
        }
    }
    prune(&root, env);
    let short = git_text(&root, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    let mut out = summary(&t, &entries, include, "committed");
    out.push(format!("Commit {}: {}", short.trim(), message.lines().next().unwrap_or("")));
    println!("{}", out.join("\n"));
    0
}

/// `cork status --mine [--session ID]`: what this session wrote that is
/// not committed yet.
pub fn status_command(args: &[String], env: &Env) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let parsed = parse_commit_args(args);
    let Some(root) = toplevel(&cwd) else {
        println!("Cork status: this is not a git repository.");
        return 1;
    };
    let me = resolve_session(&parsed.session, env);
    if me.is_empty() {
        println!("Cork status: cannot tell which session this is. Pass --session <id>.");
        return 1;
    }
    let entries = load(&root, env);
    let plans = plan(&root, Base::Head, &me, None, env);
    let mine: Vec<&FilePlan> = plans.iter()
        .filter(|p| p.owners().iter().any(|o| matches!(o, Owner::Mine) || matches!(o, Owner::Mixed(w) if w.iter().any(|s| same_session(s, &me)))))
        .collect();
    if mine.is_empty() {
        println!("Cork: nothing you wrote is uncommitted.");
        return 0;
    }
    let mut out = vec![format!("Cork: uncommitted work of yours in {} file(s):", mine.len())];
    for p in mine {
        let ours: Vec<String> = match &p.whole {
            Some(o) => vec![format!("whole file ({})", owner_word(o, &entries))],
            None => p.hunks.iter()
                .filter(|(_, o)| !matches!(o, Owner::Theirs(_) | Owner::Nobody))
                .map(|(h, o)| {
                    let mixed = if matches!(o, Owner::Mixed(_)) { ", mixed" } else { "" };
                    format!("{} (+{} -{}{mixed})", line_range(h), h.nl, h.ol)
                })
                .collect(),
        };
        out.push(format!("  {}: {}", p.rel, ours.join(", ")));
    }
    println!("{}", out.join("\n"));
    0
}

// ------------------------------------------------------------ for land and the guard

/// Whose an uncommitted file is, for `cork land`: `None` when every
/// change in it belongs to other sessions at work now (a push of committed
/// work may leave it be), else why it blocks.
pub fn blocking_reason(root: &Path, rel: &str, me: &str, entries: &[Entry], env: &Env) -> Option<String> {
    let Some(p) = plan_file(root, rel, Base::Head, entries, me, env) else { return None };
    if p.skipped.is_some() {
        return Some("not attributable".into());
    }
    let now = now_s();
    let mut why: BTreeSet<String> = BTreeSet::new();
    for owner in p.owners() {
        match owner {
            Owner::Theirs(s) if now - last_seen(s, entries) <= LIVE_S => {}
            Owner::Theirs(s) => {
                why.insert(format!("{}, not at work", session_name(s, entries)));
            }
            Owner::Mine => {
                why.insert("yours".into());
            }
            Owner::Mixed(_) => {
                why.insert("mixed".into());
            }
            Owner::Nobody => {
                why.insert("no session wrote it".into());
            }
        }
    }
    (!why.is_empty()).then(|| why.into_iter().collect::<Vec<_>>().join("; "))
}

/// For the commit guard: the files among `paths` holding hunks of another
/// session at work now (whole or mixed with this one's), with whose. The
/// content compared to HEAD is the index's for `staged`, else the tree's.
pub fn foreign(root: &Path, paths: &[String], staged: bool, me: &str, env: &Env, deadline: std::time::Instant) -> Vec<(String, Vec<String>)> {
    let entries = load(root, env);
    let now = now_s();
    let live_other = |s: &str| !same_session(s, me) && now - last_seen(s, &entries) <= LIVE_S;
    // only files another live session journaled can hold its lines
    let theirs: BTreeSet<&str> = entries.iter().filter(|e| live_other(&e.session)).map(|e| e.path.as_str()).collect();
    let real_index = index_file(root);
    let mut out = Vec::new();
    for rel in paths.iter().filter(|p| theirs.contains(p.as_str())).take(25) {
        if std::time::Instant::now() > deadline {
            break;
        }
        let Some(p) = (if staged {
            plan_staged(root, rel, &real_index, &entries, me)
        } else {
            plan_file(root, rel, Base::Head, &entries, me, env)
        }) else { continue };
        let mut who: BTreeSet<String> = BTreeSet::new();
        for owner in p.owners() {
            match owner {
                Owner::Theirs(s) if live_other(s) => {
                    who.insert(session_name(s, &entries));
                }
                Owner::Mixed(list) => {
                    for s in list.iter().filter(|s| live_other(s)) {
                        who.insert(session_name(s, &entries));
                    }
                }
                _ => {}
            }
        }
        if !who.is_empty() {
            out.push((rel.clone(), who.into_iter().collect()));
        }
    }
    out
}

/// HEAD against what the index holds for `rel` (what a plain `git commit`
/// would take).
fn plan_staged(root: &Path, rel: &str, index: &Path, entries: &[Entry], me: &str) -> Option<FilePlan> {
    let (mode, sha) = tree_entry(root, rel, Some(index))?;
    let staged = blob(root, &sha)?;
    let (base_mode, base) = match tree_entry(root, rel, None) {
        Some((m, sha)) => (Some(m), blob(root, &sha)?),
        None => (None, Vec::new()),
    };
    if base == staged || is_binary(&base) || is_binary(&staged) {
        return None;
    }
    let (bk, sk) = (keys(&split_lines(&base)), keys(&split_lines(&staged)));
    let path_entries: Vec<&Entry> = entries.iter().filter(|e| e.path == rel).collect();
    let claims = claims_for(&path_entries, &bk.iter().copied().collect());
    let hunks = diff(&bk, &sk, CLI_MAX_D).into_iter().map(|h| (h, classify(&h, &bk, &sk, &claims, me))).collect();
    Some(FilePlan {
        rel: rel.to_string(), base_mode, base: Some(base), work: Some(staged), work_mode: mode,
        hunks, whole: None, skipped: None,
    })
}

/// The command an agent runs to commit only its own lines.
pub fn commit_hint(env: &Env) -> String {
    crate::machine::own_command(env, "commit --mine")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(lines: &[&str]) -> Vec<u64> {
        lines.iter().map(|l| line_key(format!("{l}\n").as_bytes())).collect()
    }

    #[test]
    fn the_diff_finds_each_changed_run() {
        let a = k(&["a", "b", "c", "d", "e", "f"]);
        let b = k(&["a", "B", "c", "d", "e", "f", "g"]);
        assert_eq!(diff(&a, &b, 100), vec![Hunk { os: 1, ol: 1, ns: 1, nl: 1 }, Hunk { os: 6, ol: 0, ns: 6, nl: 1 }]);
        let a = k(&["x", "a", "y", "b", "z"]);
        let b = k(&["a", "b", "q"]);
        let hunks = diff(&a, &b, 100);
        let removed: usize = hunks.iter().map(|h| h.ol).sum();
        let added: usize = hunks.iter().map(|h| h.nl).sum();
        assert_eq!((removed, added), (3, 1), "{hunks:?}");
        assert!(diff(&a, &a, 100).is_empty());
        // past the edit bound, one hunk
        assert_eq!(diff(&k(&["a", "b", "c"]), &k(&["x", "y", "z"]), 1), vec![Hunk { os: 0, ol: 3, ns: 0, nl: 3 }]);
    }

    const A: &str = "aaaaaaaa-1111-session";
    const B: &str = "bbbbbbbb-2222-session";

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repo with one committed file of twenty numbered lines, and a home
    /// of its own for the journal.
    fn setup(name: &str) -> (PathBuf, PathBuf, Env) {
        let base = std::env::temp_dir().join(format!("collide-journal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), base.to_string_lossy().to_string());
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(&repo, &["config", "user.email", "t@t"]);
        sh(&repo, &["config", "user.name", "t"]);
        let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(repo.join("app.txt"), body).unwrap();
        std::fs::write(repo.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "seed"]);
        let repo = toplevel(&repo).unwrap();
        (base, repo, env)
    }

    /// An Edit as Claude Code reports it: the original file comes along.
    fn edit_as(repo: &Path, rel: &str, from: &str, to: &str, session: &str, env: &Env) {
        let path = repo.join(rel);
        let before = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(before.contains(from), "{from} not in {rel}");
        std::fs::write(&path, before.replacen(from, to, 1)).unwrap();
        record(repo, &path, rel, session, "", "", "", Some(before.into_bytes()), env);
    }

    fn commit_as(repo: &Path, session: &str, extra: &[&str], env: &Env) -> i32 {
        let mut args: Vec<String> = vec!["--mine".into(), "--session".into(), session.into()];
        args.extend(extra.iter().map(|s| s.to_string()));
        commit_in(repo, &args, env)
    }

    #[test]
    fn two_sessions_in_one_file_each_commit_only_their_own_hunks() {
        let (base, repo, env) = setup("two");
        edit_as(&repo, "app.txt", "line 2\n", "line 2 by A\n", A, &env);
        edit_as(&repo, "app.txt", "line 15\n", "line 15 by B\n", B, &env);
        let plans = plan(&repo, Base::Head, A, None, &env);
        let owners: Vec<&Owner> = plans[0].owners();
        assert_eq!(owners, vec![&Owner::Mine, &Owner::Theirs(B.into())]);

        assert_eq!(commit_as(&repo, A, &["-m", "A's change"], &env), 0);
        let head = sh(&repo, &["show", "HEAD:app.txt"]);
        assert!(head.contains("line 2 by A") && !head.contains("line 15 by B"), "{head}");
        // B's hunk is still there, unstaged; A's is not staged backwards
        assert!(sh(&repo, &["diff", "--cached"]).is_empty());
        let left = sh(&repo, &["diff"]);
        assert!(left.contains("+line 15 by B") && !left.contains("line 2"), "{left}");

        assert_eq!(commit_as(&repo, B, &["-m", "B's change"], &env), 0);
        let head = sh(&repo, &["show", "HEAD:app.txt"]);
        assert!(head.contains("line 2 by A") && head.contains("line 15 by B"));
        assert!(sh(&repo, &["status", "--porcelain"]).is_empty());
        // both commits took every journaled line: the journal is empty again
        assert!(load(&repo, &env).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn staging_without_a_message_stages_only_this_sessions_hunks() {
        let (base, repo, env) = setup("stage");
        edit_as(&repo, "app.txt", "line 3\n", "line 3 by A\n", A, &env);
        edit_as(&repo, "app.txt", "line 18\n", "line 18 by B\n", B, &env);
        // a new file A created comes along whole
        std::fs::write(repo.join("new.txt"), "fresh\n").unwrap();
        record(&repo, &repo.join("new.txt"), "new.txt", A, "", "", "", Some(Vec::new()), &env);
        assert_eq!(commit_as(&repo, A, &[], &env), 0);
        let staged = sh(&repo, &["diff", "--cached"]);
        assert!(staged.contains("+line 3 by A") && staged.contains("+fresh") && !staged.contains("line 18"), "{staged}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn overlapping_lines_are_mixed_and_left_unstaged() {
        let (base, repo, env) = setup("mixed");
        edit_as(&repo, "app.txt", "line 5\nline 6\n", "line 5 by A\nline 6 by A\n", A, &env);
        // B rewrites A's line 6 and its own line 7: one run of changed lines
        edit_as(&repo, "app.txt", "line 6 by A\nline 7\n", "line 6 by B\nline 7 by B\n", B, &env);
        let plans = plan(&repo, Base::Head, A, None, &env);
        assert_eq!(plans[0].hunks.len(), 1);
        assert!(matches!(&plans[0].hunks[0].1, Owner::Mixed(who) if who.contains(&A.to_string()) && who.contains(&B.to_string())));
        // and for B as well: its lines sit on A's
        let for_b = plan(&repo, Base::Head, B, None, &env);
        assert!(matches!(for_b[0].hunks[0].1, Owner::Mixed(_)));

        assert_eq!(commit_as(&repo, A, &["-m", "A"], &env), 0);
        assert_eq!(sh(&repo, &["rev-list", "--count", "HEAD"]).trim(), "1", "nothing of A's alone to commit");
        assert!(sh(&repo, &["diff", "--cached"]).is_empty());
        // asked to, it takes the mixed hunk
        assert_eq!(commit_as(&repo, A, &["-m", "A and B", "--include-mixed"], &env), 0);
        assert!(sh(&repo, &["show", "HEAD:app.txt"]).contains("line 7 by B"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_shell_sed_edit_is_attributed_to_its_session() {
        let (base, repo, env) = setup("sed");
        edit_as(&repo, "app.txt", "line 1\n", "line 1 by A\n", A, &env);
        // B runs sed: the gate copies what the command names, the reporter
        // finds what it wrote by mtime, exactly as the hooks do
        let command = "sed -i.bak 's/^line 12$/line 12 by sed/' app.txt && rm app.txt.bak";
        for target in crate::shell::shell_gate_paths(command, &repo) {
            note_before(&repo, &target, B, "", &env);
        }
        let ok = Command::new("sh").args(["-c", command]).current_dir(&repo).status().unwrap().success();
        assert!(ok);
        let targets = crate::shell::shell_write_targets(command, &repo, &repo);
        assert!(targets.iter().any(|t| t.ends_with("app.txt")), "{targets:?}");
        for target in targets {
            let rel = config::rel_path(&target, &repo).unwrap();
            record(&repo, &target, &rel, B, "", "", "", None, &env);
        }
        let plans = plan(&repo, Base::Head, B, None, &env);
        assert_eq!(plans[0].owners(), vec![&Owner::Theirs(A.into()), &Owner::Mine]);
        assert_eq!(commit_as(&repo, B, &["-m", "sed"], &env), 0);
        let head = sh(&repo, &["show", "HEAD:app.txt"]);
        assert!(head.contains("line 12 by sed") && !head.contains("line 1 by A"), "{head}");

        // with no gate copy (a script the command line never named), the
        // last journaled state is the before: A's edit is not claimed by B
        edit_as(&repo, "notes.txt", "one\n", "one by A\n", A, &env);
        let body = std::fs::read_to_string(repo.join("notes.txt")).unwrap();
        std::fs::write(repo.join("notes.txt"), body.replace("three\n", "three by script\n")).unwrap();
        record(&repo, &repo.join("notes.txt"), "notes.txt", B, "", "", "", None, &env);
        let notes = plan(&repo, Base::Head, B, None, &env).into_iter().find(|p| p.rel == "notes.txt").unwrap();
        assert_eq!(notes.owners(), vec![&Owner::Theirs(A.into()), &Owner::Mine]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn unattributed_edits_are_left_alone() {
        let (base, repo, env) = setup("human");
        edit_as(&repo, "app.txt", "line 4\n", "line 4 by A\n", A, &env);
        // a person edits by hand: no hook saw it
        let body = std::fs::read_to_string(repo.join("app.txt")).unwrap();
        std::fs::write(repo.join("app.txt"), body.replace("line 17\n", "line 17 by hand\n")).unwrap();
        std::fs::write(repo.join("notes.txt"), "one\ntwo\nthree\nfour by hand\n").unwrap();
        let plans = plan(&repo, Base::Head, A, None, &env);
        let app = plans.iter().find(|p| p.rel == "app.txt").unwrap();
        assert_eq!(app.owners(), vec![&Owner::Mine, &Owner::Nobody]);
        assert_eq!(plans.iter().find(|p| p.rel == "notes.txt").unwrap().owners(), vec![&Owner::Nobody]);
        assert_eq!(commit_as(&repo, A, &["-m", "A"], &env), 0);
        let head = sh(&repo, &["show", "HEAD:app.txt"]);
        assert!(head.contains("line 4 by A") && !head.contains("by hand"));
        assert!(!sh(&repo, &["show", "HEAD:notes.txt"]).contains("four"));
        let left = sh(&repo, &["diff", "--name-only"]);
        assert!(left.contains("app.txt") && left.contains("notes.txt"));
        // a dry run changes nothing
        let before = sh(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(commit_as(&repo, A, &["-m", "x", "--dry-run"], &env), 0);
        assert_eq!(sh(&repo, &["rev-parse", "HEAD"]), before);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_commit_guard_holds_a_git_add_that_would_carry_another_sessions_hunks() {
        let (base, repo, env) = setup("guard");
        edit_as(&repo, "app.txt", "line 2\n", "line 2 by A\n", A, &env);
        edit_as(&repo, "app.txt", "line 15\n", "line 15 by B\n", B, &env);
        std::fs::create_dir_all(crate::config::data_dir(&env).join("check")).unwrap();
        for s in [A, B] {
            let state = json!({"root": repo.to_string_lossy(), "written_paths": ["app.txt"]});
            std::fs::write(crate::check::state_path(s, &env), state.to_string()).unwrap();
        }
        let input = |command: &str| json!({"tool_name": "Bash", "session_id": B, "cwd": repo, "tool_input": {"command": command}}).to_string();
        let held = crate::sharedtree::commit_guard(&input("git add app.txt && git commit -m 'B'"), &env).expect("held");
        assert!(held.contains("app.txt: lines session aaaaaaaa wrote") && held.contains("commit --mine -m"), "{held}");
        assert!(crate::sharedtree::commit_guard(&input("git commit -am wip"), &env).is_some());
        // nothing staged: a plain commit takes nothing of A's
        assert!(crate::sharedtree::commit_guard(&input("git commit -m 'B'"), &env).is_none());
        // once only B's hunk is staged, the plain commit is fine
        assert_eq!(commit_as(&repo, B, &[], &env), 0);
        assert!(crate::sharedtree::commit_guard(&input("git commit -m 'B'"), &env).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn commit_rewrite_stamps_the_session() {
        let input = |command: &str| json!({"tool_name": "Bash", "session_id": A, "permission_mode": "bypassPermissions", "tool_input": {"command": command}}).to_string();
        let out: Value = serde_json::from_str(&crate::machine::commit_rewrite(&input("cork commit --mine -m 'x' && git push")).unwrap()).unwrap();
        assert_eq!(out["hookSpecificOutput"]["updatedInput"]["command"], format!("CORK_SESSION={A} cork commit --mine -m 'x' && git push"));
        // the program's name from before the rename is still ours
        let out: Value = serde_json::from_str(&crate::machine::commit_rewrite(&input("collide commit --mine -m 'x'")).unwrap()).unwrap();
        assert_eq!(out["hookSpecificOutput"]["updatedInput"]["command"], format!("CORK_SESSION={A} collide commit --mine -m 'x'"));
        assert!(crate::machine::commit_rewrite(&input("COLLIDE_SESSION=s cork commit --mine")).is_none(), "already stamped, old name");
        assert!(crate::machine::commit_rewrite(&input("collide status")).is_none());
        assert!(crate::machine::commit_rewrite(&input("collide commit --mine --session x")).is_none());
        assert!(crate::machine::commit_rewrite(&input("git commit -m x")).is_none());
    }

    #[test]
    fn splitting_keeps_terminators() {
        assert_eq!(split_lines(b"a\nb"), vec![&b"a\n"[..], &b"b"[..]]);
        assert_eq!(split_lines(b"a\r\n\n"), vec![&b"a\r\n"[..], &b"\n"[..]]);
        assert!(split_lines(b"").is_empty());
    }
}
