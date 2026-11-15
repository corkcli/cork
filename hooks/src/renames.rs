//! Renames, as this checkout's git saw them, so Cork can carry the old
//! names' history, notes and decisions over to the new ones.
//!
//! * Path moves: the checkout's own `git log` names every file git detected
//!   as renamed. Each pair is collapsed to the smallest move that explains
//!   it (`server/collide-rs/src/a.rs -> server/cork-rs/src/a.rs` is
//!   `server/collide-rs/ -> server/cork-rs/`); a folder pair seen for two
//!   files or more is a folder move, a lone pair a file move. Counted in a
//!   detached process at session start (`spawn_if_due`, `command`), from the
//!   last commit checked for this checkout (a marker in the data dir), and
//!   sent as paths, commit ids and times: no commit message or code.
//! * Repo renames: the machine's list of checkouts (`listen.rs`) names the
//!   repo each folder was, and each clone's `.git` identity. A folder that
//!   was another repo id, or the same clone moved to a new folder, is a repo
//!   whose origin changed (`renamed_from`), told to the server before the
//!   session reports anything under the new id. An entry from before the
//!   identity was kept, whose folder is gone, is asked of the old name's
//!   remote in the detached look (`renamed_from_remote`).
//! * What the trunk has: a checkout on the repo's default branch sends the
//!   source paths git tracks there (`present`), when they changed, so the
//!   server's map lets go of files that were deleted. Paths only.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::{self, Env};

/// The first look reaches back this far.
const LOOKBACK_DAYS: u64 = 30;
/// The most commits one look reads.
const MAX_COMMITS: usize = 300;
/// The most moves one report sends.
const MAX_MOVES: usize = 200;
/// git is given this long; past it the look is dropped, and tried again.
const GIT_BUDGET: Duration = Duration::from_secs(5);
/// A checkout is looked at no more often than this.
const LOOK_EVERY_S: f64 = 600.0;
/// An old checkout's remote is asked about again no sooner than this.
const ASK_REMOTE_EVERY_S: f64 = 86_400.0;
/// The most old checkouts' remotes one look asks about.
const MAX_ASKED: usize = 3;

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

// --------------------------------------------------------------- parsing

/// One rename git detected: (old path, new path, commit, commit time).
pub type Pair = (String, String, String, f64);

/// The pairs in `git log --name-status --format=%x00%H %ct` output.
pub fn parse_log(log: &str) -> Vec<Pair> {
    let mut out = Vec::new();
    let (mut commit, mut at) = (String::new(), 0.0);
    for line in log.lines() {
        if let Some(head) = line.strip_prefix('\0') {
            let mut parts = head.split_whitespace();
            commit = parts.next().unwrap_or("").to_string();
            at = parts.next().and_then(|t| t.parse::<f64>().ok()).unwrap_or(0.0);
            continue;
        }
        if !line.starts_with('R') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        // a quoted path (control characters in a name) is left alone
        if fields.len() != 3 || fields[1].starts_with('"') || fields[2].starts_with('"') || commit.is_empty() {
            continue;
        }
        out.push((fields[1].to_string(), fields[2].to_string(), commit.clone(), at));
    }
    out
}

/// The smallest move that explains each pair, grouped: folder moves (a
/// prefix pair shared by two files or more) and file moves, folders first
/// and the most files first, at most [`MAX_MOVES`].
pub fn collapse(pairs: &[Pair]) -> Vec<Value> {
    // (from folder, to folder) -> each file's path inside the folder (its
    // directories only), and the newest commit among them
    let mut folders: BTreeMap<(String, String), (Vec<Vec<String>>, String, f64, Vec<usize>)> = BTreeMap::new();
    let mut files: Vec<usize> = Vec::new();
    for (index, (old, new, commit, at)) in pairs.iter().enumerate() {
        let o: Vec<&str> = old.split('/').collect();
        let n: Vec<&str> = new.split('/').collect();
        // the longest common suffix of path components, short of the whole
        // of either: what is left in front is what moved
        let most = o.len().min(n.len()).saturating_sub(1);
        let mut k = 0;
        while k < most && o[o.len() - 1 - k] == n[n.len() - 1 - k] {
            k += 1;
        }
        if k == 0 {
            files.push(index);
            continue;
        }
        let from = format!("{}/", o[..o.len() - k].join("/"));
        let to = format!("{}/", n[..n.len() - k].join("/"));
        let inside: Vec<String> = o[o.len() - k..o.len() - 1].iter().map(|s| s.to_string()).collect();
        let slot = folders.entry((from, to)).or_insert_with(|| (Vec::new(), String::new(), f64::MIN, Vec::new()));
        slot.0.push(inside);
        slot.3.push(index);
        if *at > slot.2 {
            slot.1 = commit.clone();
            slot.2 = *at;
        }
    }
    let mut out: Vec<Value> = Vec::new();
    for ((from, to), (insides, commit, at, members)) in folders {
        if members.len() < 2 {
            files.extend(members);
            continue;
        }
        // every file sat deeper in one folder: that folder is what moved
        // (lib/foo/x.rs and lib/foo/y.rs to src/foo/ moved lib/foo/, not lib/)
        let mut common: Vec<String> = insides[0].clone();
        for inside in &insides[1..] {
            let shared = common.iter().zip(inside).take_while(|(a, b)| a == b).count();
            common.truncate(shared);
        }
        let deeper = if common.is_empty() { String::new() } else { format!("{}/", common.join("/")) };
        out.push(json!({
            "from": format!("{from}{deeper}"), "to": format!("{to}{deeper}"),
            "files": members.len(), "commit": commit, "at": at,
        }));
    }
    out.sort_by(|a, b| b["files"].as_u64().cmp(&a["files"].as_u64()));
    // a file renamed twice is sent once per pair; newest first
    files.sort_by(|a, b| pairs[*b].3.partial_cmp(&pairs[*a].3).unwrap_or(std::cmp::Ordering::Equal));
    for index in files {
        let (old, new, commit, at) = &pairs[index];
        if out.iter().any(|m| m["from"] == old.as_str() && m["to"] == new.as_str()) {
            continue;
        }
        out.push(json!({"from": old, "to": new, "files": 1, "commit": commit, "at": at}));
    }
    out.truncate(MAX_MOVES);
    out
}

// ---------------------------------------------------------------- git

/// Run git in `root`, killed with everything it started past `budget`.
fn git_timed(root: &Path, args: &[&str], budget: Duration) -> Option<String> {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    crate::git::own_group(&mut command);
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let started = Instant::now();
    let out = rx.recv_timeout(budget).ok();
    let Some(out) = out else {
        crate::git::kill_group(&mut child);
        return None;
    };
    let left = budget.saturating_sub(started.elapsed()).max(Duration::from_millis(100));
    let waited = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success().then(|| String::from_utf8_lossy(&out).to_string()),
            Ok(None) if waited.elapsed() < left => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                crate::git::kill_group(&mut child);
                return None;
            }
        }
    }
}

/// The renames git detected in commits on HEAD since `last` (or, without
/// one, the last [`LOOKBACK_DAYS`] days), at most [`MAX_COMMITS`]
/// commits; and the commit HEAD was at. `None` when git did not answer.
pub fn scan(root: &Path, last: &str) -> Option<(Vec<Pair>, String)> {
    let head = git_timed(root, &["rev-parse", "HEAD"], GIT_BUDGET)?.trim().to_string();
    if head.is_empty() {
        return None;
    }
    if head == last {
        return Some((Vec::new(), head));
    }
    let since = format!("--since={LOOKBACK_DAYS}.days");
    let count = MAX_COMMITS.to_string();
    let range = format!("{last}..{head}");
    let base = ["-c", "core.quotepath=off", "log", "-M", "-l2000", "--diff-filter=R", "--name-status", "--format=%x00%H %ct", since.as_str(), "-n", count.as_str()];
    let mut args: Vec<&str> = base.to_vec();
    args.push(if last.is_empty() { head.as_str() } else { range.as_str() });
    // a marker git no longer knows (history rewritten): the window alone
    let log = match git_timed(root, &args, GIT_BUDGET) {
        Some(log) => log,
        None if !last.is_empty() => {
            let mut args: Vec<&str> = base.to_vec();
            args.push(head.as_str());
            git_timed(root, &args, GIT_BUDGET)?
        }
        None => return None,
    };
    Some((parse_log(&log), head))
}

// ------------------------------------------------------------- the marker

fn marker_path(root: &Path, env: &Env) -> PathBuf {
    let tag = format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes()));
    config::data_dir(env).join("renames").join(format!("{}.json", &tag[..16]))
}

/// At most every ten minutes per checkout, look for renames in a detached
/// process: the session never waits on git.
pub fn spawn_if_due(root: &Path, env: &Env) {
    if config::get(env, "CORK_RENAMES").trim() == "0" {
        return;
    }
    let marker = marker_path(root, env);
    let looked = config::load_json(&marker).get("looked").and_then(Value::as_f64).unwrap_or(0.0);
    if now_s() - looked < LOOK_EVERY_S {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = std::process::Command::new(exe);
    command
        .arg("path-moves")
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// What one look found, and the marker it leaves when what it found was
/// delivered: `send` gets the moves (if any) and says whether they landed.
pub fn look(root: &Path, env: &Env, send: impl FnOnce(&[Value]) -> bool) -> Vec<Value> {
    let marker = marker_path(root, env);
    let mut state = config::load_json(&marker);
    if !state.is_object() {
        state = json!({});
    }
    state["looked"] = json!(now_s());
    crate::report::save_json(&marker, &state);
    let Some((pairs, head)) = scan(root, &text(&state, "head")) else { return Vec::new() };
    let moves = collapse(&pairs);
    // the marker moves on only once the server has them: a failed send is
    // sent again by the next look
    if moves.is_empty() || send(&moves) {
        state["head"] = json!(head);
        crate::report::save_json(&marker, &state);
    }
    moves
}

// ------------------------------------------------------ what the trunk has

/// The most paths one list sends; past it the map is left as it is.
const MAX_PRESENT: usize = 50_000;
/// An unchanged list goes again after this long, so a file another checkout
/// brought into the map that the trunk never had still leaves it.
const PRESENT_EVERY_S: f64 = 86_400.0;

/// The branch the repo's work lands on is the one checked out here: the
/// remote's default (`origin/HEAD`), else `main` or `master` when the remote
/// names none. Only the trunk's files say what the repo has: a branch lacks
/// what was added since it was made.
fn on_trunk(root: &Path) -> bool {
    let branch = crate::branches::current(root);
    if branch.is_empty() {
        return false;
    }
    let default = git_timed(root, &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"], GIT_BUDGET)
        .map(|name| name.trim().trim_start_matches("origin/").to_string())
        .unwrap_or_default();
    if default.is_empty() { matches!(branch.as_str(), "main" | "master") } else { branch == default }
}

/// What the map should hold, as the trunk checked out here has it: every
/// source path git tracks, and when HEAD was made (the server lets go of a
/// file only if nobody wrote or read it since). With the list's fingerprint,
/// for the marker. `None` off the trunk, when git does not answer, or when
/// the list went unchanged less than [`PRESENT_EVERY_S`] ago. The map only
/// ever learned files, so every file deleted or moved stayed in it.
pub fn present(root: &Path, state: &Value) -> Option<(Value, String)> {
    if !on_trunk(root) {
        return None;
    }
    let head_ts: f64 = git_timed(root, &["log", "-1", "--format=%ct", "HEAD"], GIT_BUDGET)?.trim().parse().ok()?;
    let mut paths = crate::report::tracked_sources(root);
    if paths.is_empty() || paths.len() > MAX_PRESENT {
        return None;
    }
    paths.sort();
    let tag = format!("{:x}", Sha256::digest(paths.join("\n").as_bytes()))[..16].to_string();
    let sent_at = state.get("present_at").and_then(Value::as_f64).unwrap_or(0.0);
    if text(state, "present_fp") == tag && now_s() - sent_at < PRESENT_EVERY_S {
        return None;
    }
    Some((json!({"paths": paths, "head_ts": head_ts}), tag))
}

/// Send [`present`] when it is due, and mark it sent once the server took it.
fn send_present(root: &Path, env: &Env, send: impl FnOnce(&Value) -> bool) -> bool {
    let marker = marker_path(root, env);
    let mut state = config::load_json(&marker);
    if !state.is_object() {
        state = json!({});
    }
    let Some((present, tag)) = present(root, &state) else { return false };
    if !send(&present) {
        return false;
    }
    state["present_fp"] = json!(tag);
    state["present_at"] = json!(now_s());
    crate::report::save_json(&marker, &state);
    true
}

/// `cork-hook path-moves <root>`: look, and send what moved, and, on the
/// trunk, what the checkout has.
pub fn command(root: &str, env: &Env) -> i32 {
    let root = if root.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(root) };
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let post = |body: &Value| {
        crate::http::post(&cfg.server, "/renames", &cfg.token, &crate::report::user_agent(), body, Duration::from_secs(20))
            .is_ok_and(|answer| answer.get("ok").and_then(Value::as_bool) == Some(true))
    };
    look(&root, env, |moves| post(&json!({"repo_id": cfg.repo_id, "moves": moves, "agent": "cork-hook"})));
    send_present(&root, env, |present| post(&json!({"repo_id": cfg.repo_id, "present": present, "agent": "cork-hook"})));
    // a clone whose folder moved before checkouts kept an identity: asked
    // of the old name's remote here, where the session never waits on it
    if !cfg.machine_local {
        if let Some(old) = renamed_from_remote(&root, &cfg.repo_id, env) {
            send_repo_rename(&old, &cfg, env);
        }
    }
    0
}

// ----------------------------------------------------------- repo renames

/// When this checkout was cloned, or as close as git still says: the
/// oldest entry of HEAD's reflog, and the `.git` folder's birth where the
/// system keeps one. `None` when neither is known.
fn cloned_at(root: &Path) -> Option<f64> {
    let git = root.join(".git");
    let mut best: Option<f64> = None;
    if let Ok(log) = std::fs::read_to_string(git.join("logs").join("HEAD")) {
        // "<old> <new> Name <email> <unix time> <tz>\t<message>"
        if let Some(first) = log.lines().next() {
            let head = first.split('\t').next().unwrap_or("");
            let parts: Vec<&str> = head.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(ts) = parts[parts.len() - 2].parse::<f64>() {
                    best = Some(ts);
                }
            }
        }
    }
    if let Some(born) = std::fs::metadata(&git).and_then(|m| m.created()).ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
    {
        let born = born.as_secs_f64();
        best = Some(best.map_or(born, |b| b.min(born)));
    }
    best
}

fn base_id(repo_id: &str) -> &str {
    repo_id.split('#').next().unwrap_or(repo_id)
}

/// The repo id this folder was before it was `repo_id`: an entry of the
/// machine's checkouts (`listen.rs`) for another id, made after this clone
/// was already here (a folder deleted and cloned again from another repo is
/// not a rename), at the same folder (the origin changed) or of the same
/// clone by its `.git` identity (the folder moved too). The newest such
/// entry; `None` when there is none.
pub fn renamed_from(root: &Path, repo_id: &str, env: &Env) -> Option<String> {
    // a linked worktree is not where a repo lives
    if repo_id.is_empty() || !root.join(".git").is_dir() {
        return None;
    }
    let all = crate::listen::checkouts_all(env);
    let folder = root.to_string_lossy().to_string();
    let identity = crate::listen::git_identity(root);
    let mut best: Option<(f64, String)> = None;
    for (id, row) in all.as_object().into_iter().flatten() {
        if id == repo_id || base_id(id) == base_id(repo_id) {
            continue;
        }
        let theirs = text(row, "git");
        // another clone, by its identity: not this one renamed
        if !identity.is_empty() && !theirs.is_empty() && theirs != identity {
            continue;
        }
        if text(row, "root") != folder && (identity.is_empty() || theirs != identity) {
            continue;
        }
        let seen = row.get("seen").and_then(Value::as_f64).unwrap_or(0.0);
        if best.as_ref().is_none_or(|(at, _)| seen > *at) {
            best = Some((seen, id.clone()));
        }
    }
    let (seen, old) = best?;
    let cloned = cloned_at(root)?;
    (cloned < seen).then_some(old)
}

/// The URL `old_id` had, spelled like this checkout's origin (ssh or
/// https); `None` when the hosts differ.
fn old_url(origin: &str, old_id: &str) -> Option<String> {
    let (host, path) = base_id(old_id).split_once('/')?;
    let origin = origin.trim();
    if crate::git::normalize_repo_url(origin).split('/').next() != Some(host) {
        return None;
    }
    Some(if origin.starts_with("git@") || origin.starts_with("ssh://") {
        format!("git@{host}:{path}.git")
    } else {
        format!("https://{host}/{path}.git")
    })
}

/// The commit a remote's HEAD names; `None` when it did not answer.
fn remote_head(root: &Path, url: &str) -> Option<String> {
    let out = git_timed(root, &["ls-remote", url, "HEAD"], GIT_BUDGET)?;
    out.split_whitespace().next().filter(|sha| sha.len() >= 40).map(str::to_string)
}

fn asked_path(env: &Env) -> PathBuf {
    config::data_dir(env).join("renames").join("asked.json")
}

/// The repo id this clone was before its folder moved, for entries noted
/// before checkouts kept a `.git` identity (`renamed_from` cannot tell
/// those apart from a deleted repo's). Proven, never guessed: the old
/// name's URL still serves this repo (a host redirects a renamed repo), its
/// HEAD a commit this clone has or origin's own. Asked in the detached look,
/// of entries whose folder is gone, newest first, each at most daily.
pub fn renamed_from_remote(root: &Path, repo_id: &str, env: &Env) -> Option<String> {
    if repo_id.is_empty() || !root.join(".git").is_dir() {
        return None;
    }
    let cloned = cloned_at(root)?;
    let all = crate::listen::checkouts_all(env);
    let mut asked = config::load_json(&asked_path(env));
    if !asked.is_object() {
        asked = json!({});
    }
    let now = now_s();
    let mut candidates: Vec<(f64, String)> = Vec::new();
    for (id, row) in all.as_object().into_iter().flatten() {
        let (was, seen) = (text(row, "root"), row.get("seen").and_then(Value::as_f64).unwrap_or(0.0));
        let fresh = now - asked.get(id).and_then(Value::as_f64).unwrap_or(0.0) >= ASK_REMOTE_EVERY_S;
        if id != repo_id && base_id(id) != base_id(repo_id) && text(row, "git").is_empty()
            && !was.is_empty() && !Path::new(&was).exists() && cloned < seen && fresh
        {
            candidates.push((seen, id.clone()));
        }
    }
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let origin = git_timed(root, &["config", "--get", "remote.origin.url"], GIT_BUDGET)?;
    let mut origin_head: Option<Option<String>> = None;
    let mut found = None;
    for (_, old) in candidates.into_iter().take(MAX_ASKED) {
        asked[old.as_str()] = json!(now);
        let Some(head) = old_url(&origin, &old).and_then(|url| remote_head(root, &url)) else { continue };
        let have = git_timed(root, &["cat-file", "-e", &format!("{head}^{{commit}}")], GIT_BUDGET).is_some();
        if have || origin_head.get_or_insert_with(|| remote_head(root, origin.trim())).as_deref() == Some(head.as_str()) {
            found = Some(old);
            break;
        }
    }
    crate::report::save_json(&asked_path(env), &asked);
    found
}

/// Tell the server this checkout's repo was `old` and is now
/// `cfg.repo_id`, and once it has heard, drop the old entry so it is said
/// once.
fn send_repo_rename(old: &str, cfg: &config::Config, env: &Env) {
    let body = json!({"repo_id": cfg.repo_id, "renamed_from": old, "agent": "cork-hook"});
    let answered = crate::http::post(&cfg.server, "/renames", &cfg.token, &crate::report::user_agent(), &body, Duration::from_secs(3));
    // heard (carried over, waiting on the owner, or nothing to carry): said
    // once. Refused or unreachable: said again at the next start
    if answered.is_ok_and(|a| a.get("ok").and_then(Value::as_bool) == Some(true)) {
        crate::listen::forget_checkout(old, env);
    }
}

/// At session start, before anything is reported under the new id: a
/// rename this machine can tell without asking anyone (`renamed_from`).
pub fn report_repo_rename(root: &Path, cfg: &config::Config, env: &Env) {
    if let Some(old) = renamed_from(root, &cfg.repo_id, env) {
        send_repo_rename(&old, cfg, env);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn sh(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    fn repo(name: &str) -> (PathBuf, Env) {
        let base = std::env::temp_dir().join(format!("cork-renames-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("checkout");
        std::fs::create_dir_all(root.join("server/collide-rs/src")).unwrap();
        std::fs::create_dir_all(base.join("home")).unwrap();
        sh(&root, &["init", "-q"]);
        for (path, body) in [
            ("server/collide-rs/Cargo.toml", "[package]\nname = \"collide\"\n"),
            ("server/collide-rs/src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n"),
            ("server/collide-rs/src/store.rs", "pub fn open() -> u32 {\n    42\n}\n"),
            ("docs/old-name.md", "# Notes\n\nSome words that stay the same.\n"),
            ("README.md", "readme\n"),
        ] {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), body).unwrap();
        }
        sh(&root, &["add", "-A"]);
        sh(&root, &["commit", "-q", "-m", "start"]);
        let mut env = Env::new();
        env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
        (root, env)
    }

    #[test]
    fn a_folder_moved_with_git_mv_is_one_folder_move_and_a_lone_rename_a_file_move() {
        let (root, env) = repo("moves");
        std::fs::create_dir_all(root.join("server")).unwrap();
        sh(&root, &["mv", "server/collide-rs", "server/cork-rs"]);
        sh(&root, &["mv", "docs/old-name.md", "docs/new-name.md"]);
        sh(&root, &["commit", "-q", "-m", "rename"]);

        let mut sent: Vec<Value> = Vec::new();
        let moves = look(&root, &env, |m| {
            sent = m.to_vec();
            true
        });
        assert_eq!(moves, sent);
        assert_eq!(moves.len(), 2, "{moves:?}");
        assert_eq!(moves[0]["from"], "server/collide-rs/");
        assert_eq!(moves[0]["to"], "server/cork-rs/");
        assert_eq!(moves[0]["files"], 3);
        assert_eq!(moves[0]["commit"].as_str().unwrap().len(), 40);
        assert!(moves[0]["at"].as_f64().unwrap() > 1.0e9);
        assert_eq!(moves[1], json!({"from": "docs/old-name.md", "to": "docs/new-name.md", "files": 1,
                                    "commit": moves[0]["commit"], "at": moves[0]["at"]}));

        // the marker: a second look sends nothing new
        let again = look(&root, &env, |_| panic!("nothing new to send"));
        assert!(again.is_empty(), "{again:?}");

        // a later move is found on its own
        sh(&root, &["mv", "README.md", "docs/README.md"]);
        sh(&root, &["commit", "-q", "-m", "move the readme"]);
        let later = look(&root, &env, |_| true);
        assert_eq!(later.len(), 1, "{later:?}");
        assert_eq!((later[0]["from"].as_str(), later[0]["to"].as_str()), (Some("README.md"), Some("docs/README.md")));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn a_send_that_failed_is_sent_again() {
        let (root, env) = repo("retry");
        sh(&root, &["mv", "docs/old-name.md", "docs/new-name.md"]);
        sh(&root, &["commit", "-q", "-m", "rename"]);
        assert_eq!(look(&root, &env, |_| false).len(), 1);
        assert_eq!(look(&root, &env, |_| true).len(), 1, "the marker did not move");
        assert!(look(&root, &env, |_| true).is_empty());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// Cork's own map kept `server/collide-rs/src/billing.rs` long after the
    /// folder became `server/cork-rs`, and every file anyone deleted: the
    /// index only ever sent files. The trunk now says what it has.
    #[test]
    fn the_trunk_sends_what_it_tracks_when_that_changed_and_a_branch_never_does() {
        let (root, env) = repo("present");
        sh(&root, &["checkout", "-q", "-B", "main"]);
        let mut sent: Vec<Value> = Vec::new();
        assert!(send_present(&root, &env, |p| {
            sent.push(p.clone());
            true
        }));
        assert_eq!(sent[0]["paths"], json!(["server/collide-rs/src/main.rs", "server/collide-rs/src/store.rs"]), "sources only");
        assert!(sent[0]["head_ts"].as_f64().unwrap() > 1.0e9);
        // unchanged: not again
        assert!(!send_present(&root, &env, |_| panic!("nothing changed")));

        // a file deleted on the trunk: the list goes again, without it
        sh(&root, &["rm", "-q", "server/collide-rs/src/store.rs"]);
        sh(&root, &["commit", "-q", "-m", "drop the store"]);
        assert!(!send_present(&root, &env, |_| false), "the server did not take it");
        assert!(send_present(&root, &env, |p| {
            sent.push(p.clone());
            true
        }), "sent again: the marker did not move");
        assert_eq!(sent.last().unwrap()["paths"], json!(["server/collide-rs/src/main.rs"]));

        // a branch lacks what others added since it was made: it never says
        sh(&root, &["checkout", "-q", "-b", "feature"]);
        sh(&root, &["rm", "-q", "server/collide-rs/src/main.rs"]);
        sh(&root, &["commit", "-q", "-m", "on a branch"]);
        let marker = marker_path(&root, &env);
        let _ = std::fs::remove_file(&marker);
        assert!(!send_present(&root, &env, |_| panic!("a branch's list is not the repo's")));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    fn pair(old: &str, new: &str) -> Pair {
        (old.to_string(), new.to_string(), "c1".to_string(), 10.0)
    }

    #[test]
    fn pairs_collapse_to_the_smallest_move_that_explains_them() {
        // only lib/foo moved: not all of lib/
        let moves = collapse(&[pair("lib/foo/x.rs", "src/foo/x.rs"), pair("lib/foo/y/z.rs", "src/foo/y/z.rs")]);
        assert_eq!((moves[0]["from"].as_str(), moves[0]["to"].as_str()), (Some("lib/foo/"), Some("src/foo/")));
        // a folder moved into another
        let moves = collapse(&[pair("a/x.rs", "b/a/x.rs"), pair("a/y.rs", "b/a/y.rs")]);
        assert_eq!((moves[0]["from"].as_str(), moves[0]["to"].as_str()), (Some("a/"), Some("b/a/")));
        // one file of a "folder" pair is a file move, with its whole path
        let moves = collapse(&[pair("one/x.rs", "two/x.rs")]);
        assert_eq!(moves, vec![json!({"from": "one/x.rs", "to": "two/x.rs", "files": 1, "commit": "c1", "at": 10.0})]);
        // a log line of git's own
        let log = "\0abc123 1790000000\n\nR100\tserver/collide-rs/a.rs\tserver/cork-rs/a.rs\nR087\tserver/collide-rs/b/c.rs\tserver/cork-rs/b/c.rs\nM\tREADME.md\n";
        let moves = collapse(&parse_log(log));
        assert_eq!(moves, vec![json!({"from": "server/collide-rs/", "to": "server/cork-rs/", "files": 2, "commit": "abc123", "at": 1.79e9})]);
    }

    #[test]
    fn a_folder_that_was_another_repo_id_is_a_repo_rename() {
        let (root, env) = repo("repoid");
        let data = config::data_dir(&env);
        std::fs::create_dir_all(&data).unwrap();
        let folder = root.to_string_lossy().to_string();
        let later = now_s() + 60.0;
        std::fs::write(data.join("checkouts.json"), json!({
            "github.com/lithometric/collide-cloud": {"root": folder, "server": "https://mcp.collidemcp.com", "seen": later},
            "github.com/lithometric/elsewhere": {"root": "/somewhere/else", "server": "https://mcp.corkcli.com", "seen": later},
        }).to_string()).unwrap();
        assert_eq!(renamed_from(&root, "github.com/lithometric/cork-cloud", &env).as_deref(), Some("github.com/lithometric/collide-cloud"));
        assert_eq!(renamed_from(&root, "github.com/lithometric/collide-cloud", &env), None, "the same id is no rename");
        assert_eq!(renamed_from(&root, "github.com/lithometric/collide-cloud#dev", &env), None, "a branch scope is no rename");

        // an entry made before this clone existed: a folder cloned again, not a rename
        std::fs::write(data.join("checkouts.json"), json!({
            "github.com/lithometric/collide-cloud": {"root": folder, "server": "https://mcp.corkcli.com", "seen": 1000.0},
        }).to_string()).unwrap();
        assert_eq!(renamed_from(&root, "github.com/lithometric/cork-cloud", &env), None);

        // noting the checkout under its new id, and forgetting the old
        std::fs::write(data.join("checkouts.json"), json!({
            "github.com/lithometric/collide-cloud": {"root": folder, "server": "https://mcp.corkcli.com", "seen": later},
        }).to_string()).unwrap();
        crate::listen::forget_checkout("github.com/lithometric/collide-cloud", &env);
        crate::listen::note_checkout(&root, "https://mcp.corkcli.com", "github.com/lithometric/cork-cloud", &env);
        let all = crate::listen::checkouts_all(&env);
        assert!(all.get("github.com/lithometric/collide-cloud").is_none(), "{all}");
        assert_eq!(all["github.com/lithometric/cork-cloud"]["root"], folder.as_str());
        assert_eq!(renamed_from(&root, "github.com/lithometric/cork-cloud", &env), None, "said once");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn a_clone_whose_folder_moved_too_is_a_repo_rename() {
        let (root, env) = repo("moved");
        let data = config::data_dir(&env);
        std::fs::create_dir_all(&data).unwrap();
        let later = now_s() + 60.0;
        let identity = crate::listen::git_identity(&root);
        assert!(!identity.is_empty());
        std::fs::write(data.join("checkouts.json"), json!({
            "github.com/lithometric/collide-cloud": {"root": root.to_string_lossy(), "server": "https://mcp.corkcli.com", "seen": later, "git": identity},
        }).to_string()).unwrap();
        let moved = root.with_file_name("cork-cloud");
        std::fs::rename(&root, &moved).unwrap();
        assert_eq!(crate::listen::git_identity(&moved), identity, "a move keeps the identity");
        assert_eq!(renamed_from(&moved, "github.com/lithometric/cork-cloud", &env), Some("github.com/lithometric/collide-cloud".into()));

        // another clone at the old folder's place is not this one
        std::fs::write(data.join("checkouts.json"), json!({
            "github.com/lithometric/collide-cloud": {"root": moved.to_string_lossy(), "server": "https://mcp.corkcli.com", "seen": later, "git": "1:2"},
        }).to_string()).unwrap();
        assert_eq!(renamed_from(&moved, "github.com/lithometric/cork-cloud", &env), None);
        let _ = std::fs::remove_dir_all(moved.parent().unwrap());
    }

    #[test]
    fn an_old_entry_whose_folder_is_gone_is_a_rename_only_when_its_remote_serves_this_repo() {
        let (root, env) = repo("remote");
        let base = root.parent().unwrap().to_path_buf();
        // the "host": bare repos at file URLs, the old name redirecting
        // (here: a copy serving the same commits), another repo apart
        let host = base.join("host");
        std::fs::create_dir_all(&host).unwrap();
        for name in ["cork-cloud", "collide-cloud"] {
            sh(&base, &["clone", "-q", "--bare", &root.to_string_lossy(), &host.join(format!("{name}.git")).to_string_lossy()]);
        }
        let other = base.join("other");
        std::fs::create_dir_all(&other).unwrap();
        sh(&other, &["init", "-q"]);
        std::fs::write(other.join("x"), "x").unwrap();
        sh(&other, &["add", "-A"]);
        sh(&other, &["commit", "-q", "-m", "other"]);
        sh(&base, &["clone", "-q", "--bare", &other.to_string_lossy(), &host.join("maxq.git").to_string_lossy()]);
        let url = format!("file://{}/cork-cloud.git", host.to_string_lossy());
        sh(&root, &["remote", "add", "origin", &url]);
        let origin = |id: &str| format!("file://{}/{}.git", host.to_string_lossy(), id.rsplit('/').next().unwrap());
        assert_eq!(old_url(&url, "github.com/lithometric/collide-cloud"), None, "another host");
        assert_eq!(old_url("git@github.com:lithometric/cork-cloud.git", "github.com/lithometric/collide-cloud").as_deref(),
                   Some("git@github.com:lithometric/collide-cloud.git"));
        assert_eq!(old_url("https://github.com/lithometric/cork-cloud", "github.com/lithometric/collide-cloud").as_deref(),
                   Some("https://github.com/lithometric/collide-cloud.git"));
        assert_eq!(remote_head(&root, &origin("collide-cloud")), remote_head(&root, &url));
        assert_ne!(remote_head(&root, &origin("maxq")), remote_head(&root, &url));
        assert!(git_timed(&root, &["cat-file", "-e", &format!("{}^{{commit}}", remote_head(&root, &origin("maxq")).unwrap())], GIT_BUDGET).is_none());

        // the walk: an old entry without an identity and a gone folder is
        // asked, and one whose remote does not answer is passed over
        let data = config::data_dir(&env);
        std::fs::create_dir_all(&data).unwrap();
        let later = now_s() + 60.0;
        let row = |folder: &str, seen: f64| json!({"root": base.join(folder).to_string_lossy(), "server": "https://mcp.corkcli.com", "seen": seen});
        std::fs::write(data.join("checkouts.json"), json!({
            "github.com/lithometric/collide-cloud": row("collide", later),
            "github.com/lithometric/maxq": row("maxq", later + 1.0),
            "github.com/lithometric/home": row("home", later + 2.0),
        }).to_string()).unwrap();
        // file URLs are not a host's: no candidate's URL can be spelled
        assert_eq!(renamed_from_remote(&root, "github.com/lithometric/cork-cloud", &env), None);
        let asked = config::load_json(&asked_path(&env));
        assert!(asked.get("github.com/lithometric/maxq").is_some() && asked.get("github.com/lithometric/collide-cloud").is_some(), "{asked}");
        assert!(asked.get("github.com/lithometric/home").is_none(), "a folder still there is not asked about: {asked}");
        // asked once a day: nothing is asked again
        assert_eq!(renamed_from_remote(&root, "github.com/lithometric/cork-cloud", &env), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
