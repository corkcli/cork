//! Cork in pull requests: a check on every PR, and one comment when there
//! is something worth reading. Everyone reading the PR sees it, including
//! people who never installed Cork.
//!
//! The GitHub App's webhook hands `pull_request`, `check_run` /
//! `check_suite` reruns and default-branch `push` deliveries here. The
//! delivery is answered at once and the work runs behind it: a delivery
//! GitHub sends twice runs once (`first_delivery`), and rapid pushes to one
//! PR collapse into one run on the newest head (`Queue`).
//!
//! A run maps the PR's changed hunks to symbols and reports:
//!
//! - other open PRs that change the same symbols, or rename, remove or
//!   re-signature one this PR calls (and the reverse). Each open PR's shape
//!   (files, symbols touched, changes made, names called) is kept in kv
//!   while it is open, so a run compares against the others without asking
//!   GitHub about them; both sides of a new overlap are told;
//! - other people's agents on those files or symbols right now (hot
//!   markers and intents), never the PR author's own;
//! - callers in the map of what the PR renames, removes or re-signatures
//!   that the PR leaves alone;
//! - pinned notes and standing decisions on the code it changes;
//! - what Cork stopped on the PR's branch while it was open.
//!
//! Output: a check run named `Cork` on the head (`success`, or `neutral`
//! with findings, never `failure`: Cork fails open), and one comment found
//! by its marker, made only when there is something to say, edited after
//! that, and edited down to a resolved line when nothing is left.
//!
//! **Reading code is the workspace's choice** (`prchecks_mode`; off until
//! the owner answers). On: the changed files are fetched at base and head
//! through the Contents API, and the default branch's tarball once to build
//! the map, all parsed in memory by the hooks' own parser; only structure is
//! kept, in the same code graph the hooks fill. Off: only file names and the
//! `@@` hunk ranges of the PR files API are used (the patch text is dropped
//! unparsed), with the map and the changes the workspace's agents already
//! reported. Either way no file's text is stored or logged.
//!
//! PR checks are free on every plan and cost no agent hours: no plan or
//! meter is consulted here.
//!
//! **The badge.** A PR that opens (or leaves draft) in a watched repo gets
//! the "coordinated by cork" badge at the end of its description, once.
//! Agents were told to add it themselves, but an agent told by its harness
//! to add no attribution drops Cork's badge too. The App adds it at most
//! once per PR, never a second, never to a closed PR, and never again after
//! someone removed it; the workspace owner can turn it off (on by default).
//!
//!   prcheck        `<repo_id>#<number>` -> the open PR's shape, its comment and what it flagged
//!   prcheck_seeded `<repo_id>`          -> {ts}: the repo's open PRs were read once
//!   prchecks_mode  `<workspace_id>`     -> {read_code, badge, by, ts}  (no read_code: off, not asked yet;
//!                                          no badge: on)
//!   prbadge        `<repo_id>#<number>` -> {ts, added|present}: the badge was handled; kept
//!                                          past a close, so a reopened PR is not badged twice
//!   prmap          `<scope>`            -> {ref, files, ts}: the map built from the default branch

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::github_app::{installation_token, Config};
use crate::repo::Aliases;
use crate::store::{now, Store};
use crate::workspaces;

/// The check run's name, and how a rerun of ours is told from others'.
pub const NAME: &str = "Cork";
/// How Cork finds its own comment on a PR.
pub const MARKER: &str = "<!-- cork:pr-check -->";
const FOOTER: &str = "<sub>Catch these before you push: `npx corkcli`</sub>\n\n\
[![coordinated by cork](https://corkcli.com/badge.svg)](https://corkcli.com)";

const PRS: &str = "prcheck";
const SEEDED: &str = "prcheck_seeded";
const MODE: &str = "prchecks_mode";
const MAPS: &str = "prmap";
const BADGES: &str = "prbadge";

/// What the App adds to a PR's description, and how it knows one is there.
pub const BADGE: &str = "[![coordinated by cork](https://corkcli.com/badge.svg)](https://corkcli.com)";
const BADGE_MARK: &str = "corkcli.com/badge.svg";

/// Pushes this close together are one run, on the newest head.
const DEBOUNCE: Duration = Duration::from_secs(5);
/// GitHub redelivers within days, if ever.
const DELIVERY_TTL_S: f64 = 3.0 * 86400.0;
const MAX_PR_FILES: usize = 300;
/// Changed files read at base and head, per run (two calls each).
const MAX_FETCHED: usize = 150;
const MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_JSON_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TARBALL_BYTES: u64 = 150 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_MAP_FILES: usize = 20_000;
const MAP_BATCH: usize = 200;
/// Open PRs read (their file lists only) the first time a repo is checked.
const MAX_SEEDED: usize = 30;
/// `id` or `run` matches everything; a name this long means something.
const MIN_NAME: usize = 4;
/// The longest GitHub's rate limit is waited out in place.
const MAX_WAIT_S: f64 = 60.0;
const MAX_LINES: usize = 8;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn at(value: &Value, pointer: &str) -> String {
    value.pointer(pointer).and_then(Value::as_str).unwrap_or("").to_string()
}

fn num(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn id_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The last segment of `Class.method` or `mod::name`: what a caller writes.
fn bare(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name).rsplit('.').next().unwrap_or(name)
}

fn squash(signature: &str) -> String {
    signature.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn pr_key(repo_id: &str, number: u64) -> String {
    format!("{repo_id}#{number}")
}

// ------------------------------------------------------------- the choice

/// The workspace's PR checks settings. Whether they may read code: off,
/// and not asked, until the owner answers. Whether the App adds the badge
/// to PR descriptions: on until the owner turns it off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mode {
    pub read_code: bool,
    pub asked: bool,
    pub badge: bool,
}

pub fn mode(store: &Store, workspace_id: &str) -> Mode {
    let saved = store.kv_get(MODE, workspace_id).unwrap_or(json!({}));
    Mode {
        read_code: saved.get("read_code").and_then(Value::as_bool).unwrap_or(false),
        asked: saved.get("read_code").is_some_and(Value::is_boolean),
        badge: saved.get("badge").and_then(Value::as_bool).unwrap_or(true),
    }
}

/// The owner's answer on reading code; the badge setting stays as it was.
pub fn set_mode(store: &Store, workspace_id: &str, read_code: bool, by: &str) -> Mode {
    save_mode(store, workspace_id, "read_code", read_code, by)
}

/// The owner's badge switch; the code-reading answer stays as it was.
pub fn set_badge(store: &Store, workspace_id: &str, on: bool, by: &str) -> Mode {
    save_mode(store, workspace_id, "badge", on, by)
}

fn save_mode(store: &Store, workspace_id: &str, field: &str, value: bool, by: &str) -> Mode {
    let mut saved = store.kv_get(MODE, workspace_id).filter(Value::is_object).unwrap_or(json!({}));
    saved[field] = json!(value);
    saved["by"] = json!(by);
    saved["ts"] = json!(now());
    let _ = store.kv_put(MODE, workspace_id, &saved, now());
    mode(store, workspace_id)
}

fn workspace_of(store: &Store, installation: &str) -> String {
    crate::github_app::record(store, installation).map(|rec| text(&rec, "workspace_id")).unwrap_or_default()
}

// --------------------------------------------------------- the deliveries

/// False for a delivery already taken: GitHub may send one twice, and a
/// redelivery from its settings page is the same id. A delivery with no id
/// is always taken.
pub fn first_delivery(store: &Store, delivery: &str) -> bool {
    let delivery = delivery.trim();
    if delivery.is_empty() {
        return true;
    }
    static TAKING: OnceLock<Mutex<()>> = OnceLock::new();
    let _held = TAKING.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner());
    let key = format!("ghdelivery:{delivery}");
    if store.eph_get(&key).is_some() {
        return false;
    }
    let _ = store.eph_set(&key, &json!({"t": now()}), Some(DELIVERY_TTL_S));
    true
}

/// The repo a delivery is about, as GitHub named it.
#[derive(Clone, Debug, Default)]
pub struct Repo {
    pub installation: String,
    /// `owner/name` as GitHub spells it: API paths use it as is
    pub full_name: String,
    pub private: bool,
    pub default_branch: String,
}

impl Repo {
    pub fn id(&self) -> String {
        format!("github.com/{}", self.full_name.trim().to_lowercase())
    }
}

/// One piece of background work. A PR's jobs share one key, so a newer one
/// (a push, or the PR closing) replaces one still waiting.
#[derive(Clone, Debug)]
pub enum Job {
    /// Analyze a PR. `pull` is the webhook's copy; `None` reads it from GitHub.
    Pr { repo: Repo, number: u64, pull: Option<Value> },
    /// A merged PR never reopens: its badge record goes with it.
    Closed { repo: Repo, number: u64, merged: bool },
    /// The badge on a PR that opened or left draft. Its own key: a push
    /// right behind the open must not replace it.
    Badge { repo: Repo, number: u64 },
    /// Someone pressed "Re-run" on GitHub.
    Rerun { repo: Repo, sha: String, numbers: Vec<u64> },
    /// The default branch moved: the map follows when code reading is on.
    Push { repo: Repo, before: String, after: String },
}

impl Job {
    fn key(&self) -> String {
        match self {
            Job::Pr { repo, number, .. } | Job::Closed { repo, number, .. } => format!("pr:{}", pr_key(&repo.id(), *number)),
            Job::Badge { repo, number } => format!("badge:{}", pr_key(&repo.id(), *number)),
            Job::Rerun { repo, sha, .. } => format!("rerun:{}@{sha}", repo.id()),
            Job::Push { repo, .. } => format!("push:{}", repo.id()),
        }
    }
}

/// The jobs one delivery asks for; nothing for events PR checks ignore.
pub fn jobs_for(event: &str, body: &Value) -> Vec<Job> {
    let repository = body.get("repository").cloned().unwrap_or(json!({}));
    let repo = Repo {
        installation: id_of(body.pointer("/installation/id")),
        full_name: text(&repository, "full_name"),
        // unknown is treated as private: nothing written is shown to strangers
        private: repository.get("private").and_then(Value::as_bool).unwrap_or(true),
        default_branch: text(&repository, "default_branch"),
    };
    if repo.installation.is_empty() || repo.full_name.is_empty() {
        return Vec::new();
    }
    let action = text(body, "action");
    match event {
        "pull_request" => {
            let pull = body.get("pull_request").cloned().unwrap_or(json!({}));
            let number = num(&pull, "number");
            if number == 0 {
                return Vec::new();
            }
            let draft = pull.get("draft").and_then(Value::as_bool).unwrap_or(false);
            match action.as_str() {
                // a draft gets its badge when it is marked ready
                "opened" | "reopened" | "ready_for_review" if !draft => {
                    vec![Job::Badge { repo: repo.clone(), number }, Job::Pr { repo, number, pull: Some(pull) }]
                }
                "opened" | "synchronize" | "reopened" | "ready_for_review" => vec![Job::Pr { repo, number, pull: Some(pull) }],
                // a new base changes what the PR is compared against; a new title does not
                "edited" if body.pointer("/changes/base").is_some() => vec![Job::Pr { repo, number, pull: Some(pull) }],
                "closed" => {
                    let merged = pull.get("merged").and_then(Value::as_bool).unwrap_or(false);
                    vec![Job::Closed { repo, number, merged }]
                }
                _ => Vec::new(),
            }
        }
        "check_run" | "check_suite" if action == "rerequested" => {
            let (suite, sha) = if event == "check_run" {
                if at(body, "/check_run/name") != NAME {
                    return Vec::new();
                }
                (body.pointer("/check_run/check_suite"), at(body, "/check_run/head_sha"))
            } else {
                (body.get("check_suite"), at(body, "/check_suite/head_sha"))
            };
            let numbers: Vec<u64> = suite
                .and_then(|s| s.get("pull_requests"))
                .and_then(Value::as_array)
                .map(|prs| prs.iter().map(|p| num(p, "number")).filter(|n| *n > 0).collect())
                .unwrap_or_default();
            if sha.is_empty() && numbers.is_empty() {
                return Vec::new();
            }
            vec![Job::Rerun { repo, sha, numbers }]
        }
        "push" => {
            let default = repo.default_branch.clone();
            let deleted = body.get("deleted").and_then(Value::as_bool).unwrap_or(false);
            if default.is_empty() || deleted || text(body, "ref") != format!("refs/heads/{default}") {
                return Vec::new();
            }
            vec![Job::Push { repo, before: text(body, "before"), after: text(body, "after") }]
        }
        _ => Vec::new(),
    }
}

/// What waits and what runs. Only the newest job of a key waits; one
/// worker per key at a time, so two runs of one PR never race to comment.
#[derive(Default)]
struct Queue {
    wanted: HashMap<String, Job>,
    running: HashSet<String>,
}

impl Queue {
    /// True when nobody works this key yet: start a worker.
    fn offer(&mut self, job: Job) -> bool {
        let key = job.key();
        self.wanted.insert(key.clone(), job);
        self.running.insert(key)
    }

    /// The newest job of `key`; none, and the worker stops.
    fn take(&mut self, key: &str) -> Option<Job> {
        let job = self.wanted.remove(key);
        if job.is_none() {
            self.running.remove(key);
        }
        job
    }
}

fn queue() -> &'static Mutex<Queue> {
    static QUEUE: OnceLock<Mutex<Queue>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(Queue::default()))
}

/// A verified delivery: true when PR checks took it (queued to run behind
/// the answer), false for the App's other events.
pub fn on_delivery(app: &Arc<crate::App>, cfg: &Config, event: &str, body: &Value) -> bool {
    if !matches!(event, "pull_request" | "check_run" | "check_suite" | "push") {
        return false;
    }
    for job in jobs_for(event, body) {
        enqueue(app, cfg, job);
    }
    true
}

fn enqueue(app: &Arc<crate::App>, cfg: &Config, job: Job) {
    let key = job.key();
    let start = queue().lock().unwrap_or_else(|e| e.into_inner()).offer(job);
    if start {
        tokio::spawn(drain(Arc::clone(app), cfg.clone(), key));
    }
}

async fn drain(app: Arc<crate::App>, cfg: Config, key: String) {
    loop {
        tokio::time::sleep(DEBOUNCE).await;
        let Some(job) = queue().lock().unwrap_or_else(|e| e.into_inner()).take(&key) else { return };
        let worker = Arc::clone(&app);
        let worker_cfg = cfg.clone();
        let ctx = Ctx { hot_ttl_s: app.hot_ttl_s, dashboard_url: app.dashboard_url.clone() };
        let next = tokio::task::spawn_blocking(move || run(&worker.store, &worker.aliases, &worker_cfg, &ctx, job))
            .await
            .unwrap_or_default();
        for job in next {
            enqueue(&app, &cfg, job);
        }
    }
}

/// What a run needs from the server besides the store.
#[derive(Clone, Debug)]
pub struct Ctx {
    pub hot_ttl_s: f64,
    pub dashboard_url: String,
}

/// One job, to the end. Returns the jobs it leads to: the other side of an
/// overlap that is new or gone, a rerun's PRs.
pub fn run(store: &Store, aliases: &Aliases, cfg: &Config, ctx: &Ctx, job: Job) -> Vec<Job> {
    match job {
        Job::Pr { repo, number, pull } => analyze(store, aliases, cfg, ctx, &repo, number, pull),
        Job::Closed { repo, number, merged } => {
            if merged {
                let _ = store.kv_delete(BADGES, &pr_key(&repo.id(), number));
            }
            closed(store, &repo, number)
        }
        Job::Badge { repo, number } => {
            if let Err(problem) = badge(store, aliases, cfg, &repo, number) {
                tracing::warn!("pr badge {}#{number}: {problem}", repo.full_name);
            }
            Vec::new()
        }
        Job::Rerun { repo, sha, numbers } => rerun(store, cfg, &repo, &sha, numbers),
        Job::Push { repo, before, after } => {
            pushed(store, aliases, cfg, ctx, &repo, &before, &after);
            Vec::new()
        }
    }
}

// ----------------------------------------------------------- GitHub calls

/// GitHub as one installation. Its rate limits are kept: a 429, or a 403
/// that says the limit is spent, waits as long as GitHub asks (up to a
/// minute) and tries once more; asked to wait longer, every call for that
/// installation stops until GitHub's reset.
pub struct Gh<'a> {
    cfg: &'a Config,
    store: &'a Store,
    installation: String,
    token: String,
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .timeout_write(Duration::from_secs(30))
            .build()
    })
}

/// A path segment by segment, percent-encoded, slashes kept.
fn encode_path(path: &str) -> String {
    let mut out = String::new();
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// How long GitHub asked to wait: Retry-After, else until the reset.
fn wait_s(response: &ureq::Response) -> f64 {
    if let Some(after) = response.header("retry-after").and_then(|v| v.trim().parse::<f64>().ok()) {
        return after.max(0.0);
    }
    if let Some(reset) = response.header("x-ratelimit-reset").and_then(|v| v.trim().parse::<f64>().ok()) {
        return (reset - now()).max(0.0);
    }
    MAX_WAIT_S
}

impl<'a> Gh<'a> {
    pub fn new(store: &'a Store, cfg: &'a Config, installation: &str) -> Result<Self, String> {
        let token = installation_token(store, cfg, installation)?;
        Ok(Gh { cfg, store, installation: installation.to_string(), token })
    }

    fn pause_key(&self) -> String {
        format!("ghpause:{}", self.installation)
    }

    fn send(&self, method: &str, path: &str, accept: &str, body: Option<&Value>, whole_s: Option<u64>) -> Result<ureq::Response, String> {
        let paused = self.store.eph_get(&self.pause_key()).and_then(|v| v.get("until").and_then(Value::as_f64));
        if paused.is_some_and(|until| until > now()) {
            return Err("GitHub asked Cork to wait; skipped until its limit resets".to_string());
        }
        for attempt in 0..2 {
            let mut request = agent()
                .request(method, &format!("{}{path}", self.cfg.api))
                .set("Authorization", &format!("Bearer {}", self.token))
                .set("Accept", accept)
                .set("X-GitHub-Api-Version", "2022-11-28")
                .set("User-Agent", "cork-server");
            if let Some(seconds) = whole_s {
                request = request.timeout(Duration::from_secs(seconds));
            }
            let result = match body {
                Some(body) => request.set("Content-Type", "application/json").send_string(&body.to_string()),
                None if method == "POST" => request.send_string(""),
                None => request.call(),
            };
            let response = match result {
                Ok(response) => response,
                Err(ureq::Error::Status(_, response)) => response,
                Err(problem) => return Err(format!("GitHub unreachable: {problem}")),
            };
            let status = response.status();
            let spent = response.header("x-ratelimit-remaining") == Some("0") || response.header("retry-after").is_some();
            if status != 429 && !(status == 403 && spent) {
                return Ok(response);
            }
            let wait = wait_s(&response);
            if attempt == 0 && wait <= MAX_WAIT_S {
                std::thread::sleep(Duration::from_secs_f64(wait));
                continue;
            }
            let _ = self.store.eph_set(&self.pause_key(), &json!({"until": now() + wait}), Some(wait.max(1.0)));
            return Err(format!("GitHub's rate limit ({status}); paused {}s", wait.ceil() as i64));
        }
        Err("GitHub's rate limit".to_string())
    }

    /// A JSON call: (status, body or null).
    pub fn json(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
        let response = self.send(method, path, "application/vnd.github+json", body, Some(20))?;
        let status = response.status();
        let mut raw = Vec::new();
        let _ = response.into_reader().take(MAX_JSON_BYTES).read_to_end(&mut raw);
        Ok((status, serde_json::from_slice(&raw).unwrap_or(Value::Null)))
    }

    /// One file at a commit, as text: `None` when it is missing, over the
    /// size cap, binary or generated. The text lives only as long as the
    /// parse that reads it.
    pub fn file(&self, full_name: &str, path: &str, sha: &str) -> Result<Option<String>, String> {
        let response = self.send(
            "GET", &format!("/repos/{full_name}/contents/{}?ref={sha}", encode_path(path)),
            "application/vnd.github.raw", None, Some(20),
        )?;
        if response.status() != 200 {
            return Ok(None);
        }
        let declared = response.header("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
        if declared > MAX_FILE_BYTES {
            return Ok(None);
        }
        let mut raw = Vec::new();
        let _ = response.into_reader().take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut raw);
        Ok(readable(raw))
    }

    /// The repo at `reference` as a gzipped tar stream, size-capped.
    pub fn tarball(&self, full_name: &str, reference: &str) -> Result<Box<dyn Read + Send + Sync>, String> {
        let response = self.send(
            "GET", &format!("/repos/{full_name}/tarball/{}", encode_path(reference)), "application/vnd.github+json", None, None,
        )?;
        if response.status() != 200 {
            return Err(format!("GitHub answered {} for the tarball", response.status()));
        }
        Ok(Box::new(response.into_reader().take(MAX_TARBALL_BYTES)))
    }
}

/// Text worth parsing: within the cap, no NUL byte, UTF-8, and not marked
/// as generated.
fn readable(raw: Vec<u8>) -> Option<String> {
    if raw.len() > MAX_FILE_BYTES || raw.iter().take(8000).any(|b| *b == 0) {
        return None;
    }
    let content = String::from_utf8(raw).ok()?;
    let head: String = content.chars().take(1000).collect();
    if ["@generated", "DO NOT EDIT", "Code generated by", "<auto-generated"].iter().any(|mark| head.contains(mark)) {
        return None;
    }
    Some(content)
}

/// Vendored, built, generated or lock files: never fetched, never parsed.
pub fn skipped_path(path: &str) -> bool {
    const DIRS: [&str; 22] = [
        "node_modules", "vendor", "vendors", "third_party", "third-party", "external", "dist", "build", "out", "target",
        ".git", "__pycache__", ".next", ".nuxt", "coverage", "Pods", "bower_components", ".venv", "venv",
        "site-packages", "generated", "__generated__",
    ];
    if crate::codegraph::is_collide_artifact(path) {
        return true;
    }
    let mut parts: Vec<&str> = path.split('/').collect();
    let name = parts.pop().unwrap_or("").to_lowercase();
    if parts.iter().any(|dir| DIRS.contains(dir)) {
        return true;
    }
    [".min.js", ".min.css", ".map", ".lock", "-lock.json", "-lock.yaml", ".pb.go", "_pb2.py", "_pb2_grpc.py", ".g.dart", ".freezed.dart"]
        .iter()
        .any(|end| name.ends_with(end))
        || name.contains(".generated.")
}

/// A language the hooks' parser reads.
fn parses(path: &str) -> bool {
    cork_core::langs::spec_for_ext(&cork_core::engine::extension_of(path)).is_some()
}

// ---------------------------------------------------- what a PR changes

/// One changed file of a PR, as the PR files API lists it: the name and
/// the hunks' base-side `[start, lines]`. Never the patch.
#[derive(Clone, Debug, Default)]
pub struct Changed {
    pub path: String,
    /// the name at base, for a renamed file
    pub previous: String,
    pub status: String,
    pub old: Vec<[i64; 2]>,
}

impl Changed {
    fn base_path(&self) -> &str {
        if self.previous.is_empty() { &self.path } else { &self.previous }
    }
}

/// The hunks of a patch, `[start, lines]` before and after, read from the
/// `@@ -a,b +c,d @@` headers alone: no other line is looked at.
pub fn hunks_of(patch: &str) -> (Vec<[i64; 2]>, Vec<[i64; 2]>) {
    let range = |spec: &str| -> Option<[i64; 2]> {
        let (start, lines) = spec.split_once(',').unwrap_or((spec, "1"));
        Some([start.parse().ok()?, lines.parse().ok()?])
    };
    let (mut old, mut new) = (Vec::new(), Vec::new());
    for line in patch.lines().filter(|l| l.starts_with("@@ ")) {
        let mut parts = line.split_whitespace().skip(1);
        let (Some(minus), Some(plus)) = (parts.next(), parts.next()) else { continue };
        if let (Some(a), Some(b)) = (minus.strip_prefix('-').and_then(range), plus.strip_prefix('+').and_then(range)) {
            old.push(a);
            new.push(b);
        }
    }
    (old, new)
}

/// The PR's changed files. The patch GitHub sends with each is read for
/// its hunk headers and dropped.
fn pr_files(gh: &Gh, full_name: &str, number: u64) -> Result<Vec<Changed>, String> {
    let mut out = Vec::new();
    for page in 1..=(MAX_PR_FILES / 100) {
        let (status, list) = gh.json("GET", &format!("/repos/{full_name}/pulls/{number}/files?per_page=100&page={page}"), None)?;
        if status != 200 {
            return Err(format!("GitHub answered {status} listing the PR's files"));
        }
        let items = list.as_array().cloned().unwrap_or_default();
        for item in &items {
            let (old, _new) = hunks_of(item.get("patch").and_then(Value::as_str).unwrap_or(""));
            out.push(Changed {
                path: text(item, "filename"),
                previous: text(item, "previous_filename"),
                status: text(item, "status"),
                old,
            });
        }
        if items.len() < 100 {
            break;
        }
    }
    out.retain(|c| !c.path.is_empty());
    Ok(out)
}

/// What one PR does to the code, kept while it is open so the others can be
/// compared with it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shape {
    pub files: Vec<String>,
    /// base-side hunks per file: two PRs on the same lines of a file whose
    /// symbols neither knows
    pub ranges: BTreeMap<String, Vec<[i64; 2]>>,
    /// files whose symbols are known (parsed, or in the map)
    pub mapped: BTreeSet<String>,
    /// `path::symbol` the PR adds, changes, renames or removes
    pub touched: BTreeSet<String>,
    /// renames, removals and signature changes: {path, symbol, kind, new_name, signature}
    pub changes: Vec<Value>,
    /// names the PR's changed code calls, to the first file calling each
    pub calls: BTreeMap<String, String>,
}

impl Shape {
    fn write_into(&self, rec: &mut Value) {
        let ranges: serde_json::Map<String, Value> = self.ranges.iter().map(|(p, r)| (p.clone(), json!(r))).collect();
        rec["files"] = json!(self.files);
        rec["ranges"] = Value::Object(ranges);
        rec["mapped"] = json!(self.mapped);
        rec["touched"] = json!(self.touched.iter().take(500).collect::<Vec<_>>());
        rec["changes"] = json!(self.changes.iter().take(100).collect::<Vec<_>>());
        rec["calls"] = json!(self.calls.iter().take(1000).collect::<BTreeMap<_, _>>());
    }

    fn from_json(rec: &Value) -> Shape {
        let ranges = rec
            .get("ranges")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(path, list)| {
                        let list: Vec<[i64; 2]> = list
                            .as_array()
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(|r| Some([r.get(0)?.as_i64()?, r.get(1)?.as_i64()?]))
                                    .collect()
                            })
                            .unwrap_or_default();
                        (path.clone(), list)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Shape {
            files: strings(rec.get("files")),
            ranges,
            mapped: strings(rec.get("mapped")).into_iter().collect(),
            touched: strings(rec.get("touched")).into_iter().collect(),
            changes: rec.get("changes").and_then(Value::as_array).cloned().unwrap_or_default(),
            calls: rec
                .get("calls")
                .and_then(Value::as_object)
                .map(|map| map.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect())
                .unwrap_or_default(),
        }
    }

    fn start(changed: &[Changed]) -> Shape {
        let mut shape = Shape::default();
        for file in changed.iter().take(MAX_PR_FILES) {
            shape.files.push(file.path.clone());
            if !file.old.is_empty() {
                shape.ranges.insert(file.base_path().to_string(), file.old.clone());
            }
        }
        shape
    }

    fn call(&mut self, name: String, path: &str) {
        if name.chars().count() >= MIN_NAME {
            self.calls.entry(name).or_insert_with(|| path.to_string());
        }
    }
}

/// The names a declaration's edges call: what the file imports (by name,
/// or through an imported module), and what it calls by bare name. A local
/// that merely shares a name is not a call.
fn calls_of<'e>(edges: impl Iterator<Item = (&'e str, &'e str, &'e str)>, imported: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    for (root, member, kind) in edges {
        if imported.contains(root) {
            out.push(if member.is_empty() { root.to_string() } else { member.to_string() });
        } else if member.is_empty() && matches!(kind, "calls" | "inherits" | "uses_type") {
            out.push(root.to_string());
        }
    }
    out
}

struct Parsed {
    /// name -> (signature, hash)
    symbols: BTreeMap<String, (String, String)>,
    /// name -> the names its edges call
    calls: BTreeMap<String, Vec<String>>,
}

/// A clean parse, in memory; `None` for a language the parser does not
/// read or a file that does not parse.
fn parse(path: &str, content: &str) -> Option<Parsed> {
    let out = cork_core::engine::parse(path, content).filter(|o| o.status == crate::semantics::CLEAN)?;
    let imported: BTreeSet<String> = out
        .imports
        .iter()
        .flat_map(|import| import.names.iter().map(|(local, _)| local.clone()))
        .filter(|local| local != "*")
        .collect();
    let mut parsed = Parsed { symbols: BTreeMap::new(), calls: BTreeMap::new() };
    for symbol in &out.symbols {
        parsed.symbols.insert(symbol.name.clone(), (symbol.signature.clone(), symbol.hash.clone()));
        let edges = symbol.edges.iter().map(|(t, m, k)| (t.as_str(), m.as_str(), k.as_str()));
        parsed.calls.insert(symbol.name.clone(), calls_of(edges, &imported));
    }
    Some(parsed)
}

/// The shape from both sides of every changed file, parsed in memory
/// (reading code on). `base` and `head` are `None` where the file could not
/// be read: that file counts by its lines.
pub fn shape_from_code(files: &[(Changed, Option<String>, Option<String>)]) -> Shape {
    let changed: Vec<Changed> = files.iter().map(|(c, _, _)| c.clone()).collect();
    let mut shape = Shape::start(&changed);
    for (file, base, head) in files.iter().take(MAX_PR_FILES) {
        let (path, base_path) = (file.path.as_str(), file.base_path());
        let empty = || Some(Parsed { symbols: BTreeMap::new(), calls: BTreeMap::new() });
        let before = if file.status == "added" { empty() } else { base.as_deref().and_then(|c| parse(base_path, c)) };
        let after = if file.status == "removed" { empty() } else { head.as_deref().and_then(|c| parse(path, c)) };
        let (Some(before), Some(after)) = (before, after) else { continue };
        shape.mapped.insert(path.to_string());
        shape.mapped.insert(base_path.to_string());

        let mut gone: Vec<&String> = before.symbols.keys().filter(|n| !after.symbols.contains_key(*n)).collect();
        let mut new: Vec<&String> = after.symbols.keys().filter(|n| !before.symbols.contains_key(*n)).collect();
        // the same declaration under a new name, paired one to one
        let mut renamed: Vec<(&String, &String)> = Vec::new();
        gone.retain(|old| {
            let old_sig = &before.symbols[*old].0;
            match new.iter().position(|n| crate::semantics::looks_like_rename(old, old_sig, n, &after.symbols[*n].0)) {
                Some(i) => {
                    renamed.push((*old, new.remove(i)));
                    false
                }
                None => true,
            }
        });
        let mut moved: Vec<&String> = Vec::new();
        for (old, to) in renamed {
            shape.touched.insert(format!("{base_path}::{old}"));
            shape.touched.insert(format!("{path}::{to}"));
            shape.changes.push(json!({"path": base_path, "symbol": old, "kind": "renamed", "new_name": to,
                                      "signature": after.symbols[to].0}));
            moved.push(to);
        }
        for old in &gone {
            shape.touched.insert(format!("{base_path}::{old}"));
            shape.changes.push(json!({"path": base_path, "symbol": old, "kind": "removed"}));
        }
        for name in new {
            shape.touched.insert(format!("{path}::{name}"));
            moved.push(name);
        }
        for (name, (signature, hash)) in &after.symbols {
            let Some((was, was_hash)) = before.symbols.get(name) else { continue };
            if was_hash != hash {
                shape.touched.insert(format!("{path}::{name}"));
                moved.push(name);
            }
            if squash(was) != squash(signature) {
                shape.changes.push(json!({"path": path, "symbol": name, "kind": "signature",
                                          "signature": signature, "before": was}));
            }
        }
        for name in moved {
            for called in after.calls.get(name).cloned().unwrap_or_default() {
                shape.call(called, path);
            }
        }
    }
    shape
}

/// Whether a `[first, last]` span meets any `[start, lines]` hunk.
fn meets(span: (i64, i64), hunks: &[[i64; 2]]) -> bool {
    hunks.iter().any(|&[start, lines]| {
        let end = start + lines.max(1) - 1;
        span.0 <= end && start <= span.1
    })
}

/// The shape from hunks alone (reading code off): the map's symbols whose
/// lines the hunks meet, and the renames, removals and signature changes
/// the workspace's agents reported on the PR's branch.
pub fn shape_from_map(store: &Store, scope: &str, changed: &[Changed], branch: &str) -> Shape {
    let mut shape = Shape::start(changed);
    if scope.is_empty() {
        return shape;
    }
    for file in changed.iter().take(MAX_PR_FILES) {
        let base_path = file.base_path();
        let key = format!("{scope}:{}", crate::codegraph::path_key(base_path));
        let Some(record) = store.kv_get(crate::codegraph::GRAPH_BUCKET, &key) else { continue };
        let Some(symbols) = record.get("symbols").and_then(Value::as_object) else { continue };
        shape.mapped.insert(base_path.to_string());
        let imported: BTreeSet<String> = record
            .get("imports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|import| import.get("names").and_then(Value::as_array).cloned().unwrap_or_default())
            .filter_map(|pair| pair.get(0).and_then(Value::as_str).map(str::to_string))
            .filter(|local| local != "*")
            .collect();
        for (name, entry) in symbols {
            let span = entry.get("span").and_then(Value::as_array);
            let first = span.and_then(|s| s.first()).and_then(Value::as_i64).unwrap_or(0);
            let last = span.and_then(|s| s.get(1)).and_then(Value::as_i64).unwrap_or(first);
            if file.status != "removed" && !meets((first, last), &file.old) {
                continue;
            }
            shape.touched.insert(format!("{base_path}::{name}"));
            let edges: Vec<(String, String, String)> = entry
                .get("edges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|e| Some((e.get(0)?.as_str()?.to_string(), e.get(1)?.as_str()?.to_string(), e.get(2)?.as_str()?.to_string())))
                .collect();
            let edges = edges.iter().map(|(t, m, k)| (t.as_str(), m.as_str(), k.as_str()));
            for called in calls_of(edges, &imported) {
                shape.call(called, base_path);
            }
        }
    }
    if branch.is_empty() {
        return shape;
    }
    // what agents on this branch reported renaming, removing or re-signaturing
    let registry = store.kv_get("semlint", scope).unwrap_or(json!({}));
    let files: BTreeSet<&str> = changed.iter().flat_map(|c| [c.path.as_str(), c.base_path()]).collect();
    for (bucket, kind) in [("renames", "renamed"), ("removed", "removed"), ("signatures", "signature")] {
        for rule in registry.get(bucket).and_then(Value::as_object).into_iter().flat_map(|m| m.values()) {
            let path = text(rule, "path");
            let symbol = text(rule, "symbol");
            if text(rule, "branch") != branch || symbol.is_empty() || !files.contains(path.as_str()) {
                continue;
            }
            shape.touched.insert(format!("{path}::{symbol}"));
            let mut change = json!({"path": path, "symbol": symbol, "kind": kind});
            if kind == "renamed" {
                change["new_name"] = json!(text(rule, "new"));
                shape.touched.insert(format!("{path}::{}", text(rule, "new")));
            }
            if kind == "signature" {
                change["signature"] = json!(text(rule, "signature"));
            }
            shape.changes.push(change);
        }
    }
    shape
}

// ------------------------------------------------------------- findings

/// "renames `a` to `b`", "removes `a`", "changes the signature of `a`".
fn what(change: &Value) -> String {
    let symbol = text(change, "symbol");
    match text(change, "kind").as_str() {
        "renamed" => format!("renames `{symbol}` to `{}`", text(change, "new_name")),
        "removed" => format!("removes `{symbol}`"),
        _ => format!("changes the signature of `{symbol}`"),
    }
}

fn change_node(change: &Value) -> String {
    format!("{}::{}", text(change, "path"), text(change, "symbol"))
}

/// A change of one PR that the other's code calls, by name.
fn breaks<'s>(changes: &'s [Value], calls: &'s BTreeMap<String, String>, theirs_touched: &BTreeSet<String>) -> Option<(&'s Value, &'s String)> {
    changes.iter().find_map(|change| {
        let symbol = text(change, "symbol");
        let name = bare(&symbol);
        if name.chars().count() < MIN_NAME || theirs_touched.contains(&change_node(change)) {
            return None;
        }
        calls.get(name).map(|at| (change, at))
    })
}

/// What this PR and one other open PR do to each other, the most pressing
/// first: a break either way, then the same symbols, then the same lines of
/// a file whose symbols neither knows. `None`: they do not meet.
pub fn against(mine: &Shape, theirs: &Shape, other: &Value) -> Option<Value> {
    let number = num(other, "number");
    let who = text(other, "author");
    let pr = if who.is_empty() { format!("#{number}") } else { format!("#{number} ({who})") };
    let finding = |short: String, line: String| Some(json!({"kind": "pr", "number": number, "short": short, "line": line}));
    if let Some((change, at)) = breaks(&mine.changes, &theirs.calls, &theirs.touched) {
        return finding(
            what(change),
            format!("This {}; {pr} calls it in `{at}`. Rebase one onto the other, or coordinate.", what(change)),
        );
    }
    if let Some((change, at)) = breaks(&theirs.changes, &mine.calls, &mine.touched) {
        return finding(
            what(change),
            format!("{pr} {}, and this PR calls it in `{at}`. Rebase onto #{number} once it merges, or coordinate.", what(change)),
        );
    }
    let same: Vec<&String> = mine.touched.intersection(&theirs.touched).collect();
    if let Some(first) = same.first() {
        let (path, symbol) = first.split_once("::").unwrap_or((first.as_str(), ""));
        let more = if same.len() > 1 { format!(" and {} more", same.len() - 1) } else { String::new() };
        return finding(
            format!("also changes `{symbol}`"),
            format!("{pr} also changes `{symbol}` in `{path}`{more}. Whichever merges second needs a rebase; agree on the order."),
        );
    }
    for (path, ranges) in &mine.ranges {
        if mine.mapped.contains(path) || theirs.mapped.contains(path) {
            continue;
        }
        let Some(theirs_ranges) = theirs.ranges.get(path) else { continue };
        if let Some([from, to]) = crate::clash::overlap(ranges, theirs_ranges) {
            let lines = if from == to { format!("line {from}") } else { format!("lines {from}-{to}") };
            return finding(
                format!("same lines of `{path}`"),
                format!("{pr} also changes {lines} of `{path}`. Expect a merge conflict; agree on the order."),
            );
        }
    }
    None
}

/// Stacked PRs are meant to overlap: one's base is the other's branch.
fn stacked(mine: &Value, theirs: &Value) -> bool {
    let (my_head, my_base) = (text(mine, "head_ref"), text(mine, "base_ref"));
    let (their_head, their_base) = (text(theirs, "head_ref"), text(theirs, "base_ref"));
    (!my_base.is_empty() && my_base == their_head) || (!my_head.is_empty() && my_head == their_base)
}

/// Everything the PR author goes by in Cork: their own agents are never
/// reported to them.
fn author_ids(store: &Store, workspace: &str, github_id: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    if github_id.is_empty() {
        return ids;
    }
    if let Some(uid) = store.kv_get(crate::github_app::USER_INDEX, github_id).map(|v| text(&v, "uid")).filter(|u| !u.is_empty()) {
        if let Some(profile) = store.kv_get("profile", &uid) {
            ids.insert(text(&profile, "email"));
        }
        ids.insert(uid);
    }
    if !workspace.is_empty() {
        for member in workspaces::members(store, workspace) {
            if text(&member, "github_id") == github_id {
                ids.insert(text(&member, "uid"));
                ids.insert(text(&member, "email"));
            }
        }
    }
    ids.remove("");
    ids
}

/// A teammate as a PR comment may name them: their GitHub login, else the
/// first word of their name. Never an email: comments on public repos are
/// public.
fn public_name(store: &Store, workspace: &str, user_id: &str) -> String {
    let member = workspaces::members(store, workspace)
        .into_iter()
        .find(|m| text(m, "email") == user_id || text(m, "uid") == user_id);
    let Some(member) = member else { return "a teammate".to_string() };
    let profile = store.kv_get("profile", &text(&member, "uid")).unwrap_or(json!({}));
    for login in [text(&member, "github_login"), text(&profile, "github_login")] {
        if !login.trim().is_empty() {
            return login.trim().to_string();
        }
    }
    text(&member, "name")
        .split_whitespace()
        .next()
        .filter(|first| !first.contains('@'))
        .map(str::to_string)
        .unwrap_or_else(|| "a teammate".to_string())
}

/// Other people's agents on the PR's files or symbols right now: live hot
/// markers and open intents. Never the author's own.
pub fn agents_on(store: &Store, workspace: &str, scope: &str, shape: &Shape, authors: &BTreeSet<String>) -> Vec<Value> {
    // who -> (files, symbols, branch)
    let mut on: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>, String)> = BTreeMap::new();
    let files: BTreeSet<&str> = shape.files.iter().map(String::as_str).chain(shape.ranges.keys().map(String::as_str)).collect();
    for path in &files {
        for (_key, marker) in store.eph_scan(&format!("hot:{scope}:{}:", crate::repo::path_key(path))) {
            let user = text(&marker, "user");
            if user.is_empty() || authors.contains(&user) || text(&marker, "path") != *path {
                continue;
            }
            let entry = on.entry(user.clone()).or_default();
            entry.0.insert(path.to_string());
            for event in marker.get("events").and_then(Value::as_array).into_iter().flatten() {
                let symbol = text(event, "symbol");
                if !symbol.is_empty() {
                    entry.1.insert(symbol);
                }
            }
            if entry.2.is_empty() {
                entry.2 = crate::branches::of(store, scope, &user, &text(&marker, "session"));
            }
        }
    }
    let names: BTreeSet<&str> = shape.touched.iter().filter_map(|t| t.split_once("::").map(|(_, s)| bare(s))).collect();
    for (_key, intent) in store.eph_scan(&format!("intent:{scope}:")) {
        let owner = text(&intent, "owner");
        if owner.is_empty() || authors.contains(&owner) {
            continue;
        }
        let paths: Vec<String> = strings(intent.get("paths")).into_iter().filter(|p| files.contains(p.as_str())).collect();
        let symbols: Vec<String> = strings(intent.get("symbols")).into_iter().filter(|s| names.contains(bare(s))).collect();
        if paths.is_empty() && symbols.is_empty() {
            continue;
        }
        let entry = on.entry(owner.clone()).or_default();
        entry.0.extend(paths);
        entry.1.extend(symbols);
        if entry.2.is_empty() {
            entry.2 = crate::branches::of(store, scope, &owner, &text(&intent, "session"));
        }
    }
    on.into_iter()
        .map(|(user, (files, symbols, branch))| {
            let who = public_name(store, workspace, &user);
            let file = files.iter().next().cloned().unwrap_or_default();
            let target = match symbols.iter().next() {
                Some(symbol) if !file.is_empty() => format!("`{symbol}` in `{file}`"),
                Some(symbol) => format!("`{symbol}`"),
                None => format!("`{file}`"),
            };
            let branch = if branch.is_empty() { String::new() } else { format!(" (branch `{branch}`)") };
            json!({
                "kind": "agent",
                "short": format!("{who}'s agent is on {target}"),
                "line": format!("{who}'s agent is working on {target} right now{branch}. Check with them before merging."),
            })
        })
        .collect()
}

/// What this PR renames, removes or re-signatures, with callers in the map
/// that the PR leaves alone.
pub fn callers_left(store: &Store, scope: &str, shape: &Shape) -> Vec<Value> {
    let mut out = Vec::new();
    for change in shape.changes.iter().take(10) {
        let (path, symbol) = (text(change, "path"), text(change, "symbol"));
        let left: Vec<String> = crate::recipes::callers(store, scope, &path, &symbol)
            .iter()
            .map(|c| (text(c, "path"), text(c, "symbol")))
            .filter(|(p, s)| {
                // a caller in a changed file whose symbols are unknown may be updated
                !shape.touched.contains(&format!("{p}::{s}")) && !(shape.files.contains(p) && !shape.mapped.contains(p))
            })
            .map(|(p, s)| format!("{p}::{s}"))
            .collect();
        if left.is_empty() {
            continue;
        }
        let shown: Vec<String> = left.iter().take(3).map(|c| format!("`{c}`")).collect();
        let more = if left.len() > 3 { format!(" and {} more", left.len() - 3) } else { String::new() };
        let one = left.len() == 1;
        let (verb, fix) = if text(change, "kind") == "removed" {
            (if one { "still uses it" } else { "still use it" }, if one { "Update that call here." } else { "Update those calls here." })
        } else {
            (if one { "isn't updated in this PR" } else { "aren't updated in this PR" }, if one { "Update it here." } else { "Update them here." })
        };
        out.push(json!({
            "kind": "callers",
            "short": format!("{} not updated", plural(left.len(), "caller", "callers")),
            "line": format!("This {}, and {} {verb}: {}{more}. {fix}", what(change), plural(left.len(), "caller", "callers"), shown.join(", ")),
        }));
    }
    out
}

/// Standing decisions on the PR's files, and pinned notes on the symbols it
/// touches. On a public repo the text stays in Cork: the comment says one
/// is there.
pub fn notes_on(store: &Store, workspace: &str, scope: &str, shape: &Shape, private: bool) -> Vec<Value> {
    let mut decisions: Vec<Value> = Vec::new();
    let mut notes: Vec<Value> = Vec::new();
    for (_key, memory) in store.kv_list("memory", &format!("{scope}:")) {
        let anchor = if crate::decisions::is_decision(&memory) {
            if !crate::decisions::is_live(&memory) {
                continue;
            }
            let Some(anchor) = crate::decisions::anchors_of(&memory)
                .into_iter()
                .find(|a| shape.files.iter().any(|f| crate::decisions::covers(a, f)))
            else {
                continue;
            };
            anchor
        } else {
            if crate::compat::truthy(memory.get("superseded_by"))
                || crate::compat::truthy(memory.get("withdrawn"))
                || !text(&memory, "auto").is_empty()
            {
                continue;
            }
            let Some(anchor) = memory.get("anchor").filter(|a| text(a, "kind") == "symbol") else { continue };
            if !shape.touched.contains(&format!("{}::{}", text(anchor, "path"), text(anchor, "symbol"))) {
                continue;
            }
            anchor.clone()
        };
        let label = crate::decisions::anchor_text(&anchor);
        let decision = crate::decisions::is_decision(&memory);
        let kind = if decision { "decision" } else { "note" };
        let line = if private {
            let fact: String = text(&memory, "fact").split_whitespace().collect::<Vec<_>>().join(" ");
            let fact = if fact.chars().count() > 240 { format!("{}…", fact.chars().take(240).collect::<String>()) } else { fact };
            let who = public_name(store, workspace, &text(&memory, "owner"));
            format!("Pinned {kind} on `{label}` ({who}): \"{fact}\"")
        } else {
            format!("Cork has a pinned {kind} on `{label}`. Your team can read it in Cork before merging.")
        };
        let finding = json!({"kind": "note", "short": format!("a pinned {kind} on `{label}`"), "line": line});
        if decision { decisions.push(finding) } else { notes.push(finding) }
    }
    decisions.into_iter().chain(notes).take(3).collect()
}

/// Blocks Cork made on the PR's branch since the last run, added to the
/// PR's tally: a stopped overwrite, a held plan, a claim kept.
fn tally_catches(store: &Store, scope: &str, branch: &str, rec: &mut Value) -> u64 {
    let mut caught = num(rec, "catches");
    if scope.is_empty() || branch.is_empty() {
        return caught;
    }
    let since = rec.get("catch_ts").and_then(Value::as_f64).or_else(|| rec.get("created").and_then(Value::as_f64)).unwrap_or_else(now);
    let stamp = now();
    for row in store.ledger_since_kinds(scope, since, &["gate_block", "collision_prevented", "claim_prevented"]) {
        // a hold on a standing decision is not an overwrite
        if row.ts <= since || (row.kind == "gate_block" && row.payload.get("decision_ids").is_some()) {
            continue;
        }
        if crate::branches::of(store, scope, &text(&row.payload, "user"), &text(&row.payload, "session")) == branch {
            caught += 1;
        }
    }
    rec["catches"] = json!(caught);
    rec["catch_ts"] = json!(stamp);
    caught
}

// --------------------------------------------------------------- output

fn catches_line(catches: u64) -> String {
    format!("Cork stopped {} on this branch.", plural(catches as usize, "overwrite", "overwrites"))
}

/// The check's one-line summary: "✓ no overlap with 6 open PRs", "⚠
/// overlaps #42 (renames `userId`)".
pub fn headline(findings: &[Value], others: usize) -> String {
    let prs: Vec<&Value> = findings.iter().filter(|f| text(f, "kind") == "pr").collect();
    if let Some(first) = prs.first() {
        let more = if prs.len() > 1 { format!(" and {} more", prs.len() - 1) } else { String::new() };
        return format!("⚠ overlaps #{} ({}){more}", num(first, "number"), text(first, "short"));
    }
    if let Some(first) = findings.first() {
        let more = if findings.len() > 1 { format!(", and {} more", findings.len() - 1) } else { String::new() };
        return format!("⚠ {}{more}", text(first, "short"));
    }
    match others {
        0 => "✓ no other open PRs to overlap".to_string(),
        n => format!("✓ no overlap with {}", plural(n, "open PR", "open PRs")),
    }
}

fn bullets(findings: &[Value]) -> String {
    let mut lines: Vec<String> = findings.iter().take(MAX_LINES).map(|f| format!("- {}", text(f, "line"))).collect();
    if findings.len() > MAX_LINES {
        lines.push(format!("- and {} more", findings.len() - MAX_LINES));
    }
    lines.join("\n")
}

/// The check run for a head: `success`, or `neutral` with findings or when
/// the run could not finish. Never `failure`: Cork fails open.
pub fn check_payload(head_sha: &str, findings: &[Value], catches: u64, others: usize, details_url: &str, problem: Option<&str>) -> Value {
    let (conclusion, title, summary) = match problem {
        Some(problem) => (
            "neutral",
            "Cork skipped this run".to_string(),
            format!("Cork couldn't finish checking this PR ({problem}). Nothing is blocked; push again or re-run to retry."),
        ),
        None => {
            let prs = findings.iter().filter(|f| text(f, "kind") == "pr").count();
            let title = if findings.is_empty() {
                "No overlap".to_string()
            } else if prs > 0 {
                format!("Overlaps #{}", findings.iter().find(|f| text(f, "kind") == "pr").map(|f| num(f, "number")).unwrap_or(0))
            } else {
                format!("{} to look at", plural(findings.len(), "thing", "things"))
            };
            let mut summary = headline(findings, others);
            if !findings.is_empty() {
                summary.push_str("\n\n");
                summary.push_str(&bullets(findings));
            }
            if catches > 0 {
                summary.push_str("\n\n");
                summary.push_str(&catches_line(catches));
            }
            summary.push_str("\n\n");
            summary.push_str(FOOTER);
            (if findings.is_empty() { "success" } else { "neutral" }, title, summary)
        }
    };
    let mut payload = json!({
        "name": NAME,
        "head_sha": head_sha,
        "status": "completed",
        "conclusion": conclusion,
        "output": {"title": title, "summary": summary},
    });
    if details_url.starts_with("https://") || details_url.starts_with("http://") {
        payload["details_url"] = json!(details_url);
    }
    payload
}

/// The sticky comment's body, or `None` when there is nothing worth reading.
pub fn comment_body(findings: &[Value], catches: u64) -> Option<String> {
    if findings.is_empty() && catches == 0 {
        return None;
    }
    let mut body = format!("{MARKER}\n");
    if findings.is_empty() {
        body.push_str("**Cork**: nothing overlaps this PR.\n");
    } else {
        body.push_str(&format!("**Cork** found {} before this merges:\n\n", plural(findings.len(), "thing to look at", "things to look at")));
        body.push_str(&bullets(findings));
        body.push('\n');
    }
    if catches > 0 {
        body.push('\n');
        body.push_str(&catches_line(catches));
        body.push('\n');
    }
    body.push('\n');
    body.push_str(FOOTER);
    Some(body)
}

/// What the comment says once its findings have cleared.
pub fn resolved_body() -> String {
    format!("{MARKER}\n**Cork**: resolved. Nothing overlaps this PR now.\n\n{FOOTER}")
}

fn post_check(gh: &Gh, full_name: &str, payload: &Value) -> Result<(), String> {
    match gh.json("POST", &format!("/repos/{full_name}/check-runs"), Some(payload))? {
        (200 | 201, _) => Ok(()),
        (status, _) => Err(format!("GitHub answered {status} to the check run")),
    }
}

/// Cork's comment on the PR, by its marker and the App's bot account.
fn find_comment(gh: &Gh, full_name: &str, number: u64) -> Result<u64, String> {
    let bot = format!("{}[bot]", gh.cfg.slug).to_lowercase();
    for page in 1..=10 {
        let (status, list) =
            gh.json("GET", &format!("/repos/{full_name}/issues/{number}/comments?per_page=100&page={page}"), None)?;
        if status != 200 {
            return Err(format!("GitHub answered {status} listing the comments"));
        }
        let items = list.as_array().cloned().unwrap_or_default();
        for comment in &items {
            let user = comment.get("user").cloned().unwrap_or(json!({}));
            let ours = text(&user, "type") == "Bot" && (gh.cfg.slug.is_empty() || text(&user, "login").to_lowercase() == bot);
            if ours && text(comment, "body").contains(MARKER) {
                return Ok(num(comment, "id"));
            }
        }
        if items.len() < 100 {
            break;
        }
    }
    Ok(0)
}

/// One comment per PR: made when there is something to say, edited after
/// that (to the resolved line when nothing is left), never deleted and never
/// doubled. `rec` remembers the comment and what it last said, so a run
/// that would say the same writes nothing.
pub fn upsert_comment(gh: &Gh, full_name: &str, number: u64, rec: &mut Value, body: Option<&str>) -> Result<&'static str, String> {
    let mut id = num(rec, "comment_id");
    if id == 0 && !crate::compat::truthy(rec.get("comment_searched")) {
        id = find_comment(gh, full_name, number)?;
        rec["comment_searched"] = json!(true);
        rec["comment_id"] = json!(id);
    }
    let wanted = match (body, id) {
        (Some(body), _) => body.to_string(),
        (None, 0) => return Ok("none"),
        (None, _) => resolved_body(),
    };
    let digest = crate::hashing::sha256_hex(&wanted)[..16].to_string();
    if id > 0 {
        if text(rec, "comment_digest") == digest {
            return Ok("unchanged");
        }
        let (status, _) = gh.json("PATCH", &format!("/repos/{full_name}/issues/comments/{id}"), Some(&json!({"body": wanted})))?;
        if (200..300).contains(&status) {
            rec["comment_digest"] = json!(digest);
            return Ok("edited");
        }
        if status != 404 {
            return Err(format!("GitHub answered {status} editing the comment"));
        }
        // someone deleted it: a new one only when there is something to say
        rec["comment_id"] = json!(0);
        rec["comment_digest"] = json!("");
        if body.is_none() {
            return Ok("none");
        }
    }
    let (status, made) = gh.json("POST", &format!("/repos/{full_name}/issues/{number}/comments"), Some(&json!({"body": wanted})))?;
    if status != 201 {
        return Err(format!("GitHub answered {status} making the comment"));
    }
    rec["comment_id"] = json!(num(&made, "id"));
    rec["comment_digest"] = json!(digest);
    Ok("created")
}

// ------------------------------------------------------------- the runs

fn flagged(rec: &Value) -> BTreeSet<u64> {
    rec.get("flagged").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default()
}

fn details_url(ctx: &Ctx, workspace: &str) -> String {
    let base = ctx.dashboard_url.trim_end_matches('/');
    if base.is_empty() || workspace.is_empty() {
        return "https://corkcli.com".to_string();
    }
    format!("{base}/dashboard?workspace={workspace}")
}

/// A PR's record without its shape, from GitHub's copy of the PR.
fn describe(rec: &mut Value, pull: &Value) {
    rec["number"] = json!(num(pull, "number"));
    rec["title"] = json!(text(pull, "title").chars().take(200).collect::<String>());
    rec["author"] = json!(at(pull, "/user/login"));
    rec["author_id"] = json!(id_of(pull.pointer("/user/id")));
    rec["head_sha"] = json!(at(pull, "/head/sha"));
    rec["head_ref"] = json!(at(pull, "/head/ref"));
    rec["base_ref"] = json!(at(pull, "/base/ref"));
}

/// Both sides of each changed file the parser reads, fetched for one parse.
fn fetch_sides(gh: &Gh, full_name: &str, changed: &[Changed], base_sha: &str, head_sha: &str) -> Vec<(Changed, Option<String>, Option<String>)> {
    let mut fetched = 0;
    changed
        .iter()
        .take(MAX_PR_FILES)
        .map(|file| {
            let wanted = fetched < MAX_FETCHED && !skipped_path(&file.path) && parses(&file.path);
            if !wanted {
                return (file.clone(), None, None);
            }
            fetched += 1;
            let base = if file.status == "added" { None } else { gh.file(full_name, file.base_path(), base_sha).ok().flatten() };
            let head = if file.status == "removed" { None } else { gh.file(full_name, &file.path, head_sha).ok().flatten() };
            (file.clone(), base, head)
        })
        .collect()
}

/// The repo's other open PRs, read once (file lists only, against the map)
/// so the first PR checked is compared with the ones already open.
fn seed_open_prs(store: &Store, gh: &Gh, repo: &Repo, scope: &str, except: u64) {
    let repo_id = repo.id();
    if store.kv_get(SEEDED, &repo_id).is_some() {
        return;
    }
    let _ = store.kv_put(SEEDED, &repo_id, &json!({"ts": now()}), now());
    let Ok((200, list)) = gh.json("GET", &format!("/repos/{}/pulls?state=open&per_page={MAX_SEEDED}", repo.full_name), None) else {
        return;
    };
    for pull in list.as_array().cloned().unwrap_or_default() {
        let number = num(&pull, "number");
        let key = pr_key(&repo_id, number);
        if number == 0 || number == except || store.kv_get(PRS, &key).is_some() {
            continue;
        }
        let Ok(changed) = pr_files(gh, &repo.full_name, number) else { continue };
        let mut rec = json!({"created": now(), "lite": true});
        describe(&mut rec, &pull);
        shape_from_map(store, scope, &changed, &at(&pull, "/head/ref")).write_into(&mut rec);
        let _ = store.kv_put(PRS, &key, &rec, now());
    }
}

fn analyze(store: &Store, aliases: &Aliases, cfg: &Config, ctx: &Ctx, repo: &Repo, number: u64, pull: Option<Value>) -> Vec<Job> {
    let gh = match Gh::new(store, cfg, &repo.installation) {
        Ok(gh) => gh,
        Err(problem) => {
            tracing::warn!("pr check {}#{number}: {problem}", repo.full_name);
            return Vec::new();
        }
    };
    let pull = match pull {
        Some(pull) => pull,
        None => match gh.json("GET", &format!("/repos/{}/pulls/{number}", repo.full_name), None) {
            Ok((200, pull)) => pull,
            _ => return Vec::new(),
        },
    };
    if text(&pull, "state") != "open" {
        return closed(store, repo, number);
    }
    let head_sha = at(&pull, "/head/sha");
    if head_sha.is_empty() {
        return Vec::new();
    }
    let repo_id = repo.id();
    let workspace = workspace_of(store, &repo.installation);
    let scope = if workspace.is_empty() { String::new() } else { aliases.scope_for(&workspace, &repo_id) };
    let read_code = !workspace.is_empty() && mode(store, &workspace).read_code;
    let details = details_url(ctx, &workspace);

    let changed = match pr_files(&gh, &repo.full_name, number) {
        Ok(changed) => changed,
        Err(problem) => {
            let _ = post_check(&gh, &repo.full_name, &check_payload(&head_sha, &[], 0, 0, &details, Some(&problem)));
            return Vec::new();
        }
    };
    let head_ref = at(&pull, "/head/ref");
    let shape = if read_code {
        ensure_map(store, &gh, repo, &scope, ctx.hot_ttl_s);
        shape_from_code(&fetch_sides(&gh, &repo.full_name, &changed, &at(&pull, "/base/sha"), &head_sha))
    } else {
        shape_from_map(store, &scope, &changed, &head_ref)
    };
    seed_open_prs(store, &gh, repo, &scope, number);

    let key = pr_key(&repo_id, number);
    let mut rec = store.kv_get(PRS, &key).unwrap_or_else(|| json!({"created": now()}));
    describe(&mut rec, &pull);
    rec["lite"] = json!(false);
    shape.write_into(&mut rec);

    let others: Vec<Value> = store
        .kv_list(PRS, &format!("{repo_id}#"))
        .into_iter()
        .map(|(_, other)| other)
        .filter(|other| num(other, "number") != number && !stacked(&rec, other))
        .collect();
    let mut findings: Vec<Value> = others.iter().filter_map(|other| against(&shape, &Shape::from_json(other), other)).collect();
    if !scope.is_empty() {
        let authors = author_ids(store, &workspace, &text(&rec, "author_id"));
        findings.extend(agents_on(store, &workspace, &scope, &shape, &authors));
        findings.extend(callers_left(store, &scope, &shape));
        findings.extend(notes_on(store, &workspace, &scope, &shape, repo.private));
    }
    let catches = tally_catches(store, &scope, &head_ref, &mut rec);
    let now_flagged: BTreeSet<u64> = findings.iter().map(|f| num(f, "number")).filter(|n| *n > 0).collect();
    rec["flagged"] = json!(now_flagged);

    let payload = check_payload(&head_sha, &findings, catches, others.len(), &details, None);
    if let Err(problem) = post_check(&gh, &repo.full_name, &payload) {
        tracing::warn!("pr check {}#{number}: {problem}", repo.full_name);
    }
    let body = comment_body(&findings, catches);
    if let Err(problem) = upsert_comment(&gh, &repo.full_name, number, &mut rec, body.as_deref()) {
        tracing::warn!("pr check {}#{number}: {problem}", repo.full_name);
    }
    rec["updated"] = json!(now());
    let _ = store.kv_put(PRS, &key, &rec, now());

    // an overlap that is new, or gone, is told to the other side too
    others
        .iter()
        .filter(|other| now_flagged.contains(&num(other, "number")) != flagged(other).contains(&number))
        .map(|other| Job::Pr { repo: repo.clone(), number: num(other, "number"), pull: None })
        .collect()
}

/// A closed or merged PR leaves the comparison; the PRs it overlapped are
/// checked again, so their findings about it clear.
fn closed(store: &Store, repo: &Repo, number: u64) -> Vec<Job> {
    let repo_id = repo.id();
    let _ = store.kv_delete(PRS, &pr_key(&repo_id, number));
    store
        .kv_list(PRS, &format!("{repo_id}#"))
        .into_iter()
        .filter(|(_, other)| flagged(other).contains(&number))
        .map(|(_, other)| Job::Pr { repo: repo.clone(), number: num(&other, "number"), pull: None })
        .collect()
}

/// "Re-run" on GitHub: the suite's PRs again. A fork's PR is not in the
/// suite's list, so its commit is asked for its PRs.
fn rerun(store: &Store, cfg: &Config, repo: &Repo, sha: &str, mut numbers: Vec<u64>) -> Vec<Job> {
    if numbers.is_empty() && !sha.is_empty() {
        let Ok(gh) = Gh::new(store, cfg, &repo.installation) else { return Vec::new() };
        if let Ok((200, list)) = gh.json("GET", &format!("/repos/{}/commits/{sha}/pulls", repo.full_name), None) {
            numbers = list
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| text(p, "state") == "open")
                .map(|p| num(p, "number"))
                .filter(|n| *n > 0)
                .collect();
        }
    }
    numbers.into_iter().map(|number| Job::Pr { repo: repo.clone(), number, pull: None }).collect()
}

/// The badge, once per PR: on an open PR that is not a draft, in a repo a
/// Cork workspace watches, with the workspace's switch on. The description
/// is read fresh and written back with the badge after it; one that already
/// has it is left as it is and remembered, so a person who removes it later
/// is never overruled. What happened, for the log and the tests.
pub fn badge(store: &Store, aliases: &Aliases, cfg: &Config, repo: &Repo, number: u64) -> Result<&'static str, String> {
    let repo_id = repo.id();
    let key = pr_key(&repo_id, number);
    if store.kv_get(BADGES, &key).is_some() {
        return Ok("handled before");
    }
    let workspace = workspace_of(store, &repo.installation);
    let watched = !workspace.is_empty()
        && (crate::github_app::covers(store, &workspace, &repo_id)
            || crate::watch::binding_of(store, aliases, &workspace, &repo_id).is_some());
    if !watched {
        return Ok("not watched");
    }
    if !mode(store, &workspace).badge {
        return Ok("off");
    }
    let gh = Gh::new(store, cfg, &repo.installation)?;
    let path = format!("/repos/{}/pulls/{number}", repo.full_name);
    let pull = match gh.json("GET", &path, None)? {
        (200, pull) => pull,
        (status, _) => return Err(format!("GitHub answered {status} reading the PR")),
    };
    if text(&pull, "state") != "open" {
        return Ok("closed");
    }
    if pull.get("draft").and_then(Value::as_bool).unwrap_or(false) {
        return Ok("draft");
    }
    let body = text(&pull, "body");
    if body.contains(BADGE_MARK) {
        let _ = store.kv_put(BADGES, &key, &json!({"ts": now(), "present": true}), now());
        return Ok("present");
    }
    let with_badge = if body.trim().is_empty() { BADGE.to_string() } else { format!("{}\n\n{BADGE}", body.trim_end()) };
    match gh.json("PATCH", &path, Some(&json!({"body": with_badge})))? {
        (200, _) => {
            let _ = store.kv_put(BADGES, &key, &json!({"ts": now(), "added": true}), now());
            Ok("added")
        }
        (status, _) => Err(format!("GitHub answered {status} adding the badge")),
    }
}

// --------------------------------------------------------------- the map

/// Each regular file of a tar stream, in order: `f(path, size, body)` may
/// read the file from `body`; whatever it leaves is skipped. `f` answers
/// whether to go on. Pax and GNU long names are honoured.
fn each_tar_file(mut reader: impl Read, mut f: impl FnMut(&str, u64, &mut dyn Read) -> bool) -> std::io::Result<()> {
    fn octal(field: &[u8]) -> u64 {
        if field.first().is_some_and(|b| b & 0x80 != 0) {
            return field[1..].iter().fold(0u64, |n, b| n.saturating_mul(256).saturating_add(*b as u64));
        }
        let digits: String = field.iter().map(|b| *b as char).filter(|c| c.is_ascii_digit()).collect();
        u64::from_str_radix(&digits, 8).unwrap_or(0)
    }
    fn field(bytes: &[u8]) -> String {
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).to_string()
    }
    fn skip(reader: &mut impl Read, n: u64) -> std::io::Result<()> {
        std::io::copy(&mut reader.take(n), &mut std::io::sink()).map(|_| ())
    }
    let mut header = [0u8; 512];
    let mut long_name: Option<String> = None;
    loop {
        let mut filled = 0;
        while filled < 512 {
            let n = reader.read(&mut header[filled..])?;
            if n == 0 {
                return Ok(());
            }
            filled += n;
        }
        if header.iter().all(|b| *b == 0) {
            return Ok(());
        }
        let size = octal(&header[124..136]);
        let padding = (512 - size % 512) % 512;
        match header[156] {
            kind @ (b'x' | b'L') => {
                let mut extra = Vec::new();
                (&mut reader).take(size.min(65_536)).read_to_end(&mut extra)?;
                skip(&mut reader, size.saturating_sub(65_536) + padding)?;
                let extra = String::from_utf8_lossy(&extra).to_string();
                long_name = if kind == b'L' {
                    Some(extra.trim_end_matches('\0').to_string())
                } else {
                    extra.lines().find_map(|record| record.split_once(" path=").map(|(_, path)| path.to_string()))
                };
            }
            b'0' | 0 | b'7' => {
                let name = long_name.take().unwrap_or_else(|| {
                    let (name, prefix) = (field(&header[0..100]), field(&header[345..500]));
                    if &header[257..262] == b"ustar" && !prefix.is_empty() { format!("{prefix}/{name}") } else { name }
                });
                let mut body = (&mut reader).take(size);
                let go = f(&name, size, &mut body);
                std::io::copy(&mut body, &mut std::io::sink())?;
                skip(&mut reader, padding)?;
                if !go {
                    return Ok(());
                }
            }
            _ => {
                long_name = None;
                skip(&mut reader, size + padding)?;
            }
        }
    }
}

/// A repo's gzipped tarball folded into the map, the way the hooks' index
/// sweep folds a checkout: each source file parsed in memory, its structure
/// kept and its text dropped with the batch. Vendored, generated, binary and
/// oversized files are skipped. Returns how many files were parsed.
pub fn index_tarball(store: &Store, scope: &str, gzipped: impl Read, hot_ttl_s: f64) -> usize {
    let expanded = flate2::read::GzDecoder::new(gzipped).take(MAX_EXPANDED_BYTES);
    let mut batch: Vec<(String, String)> = Vec::new();
    let mut parsed = 0usize;
    let fold = |batch: &mut Vec<(String, String)>| -> usize {
        let files: Vec<(&str, &str, Option<&Value>)> = batch.iter().map(|(p, c)| (p.as_str(), c.as_str(), None)).collect();
        let answers = crate::report::observe_batch(store, scope, &files, hot_ttl_s);
        batch.clear();
        answers.iter().filter(|a| text(a, "parse") == "clean").count()
    };
    let result = each_tar_file(expanded, |name, size, body| {
        // the archive's top folder is `owner-repo-sha/`
        let Some(path) = name.split_once('/').map(|(_, rest)| rest).filter(|p| !p.is_empty()) else { return true };
        if size as usize > MAX_FILE_BYTES || skipped_path(path) || !parses(path) {
            return true;
        }
        let mut raw = Vec::with_capacity(size as usize);
        if body.read_to_end(&mut raw).is_err() {
            return true;
        }
        if let Some(content) = readable(raw) {
            batch.push((path.to_string(), content));
        }
        if batch.len() >= MAP_BATCH {
            parsed += fold(&mut batch);
        }
        parsed + batch.len() < MAX_MAP_FILES
    });
    if let Err(problem) = result {
        tracing::warn!("tarball for {scope} ended early: {problem}");
    }
    parsed + fold(&mut batch)
}

/// The map from the default branch, built once: the first PR check with
/// code reading on reads the repo's tarball. One run builds it at a time.
fn ensure_map(store: &Store, gh: &Gh, repo: &Repo, scope: &str, hot_ttl_s: f64) {
    if scope.is_empty() || store.kv_get(MAPS, scope).is_some() {
        return;
    }
    let holder = crate::compat::new_id();
    if !store.kv_lease("prmap_lease", scope, &holder, 900.0, now()).map(|(granted, _, _)| granted).unwrap_or(false) {
        return;
    }
    let reference = if repo.default_branch.is_empty() { "HEAD".to_string() } else { repo.default_branch.clone() };
    match gh.tarball(&repo.full_name, &reference) {
        Ok(stream) => {
            let files = index_tarball(store, scope, stream, hot_ttl_s);
            let _ = store.kv_put(MAPS, scope, &json!({"ref": reference, "files": files, "ts": now()}), now());
            tracing::info!("pr checks built the map for {scope}: {files} files");
        }
        Err(problem) => tracing::warn!("pr checks could not read {}'s tarball: {problem}", repo.full_name),
    }
    let _ = store.kv_release_lease("prmap_lease", scope, &holder);
}

/// The default branch moved: with code reading on, the map reads the files
/// that changed (or the whole tarball again, past what is worth fetching
/// one by one).
fn pushed(store: &Store, aliases: &Aliases, cfg: &Config, ctx: &Ctx, repo: &Repo, before: &str, after: &str) {
    let workspace = workspace_of(store, &repo.installation);
    if workspace.is_empty() || !mode(store, &workspace).read_code || after.is_empty() {
        return;
    }
    let scope = aliases.scope_for(&workspace, &repo.id());
    let Ok(gh) = Gh::new(store, cfg, &repo.installation) else { return };
    if store.kv_get(MAPS, &scope).is_none() {
        ensure_map(store, &gh, repo, &scope, ctx.hot_ttl_s);
        return;
    }
    let fresh = before.is_empty() || before.chars().all(|c| c == '0');
    let paths: Option<Vec<String>> = if fresh {
        None
    } else {
        match gh.json("GET", &format!("/repos/{}/compare/{before}...{after}", repo.full_name), None) {
            Ok((200, compare)) => Some(
                compare
                    .get("files")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|f| text(f, "status") != "removed")
                    .map(|f| text(f, "filename"))
                    .filter(|p| !p.is_empty() && !skipped_path(p) && parses(p))
                    .collect(),
            ),
            _ => None,
        }
    };
    match paths {
        Some(paths) if paths.len() <= MAX_FETCHED => {
            let read: Vec<(String, String)> = paths
                .into_iter()
                .filter_map(|path| gh.file(&repo.full_name, &path, after).ok().flatten().map(|content| (path, content)))
                .collect();
            let files: Vec<(&str, &str, Option<&Value>)> = read.iter().map(|(p, c)| (p.as_str(), c.as_str(), None)).collect();
            crate::report::observe_batch(store, &scope, &files, ctx.hot_ttl_s);
            let mut map = store.kv_get(MAPS, &scope).unwrap_or(json!({}));
            map["ts"] = json!(now());
            let _ = store.kv_put(MAPS, &scope, &map, now());
        }
        _ => {
            let _ = store.kv_delete(MAPS, &scope);
            ensure_map(store, &gh, repo, &scope, ctx.hot_ttl_s);
        }
    }
}

// ----------------------------------------------------------------- routes

pub fn router() -> Router<Arc<crate::App>> {
    Router::new().route("/api/workspaces/:workspace_id/pr-checks", get(api_mode).put(api_set_mode))
}

fn is_owner(store: &Store, workspace_id: &str, uid: &str) -> bool {
    workspaces::member(store, workspace_id, uid).as_ref().map(|m| text(m, "role")).as_deref() == Some("owner")
}

/// `GET /api/workspaces/{id}/pr-checks`: whether PR checks read code,
/// whether the owner was asked yet, and whether the App adds the badge to
/// PR descriptions. Every member may read it.
async fn api_mode(State(app): State<Arc<crate::App>>, Path(workspace_id): Path<String>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    let principal = app.dashboard_auth.principal_of(&app.store, &headers, "", now());
    let auth = match crate::auth::workspace_auth(&app.store, principal, &workspace_id) {
        Ok(auth) => auth,
        Err((status, body)) => return (StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN), Json(body)),
    };
    let chosen = mode(&app.store, &auth.workspace);
    (
        StatusCode::OK,
        Json(json!({"read_code": chosen.read_code, "asked": chosen.asked, "badge": chosen.badge,
                    "can_configure": is_owner(&app.store, &auth.workspace, &auth.uid)})),
    )
}

/// `PUT /api/workspaces/{id}/pr-checks {read_code?, badge?}`: owner only,
/// either or both. Turning code reading off stops every read at once; the
/// structure already in the map stays, as the hooks' does (Settings, the
/// repo's data, clears it). Turning the badge off adds no more; badges
/// already added stay where people can see and remove them.
async fn api_set_mode(
    State(app): State<Arc<crate::App>>, Path(workspace_id): Path<String>, headers: HeaderMap, Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    // what Cork may read is the person's to choose, never their agent's
    let principal = match app.dashboard_auth.owner_power(
        &app.store, &headers, now(), &crate::auth::settings_link(&app.dashboard_url, &workspace_id, "integrations"),
    ) {
        Ok(person) => Some(person),
        Err((status, body)) => return (StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN), Json(body)),
    };
    let auth = match crate::auth::workspace_auth(&app.store, principal, &workspace_id) {
        Ok(auth) => auth,
        Err((status, body)) => return (StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN), Json(body)),
    };
    if !is_owner(&app.store, &auth.workspace, &auth.uid) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "only the workspace owner can change PR checks"})));
    }
    let read_code = body.get("read_code").and_then(Value::as_bool);
    let badge = body.get("badge").and_then(Value::as_bool);
    if read_code.is_none() && badge.is_none() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "send read_code or badge, true or false"})));
    }
    let mut chosen = mode(&app.store, &auth.workspace);
    if let Some(read_code) = read_code {
        chosen = set_mode(&app.store, &auth.workspace, read_code, &auth.user_id);
        crate::analytics::capture(&auth.uid, "pr_checks_mode", &auth.workspace, json!({"read_code": read_code}));
    }
    if let Some(badge) = badge {
        chosen = set_badge(&app.store, &auth.workspace, badge, &auth.user_id);
        crate::analytics::capture(&auth.uid, "pr_checks_badge", &auth.workspace, json!({"badge": badge}));
    }
    (StatusCode::OK, Json(json!({"ok": true, "read_code": chosen.read_code, "asked": chosen.asked, "badge": chosen.badge})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("in-memory store")
    }

    /// A stand-in GitHub that remembers: `answer` gets "METHOD /path?query"
    /// and the request body, and says (status, body). Every request is kept.
    struct Fake {
        base: String,
        seen: Arc<Mutex<Vec<(String, String)>>>,
    }

    type Answer = dyn Fn(&str, &str) -> (u16, Vec<u8>) + Send + Sync;

    fn said(text: impl ToString) -> Vec<u8> {
        text.to_string().into_bytes()
    }

    fn fake(answer: Arc<Answer>) -> Fake {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut length = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let mut parts = line.split_whitespace();
                let key = format!("{} {}", parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let body = String::from_utf8_lossy(&body).to_string();
                log.lock().unwrap().push((key.clone(), body.clone()));
                let (status, payload) = answer(&key, &body);
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&payload);
            }
        });
        Fake { base, seen }
    }

    fn config(api: &str) -> Config {
        Config {
            app_id: "4242".into(),
            slug: "cork-test".into(),
            private_key: String::new(),
            webhook_secret: "shh".into(),
            api: api.into(),
            client_id: String::new(),
            client_secret: String::new(),
            web: api.into(),
        }
    }

    /// The installation's token, as if GitHub had issued it already.
    fn with_token(s: &Store, installation: &str) {
        s.eph_set(&format!("ghapptoken:{installation}"), &json!({"token": "ghs_test"}), Some(600.0)).unwrap();
    }

    fn linked(s: &Store, installation: &str, workspace: &str) {
        s.kv_put(crate::github_app::BUCKET, installation, &json!({
            "installation_id": installation, "account": "acme", "account_type": "Organization",
            "workspace_id": workspace, "repos": ["acme/api"], "suspended": false,
        }), now()).unwrap();
        s.kv_put(crate::github_app::ACCOUNT_INDEX, "acme", &json!({"installation_id": installation}), now()).unwrap();
    }

    fn repo() -> Repo {
        Repo { installation: "77".into(), full_name: "acme/api".into(), private: true, default_branch: "main".into() }
    }

    fn ctx() -> Ctx {
        Ctx { hot_ttl_s: 0.0, dashboard_url: "https://corkcli.com".into() }
    }

    fn file(path: &str, status: &str) -> Changed {
        Changed { path: path.into(), status: status.into(), old: vec![[1, 3]], ..Default::default() }
    }

    #[test]
    fn hunk_ranges_come_from_the_headers_alone() {
        let patch = "@@ -10,4 +10,6 @@ def charge(user_id):\n-    old line\n+    new line\n@@ -40 +42,0 @@\n-gone\n@@ not a hunk\n";
        let (old, new) = hunks_of(patch);
        assert_eq!(old, vec![[10, 4], [40, 1]]);
        assert_eq!(new, vec![[10, 6], [42, 0]]);
        assert_eq!(hunks_of(""), (vec![], vec![]));
    }

    #[test]
    fn one_pr_renaming_what_another_calls_flags_both() {
        // #41 renames `user_id` in billing/users.py
        let a = shape_from_code(&[(
            file("billing/users.py", "modified"),
            Some("def user_id(account):\n    return account.id\n".into()),
            Some("def account_id(account):\n    return account.id\n".into()),
        )]);
        assert_eq!(a.changes.len(), 1, "{:?}", a.changes);
        assert_eq!(text(&a.changes[0], "kind"), "renamed");
        // #42 starts calling it in billing/charge.py
        let b = shape_from_code(&[(
            file("billing/charge.py", "modified"),
            Some("from billing.users import user_id\n\ndef charge(account):\n    return 1\n".into()),
            Some("from billing.users import user_id\n\ndef charge(account):\n    return user_id(account)\n".into()),
        )]);
        assert_eq!(b.calls.get("user_id").map(String::as_str), Some("billing/charge.py"), "{:?}", b.calls);

        let rec_a = json!({"number": 41, "author": "kevin"});
        let rec_b = json!({"number": 42, "author": "shira"});
        let seen_by_a = against(&a, &b, &rec_b).expect("#41 is told about #42");
        let seen_by_b = against(&b, &a, &rec_a).expect("#42 is told about #41");
        assert_eq!(text(&seen_by_a, "line"),
            "This renames `user_id` to `account_id`; #42 (shira) calls it in `billing/charge.py`. Rebase one onto the other, or coordinate.");
        assert!(text(&seen_by_b, "line").starts_with("#41 (kevin) renames `user_id` to `account_id`, and this PR calls it in `billing/charge.py`"),
            "{seen_by_b}");
        assert_eq!(headline(&[seen_by_a.clone()], 3), "⚠ overlaps #42 (renames `user_id` to `account_id`)");

        // a third PR on unrelated code meets neither
        let c = shape_from_code(&[(file("docs/build.py", "modified"), Some("def main():\n    pass\n".into()), Some("def main():\n    return 2\n".into()))]);
        assert!(against(&c, &a, &rec_a).is_none() && against(&c, &b, &rec_b).is_none());
        // two PRs changing one function meet too
        let d = shape_from_code(&[(
            file("billing/charge.py", "modified"),
            Some("def charge(account):\n    return 1\n".into()),
            Some("def charge(account):\n    return 3\n".into()),
        )]);
        let same = against(&d, &b, &rec_b).expect("same symbol");
        assert!(text(&same, "line").contains("also changes `charge` in `billing/charge.py`"), "{same}");
    }

    #[test]
    fn both_sides_of_a_new_overlap_are_checked_and_a_closed_pr_clears_them() {
        let s = store();
        let r = repo();
        // #41 was checked before #42 existed: it flagged nobody
        let a = shape_from_code(&[(
            file("billing/users.py", "modified"),
            Some("def user_id(account):\n    return account.id\n".into()),
            Some("def account_id(account):\n    return account.id\n".into()),
        )]);
        let mut rec_a = json!({"number": 41, "author": "kevin", "flagged": []});
        a.write_into(&mut rec_a);
        s.kv_put(PRS, &pr_key(&r.id(), 41), &rec_a, now()).unwrap();
        assert_eq!(Shape::from_json(&rec_a), a, "the shape survives kv");
        // #42 flags #41 and is closed: #41 is checked again
        let mut rec_b = json!({"number": 42, "flagged": [41]});
        Shape::default().write_into(&mut rec_b);
        s.kv_put(PRS, &pr_key(&r.id(), 42), &rec_b, now()).unwrap();
        let mut a_flags = rec_a.clone();
        a_flags["flagged"] = json!([42]);
        s.kv_put(PRS, &pr_key(&r.id(), 41), &a_flags, now()).unwrap();
        let next = closed(&s, &r, 42);
        assert!(matches!(next.as_slice(), [Job::Pr { number: 41, pull: None, .. }]), "{next:?}");
        assert!(s.kv_get(PRS, &pr_key(&r.id(), 42)).is_none());
    }

    #[test]
    fn callers_the_pr_leaves_alone_are_named() {
        let s = store();
        let scope = "ws1:github.com/acme/api";
        crate::report::observe_batch(&s, scope, &[
            ("shop/tax.py", "def compute_tax(amount):\n    return amount\n", None),
            ("shop/cart.py", "from shop.tax import compute_tax\n\ndef total(x):\n    return compute_tax(x)\n", None),
            ("shop/report.py", "from shop.tax import compute_tax\n\ndef summary(x):\n    return compute_tax(x)\n", None),
        ], 0.0);
        // the PR renames compute_tax and updates cart.py, not report.py
        let shape = shape_from_code(&[
            (file("shop/tax.py", "modified"),
             Some("def compute_tax(amount):\n    return amount\n".into()),
             Some("def calculate_tax(amount):\n    return amount\n".into())),
            (file("shop/cart.py", "modified"),
             Some("from shop.tax import compute_tax\n\ndef total(x):\n    return compute_tax(x)\n".into()),
             Some("from shop.tax import calculate_tax\n\ndef total(x):\n    return calculate_tax(x)\n".into())),
        ]);
        let found = callers_left(&s, scope, &shape);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(text(&found[0], "line"),
            "This renames `compute_tax` to `calculate_tax`, and 1 caller isn't updated in this PR: `shop/report.py::summary`. Update it here.");
        // once report.py is in the PR too, nothing is left
        let mut all = shape.clone();
        all.touched.insert("shop/report.py::summary".into());
        assert!(callers_left(&s, scope, &all).is_empty());
    }

    fn pull(number: u64, head: &str) -> Value {
        json!({"number": number, "state": "open", "title": "Charge in cents",
               "user": {"login": "shira", "id": 600},
               "head": {"sha": head, "ref": "feat/cents"}, "base": {"sha": "b0", "ref": "main"}})
    }

    #[test]
    fn reading_code_off_makes_no_contents_calls_and_keeps_no_patch() {
        let s = store();
        with_token(&s, "77");
        linked(&s, "77", "ws1");
        let answer: Arc<Answer> = Arc::new(|key: &str, _body: &str| match key {
            "GET /repos/acme/api/pulls/42/files?per_page=100&page=1" => (200, json!([
                {"filename": "billing/charge.py", "status": "modified",
                 "patch": "@@ -1,2 +1,2 @@\n-SECRET_OLD_LINE\n+SECRET_NEW_LINE\n"}]).to_string().into_bytes()),
            "GET /repos/acme/api/pulls?state=open&per_page=30" => (200, said("[]")),
            "POST /repos/acme/api/check-runs" => (201, "{\"id\": 1}".into()),
            "GET /repos/acme/api/issues/42/comments?per_page=100&page=1" => (200, said("[]")),
            _ => (404, said("{}")),
        });
        let gh = fake(answer);
        let next = run(&s, &Aliases::new(), &config(&gh.base), &ctx(), Job::Pr { repo: repo(), number: 42, pull: Some(pull(42, "h1")) });
        assert!(next.is_empty());
        let seen = gh.seen.lock().unwrap().clone();
        assert!(!seen.iter().any(|(k, _)| k.contains("/contents/") || k.contains("/tarball/")), "{seen:?}");
        let check = seen.iter().find(|(k, _)| k == "POST /repos/acme/api/check-runs").expect("a check run");
        let check: Value = serde_json::from_str(&check.1).unwrap();
        assert_eq!((text(&check, "name"), text(&check, "head_sha"), text(&check, "conclusion")), ("Cork".into(), "h1".into(), "success".into()));
        assert_eq!(at(&check, "/output/summary").lines().next(), Some("✓ no other open PRs to overlap"));
        // the hunks are kept, the patch is not
        let rec = s.kv_get(PRS, "github.com/acme/api#42").expect("the open PR is kept");
        assert_eq!(rec["ranges"]["billing/charge.py"], json!([[1, 2]]));
        assert!(!rec.to_string().contains("SECRET"), "{rec}");
        assert!(!seen.iter().any(|(k, _)| k.contains("/issues/42/comments") && k.starts_with("POST")), "no finding, no comment");
    }

    /// A gzipped tarball as GitHub serves one: `owner-repo-sha/` first.
    fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = Vec::new();
        for (path, content) in files {
            let mut header = [0u8; 512];
            let name = format!("acme-api-abc123/{path}");
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[124..135].copy_from_slice(format!("{:011o}", content.len()).as_bytes());
            header[156] = b'0';
            header[257..262].copy_from_slice(b"ustar");
            tar.extend_from_slice(&header);
            tar.extend_from_slice(content);
            tar.extend(std::iter::repeat(0u8).take((512 - content.len() % 512) % 512));
        }
        tar.extend([0u8; 1024]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn the_tarball_builds_the_map_and_skips_vendored_generated_and_binary() {
        let s = store();
        let scope = "ws1:github.com/acme/api";
        let gz = tarball(&[
            ("shop/tax.py", b"def compute_tax(amount):\n    note = 'SECRET_BODY'\n    return amount\n"),
            ("node_modules/left/index.js", b"function pad() {}\n"),
            ("shop/gen.py", b"# Code generated by protoc. DO NOT EDIT.\ndef made():\n    pass\n"),
            ("shop/blob.py", b"def x():\n\0\0\0"),
            ("README.md", b"# hello\n"),
        ]);
        let parsed = index_tarball(&s, scope, std::io::Cursor::new(gz), 0.0);
        assert_eq!(parsed, 1);
        assert!(crate::codegraph::has_file(&s, scope, "shop/tax.py"));
        for skipped in ["node_modules/left/index.js", "shop/gen.py", "shop/blob.py"] {
            assert!(!crate::codegraph::has_file(&s, scope, skipped), "{skipped}");
        }
        let kept = crate::codegraph::symbols_of(&s, scope, "shop/tax.py");
        assert!(kept.contains_key("compute_tax"));
        assert!(!Value::Object(kept).to_string().contains("SECRET_BODY"), "structure only");
    }

    #[test]
    fn reading_code_on_reads_both_sides_and_the_tarball_once() {
        let s = store();
        with_token(&s, "77");
        linked(&s, "77", "ws1");
        set_mode(&s, "ws1", true, "owner@x.dev");
        let gz = tarball(&[("billing/users.py", b"def user_id(account):\n    return account.id\n")]);
        let answer: Arc<Answer> = Arc::new(move |key: &str, _body: &str| match key {
            "GET /repos/acme/api/pulls/41/files?per_page=100&page=1" => (200, json!([
                {"filename": "billing/users.py", "status": "modified", "patch": "@@ -1,2 +1,2 @@"}]).to_string().into_bytes()),
            "GET /repos/acme/api/tarball/main" => (200, gz.clone()),
            "GET /repos/acme/api/contents/billing/users.py?ref=b0" => (200, said("def user_id(account):\n    return account.id\n")),
            "GET /repos/acme/api/contents/billing/users.py?ref=h1" => (200, said("def account_id(account):\n    return account.id\n")),
            "GET /repos/acme/api/pulls?state=open&per_page=30" => (200, said("[]")),
            "POST /repos/acme/api/check-runs" => (201, said("{}")),
            "GET /repos/acme/api/issues/41/comments?per_page=100&page=1" => (200, said("[]")),
            _ => (404, said("{}")),
        });
        let gh = fake(answer);
        run(&s, &Aliases::new(), &config(&gh.base), &ctx(), Job::Pr { repo: repo(), number: 41, pull: Some(pull(41, "h1")) });
        let rec = s.kv_get(PRS, "github.com/acme/api#41").unwrap();
        assert_eq!(rec["changes"][0]["kind"], json!("renamed"), "{rec}");
        let seen: Vec<String> = gh.seen.lock().unwrap().iter().map(|(k, _)| k.clone()).collect();
        assert!(seen.iter().any(|k| k == "GET /repos/acme/api/contents/billing/users.py?ref=b0"), "{seen:?}");
        assert!(s.kv_get(MAPS, "ws1:github.com/acme/api").is_some(), "the map was built");
        assert!(!rec.to_string().contains("account.id"), "no file text kept: {rec}");
        // a second run reads the PR's files again, not the tarball
        run(&s, &Aliases::new(), &config(&gh.base), &ctx(), Job::Pr { repo: repo(), number: 41, pull: Some(pull(41, "h1")) });
        let tarballs = gh.seen.lock().unwrap().iter().filter(|(k, _)| k.contains("/tarball/")).count();
        assert_eq!(tarballs, 1);
    }

    #[test]
    fn the_comment_is_made_once_edited_after_and_resolved_never_deleted() {
        let s = store();
        with_token(&s, "77");
        let comments: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let held = Arc::clone(&comments);
        let answer: Arc<Answer> = Arc::new(move |key: &str, body: &str| {
            let mut list = held.lock().unwrap();
            match key {
                "GET /repos/acme/api/issues/42/comments?per_page=100&page=1" => (200, json!(*list).to_string().into_bytes()),
                "POST /repos/acme/api/issues/42/comments" => {
                    let made = json!({"id": 501 + list.len() as u64, "body": serde_json::from_str::<Value>(body).unwrap()["body"],
                                      "user": {"login": "cork-test[bot]", "type": "Bot"}});
                    list.push(made.clone());
                    (201, said(made))
                }
                "PATCH /repos/acme/api/issues/comments/501" => {
                    list[0]["body"] = serde_json::from_str::<Value>(body).unwrap()["body"].clone();
                    (200, said(&list[0]))
                }
                _ => (404, said("{}")),
            }
        });
        let fake_gh = fake(answer);
        let cfg = config(&fake_gh.base);
        let gh = Gh::new(&s, &cfg, "77").unwrap();
        let finding = |line: &str| json!({"kind": "callers", "short": "1 caller not updated", "line": line});
        let mut rec = json!({});

        // nothing to say and no comment yet: nothing is made
        assert_eq!(upsert_comment(&gh, "acme/api", 42, &mut rec, comment_body(&[], 0).as_deref()).unwrap(), "none");
        let first = comment_body(&[finding("one")], 0);
        assert_eq!(upsert_comment(&gh, "acme/api", 42, &mut rec, first.as_deref()).unwrap(), "created");
        assert_eq!(upsert_comment(&gh, "acme/api", 42, &mut rec, first.as_deref()).unwrap(), "unchanged");
        let second = comment_body(&[finding("two")], 2);
        assert_eq!(upsert_comment(&gh, "acme/api", 42, &mut rec, second.as_deref()).unwrap(), "edited");
        assert_eq!(upsert_comment(&gh, "acme/api", 42, &mut rec, None).unwrap(), "edited");
        // a record lost (the PR reopened) finds the same comment by its marker
        let mut fresh = json!({});
        assert_eq!(upsert_comment(&gh, "acme/api", 42, &mut fresh, second.as_deref()).unwrap(), "edited");
        assert_eq!(num(&fresh, "comment_id"), 501);

        let list = comments.lock().unwrap();
        assert_eq!(list.len(), 1, "one comment, ever");
        let seen = fake_gh.seen.lock().unwrap();
        assert_eq!(seen.iter().filter(|(k, _)| k.starts_with("DELETE")).count(), 0, "never deleted");
        let resolved: Value = serde_json::from_str(&seen.iter().rev().find(|(k, _)| k.starts_with("PATCH")).unwrap().1).unwrap();
        assert!(text(&resolved, "body").contains("Cork stopped 2 overwrites on this branch."), "the last edit is the reopened PR's");
        let bodies: Vec<String> = seen.iter().filter(|(k, _)| k.starts_with("PATCH")).map(|(_, b)| b.clone()).collect();
        assert!(bodies.iter().any(|b| b.contains("resolved. Nothing overlaps this PR now.")), "{bodies:?}");
        assert!(bodies.iter().all(|b| b.contains("cork:pr-check") && b.contains("npx corkcli") && b.contains("https://corkcli.com/badge.svg")));
    }

    #[test]
    fn the_check_run_is_never_a_failure() {
        let pr = json!({"kind": "pr", "number": 42, "short": "renames `userId`", "line": "This renames `userId`…"});
        let agent = json!({"kind": "agent", "short": "dana's agent is on `charge`", "line": "dana's agent…"});
        for (findings, problem) in [(vec![], None), (vec![pr.clone()], None), (vec![agent.clone(), pr], None), (vec![agent], None), (vec![], Some("GitHub unreachable"))] {
            let payload = check_payload("abc", &findings, 0, 6, "https://corkcli.com", problem);
            let conclusion = text(&payload, "conclusion");
            assert!(conclusion == "success" || conclusion == "neutral", "{payload}");
            assert_eq!(text(&payload, "name"), "Cork");
            assert_eq!(text(&payload, "status"), "completed");
            assert_eq!(conclusion == "success", findings.is_empty() && problem.is_none());
        }
        let clean = check_payload("abc", &[], 1, 6, "", None);
        assert!(at(&clean, "/output/summary").starts_with("✓ no overlap with 6 open PRs\n\nCork stopped 1 overwrite on this branch."));
        assert!(clean.get("details_url").is_none());
    }

    #[test]
    fn a_delivery_is_handled_once() {
        let s = store();
        assert!(first_delivery(&s, "72d3162e-cc78-11e3-81ab-4c9367dc0958"));
        assert!(!first_delivery(&s, "72d3162e-cc78-11e3-81ab-4c9367dc0958"), "GitHub sent it twice");
        assert!(first_delivery(&s, "another"));
        assert!(first_delivery(&s, "") && first_delivery(&s, ""), "no id: never dropped");
    }

    #[test]
    fn rapid_pushes_run_once_on_the_newest_head() {
        let mut q = Queue::default();
        let push = |head: &str| Job::Pr { repo: repo(), number: 42, pull: Some(pull(42, head)) };
        assert!(q.offer(push("h1")), "the first starts a worker");
        assert!(!q.offer(push("h2")), "the worker is already there");
        assert!(!q.offer(push("h3")));
        assert!(q.offer(Job::Pr { repo: repo(), number: 43, pull: None }), "another PR, another worker");
        match q.take("pr:github.com/acme/api#42") {
            Some(Job::Pr { pull: Some(pull), .. }) => assert_eq!(at(&pull, "/head/sha"), "h3"),
            other => panic!("{other:?}"),
        }
        assert!(q.take("pr:github.com/acme/api#42").is_none(), "nothing older runs");
        assert!(q.offer(push("h4")), "the worker stopped: a new push starts one");
        // a close replaces a push still waiting
        q.offer(Job::Closed { repo: repo(), number: 42, merged: false });
        assert!(matches!(q.take("pr:github.com/acme/api#42"), Some(Job::Closed { .. })));
    }

    #[test]
    fn deliveries_become_the_right_jobs() {
        let base = json!({"installation": {"id": 77}, "repository": {"full_name": "Acme/API", "private": false, "default_branch": "main"}});
        let with = |extra: Value| {
            let mut body = base.clone();
            for (k, v) in extra.as_object().unwrap() {
                body[k] = v.clone();
            }
            body
        };
        let opened = jobs_for("pull_request", &with(json!({"action": "opened", "pull_request": pull(7, "h")})));
        assert!(matches!(&opened[..], [Job::Badge { number: 7, .. }, Job::Pr { number: 7, repo, .. }]
            if repo.id() == "github.com/acme/api" && !repo.private));
        assert_eq!(jobs_for("pull_request", &with(json!({"action": "synchronize", "pull_request": pull(7, "h")}))).len(), 1, "no badge on a push");
        assert!(jobs_for("pull_request", &with(json!({"action": "edited", "pull_request": pull(7, "h")}))).is_empty(), "a new title");
        assert_eq!(jobs_for("pull_request", &with(json!({"action": "edited", "pull_request": pull(7, "h"),
            "changes": {"base": {"ref": {"from": "dev"}}}}))).len(), 1, "a new base");
        assert!(matches!(&jobs_for("pull_request", &with(json!({"action": "closed", "pull_request": pull(7, "h")})))[..], [Job::Closed { number: 7, .. }]));
        let rerun = jobs_for("check_run", &with(json!({"action": "rerequested", "check_run": {"name": "Cork", "head_sha": "h",
            "check_suite": {"pull_requests": [{"number": 7}]}}})));
        assert!(matches!(&rerun[..], [Job::Rerun { numbers, .. }] if numbers == &vec![7]));
        assert!(jobs_for("check_run", &with(json!({"action": "rerequested", "check_run": {"name": "CI", "head_sha": "h"}}))).is_empty());
        assert_eq!(jobs_for("push", &with(json!({"ref": "refs/heads/main", "before": "a", "after": "b"}))).len(), 1);
        assert!(jobs_for("push", &with(json!({"ref": "refs/heads/feat", "before": "a", "after": "b"}))).is_empty());
        assert!(jobs_for("star", &with(json!({"action": "created"}))).is_empty());
    }

    #[test]
    fn reading_code_is_off_until_the_owner_answers() {
        let s = store();
        assert_eq!(mode(&s, "ws1"), Mode { read_code: false, asked: false, badge: true });
        // the badge switch is not an answer on reading code
        assert_eq!(set_badge(&s, "ws1", false, "o@x.dev"), Mode { read_code: false, asked: false, badge: false });
        assert_eq!(set_mode(&s, "ws1", true, "o@x.dev"), Mode { read_code: true, asked: true, badge: false });
        assert_eq!(mode(&s, "ws1"), Mode { read_code: true, asked: true, badge: false });
        set_mode(&s, "ws1", false, "o@x.dev");
        set_badge(&s, "ws1", true, "o@x.dev");
        assert_eq!(mode(&s, "ws1"), Mode { read_code: false, asked: true, badge: true });
    }

    #[test]
    fn vendored_and_generated_paths_are_never_read() {
        for path in ["node_modules/x/a.js", "web/dist/app.js", "vendor/lib/a.go", "api/x.pb.go", "a/b_pb2.py", "app.min.js", "Cargo.lock", ".cork/config.json"] {
            assert!(skipped_path(path), "{path}");
        }
        for path in ["src/app.ts", "billing/charge.py", "server/main.rs"] {
            assert!(!skipped_path(path), "{path}");
        }
        assert!(readable(b"def a():\n    pass\n".to_vec()).is_some());
        assert!(readable(b"\0\x01binary".to_vec()).is_none());
        assert!(readable(vec![b'a'; MAX_FILE_BYTES + 1]).is_none());
    }

    /// PR #42 on the stand-in GitHub: read it, and write its description.
    fn pr_on_github(pr: Arc<Mutex<Value>>) -> Fake {
        fake(Arc::new(move |key: &str, body: &str| {
            let mut pr = pr.lock().unwrap();
            match key {
                "GET /repos/acme/api/pulls/42" => (200, said(&*pr)),
                "PATCH /repos/acme/api/pulls/42" => {
                    pr["body"] = serde_json::from_str::<Value>(body).unwrap()["body"].clone();
                    (200, said(&*pr))
                }
                _ => (404, said("{}")),
            }
        }))
    }

    fn open_pr(body: Value) -> Arc<Mutex<Value>> {
        Arc::new(Mutex::new(json!({"number": 42, "state": "open", "draft": false, "body": body})))
    }

    fn badged(s: &Store, gh: &Fake) -> Result<&'static str, String> {
        badge(s, &Aliases::new(), &config(&gh.base), &repo(), 42)
    }

    fn patches(gh: &Fake) -> usize {
        gh.seen.lock().unwrap().iter().filter(|(k, _)| k.starts_with("PATCH")).count()
    }

    fn linked_store() -> Store {
        let s = store();
        with_token(&s, "77");
        linked(&s, "77", "ws1");
        s
    }

    #[test]
    fn the_badge_is_added_once_and_never_again_after_someone_removes_it() {
        let s = linked_store();
        let pr = open_pr(json!("Charges in cents.\n"));
        let gh = pr_on_github(Arc::clone(&pr));
        assert_eq!(badged(&s, &gh), Ok("added"));
        assert_eq!(text(&pr.lock().unwrap(), "body"), format!("Charges in cents.\n\n{BADGE}"));
        // reopened, or marked ready again: nothing more, not even a read
        let calls = gh.seen.lock().unwrap().len();
        assert_eq!(badged(&s, &gh), Ok("handled before"));
        assert_eq!(gh.seen.lock().unwrap().len(), calls);
        // a person takes it out: it stays out
        pr.lock().unwrap()["body"] = json!("Charges in cents.");
        assert_eq!(badged(&s, &gh), Ok("handled before"));
        assert_eq!(patches(&gh), 1);
        assert_eq!(text(&pr.lock().unwrap(), "body"), "Charges in cents.");

        // a PR with no description gets the badge alone
        let s = linked_store();
        let pr = open_pr(Value::Null);
        let gh = pr_on_github(Arc::clone(&pr));
        assert_eq!(badged(&s, &gh), Ok("added"));
        assert_eq!(text(&pr.lock().unwrap(), "body"), BADGE);
    }

    #[test]
    fn a_description_that_has_the_badge_gets_no_second() {
        let s = linked_store();
        let pr = open_pr(json!(format!("Fix.\n\n{BADGE}")));
        let gh = pr_on_github(Arc::clone(&pr));
        assert_eq!(badged(&s, &gh), Ok("present"));
        // the agent's badge, removed by a person, is not put back either
        pr.lock().unwrap()["body"] = json!("Fix.");
        assert_eq!(badged(&s, &gh), Ok("handled before"));
        assert_eq!(patches(&gh), 0);
    }

    #[test]
    fn with_the_switch_off_the_app_adds_nothing() {
        let s = linked_store();
        set_badge(&s, "ws1", false, "o@x.dev");
        let pr = open_pr(json!("Fix."));
        let gh = pr_on_github(Arc::clone(&pr));
        assert_eq!(badged(&s, &gh), Ok("off"));
        assert!(gh.seen.lock().unwrap().is_empty(), "GitHub is not even asked");
        // turned back on, a PR that opens next gets it
        set_badge(&s, "ws1", true, "o@x.dev");
        assert_eq!(badged(&s, &gh), Ok("added"));
    }

    #[test]
    fn a_draft_gets_its_badge_when_it_is_marked_ready() {
        let base = json!({"installation": {"id": 77}, "repository": {"full_name": "acme/api", "default_branch": "main"}});
        let delivery = |action: &str, draft: bool| {
            let mut body = base.clone();
            body["action"] = json!(action);
            body["pull_request"] = pull(42, "h");
            body["pull_request"]["draft"] = json!(draft);
            jobs_for("pull_request", &body)
        };
        assert!(!delivery("opened", true).iter().any(|j| matches!(j, Job::Badge { .. })), "not while a draft");
        assert!(delivery("ready_for_review", false).iter().any(|j| matches!(j, Job::Badge { number: 42, .. })));

        let s = linked_store();
        let pr = Arc::new(Mutex::new(json!({"number": 42, "state": "open", "draft": true, "body": "WIP"})));
        let gh = pr_on_github(Arc::clone(&pr));
        assert_eq!(badged(&s, &gh), Ok("draft"));
        assert_eq!(patches(&gh), 0);
        pr.lock().unwrap()["draft"] = json!(false);
        assert_eq!(badged(&s, &gh), Ok("added"), "a draft is not counted as handled");
    }

    #[test]
    fn closed_prs_and_unwatched_repos_are_left_alone() {
        let s = linked_store();
        let pr = Arc::new(Mutex::new(json!({"number": 42, "state": "closed", "draft": false, "body": "Fix."})));
        let gh = pr_on_github(Arc::clone(&pr));
        assert_eq!(badged(&s, &gh), Ok("closed"));
        assert_eq!(patches(&gh), 0);
        // an installation no workspace has linked: no Cork workspace watches the repo
        let unlinked = store();
        with_token(&unlinked, "77");
        assert_eq!(badged(&unlinked, &gh), Ok("not watched"));

        // a PR closed unmerged may reopen: its record stays; a merged one's goes
        let key = pr_key(&repo().id(), 42);
        s.kv_put(BADGES, &key, &json!({"added": true}), now()).unwrap();
        let cfg = config(&gh.base);
        run(&s, &Aliases::new(), &cfg, &ctx(), Job::Closed { repo: repo(), number: 42, merged: false });
        assert!(s.kv_get(BADGES, &key).is_some());
        run(&s, &Aliases::new(), &cfg, &ctx(), Job::Closed { repo: repo(), number: 42, merged: true });
        assert!(s.kv_get(BADGES, &key).is_none());
    }
}
