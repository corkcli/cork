//! Trunk mode on this machine: agents work on the repo's trunk, and the
//! branches they do make land into it once their task is done.
//!
//! Agents branch for every feature: their defaults say never to commit to
//! the default branch, every parallel worktree needs a branch of its own,
//! and merging is left to the person, so branches pile up stacked on
//! unmerged branches and nobody can run one branch and see the work. Trunk
//! mode changes where work lands, never how many agents run.
//!
//! The setting is the server's (cork-rs trunk.rs), heard with every
//! briefing and kept here per repo (`trunk/<repo>.json`), so a tool call
//! never asks. With it on:
//!
//! - session start says so in three sentences, to every harness;
//! - a `git checkout -b`, `git switch -c` or `git worktree add` gets a
//!   one-line reminder in its tool result (never a block), and the new
//!   branch goes on the server's registry;
//! - a branch whose task is done lands into the trunk (`land_branch`): a
//!   subagent or teammate stopped on it with everything committed, a
//!   session ended on it with a clean tree and commits ahead, or the agent
//!   ran `cork land`. Rebased onto the trunk, tested and pushed through the
//!   repo's push turn (land.rs), a stacked branch after the branch under it;
//!   then deleted, here and on origin, and its agent worktree removed. Never
//!   with uncommitted work, failing tests or a force push, and never into
//!   the promotion target: a conflict is recorded for the owner's next
//!   session and the dashboard, not resolved;
//! - at each prompt a session on the trunk hears that it moved, and is
//!   fast-forwarded when its tree is clean and only behind;
//! - `cork trunk`, `cork branches`, and `cork promote` (one pull request
//!   from the trunk to the promotion target, which Cork never merges).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::land::{git, git_out, lines, names};
use crate::worktrees::Worktree;

/// A session on the trunk fetches at most this often when the server says
/// nothing moved.
const FETCH_EVERY_S: f64 = 300.0;
/// A landing of a branch that started is not started again for this long.
const LANDING_LOCK_S: u64 = 900;
/// A dashboard request is acted on again after this long, if still asked.
const WANT_AGAIN_S: f64 = 600.0;
const REPORT_TIMEOUT: Duration = Duration::from_millis(1500);
const POLICIES: [&str; 3] = ["auto", "pr", "ask"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trunk {
    /// the branch agents work on and branches land into
    pub branch: String,
    /// where `cork promote` opens its pull request to
    pub promote_to: String,
    /// `auto`, `pr` or `ask`
    pub policy: String,
}

impl Trunk {
    /// How a finished branch lands: as the policy says, except that a trunk
    /// that is also the promotion target only ever gets pull requests.
    pub fn policy_for_branches(&self) -> &str {
        if self.branch == self.promote_to { "pr" } else { &self.policy }
    }
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn key_of(text: &str) -> String {
    crate::report::sha256_hex(text.as_bytes()).chars().take(16).collect()
}

fn dir(env: &Env) -> PathBuf {
    config::data_dir(env).join("trunk")
}

fn cache_path(repo_id: &str, env: &Env) -> PathBuf {
    dir(env).join(format!("{}.json", key_of(repo_id)))
}

/// Keep the setting a briefing carried (`trunk`), for every call that
/// never asks the server. Written only when it changed.
pub fn remember(repo_id: &str, answer: &Value, env: &Env) {
    let Some(trunk) = answer.get("trunk").filter(|t| t.is_object()) else { return };
    let path = cache_path(repo_id, env);
    if repo_id.is_empty() || config::load_json(&path) == *trunk {
        return;
    }
    let _ = std::fs::create_dir_all(dir(env));
    let _ = std::fs::write(&path, trunk.to_string());
}

/// The setting as last heard (`{}` when never).
pub fn heard(repo_id: &str, env: &Env) -> Value {
    config::load_json(&cache_path(repo_id, env))
}

/// The repo's trunk mode, when it is on.
pub fn active(root: &Path, env: &Env) -> Option<Trunk> {
    active_for(root, &config::config(Some(root), env).repo_id, env)
}

pub fn active_for(root: &Path, repo_id: &str, env: &Env) -> Option<Trunk> {
    if repo_id.is_empty() {
        return None;
    }
    from_setting(root, &heard(repo_id, env))
}

/// A setting as the server says it, with an unnamed trunk picked here:
/// `dev` when origin has it, else origin's default branch.
pub fn from_setting(root: &Path, setting: &Value) -> Option<Trunk> {
    if setting.get("on").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let promote_to = Some(text(setting, "promote_to")).filter(|p| !p.is_empty()).unwrap_or_else(|| "main".into());
    let policy = Some(text(setting, "policy")).filter(|p| POLICIES.contains(&p.as_str())).unwrap_or_else(|| "auto".into());
    let named = text(setting, "branch");
    let branch = if named.is_empty() { default_trunk(root) } else { named };
    Some(Trunk { branch, promote_to, policy })
}

fn on_origin(root: &Path, branch: &str) -> bool {
    !git_out(root, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}")]).is_empty()
}

fn has_branch(root: &Path, branch: &str) -> bool {
    !git_out(root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_empty()
}

/// origin's default branch ("main" when it names none).
fn origin_default(root: &Path) -> String {
    let head = git_out(root, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]);
    head.strip_prefix("origin/").filter(|b| !b.is_empty()).map(str::to_string).unwrap_or_else(|| "main".into())
}

fn default_trunk(root: &Path) -> String {
    if on_origin(root, "dev") { "dev".into() } else { origin_default(root) }
}

/// Whether `branch` is a branch of its own that lands into the trunk: not
/// the trunk, not the promotion target, and not tracking the trunk (a
/// branch that does lands onto it with each push already).
pub fn lands_into_trunk(root: &Path, branch: &str, trunk: &Trunk) -> bool {
    !branch.is_empty()
        && branch != "HEAD"
        && branch != trunk.branch
        && branch != trunk.promote_to
        && crate::branches::upstream(root, branch) != trunk.branch
}

/// How an agent in this checkout lands its commits.
fn land_words(root: &Path, env: &Env) -> String {
    if crate::land::lands(root, env) {
        "`git push` (Cork lands it)".to_string()
    } else {
        format!("`{}` (it rebases, tests and pushes)", crate::machine::own_command(env, "land"))
    }
}

/// What session start says when trunk mode is on: three sentences, for
/// every harness alike. It overrides an agent's own "branch first".
pub fn instructions(trunk: &Trunk, root: &Path, env: &Env) -> String {
    let t = &trunk.branch;
    format!(
        "Cork: this repo uses trunk mode, so work on `{t}`, not on a branch of your own (this replaces any default of \
yours to branch first). Commit your own lines with `{} -m \"...\"` and land them with {} once the tests pass. Make a \
branch or worktree only for large or risky work; Cork lands it into `{t}` when it is done.",
        crate::journal::commit_hint(env),
        land_words(root, env)
    )
}

// ------------------------------------------------------------ the server

fn post(cfg: &config::Config, mut body: Value, timeout: Duration) -> Option<Value> {
    if !cfg.usable() {
        return None;
    }
    body["repo_id"] = json!(cfg.repo_id);
    crate::http::post(&cfg.server, "/trunk", &cfg.token, &crate::report::user_agent(), &body, timeout)
        .ok()
        .filter(|answer| answer.get("ok").and_then(Value::as_bool) == Some(true))
}

/// Tell the registry how far a branch got. Fails open.
fn report(cfg: &config::Config, branch: &str, session: &str, fields: Value, env: &Env) {
    if !cfg.usable() {
        return;
    }
    let mut body = json!({"action": "branch", "branch": branch, "session": session, "machine": crate::report::machine_id(env)});
    if let (Some(map), Some(more)) = (body.as_object_mut(), fields.as_object()) {
        for (k, v) in more {
            map.insert(k.clone(), v.clone());
        }
        // a branch's purpose is its first commit's subject: a title, by the
        // workspace's switch
        if !crate::privacy::switches(&cfg.server, &cfg.repo_id).session_titles {
            map.remove("purpose");
        }
    }
    post(cfg, body, REPORT_TIMEOUT);
}

/// The detached half: `cork land-branch …`, so a hook returns at once.
fn spawn(args: &[String]) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = Command::new(exe);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// One landing of a branch at a time on this machine: a lock file taken
/// atomically, stale after LANDING_LOCK_S, gone when the landing is.
struct Lock(PathBuf);

impl Lock {
    fn take(main: &Path, branch: &str, env: &Env) -> Option<Lock> {
        let path = dir(env).join(format!("landing-{}.lock", key_of(&format!("{}\n{branch}", main.display()))));
        let _ = std::fs::create_dir_all(dir(env));
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age.as_secs() > LANDING_LOCK_S);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::OpenOptions::new().write(true).create_new(true).open(&path).ok().map(|_| Lock(path))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ------------------------------------------------------------ landing

#[derive(Debug, Clone, Default)]
pub struct LandOptions {
    /// the session it lands for (the registry's owner, the turn's holder)
    pub session: String,
    /// the test command (`--test`), else the repo's
    pub test: String,
    /// a person asked (`cork branches land`, the dashboard): it lands
    /// whatever the policy says
    pub human: bool,
    /// the main checkout stays on the branch: a subagent stopped, and its
    /// session is still at work there
    pub leave_main: bool,
}

/// The branch `branch` is stacked on that has not landed: one whose tip
/// is in `branch` and that has commits of its own not on the trunk (by
/// change: a landed commit was rebased, a new id). It lands first.
fn unlanded_base(main: &Path, branch: &str, trunk: &Trunk, upstream: &str) -> Option<String> {
    let tip = git_out(main, &["rev-parse", &format!("refs/heads/{branch}")]);
    let promoted = format!("origin/{}", trunk.promote_to);
    let limit = if on_origin(main, &trunk.promote_to) { git_out(main, &["merge-base", &promoted, &tip]) } else { String::new() };
    let under = lines(&git_out(main, &[
        "for-each-ref", "--count=300", &format!("--merged={tip}"), "--format=%(objectname) %(refname)", "refs/heads", "refs/remotes/origin",
    ]));
    for row in under {
        let Some((sha, full)) = row.split_once(' ') else { continue };
        let name = full.strip_prefix("refs/heads/").or_else(|| full.strip_prefix("refs/remotes/origin/")).unwrap_or(full);
        if sha == tip || [branch, trunk.branch.as_str(), trunk.promote_to.as_str(), "HEAD"].contains(&name) {
            continue;
        }
        // what came from the promotion target is not this branch's to land
        let mut cherry = vec!["cherry", upstream, full];
        if !limit.is_empty() {
            cherry.push(&limit);
        }
        if git_out(main, &cherry).lines().any(|l| l.starts_with('+')) {
            return Some(name.to_string());
        }
    }
    None
}

/// Whether an agent tool made this worktree (worktrees.rs): only those are
/// ever removed.
fn agent_made(main: &Path, wt: &Worktree, env: &Env) -> bool {
    crate::worktrees::agent_homes(main, env).iter().any(|home| wt.path.starts_with(home))
}

/// After a landing: the branch's agent worktree removed, and the branch
/// deleted here and on origin, each only when everything on it is on the
/// trunk. What it did, in a sentence or two ("" when nothing).
fn tidy_landed(main: &Path, branch: &str, upstream: &str, home: Option<&Worktree>, leave_main: bool, env: &Env) -> String {
    let t = upstream.strip_prefix("origin/").unwrap_or(upstream);
    if !git(main, &["merge-base", "--is-ancestor", &format!("refs/heads/{branch}"), upstream], 30).0 {
        return format!("`{branch}` is kept: it has commits that are not on `{t}`.");
    }
    let mut said: Vec<String> = Vec::new();
    let mut deletable = true;
    if let Some(wt) = home {
        let is_main = config::normpath(&wt.path) == config::normpath(main);
        if is_main && leave_main {
            deletable = false;
            said.push(format!("The main checkout stays on `{branch}`, which has everything landed."));
        } else if is_main {
            let switched = if has_branch(main, t) {
                let on = git(main, &["switch", "--quiet", t], 60).0;
                if on {
                    git(main, &["merge", "--ff-only", "--quiet", upstream], 60);
                }
                on
            } else {
                git(main, &["switch", "--quiet", "-c", t, "--track", upstream], 60).0
            };
            if switched {
                said.push(format!("This checkout is on `{t}` now."));
            } else {
                deletable = false;
                said.push(format!("This checkout stays on `{branch}`; `git switch {t}` when you are ready."));
            }
        } else if agent_made(main, wt, env) && !wt.locked {
            // no --force: git itself refuses a worktree with changes
            if git(main, &["worktree", "remove", &wt.path.to_string_lossy()], 60).0 {
                said.push("Its agent worktree was removed.".to_string());
            } else {
                deletable = false;
            }
        } else {
            deletable = false;
            said.push(format!("`{branch}` stays checked out in {}.", wt.path.display()));
        }
    }
    if deletable && git(main, &["branch", "-q", "-D", branch], 30).0 {
        let mut gone = format!("Deleted `{branch}`");
        if on_origin(main, branch) {
            let remote = format!("refs/remotes/origin/{branch}");
            let unlanded = git_out(main, &["cherry", upstream, &remote]).lines().any(|l| l.starts_with('+'));
            if unlanded {
                gone.push_str(&format!("; origin's `{branch}` has commits not on `{t}`, so it stays"));
            } else if git(main, &["push", "--quiet", "origin", "--delete", branch], 60).0 {
                gone.push_str(" here and on origin");
            }
        }
        said.push(format!("{gone}."));
    }
    said.join(" ")
}

/// origin's web address (`https://github.com/acme/shop`), "" when it is not
/// a hosted remote.
fn web_url(root: &Path) -> String {
    let url = git_out(root, &["remote", "get-url", "origin"]);
    if url.is_empty() || crate::git::is_local_origin(&url) {
        return String::new();
    }
    let id = crate::git::normalize_repo_url(&url);
    if id.matches('/').count() >= 2 { format!("https://{id}") } else { String::new() }
}

/// The address that opens a pull request from `head` into `base`.
fn compare_url(root: &Path, base: &str, head: &str) -> String {
    let site = web_url(root);
    if site.is_empty() { String::new() } else { format!("{site}/compare/{base}...{head}?expand=1") }
}

/// The GitHub CLI when it is installed (`CORK_GH` names another).
fn gh(env: &Env) -> Option<String> {
    let named = config::get(env, "CORK_GH");
    let program = if named.is_empty() { "gh".to_string() } else { named.to_string() };
    let works = Command::new(&program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    works.then_some(program)
}

/// (ok, its output's last line) of a `gh` call.
fn run_gh(program: &str, root: &Path, args: &[&str]) -> (bool, String) {
    match Command::new(program).args(args).current_dir(root).stdin(Stdio::null()).output() {
        Ok(out) => {
            let said = String::from_utf8_lossy(&out.stdout);
            (out.status.success(), said.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("").to_string())
        }
        Err(_) => (false, String::new()),
    }
}

/// A pull request from `head` into `base`: the one already open (its body
/// brought up to date when `update` is set), else a new one. Its address,
/// or "" when `gh` is not here or refused.
fn pull_request(gh: &str, root: &Path, base: &str, head: &str, title: &str, body: &str, update: bool) -> String {
    let (_, open) = run_gh(gh, root, &["pr", "list", "--base", base, "--head", head, "--state", "open", "--json", "url", "--jq", ".[0].url"]);
    if open.starts_with("http") {
        if update {
            run_gh(gh, root, &["pr", "edit", &open, "--body", body]);
        }
        return open;
    }
    let (ok, made) = run_gh(gh, root, &["pr", "create", "--base", base, "--head", head, "--title", title, "--body", body]);
    if ok && made.starts_with("http") { made } else { String::new() }
}

fn with_badge(body: String, env: &Env) -> String {
    if crate::machine::pr_badge_on(env) { format!("{body}\n\n{}", crate::machine::PR_BADGE_LINE) } else { body }
}

/// Land `branch` into the trunk: rebased onto it, tested, pushed through the
/// repo's push turn, then tidied. What happened, as (exit code, a line for
/// whoever reads it). The registry hears every outcome.
pub fn land_branch(root: &Path, branch: &str, trunk: &Trunk, opts: &LandOptions, env: &Env) -> (i32, String) {
    let t = trunk.branch.as_str();
    if branch.is_empty() || branch == "HEAD" {
        return (1, "Cork: HEAD is detached here, so there is no branch to land.".into());
    }
    if branch == t {
        return (1, format!("Cork: `{t}` is the trunk; `git push` lands commits on it."));
    }
    if branch == trunk.promote_to {
        return (1, format!("Cork: `{branch}` is the promotion target; nothing lands from it."));
    }
    let all = crate::worktrees::list(root);
    let main = all.first().map(|w| w.path.clone()).unwrap_or_else(|| root.to_path_buf());
    if !has_branch(&main, branch) {
        return (1, format!("Cork: there is no branch `{branch}` here."));
    }
    let Some(_lock) = Lock::take(&main, branch, env) else {
        return (0, format!("Cork: `{branch}` is already landing."));
    };
    let cfg = config::config(Some(&main), env);
    let home = all.iter().find(|w| w.branch == branch).cloned();
    if let Some(wt) = &home {
        if crate::hold::rebase_in_progress(&wt.path) {
            return (1, format!("Cork: a rebase is still stopped in {}; finish it, then land again.", wt.path.display()));
        }
        let dirty = lines(&git_out(&wt.path, &["diff", "HEAD", "--name-only", "--no-renames"]));
        if !dirty.is_empty() {
            return (1, format!(
                "Cork: `{branch}` has uncommitted work ({}), and nothing lands until it is committed: `{} -m \"...\"` commits your lines.",
                names(&dirty), crate::journal::commit_hint(env)
            ));
        }
    }
    git(&main, &["fetch", "origin", "--quiet"], 60);
    if !on_origin(&main, t) {
        return (1, format!("Cork: origin has no `{t}` branch yet; `cork trunk on --branch {t}` makes it."));
    }
    let upstream = format!("origin/{t}");
    let range = format!("{upstream}..refs/heads/{branch}");
    let ahead: usize = git_out(&main, &["rev-list", "--count", &range]).parse().unwrap_or(0);
    let purpose: String = git_out(&main, &["log", "--reverse", "--format=%s", &range]).lines().next().unwrap_or("").chars().take(200).collect();
    let say = |fields: Value| {
        let mut fields = fields;
        fields["ahead"] = fields.get("ahead").cloned().unwrap_or(json!(ahead));
        if !purpose.is_empty() {
            fields["purpose"] = json!(purpose);
        }
        report(&cfg, branch, &opts.session, fields, env);
    };
    if ahead == 0 {
        let tidied = tidy_landed(&main, branch, &upstream, home.as_ref(), opts.leave_main, env);
        say(json!({"status": "landed", "problem": "", "done": true}));
        return (0, format!("Cork: everything on `{branch}` is on `{t}` already. {tidied}").trim_end().to_string());
    }
    if let Some(under) = unlanded_base(&main, branch, trunk, &upstream) {
        say(json!({"status": "open", "base": under, "problem": ""}));
        return (0, format!("Cork: `{branch}` is stacked on `{under}`, which has not landed in `{t}` yet; it lands after `{under}` does."));
    }
    let policy = trunk.policy_for_branches();
    if policy == "pr" && (!opts.human || t == trunk.promote_to) {
        let (ok, _, err) = git(&main, &["push", "--quiet", "origin", &format!("refs/heads/{branch}:refs/heads/{branch}")], 120);
        if !ok {
            say(json!({"status": "open", "problem": "its push for a pull request was refused"}));
            return (1, format!("Cork could not push `{branch}` to open its pull request into `{t}`:\n{}", err.trim()));
        }
        let title = if purpose.is_empty() { branch.to_string() } else { purpose.clone() };
        let body = with_badge(format!("{ahead} commit(s) from `{branch}` for `{t}`. This repo lands finished branches as pull requests."), env);
        let made = gh(env).map(|gh| pull_request(&gh, &main, t, branch, &title, &body, false)).unwrap_or_default();
        let url = if made.is_empty() { compare_url(&main, t, branch) } else { made.clone() };
        say(json!({"status": "open", "pr": url, "pushed": true, "problem": ""}));
        return (0, match (made.is_empty(), url.is_empty()) {
            (false, _) => format!("Cork opened a pull request for `{branch}` into `{t}`: {url}"),
            (true, false) => format!("Cork pushed `{branch}`; its pull request into `{t}` opens here: {url}"),
            (true, true) => format!("Cork pushed `{branch}`; open a pull request from it into `{t}`."),
        });
    }
    if policy == "ask" && !opts.human {
        say(json!({"status": "open", "problem": ""}));
        return (0, format!(
            "Cork: `{branch}` is ready to land into `{t}` ({ahead} commit(s)). This repo asks first: `cork branches land {branch}`, or Land on the dashboard."
        ));
    }

    // the repo's push turn: branch landings queue with every other landing
    let server = crate::land::Server { cfg: config::config(Some(&main), env), session: opts.session.clone() };
    let (waited_for, _) = crate::land::take_turn(&server);
    let started = Instant::now();
    let release = |outcome: &str, extra: Value| {
        let mut body = json!({"action": "release", "outcome": outcome, "branch": branch, "target": t});
        if let (Some(map), Some(more)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in more {
                map.insert(k.clone(), v.clone());
            }
        }
        server.post(body);
    };
    // the work happens where the branch is checked out (its dependencies
    // are there), else in a worktree made for the landing
    let scratch = home.is_none().then(|| dir(env).join(format!("land-{}", key_of(&format!("{}\n{branch}", main.display())))));
    let work = match (&home, &scratch) {
        (Some(wt), _) => wt.path.clone(),
        (None, Some(path)) => {
            let _ = std::fs::remove_dir_all(path);
            let _ = std::fs::create_dir_all(dir(env));
            git(&main, &["worktree", "prune"], 30);
            let (ok, _, err) = git(&main, &["worktree", "add", "--quiet", "--detach", &path.to_string_lossy(), &format!("refs/heads/{branch}")], 120);
            if !ok {
                release("failed", json!({}));
                return (1, format!("Cork could not check `{branch}` out to land it: {}", err.trim()));
            }
            path.clone()
        }
        (None, None) => return (1, "Cork: nowhere to land from.".into()),
    };
    let drop_scratch = || {
        if let Some(path) = &scratch {
            // a worktree of Cork's own, made for this landing
            git(&main, &["worktree", "remove", "--force", &path.to_string_lossy()], 60);
            let _ = std::fs::remove_dir_all(path);
        }
    };
    let original = git_out(&work, &["rev-parse", "HEAD"]);
    // a landing that does not happen leaves the branch as it was
    let restore = || {
        if home.is_some() {
            git(&work, &["reset", "-q", "--keep", &original], 60);
        }
    };
    let test_cmd = crate::land::test_command(&main, &opts.test);
    let mut tested: Option<bool> = None;
    let mut over = 0usize;
    let mut rebases = 0u64;
    let mut authors: Vec<String> = Vec::new();
    for attempt in 0..crate::land::ATTEMPTS {
        if attempt > 0 {
            git(&main, &["fetch", "origin", "--quiet"], 60);
        }
        let incoming: usize = git_out(&work, &["rev-list", "--count", &format!("HEAD..{upstream}")]).parse().unwrap_or(0);
        if incoming > 0 {
            for name in lines(&git_out(&work, &["log", &format!("HEAD..{upstream}"), "--format=%an"])) {
                if !authors.contains(&name) {
                    authors.push(name);
                }
            }
            over += incoming;
            if !git(&work, &["rebase", "--quiet", &upstream], 120).0 {
                let files = lines(&git_out(&work, &["diff", "--name-only", "--diff-filter=U"]));
                // nobody is here to finish a rebase: it is undone, and told
                git(&work, &["rebase", "--abort"], 60);
                restore();
                drop_scratch();
                release("conflict", json!({}));
                say(json!({"status": "conflicted", "problem": format!("conflicts with {t} in {}", names(&files))}));
                return (1, format!(
                    "Cork could not land `{branch}` into `{t}`: its commits and {incoming} new commit(s) on `{t}` from {} change \
the same lines in {}. Nothing was pushed and `{branch}` is as it was: rebase it onto `{upstream}` keeping both sides' work, \
then land it again.",
                    names(&authors), names(&files)
                ));
            }
            rebases += 1;
            tested = None;
        }
        if tested.is_none() && !test_cmd.is_empty() {
            let (ok, output, _, timed_out) = crate::check::run_verify(&work, &test_cmd, crate::land::TEST_MAX_S);
            tested = Some(ok);
            if !ok {
                restore();
                drop_scratch();
                release("tests_failed", json!({}));
                let verdict = if timed_out { "timed out" } else { "failed" };
                say(json!({"status": "open", "problem": format!("`{test_cmd}` {verdict} on it rebased onto {t}")}));
                let tail: Vec<&str> = output.lines().rev().take(crate::land::TEST_TAIL_LINES).collect::<Vec<_>>().into_iter().rev().collect();
                return (1, format!(
                    "Cork did not land `{branch}`: rebased onto `{t}`, `{test_cmd}` {verdict}. Nothing was pushed and `{branch}` is as it was.\n{}",
                    tail.join("\n")
                ));
            }
        }
        let (ok, _, err) = git(&work, &["push", "--quiet", "origin", &format!("HEAD:refs/heads/{t}")], 120);
        if ok {
            let sha = git_out(&work, &["rev-parse", "HEAD"]);
            let short = git_out(&work, &["rev-parse", "--short", "HEAD"]);
            if scratch.is_some() {
                // not checked out anywhere: the branch moves to what landed
                git(&main, &["branch", "-q", "--force", branch, &sha], 30);
            }
            drop_scratch();
            release("landed", json!({
                "commit": short, "commits": ahead, "rebases": rebases, "over": over, "authors": authors,
                "tests": test_cmd, "tests_ok": tested, "seconds": started.elapsed().as_secs_f64(),
            }));
            let tidied = tidy_landed(&main, branch, &upstream, home.as_ref(), opts.leave_main, env);
            say(json!({"status": "landed", "ahead": 0, "commit": sha, "problem": "", "done": true}));
            let mut parts = vec![format!("Cork landed `{branch}` into `{t}` ({ahead} commit(s), {short}).")];
            if !waited_for.is_empty() {
                parts.push(format!("Waited for {waited_for}'s push."));
            }
            if over > 0 {
                parts.push(format!("Rebased onto {over} new commit(s) from {}.", names(&authors)));
            }
            parts.push(match tested {
                Some(true) => format!("`{test_cmd}` passed on the rebased tree."),
                _ => "No test command found, so it was not tested.".to_string(),
            });
            if !tidied.is_empty() {
                parts.push(tidied);
            }
            return (0, parts.join(" "));
        }
        let lost_race = ["rejected", "non-fast-forward", "fetch first", "stale info"].iter().any(|w| err.contains(w));
        if !lost_race {
            restore();
            drop_scratch();
            release("failed", json!({}));
            say(json!({"status": "open", "problem": format!("the push to {t} failed")}));
            return (1, format!("Cork: the push to `{t}` failed, so `{branch}` did not land:\n{}", err.trim()));
        }
    }
    restore();
    drop_scratch();
    release("gave_up", json!({}));
    (1, format!("Cork: `{t}` kept moving; {} pushes lost the race. `{branch}` is as it was; land it again.", crate::land::ATTEMPTS))
}

/// Delete a branch whose work is all on the trunk (by change, so a landed
/// rebase counts), here and on origin. Never one with unlanded commits.
pub fn delete_branch(root: &Path, branch: &str, trunk: &Trunk, session: &str, env: &Env) -> (i32, String) {
    let t = trunk.branch.as_str();
    if [t, trunk.promote_to.as_str(), "", "HEAD"].contains(&branch) {
        return (1, format!("Cork: `{branch}` is not a branch Cork deletes."));
    }
    let all = crate::worktrees::list(root);
    let main = all.first().map(|w| w.path.clone()).unwrap_or_else(|| root.to_path_buf());
    git(&main, &["fetch", "origin", "--quiet"], 60);
    let upstream = format!("origin/{t}");
    let (here, there) = (has_branch(&main, branch), on_origin(&main, branch));
    if !here && !there {
        return (1, format!("Cork: there is no branch `{branch}` here or on origin."));
    }
    for (exists, full, whose) in [(here, format!("refs/heads/{branch}"), "it"), (there, format!("refs/remotes/origin/{branch}"), "origin's copy")] {
        let unlanded = if exists { git_out(&main, &["cherry", &upstream, &full]).lines().filter(|l| l.starts_with('+')).count() } else { 0 };
        if unlanded > 0 {
            return (1, format!("Cork: `{branch}` is kept: {whose} has {unlanded} commit(s) not on `{t}`. `cork branches land {branch}` lands them first."));
        }
    }
    if let Some(wt) = all.iter().skip(1).find(|w| w.branch == branch) {
        if !(agent_made(&main, wt, env) && !wt.locked && git(&main, &["worktree", "remove", &wt.path.to_string_lossy()], 60).0) {
            return (1, format!("Cork: `{branch}` is checked out in {}; switch it away first.", wt.path.display()));
        }
    }
    if here && !git(&main, &["branch", "-q", "-D", branch], 30).0 {
        return (1, format!("Cork: `{branch}` is checked out here; `git switch {t}` first."));
    }
    if there {
        git(&main, &["push", "--quiet", "origin", "--delete", branch], 60);
    }
    let cfg = config::config(Some(&main), env);
    report(&cfg, branch, session, json!({"status": "landed", "ahead": 0, "problem": "", "done": true}), env);
    (0, format!("Cork deleted `{branch}`{}: everything on it is on `{t}`.", if there { " here and on origin" } else { "" }))
}

/// `land-branch <root> <branch> [--session S] [--test CMD] [--human]
/// [--leave-main] [--delete]`: the detached landing a hook started, or a
/// dashboard request.
pub fn land_branch_command(args: &[String], env: &Env) -> i32 {
    let root = PathBuf::from(args.first().cloned().unwrap_or_default());
    let branch = args.get(1).cloned().unwrap_or_default();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_default();
    let has = |name: &str| args.iter().any(|a| a == name);
    let Some(trunk) = active(&root, env) else { return 0 };
    let options = LandOptions { session: flag("--session"), test: flag("--test"), human: has("--human"), leave_main: has("--leave-main") };
    let (code, said) = if has("--delete") {
        delete_branch(&root, &branch, &trunk, &options.session, env)
    } else {
        land_branch(&root, &branch, &trunk, &options, env)
    };
    println!("{said}");
    code
}

// ------------------------------------------------------------ the hooks

/// The branch a command makes: `git checkout -b X`, `git switch -c X`, `git
/// worktree add [-b X] <path>` (a branch named after the folder when it
/// names none).
pub fn creates_branch(command: &str) -> Option<String> {
    for segment in crate::shell::shell_segments(command) {
        let words: Vec<&str> = segment.split_whitespace().map(|w| w.trim_matches(|c| c == '"' || c == '\'')).collect();
        let Some(at) = words.iter().position(|w| *w == "git") else { continue };
        let mut i = at + 1;
        while i < words.len() && matches!(words[i], "-C" | "-c") {
            i += 2;
        }
        let rest = words.get(i + 1..).unwrap_or(&[]);
        let after = |flags: &[&str]| rest.iter().position(|w| flags.contains(w)).and_then(|p| rest.get(p + 1)).map(|w| w.to_string());
        let found = match (words.get(i).copied(), rest.first().copied()) {
            (Some("checkout"), _) => after(&["-b", "-B"]),
            (Some("switch"), _) => after(&["-c", "-C", "--create", "--force-create"]),
            (Some("worktree"), Some("add")) => after(&["-b", "-B"]).or_else(|| {
                let positional: Vec<&&str> = rest[1..].iter().filter(|w| !w.starts_with('-')).collect();
                let detached = rest.iter().any(|w| *w == "--detach" || *w == "-d");
                match positional.as_slice() {
                    [path] if !detached => Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()),
                    _ => None,
                }
            }),
            _ => None,
        };
        if let Some(name) = found.filter(|n| !n.is_empty() && !n.starts_with('-')) {
            return Some(name);
        }
    }
    None
}

/// After a shell command: one that made a branch, in trunk mode, gets a
/// one-line reminder (never a block) and puts the branch on the registry.
pub fn after_command(root: &Path, cfg: &config::Config, session: &str, command: &str, env: &Env) -> String {
    let Some(name) = creates_branch(command) else { return String::new() };
    let Some(trunk) = active_for(root, &cfg.repo_id, env) else { return String::new() };
    if !has_branch(root, &name) || name == trunk.branch || name == trunk.promote_to {
        return String::new();
    }
    report(cfg, &name, session, json!({"status": "open"}), env);
    let t = &trunk.branch;
    format!(
        "Cork: trunk mode is on here, so work lands on `{t}`. A branch is for large or risky work only; Cork lands `{name}` \
into `{t}` when its task is done (`{}` lands it sooner).",
        crate::machine::own_command(env, "land")
    )
}

/// Told once: a branch's problem or pull request is said to its owner's
/// next session, not every session after.
fn first_telling(repo_id: &str, what: &str, env: &Env) -> bool {
    let path = dir(env).join(format!("told-{}.json", key_of(repo_id)));
    let mut told: Vec<Value> = config::load_json(&path).get("told").and_then(Value::as_array).cloned().unwrap_or_default();
    let key = json!(key_of(what));
    if told.contains(&key) {
        return false;
    }
    told.push(key);
    let keep = told.len().saturating_sub(100);
    let _ = std::fs::create_dir_all(dir(env));
    let _ = std::fs::write(&path, json!({"told": told[keep..]}).to_string());
    true
}

/// The dashboard asked to land or delete one of this person's branches:
/// done here, detached, when it is on this machine.
fn act_on_wanted(root: &Path, cfg: &config::Config, session: &str, env: &Env) {
    for row in heard(&cfg.repo_id, env).get("mine").and_then(Value::as_array).into_iter().flatten() {
        let (name, want) = (text(row, "name"), text(row, "want"));
        if name.is_empty() || !matches!(want.as_str(), "land" | "delete") || !has_branch(root, &name) {
            continue;
        }
        let machine = text(row, "machine");
        if !machine.is_empty() && machine != crate::report::machine_id(env) {
            continue;
        }
        let stamp = dir(env).join(format!("want-{}.json", key_of(&format!("{}\n{name}\n{want}", cfg.repo_id))));
        let last = config::load_json(&stamp).get("at").and_then(Value::as_f64).unwrap_or(0.0);
        if now_s() - last < WANT_AGAIN_S {
            continue;
        }
        let _ = std::fs::write(&stamp, json!({"at": now_s()}).to_string());
        let mut args: Vec<String> = vec!["land-branch".into(), root.display().to_string(), name, "--session".into(), session.into(), "--human".into()];
        if want == "delete" {
            args.push("--delete".into());
        }
        spawn(&args);
    }
}

/// What session start says in trunk mode, and what it starts: the three
/// sentences; this session's own branch, put on the registry; this
/// person's branches that could not land or became pull requests (once
/// each); and the dashboard's requests, done detached.
pub fn session_start(root: &Path, cfg: &config::Config, session: &str, env: &Env) -> Vec<String> {
    let Some(trunk) = active_for(root, &cfg.repo_id, env) else { return Vec::new() };
    let t = &trunk.branch;
    let mut said = vec![instructions(&trunk, root, env)];
    let branch = crate::branches::current(root);
    if lands_into_trunk(root, &branch, &trunk) {
        report(cfg, &branch, session, json!({"status": "open"}), env);
        said.push(format!("Cork: this session is on `{branch}`, not `{t}`; Cork lands it into `{t}` when its task is done."));
    }
    for row in heard(&cfg.repo_id, env).get("mine").and_then(Value::as_array).into_iter().flatten() {
        let (name, problem, pr) = (text(row, "name"), text(row, "problem"), text(row, "pr"));
        if !problem.is_empty() && first_telling(&cfg.repo_id, &format!("{name}\n{problem}"), env) {
            said.push(format!(
                "Cork could not land your branch `{name}` into `{t}`: {problem}. It is as it was; fix it there and land it \
again (`{}`), or tell the user.",
                crate::machine::own_command(env, "land")
            ));
        } else if problem.is_empty() && !pr.is_empty() && first_telling(&cfg.repo_id, &format!("{name}\n{pr}"), env) {
            said.push(format!("Cork: your branch `{name}` waits as a pull request into `{t}`: {pr}"));
        }
    }
    act_on_wanted(root, cfg, session, env);
    said
}

/// A session on the trunk whose trunk moved: told once per new tip, and
/// fast-forwarded when its tree is clean and it is only behind. It fetches
/// when the server says the trunk moved, else at most every FETCH_EVERY_S.
pub fn catch_up(root: &Path, trunk: &Trunk, server_head: &str) -> String {
    let t = &trunk.branch;
    if crate::branches::current(root) != *t || crate::hold::rebase_in_progress(root) {
        return String::new();
    }
    let stamp = crate::git::stamp_path(root, "trunk");
    let mut state = config::load_json(&stamp);
    let moved = !server_head.is_empty() && text(&state, "head") != server_head;
    let fetched_at = state.get("fetched_at").and_then(Value::as_f64).unwrap_or(0.0);
    if moved || now_s() - fetched_at >= FETCH_EVERY_S {
        let _ = crate::git::background_fetch(root);
        state["fetched_at"] = json!(now_s());
        state["head"] = json!(server_head);
    }
    let upstream = format!("origin/{t}");
    let tip = git_out(root, &["rev-parse", "--verify", "--quiet", &upstream]);
    let behind: usize = git_out(root, &["rev-list", "--count", &format!("HEAD..{upstream}")]).parse().unwrap_or(0);
    let said = if tip.is_empty() || behind == 0 || text(&state, "told") == tip {
        String::new()
    } else {
        let authors = lines(&git_out(root, &["log", &format!("HEAD..{upstream}"), "--format=%an"]));
        let mut who: Vec<String> = Vec::new();
        for name in authors {
            if !who.contains(&name) {
                who.push(name);
            }
        }
        let ahead: usize = git_out(root, &["rev-list", "--count", &format!("{upstream}..HEAD")]).parse().unwrap_or(0);
        let dirty = !lines(&git_out(root, &["diff", "HEAD", "--name-only"])).is_empty();
        let news = format!("{behind} new commit(s) from {}", names(&who));
        state["told"] = json!(tip);
        if !dirty && ahead == 0 && git(root, &["merge", "--ff-only", "--quiet", &upstream], 60).0 {
            format!("Cork: `{t}` moved on origin; fast-forwarded this checkout over {news}.")
        } else if dirty {
            format!("Cork: `{t}` moved on origin ({news}). This checkout has uncommitted changes, so Cork left it as it is; landing your commits rebases onto them.")
        } else {
            format!("Cork: `{t}` moved on origin ({news}); your {ahead} commit(s) here are not landed yet, and landing them rebases onto these.")
        }
    };
    let _ = std::fs::write(&stamp, state.to_string());
    said
}

/// At a prompt: the setting the briefing carried is kept, the dashboard's
/// requests are done, and a session on the trunk catches up with it.
pub fn at_prompt(root: &Path, cfg: &config::Config, session: &str, brief: &Value, env: &Env) -> String {
    remember(&cfg.repo_id, brief, env);
    let Some(trunk) = active_for(root, &cfg.repo_id, env) else { return String::new() };
    act_on_wanted(root, cfg, session, env);
    let head = brief.pointer(&format!("/trunk/heads/{}/commit", trunk.branch.replace('~', "~0").replace('/', "~1")));
    catch_up(root, &trunk, head.and_then(Value::as_str).unwrap_or(""))
}

/// A task ended: a subagent or teammate stopped (`stop`, its worker id
/// names its own worktree), or a session ended (`end`). Its branch lands
/// into the trunk, detached, when everything on it is committed and it has
/// commits the trunk lacks.
pub fn on_done(root: &Path, session: &str, worker: &str, why: &str, env: &Env) {
    let cfg = config::config(Some(root), env);
    let Some(trunk) = active_for(root, &cfg.repo_id, env) else { return };
    let mut places: Vec<PathBuf> = vec![root.to_path_buf()];
    // a subagent in a worktree of its own: Claude Code names it after it
    let tag: String = worker.chars().take(8).collect();
    if tag.len() >= 6 {
        for wt in crate::worktrees::agents(&crate::worktrees::list(root), env) {
            let named = wt.path.file_name().map(|n| n.to_string_lossy().contains(&tag)).unwrap_or(false);
            if (named || wt.branch.contains(&tag)) && !places.contains(&wt.path) {
                places.push(wt.path);
            }
        }
    }
    for place in places {
        let branch = crate::branches::current(&place);
        if !lands_into_trunk(&place, &branch, &trunk) || crate::hold::rebase_in_progress(&place) {
            continue;
        }
        // never uncommitted work
        if !git_out(&place, &["status", "--porcelain", "--untracked-files=no"]).is_empty() {
            continue;
        }
        let ahead: usize = git_out(&place, &["rev-list", "--count", &format!("origin/{}..HEAD", trunk.branch)]).parse().unwrap_or(0);
        if ahead == 0 {
            continue;
        }
        let mut args: Vec<String> = vec!["land-branch".into(), place.display().to_string(), branch, "--session".into(), session.into()];
        if why == "stop" {
            args.push("--leave-main".into());
        }
        spawn(&args);
    }
}

// ------------------------------------------------------------ the commands

/// The repo this command runs in, its config, and (when it is a free
/// machine's) its local server started.
fn here(env: &Env) -> Result<(PathBuf, config::Config), String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let top = git_out(&cwd, &["rev-parse", "--show-toplevel"]);
    if top.is_empty() {
        return Err("Cork: this is not a git repository.".into());
    }
    let root = PathBuf::from(top);
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return Err("Cork does not cover this repo yet; start an agent session here once, or run `cork login`.".into());
    }
    if cfg.machine_local {
        crate::machine::ensure_local(env, Duration::from_secs(3));
    }
    Ok((root, cfg))
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// The setting, said in four lines.
fn describe(trunk: &Trunk, repo_id: &str) -> String {
    let t = &trunk.branch;
    let landing = match trunk.policy_for_branches() {
        "pr" if t == &trunk.promote_to => format!("pr: `{t}` is also the promotion target, so a finished branch opens a pull request into it"),
        "pr" => format!("pr: a finished branch opens a pull request into `{t}`"),
        "ask" => "ask: finished branches are listed (`cork branches`) and land when you say".to_string(),
        _ => format!("auto: a finished branch lands into `{t}` once its tests pass"),
    };
    format!(
        "Trunk mode is on for {repo_id}.\n  trunk:      {t} (agents work here)\n  promote to: {} (`cork promote` opens the pull request; Cork never merges it)\n  landing:    {landing}",
        trunk.promote_to
    )
}

/// `cork trunk on [--branch dev] [--promote-to main] [--policy auto|pr|ask]`,
/// `cork trunk off`, `cork trunk status`.
pub fn command(args: &[String], env: &Env) -> i32 {
    let verb = args.first().map(String::as_str).unwrap_or("status");
    if matches!(verb, "-h" | "--help" | "help") || !matches!(verb, "on" | "off" | "status") {
        println!(
            "usage: cork trunk on [--branch dev] [--promote-to main] [--policy auto|pr|ask]\n       cork trunk off | status\n\n\
Trunk mode: this repo's agents work on one branch (dev), Cork lands the branches they do make into it when their task \
is done, and promotion to main stays a pull request you merge. Every machine and teammate gets the same setting."
        );
        return if matches!(verb, "-h" | "--help" | "help") { 0 } else { 2 };
    }
    let (root, cfg) = match here(env) {
        Ok(found) => found,
        Err(why) => {
            println!("{why}");
            return 1;
        }
    };
    let ask = |body: Value| post(&cfg, body, Duration::from_secs(10));
    match verb {
        "status" => {
            if let Some(answer) = ask(json!({"action": "get"})) {
                remember(&cfg.repo_id, &answer, env);
            }
            match active_for(&root, &cfg.repo_id, env) {
                Some(trunk) => println!("{}", describe(&trunk, &cfg.repo_id)),
                None => println!("Trunk mode is off for {}. `cork trunk on` switches it on.", cfg.repo_id),
            }
            0
        }
        "off" => match ask(json!({"action": "set", "on": false})) {
            Some(answer) => {
                remember(&cfg.repo_id, &answer, env);
                println!("Trunk mode is off for {}: agents branch as they like again, and nothing lands by itself.", cfg.repo_id);
                0
            }
            None => {
                println!("Cork could not switch trunk mode off: the server did not take it (in a team, the workspace's owner switches it).");
                1
            }
        },
        _ => {
            let policy = flag_value(args, "--policy").unwrap_or_default();
            if !policy.is_empty() && !POLICIES.contains(&policy.as_str()) {
                println!("Cork: --policy is auto, pr or ask.");
                return 2;
            }
            git(&root, &["fetch", "origin", "--quiet"], 60);
            let promote_to = flag_value(args, "--promote-to")
                .unwrap_or_else(|| if on_origin(&root, "main") { "main".into() } else { origin_default(&root) });
            let branch = flag_value(args, "--branch").unwrap_or_else(|| default_trunk(&root));
            let mut made = String::new();
            if branch != promote_to && !on_origin(&root, &branch) {
                // a trunk named that origin lacks: made from the promotion
                // target, with a plain push
                let from = format!("refs/remotes/origin/{promote_to}");
                if !on_origin(&root, &promote_to) || !git(&root, &["push", "--quiet", "origin", &format!("{from}:refs/heads/{branch}")], 60).0 {
                    println!("Cork: origin has no `{branch}`, and it could not be made from `{promote_to}`. Push a `{branch}` branch, then run this again.");
                    return 1;
                }
                git(&root, &["fetch", "origin", "--quiet"], 60);
                made = format!("Made `{branch}` on origin from `{promote_to}`.\n");
            }
            let mut body = json!({"action": "set", "on": true, "branch": branch, "promote_to": promote_to});
            if !policy.is_empty() {
                body["policy"] = json!(policy);
            }
            let Some(answer) = ask(body) else {
                println!("Cork could not switch trunk mode on: the server did not take it (in a team, the workspace's owner switches it).");
                return 1;
            };
            remember(&cfg.repo_id, &answer, env);
            let Some(trunk) = active_for(&root, &cfg.repo_id, env) else { return 1 };
            print!("{made}");
            println!("{}", describe(&trunk, &cfg.repo_id));
            println!("Agents hear it at their next session start or prompt. `cork branches` lists the branches already here.");
            0
        }
    }
}

/// A branch as `cork branches` shows it.
struct Listed {
    name: String,
    status: String,
    owner: String,
    age: String,
    ahead: usize,
    note: String,
    clean: bool,
    here: bool,
    known: bool,
}

fn age_words(seconds: f64) -> String {
    let s = seconds.max(0.0);
    if s < 3600.0 {
        format!("{}m", (s / 60.0).round() as i64)
    } else if s < 86_400.0 {
        format!("{}h", (s / 3600.0).round() as i64)
    } else {
        format!("{}d", (s / 86_400.0).round() as i64)
    }
}

/// `cork branches`: the agents' branches (the server's registry) and the
/// ones that were here before trunk mode, with how far each is from the
/// trunk. `cork branches land <name>` and `delete <name>` act on one;
/// `cork branches land-clean` lands every one that merges cleanly and
/// deletes the ones already on the trunk.
pub fn branches_command(args: &[String], env: &Env) -> i32 {
    let (root, cfg) = match here(env) {
        Ok(found) => found,
        Err(why) => {
            println!("{why}");
            return 1;
        }
    };
    let answer = post(&cfg, json!({"action": "get"}), Duration::from_secs(10));
    if let Some(answer) = &answer {
        remember(&cfg.repo_id, answer, env);
    }
    let Some(trunk) = active_for(&root, &cfg.repo_id, env) else {
        println!("Trunk mode is off for {}. `cork trunk on` switches it on.", cfg.repo_id);
        return 1;
    };
    let session = crate::machine::running_session(env);
    let human = LandOptions { session: session.clone(), human: true, test: flag_value(args, "--test").unwrap_or_default(), ..Default::default() };
    match args.first().map(String::as_str) {
        Some("land") | Some("delete") if args.len() >= 2 => {
            let (code, said) = if args[0] == "land" {
                land_branch(&root, &args[1], &trunk, &human, env)
            } else {
                delete_branch(&root, &args[1], &trunk, &session, env)
            };
            println!("{said}");
            return code;
        }
        Some("land" | "delete") => {
            println!("usage: cork branches land <name> | cork branches delete <name>");
            return 2;
        }
        _ => {}
    }
    git(&root, &["fetch", "origin", "--quiet"], 60);
    let t = trunk.branch.clone();
    let upstream = format!("origin/{t}");
    let now = now_s();
    let registry: Vec<Value> = answer.as_ref().and_then(|a| a.get("branches")).and_then(Value::as_array).cloned().unwrap_or_default();
    let local = lines(&git_out(&root, &["for-each-ref", "--count=300", "--format=%(refname:lstrip=2)", "refs/heads"]));
    let mut rows: Vec<Listed> = Vec::new();
    let measure = |name: &str| -> (usize, bool) {
        let full = format!("refs/heads/{name}");
        let ahead = git_out(&root, &["rev-list", "--count", &format!("{upstream}..{full}")]).parse().unwrap_or(0);
        let clean = ahead > 0 && git(&root, &["merge-tree", "--write-tree", "--quiet", &upstream, &full], 30).0;
        (ahead, clean)
    };
    let machine = crate::report::machine_id(env);
    for row in &registry {
        let name = text(row, "name");
        let here = local.contains(&name);
        let (ahead, clean) = if here { measure(&name) } else { (row.get("ahead").and_then(Value::as_u64).unwrap_or(0) as usize, false) };
        let created = row.get("created").and_then(Value::as_f64).unwrap_or(now);
        let note = [text(row, "problem"), text(row, "pr")].into_iter().find(|n| !n.is_empty()).unwrap_or_default();
        let mut status = text(row, "status");
        // made on this machine, gone from it and from origin, never landed
        if matches!(status.as_str(), "open" | "conflicted") && !here && text(row, "machine") == machine && !on_origin(&root, &name) {
            status = "abandoned".into();
            report(&cfg, &name, &session, json!({"status": "abandoned"}), env);
        }
        rows.push(Listed { status, owner: text(row, "user"), age: age_words(now - created), ahead, note, clean, here, known: true, name });
    }
    for name in local {
        if name == t || name == trunk.promote_to || rows.iter().any(|r| r.name == name) {
            continue;
        }
        let (ahead, clean) = measure(&name);
        let status = if ahead == 0 { "landed" } else { "open" }.to_string();
        rows.push(Listed { name, status, owner: String::new(), age: String::new(), ahead, note: String::new(), clean, here: true, known: false });
    }
    if args.first().map(String::as_str) == Some("land-clean") {
        let mut code = 0;
        for row in rows.iter().filter(|r| r.here) {
            let said = if row.ahead == 0 {
                let checked_out = crate::worktrees::list(&root).iter().any(|w| w.branch == row.name);
                if checked_out { continue }
                delete_branch(&root, &row.name, &trunk, &session, env)
            } else if row.clean {
                land_branch(&root, &row.name, &trunk, &human, env)
            } else {
                println!("Cork: `{}` conflicts with `{t}`; it is left for you (`cork branches land {}` once it is rebased).", row.name, row.name);
                continue;
            };
            if said.0 != 0 {
                code = 1;
            }
            println!("{}", said.1);
        }
        return code;
    }
    if rows.is_empty() {
        println!("No branches but `{t}` here: everything lands on it.");
        return 0;
    }
    let line = |r: &Listed| {
        let mut what = vec![if r.ahead == 0 { "nothing to land".to_string() } else { format!("{} ahead", r.ahead) }];
        if !r.owner.is_empty() {
            what.insert(0, format!("{}, {}", r.owner, r.age));
        }
        if r.here && r.ahead > 0 {
            what.push(if r.clean { "merges cleanly".into() } else { format!("conflicts with {t}") });
        }
        if !r.here {
            what.push("not on this machine".into());
        }
        if !r.note.is_empty() {
            what.push(r.note.clone());
        }
        format!("  {:<28} {:<11} {}", r.name, r.status, what.join(", "))
    };
    let (known, before): (Vec<&Listed>, Vec<&Listed>) = rows.iter().partition(|r| r.known);
    if !known.is_empty() {
        println!("Agents' branches in {} (trunk: {t}):", cfg.repo_id);
        known.iter().for_each(|r| println!("{}", line(r)));
    }
    if !before.is_empty() {
        println!("From before trunk mode:");
        before.iter().for_each(|r| println!("{}", line(r)));
    }
    let landable = rows.iter().filter(|r| r.here && r.ahead > 0 && r.clean).count();
    let done = rows.iter().filter(|r| r.here && r.ahead == 0).count();
    if landable + done > 0 {
        println!(
            "`cork branches land-clean` lands the {landable} that merge cleanly and deletes the {done} already on `{t}`; the rest stay listed. \
`cork branches land <name>` lands one."
        );
    }
    0
}

/// `cork promote`: one pull request from the trunk to the promotion target,
/// opened or brought up to date, listing what landed since the last
/// promotion. Cork never merges it.
pub fn promote_command(_args: &[String], env: &Env) -> i32 {
    let (root, cfg) = match here(env) {
        Ok(found) => found,
        Err(why) => {
            println!("{why}");
            return 1;
        }
    };
    if let Some(answer) = post(&cfg, json!({"action": "get"}), Duration::from_secs(10)) {
        remember(&cfg.repo_id, &answer, env);
    }
    let Some(trunk) = active_for(&root, &cfg.repo_id, env) else {
        println!("Trunk mode is off for {}; `cork trunk on` first.", cfg.repo_id);
        return 1;
    };
    let (code, said) = promote(&root, &trunk, env);
    println!("{said}");
    code
}

/// The promotion pull request: (exit code, what to say). Nothing is merged
/// or pushed to either branch.
pub fn promote(root: &Path, trunk: &Trunk, env: &Env) -> (i32, String) {
    let (t, target) = (trunk.branch.as_str(), trunk.promote_to.as_str());
    if t == target {
        return (0, format!("Cork: the trunk is `{t}`, which is also the promotion target: there is nothing to promote."));
    }
    git(root, &["fetch", "origin", "--quiet"], 60);
    if !on_origin(root, t) || !on_origin(root, target) {
        return (1, format!("Cork: origin needs both `{t}` and `{target}` to promote one into the other."));
    }
    let log = git_out(root, &["log", "--no-merges", "-n", "200", "--format=%h%x09%an%x09%s", &format!("origin/{target}..origin/{t}")]);
    let landed: Vec<String> = log
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let (sha, who, subject) = (parts.next()?, parts.next()?, parts.next()?);
            Some(format!("- {subject} ({sha}, {who})"))
        })
        .collect();
    if landed.is_empty() {
        return (0, format!("Cork: `{target}` already has everything on `{t}`; nothing to promote."));
    }
    let title = format!("Promote {t} to {target}");
    let body = with_badge(
        format!(
            "What landed on `{t}` since `{target}` last took it ({} commit(s)):\n\n{}\n\nMerging this is a person's call: Cork never merges into `{target}`.",
            landed.len(),
            landed.join("\n")
        ),
        env,
    );
    if let Some(gh) = gh(env) {
        let url = pull_request(&gh, root, target, t, &title, &body, true);
        if !url.is_empty() {
            return (0, format!("Cork: the promotion pull request from `{t}` into `{target}` lists {} commit(s): {url}", landed.len()));
        }
    }
    let url = compare_url(root, target, t);
    let open = if url.is_empty() { format!("Open a pull request from `{t}` into `{target}`") } else { format!("Open it here: {url}") };
    (0, format!("Cork: {} commit(s) landed on `{t}` since `{target}` last took them. {open}\n\n{title}\n\n{body}", landed.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let (ok, _, err) = git(dir, args, 60);
        assert!(ok, "git {args:?}: {err}");
    }

    /// Unique per test in this run: tests run in parallel.
    fn unique() -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// A bare origin with `main` and `dev`, and a clone on `dev`. The env
    /// keeps Cork's data in the test's own folder, with no server.
    fn repo(name: &str) -> (PathBuf, PathBuf, Env) {
        let base = std::env::temp_dir().join(format!("cork-trunk-{name}-{}-{}", std::process::id(), unique()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        sh(&base, &["clone", "-q", "origin.git", "a"]);
        let a = base.join("a");
        for (k, v) in [("user.email", "a@x"), ("user.name", "alice")] {
            sh(&a, &["config", k, v]);
        }
        std::fs::write(a.join("lib.py"), "def total(x):\n    return x\n").unwrap();
        sh(&a, &["add", "."]);
        sh(&a, &["commit", "-qm", "seed"]);
        sh(&a, &["push", "-q", "-u", "origin", "main"]);
        sh(&a, &["checkout", "-q", "-b", "dev"]);
        sh(&a, &["push", "-q", "-u", "origin", "dev"]);
        let mut env = Env::new();
        env.insert("HOME".into(), base.join("home").to_string_lossy().to_string());
        env.insert("CORK_GH".into(), base.join("no-gh").to_string_lossy().to_string());
        (base, a, env)
    }

    fn trunk() -> Trunk {
        Trunk { branch: "dev".into(), promote_to: "main".into(), policy: "auto".into() }
    }

    fn commit(dir: &Path, file: &str, body: &str, message: &str) {
        std::fs::write(dir.join(file), body).unwrap();
        sh(dir, &["add", file]);
        sh(dir, &["commit", "-qm", message]);
    }

    fn origin_has(dir: &Path, branch: &str, file: &str) -> bool {
        sh(dir, &["fetch", "-q", "origin"]);
        git(dir, &["cat-file", "-e", &format!("origin/{branch}:{file}")], 30).0
    }

    fn land(dir: &Path, branch: &str, test: &str, env: &Env) -> (i32, String) {
        land_branch(dir, branch, &trunk(), &LandOptions { test: test.into(), ..Default::default() }, env)
    }

    #[test]
    fn auto_land_refuses_uncommitted_work_and_failing_tests_and_leaves_the_branch_as_it_was() {
        let (base, a, env) = repo("refuse");
        sh(&a, &["checkout", "-q", "-b", "feat"]);
        commit(&a, "refund.py", "def refund():\n    return 1\n", "Add refunds");
        std::fs::write(a.join("lib.py"), "def total(x):\n    return x  # half done\n").unwrap();
        let (code, said) = land(&a, "feat", "true", &env);
        assert_eq!(code, 1, "{said}");
        assert!(said.contains("uncommitted work (lib.py)"), "{said}");
        assert!(!origin_has(&a, "dev", "refund.py"), "nothing pushed");
        sh(&a, &["checkout", "-q", "--", "lib.py"]);

        // a teammate lands on dev meanwhile, so the branch is rebased and tested
        sh(&base, &["clone", "-q", "-b", "dev", "origin.git", "b"]);
        let b = base.join("b");
        sh(&b, &["config", "user.email", "b@x"]);
        sh(&b, &["config", "user.name", "bob"]);
        commit(&b, "notes.txt", "hi\n", "Notes");
        sh(&b, &["push", "-q"]);
        let tip = git_out(&a, &["rev-parse", "feat"]);
        let (code, said) = land(&a, "feat", "false", &env);
        assert_eq!(code, 1, "{said}");
        assert!(said.contains("`false` failed"), "{said}");
        assert!(!origin_has(&a, "dev", "refund.py"), "failing tests push nothing");
        assert_eq!(git_out(&a, &["rev-parse", "feat"]), tip, "the branch is as it was");
        assert!(has_branch(&a, "feat"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_landed_branch_is_deleted_here_and_on_origin_only_when_nothing_on_it_is_unlanded() {
        let (base, a, env) = repo("delete");
        // a branch an agent pushed, not checked out anywhere now
        sh(&a, &["checkout", "-q", "-b", "feat"]);
        commit(&a, "refund.py", "def refund():\n    return 1\n", "Add refunds");
        sh(&a, &["push", "-q", "origin", "feat"]);
        sh(&a, &["checkout", "-q", "dev"]);
        let (code, said) = land(&a, "feat", "true", &env);
        assert_eq!(code, 0, "{said}");
        assert!(said.contains("Cork landed `feat` into `dev` (1 commit(s)"), "{said}");
        assert!(said.contains("Deleted `feat` here and on origin"), "{said}");
        assert!(origin_has(&a, "dev", "refund.py"));
        let origin = base.join("origin.git");
        assert!(!has_branch(&a, "feat") && !has_branch(&origin, "feat"), "deleted here and on origin");
        assert!(!origin_has(&a, "main", "refund.py"), "the promotion target is never touched");

        // a branch with work not on dev is never deleted
        sh(&a, &["checkout", "-q", "-b", "wip"]);
        commit(&a, "wip.py", "X = 1\n", "Half a feature");
        sh(&a, &["checkout", "-q", "dev"]);
        let (code, said) = delete_branch(&a, "wip", &trunk(), "", &env);
        assert_eq!(code, 1, "{said}");
        assert!(said.contains("1 commit(s) not on `dev`"), "{said}");
        assert!(has_branch(&a, "wip"));
        // nor is one that landed but whose tidy found it still checked out by a person
        sh(&a, &["worktree", "add", "-q", &base.join("mine").to_string_lossy(), "wip"]);
        let (code, said) = land(&a, "wip", "true", &env);
        assert_eq!(code, 0, "{said}");
        assert!(said.contains("stays checked out in"), "{said}");
        assert!(has_branch(&a, "wip"), "a person's worktree keeps its branch");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_stacked_branch_lands_after_the_branch_it_is_based_on() {
        let (base, a, env) = repo("stacked");
        sh(&a, &["checkout", "-q", "-b", "feat-a"]);
        commit(&a, "a.py", "A = 1\n", "Feature A");
        sh(&a, &["checkout", "-q", "-b", "feat-b"]);
        commit(&a, "b.py", "B = 1\n", "Feature B on A");
        sh(&a, &["checkout", "-q", "dev"]);
        let (code, said) = land(&a, "feat-b", "true", &env);
        assert_eq!(code, 0, "{said}");
        assert!(said.contains("stacked on `feat-a`"), "{said}");
        assert!(!origin_has(&a, "dev", "b.py") && !origin_has(&a, "dev", "a.py"), "it waits");
        let (code, said) = land(&a, "feat-a", "true", &env);
        assert_eq!(code, 0, "{said}");
        assert!(origin_has(&a, "dev", "a.py") && !origin_has(&a, "dev", "b.py"));
        // its base landed (rebased, deleted): now it goes
        let (code, said) = land(&a, "feat-b", "true", &env);
        assert_eq!(code, 0, "{said}");
        assert!(said.contains("Cork landed `feat-b` into `dev` (1 commit(s)"), "the base's commit went with the base: {said}");
        assert!(origin_has(&a, "dev", "b.py"));
        assert!(!has_branch(&a, "feat-a") && !has_branch(&a, "feat-b"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_policy_decides_how_a_finished_branch_lands_and_a_person_can_always_land_it() {
        let (base, a, env) = repo("policy");
        sh(&a, &["checkout", "-q", "-b", "feat"]);
        commit(&a, "refund.py", "def refund():\n    return 1\n", "Add refunds");
        sh(&a, &["checkout", "-q", "dev"]);
        let ask = Trunk { policy: "ask".into(), ..trunk() };
        let (code, said) = land_branch(&a, "feat", &ask, &LandOptions { test: "true".into(), ..Default::default() }, &env);
        assert_eq!(code, 0);
        assert!(said.contains("asks first"), "{said}");
        assert!(!origin_has(&a, "dev", "refund.py"));
        // a pull request: the branch is pushed, the trunk is not touched
        let pr = Trunk { policy: "pr".into(), ..trunk() };
        let (code, said) = land_branch(&a, "feat", &pr, &LandOptions::default(), &env);
        assert_eq!(code, 0, "{said}");
        assert!(said.contains("Cork pushed `feat`"), "{said}");
        assert!(on_origin(&a, "feat") && !origin_has(&a, "dev", "refund.py"));
        // a trunk that is also the promotion target only ever gets pull requests
        let same = Trunk { branch: "main".into(), promote_to: "main".into(), policy: "auto".into() };
        let human = LandOptions { test: "true".into(), human: true, ..Default::default() };
        let (_, said) = land_branch(&a, "feat", &same, &human, &env);
        assert!(said.contains("pull request") && !origin_has(&a, "main", "refund.py"), "{said}");
        // a person's Land lands whatever the policy
        let (code, said) = land_branch(&a, "feat", &ask, &human, &env);
        assert_eq!(code, 0, "{said}");
        assert!(origin_has(&a, "dev", "refund.py"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn catching_up_fast_forwards_a_clean_tree_and_never_touches_a_dirty_one() {
        let (base, a, _env) = repo("catchup");
        sh(&base, &["clone", "-q", "-b", "dev", "origin.git", "b"]);
        let b = base.join("b");
        sh(&b, &["config", "user.email", "b@x"]);
        sh(&b, &["config", "user.name", "bob"]);
        commit(&b, "notes.txt", "one\n", "Notes");
        sh(&b, &["push", "-q"]);
        let said = catch_up(&a, &trunk(), "");
        assert!(said.contains("fast-forwarded this checkout over 1 new commit(s) from bob"), "{said}");
        assert!(a.join("notes.txt").exists());
        assert!(catch_up(&a, &trunk(), "").is_empty(), "said once");

        // bob lands again; this checkout has uncommitted work
        commit(&b, "notes.txt", "one\ntwo\n", "More notes");
        sh(&b, &["push", "-q"]);
        let head = git_out(&b, &["rev-parse", "HEAD"]);
        std::fs::write(a.join("lib.py"), "def total(x):\n    return x  # editing\n").unwrap();
        let before = git_out(&a, &["rev-parse", "HEAD"]);
        // the server says dev moved: fetched now, not in five minutes
        let said = catch_up(&a, &trunk(), &head);
        assert!(said.contains("uncommitted changes, so Cork left it as it is"), "{said}");
        assert_eq!(git_out(&a, &["rev-parse", "HEAD"]), before, "not moved");
        assert!(std::fs::read_to_string(a.join("lib.py")).unwrap().contains("# editing"));
        assert_eq!(std::fs::read_to_string(a.join("notes.txt")).unwrap(), "one\n");
        // a session on another branch hears nothing
        sh(&a, &["stash", "-q"]);
        sh(&a, &["checkout", "-q", "-b", "feat"]);
        assert!(catch_up(&a, &trunk(), "other").is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn session_start_speaks_only_when_trunk_mode_is_on() {
        let (base, a, env) = repo("start");
        let cfg = config::Config {
            server: String::new(), token: String::new(), repo_id: "github.com/acme/shop".into(),
            verify: String::new(), local_parse: true, machine_local: true,
        };
        assert!(session_start(&a, &cfg, "s1", &env).is_empty(), "never heard: off");
        remember(&cfg.repo_id, &json!({"trunk": {"on": false, "branch": "dev"}}), &env);
        assert!(session_start(&a, &cfg, "s1", &env).is_empty(), "off");
        remember(&cfg.repo_id, &json!({"trunk": {"on": true, "branch": "", "promote_to": "main", "policy": "auto"}}), &env);
        let said = session_start(&a, &cfg, "s1", &env);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with("Cork: this repo uses trunk mode, so work on `dev`"), "an unnamed trunk is dev when origin has it: {}", said[0]);
        assert!(said[0].contains("commit --mine -m") && said[0].contains("only for large or risky work"), "{}", said[0]);
        // on a branch of its own, the session hears where it lands, and its
        // owner hears once what could not land
        sh(&a, &["checkout", "-q", "-b", "feat"]);
        remember(&cfg.repo_id, &json!({"trunk": {"on": true, "branch": "dev", "mine": [{"name": "old", "status": "conflicted", "problem": "conflicts with dev in a.py"}]}}), &env);
        let said = session_start(&a, &cfg, "s1", &env);
        assert!(said.iter().any(|l| l.contains("this session is on `feat`, not `dev`")), "{said:?}");
        assert!(said.iter().any(|l| l.contains("could not land your branch `old` into `dev`: conflicts with dev in a.py")), "{said:?}");
        assert!(!session_start(&a, &cfg, "s2", &env).iter().any(|l| l.contains("`old`")), "told once");
        // a branch an agent makes is reminded of once, never held
        sh(&a, &["checkout", "-q", "-b", "spike"]);
        let reminder = after_command(&a, &cfg, "s1", "git checkout -b spike", &env);
        assert!(reminder.starts_with("Cork: trunk mode is on here, so work lands on `dev`.") && reminder.contains("Cork lands `spike` into `dev`"), "{reminder}");
        assert!(after_command(&a, &cfg, "s1", "git checkout dev", &env).is_empty());
        assert!(after_command(&a, &cfg, "s1", "git checkout -b never-made", &env).is_empty(), "a command that failed made nothing");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn promotion_opens_or_updates_one_pull_request_and_never_merges() {
        let (base, a, mut env) = repo("promote");
        commit(&a, "refund.py", "def refund():\n    return 1\n", "Add refunds");
        sh(&a, &["push", "-q"]);
        let main_before = git_out(&a, &["rev-parse", "origin/main"]);
        // no gh: the listing, and where to open it
        let (code, said) = promote(&a, &trunk(), &env);
        assert_eq!(code, 0, "{said}");
        assert!(said.contains("1 commit(s) landed on `dev`") && said.contains("- Add refunds"), "{said}");
        // a gh that records what it is asked
        let calls = base.join("gh-calls");
        let fake = base.join("gh");
        std::fs::write(&fake, format!(
            "#!/bin/sh\necho \"$@\" >> {}\ncase \"$1 $2\" in\n  \"pr create\") echo https://github.com/acme/shop/pull/7 ;;\nesac\nexit 0\n",
            calls.display()
        )).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        env.insert("CORK_GH".into(), fake.to_string_lossy().to_string());
        let (code, said) = promote(&a, &trunk(), &env);
        assert_eq!(code, 0, "{said}");
        assert!(said.contains("https://github.com/acme/shop/pull/7"), "{said}");
        let asked = std::fs::read_to_string(&calls).unwrap();
        assert!(asked.contains("pr create --base main --head dev --title Promote dev to main"), "{asked}");
        assert!(!asked.lines().any(|l| l.starts_with("pr merge")), "never merged: {asked}");
        sh(&a, &["fetch", "-q"]);
        assert_eq!(git_out(&a, &["rev-parse", "origin/main"]), main_before, "main is as it was");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_commands_that_make_a_branch_are_recognised() {
        for (command, made) in [
            ("git checkout -b feat/x", Some("feat/x")),
            ("cd app && git switch -c fix-1 origin/dev", Some("fix-1")),
            ("git switch --create spike", Some("spike")),
            ("git -C repo checkout -B again", Some("again")),
            ("git worktree add -b agent-2 ../wt dev", Some("agent-2")),
            ("git worktree add ../hotfix", Some("hotfix")),
            ("git worktree add ../wt existing-branch", None),
            ("git worktree add --detach ../wt", None),
            ("git checkout dev", None),
            ("git switch main", None),
            ("git branch -d old", None),
        ] {
            assert_eq!(creates_branch(command).as_deref(), made, "{command}");
        }
        assert!(!lands_into_trunk(Path::new("/nowhere"), "dev", &trunk()));
        assert!(!lands_into_trunk(Path::new("/nowhere"), "main", &trunk()));
        assert!(lands_into_trunk(Path::new("/nowhere"), "feat/x", &trunk()));
    }
}
