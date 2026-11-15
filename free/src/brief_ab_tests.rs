//! The second two-session test (Lattice, hooks v104), as a fixture: session
//! B adds an issue `start_date` (src/db.rs, src/api.rs, tests, API.md,
//! issue.js) while session A, live, works on the 3D views and has left turn
//! notes and a trail behind. B's briefings were about 2.5k tokens, mostly
//! db.rs and space.js constants, stale notes, A's closing words, and two
//! old handoffs quoting other sessions' replies. Each assertion here is one
//! of those, gone; the sizes print beside the ceiling they are held to.

use serde_json::{json, Value};

use crate::store::{now, Store};

const SCOPE: &str = "w:github.com/kevinveramd/cork-linear-test";

const DB_RS: &str = r##"//! SQLite schema, seed data, row loaders and the activity log.
use rusqlite::{Connection, Row};

pub const STATUSES: [&str; 6] = ["backlog", "todo", "in_progress", "in_review", "done", "canceled"];
pub const PROJECT_STATES: [&str; 5] = ["planned", "started", "paused", "completed", "canceled"];
/// Deleted issues can be restored for this long, then they're purged on startup.
pub const TRASH_DAYS: i64 = 30;
/// Activity rows by the same actor of the same kind merge within this window.
pub const BATCH_SECS: i64 = 60;
/// The emoji a feed post can be reacted with, in display order.
pub const REACTIONS: [&str; 3] = ["+1", "heart", "tada"];
const COLORS: [&str; 2] = ["#e5484d", "#30a46c"];

pub fn now() -> String {
    String::new()
}

pub fn is_closed(status: &str) -> bool {
    status == "done" || status == "canceled"
}

pub struct User {
    pub id: i64,
    pub name: String,
}

/// One issue as the API returns it.
pub struct Issue {
    pub id: i64,
    pub title: String,
    pub due_date: Option<String>,
}

const SCHEMA: &str = "CREATE TABLE issues (id INTEGER PRIMARY KEY, title TEXT, due_date TEXT);";

/// Opens the database and creates the schema.
pub fn open(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// The columns issue_from_row reads, in order.
pub const ISSUE_SELECT: &str = "SELECT id, title, due_date FROM issues";

/// An issue from a row of ISSUE_SELECT.
pub fn issue_from_row(r: &Row) -> rusqlite::Result<Issue> {
    Ok(Issue { id: r.get(0)?, title: r.get(1)?, due_date: r.get(2)? })
}

/// Appends to the activity log, merging within BATCH_SECS.
pub fn log_activity(conn: &Connection, issue: i64, kind: &str) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO activity (issue, kind) VALUES (?1, ?2)", (issue, kind))?;
    Ok(())
}

/// The newest activity of an issue, oldest first.
pub fn recent_activity(conn: &Connection, issue: i64) -> rusqlite::Result<Vec<String>> {
    let _ = (conn, issue);
    Ok(Vec::new())
}

pub fn users(conn: &Connection) -> rusqlite::Result<Vec<User>> {
    let _ = conn;
    Ok(Vec::new())
}

pub fn teams(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let _ = conn;
    Ok(Vec::new())
}

pub fn labels(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let _ = conn;
    Ok(Vec::new())
}

pub fn projects(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let _ = conn;
    Ok(Vec::new())
}

pub fn cycles(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let _ = conn;
    Ok(Vec::new())
}
"##;

const API_RS: &str = r##"//! The HTTP API: one handler per route.
use crate::db;

/// A YYYY-MM-DD date from a field, or a 400.
pub fn parse_date(v: &Value, field: &str) -> Result<Option<String>, AppError> {
    let _ = (v, field);
    Ok(None)
}

/// POST /api/issues
pub async fn create_issue(me: Me, State(app): State<AppState>, body: Bytes) -> ApiResult<Json<db::Issue>> {
    let due = parse_date(&body, "due_date")?;
    db::log_activity(&app.conn, 1, "created")?;
    let _ = (me, due);
    todo!()
}

/// PATCH /api/issues/:key
pub async fn update_issue(me: Me, State(app): State<AppState>, body: Bytes) -> ApiResult<Json<db::Issue>> {
    let due = parse_date(&body, "due_date")?;
    db::log_activity(&app.conn, 1, "due_date")?;
    let _ = (me, due);
    todo!()
}
"##;

const ISSUE_JS: &str = r##"// Issue detail: editable title and description, activity, a property sidebar.

const DONE = (i) => i.status === 'done' || i.status === 'canceled';

export function mountIssue(root, { key }) {
  const due = pickDate(root, 'Due date');
  return due;
}

function pickDate(root, label) {
  return { root, label };
}
"##;

const SPACE_JS: &str = r##"// Space: every issue as a block in a 3D scene, one lane per status.

const PREFS_KEY = 'lattice.space.group';
const GROUPS = [['status', 'Status'], ['project', 'Project'], ['assignee', 'Assignee']];
const COLS = 3;
const CELL = 1.15;
const BLOCK = 0.82;
const LANE_W = COLS * CELL + 0.5;
const LANE_PITCH = LANE_W + 0.8;
const LIFT = 0.22;
const GRAY = '#95a2b3';
const HEIGHTS = { 1: 1.7, 2: 1.25, 3: 0.95, 4: 0.7, 0: 0.5 };

function hexToRgb(hex) {
  return [hex, hex, hex];
}

function isDark() {
  return false;
}

export function mountSpace(root) {
  return { root, rgb: hexToRgb(GRAY), dark: isDark() };
}
"##;

const TESTS_RS: &str = r##"use lattice::api;

#[tokio::test]
async fn due_date_is_validated() {
    let _ = api::create_issue;
}
"##;

fn observe(store: &Store, files: &[(&str, &str)]) {
    let batch: Vec<(&str, &str, Option<&Value>)> = files.iter().map(|(p, c)| (*p, *c, None)).collect();
    crate::report::observe_batch(store, SCOPE, &batch, 0.0);
}

fn note(store: &Store, user: &str, fact: &str, anchor: &str, tags: &[&str], auto: &str) -> String {
    let saved = crate::memory::save(store, &crate::memory::SaveInput {
        scope: SCOPE, user_id: user, fact, tags, agent: "claude-code", anchor, auto, supersedes: "",
    });
    saved["memory_id"].as_str().unwrap_or("").to_string()
}

/// The repo as session B found it.
fn lattice() -> Store {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    observe(&store, &[
        ("src/db.rs", DB_RS), ("src/api.rs", API_RS), ("static/js/views/issue.js", ISSUE_JS),
        ("static/js/views/space.js", SPACE_JS), ("tests/api.rs", TESTS_RS),
    ]);
    let stamp = now();
    // the week's writes: A's 3D work ten minutes ago, the rest last night
    for (path, ago) in [("static/js/views/space.js", 600.0), ("src/db.rs", 11.0 * 3600.0), ("static/js/views/issue.js", 14.0 * 3600.0)] {
        store.ledger_append(SCOPE, "edit_reported", &json!({"path": path, "user": "kevin@x.io", "agent": "claude-code", "session": "A"}), stamp - ago).unwrap();
    }
    // A's turns left their closing words as notes on what they changed
    note(&store, "kevin@x.io", "The engine is done and tested; the helper is now building the Space view and wiring it into Lattice. I'll wait for its report.",
        "static/js/views/space.js::COLS", &["rationale", "turn"], "rationale");
    note(&store, "kevin@x.io", "Your request was open to a few readings, so I went with the simplest one: the sidebar now lists only a team called Python.",
        "src/db.rs::open", &["rationale", "turn"], "rationale");
    // a note on code that changed since it was written (open() then had
    // another body: the hash the note was written against)
    let id = note(&store, "kevin@x.io", "open() runs every migration on each start; keep them idempotent.", "src/db.rs::open", &["convention"], "");
    let key = format!("{SCOPE}:{id}");
    let mut stale = store.kv_get("memory", &key).unwrap();
    stale["anchor_hash"] = json!("0000000000000000");
    store.kv_put("memory", &key, &stale, stamp).unwrap();
    // A's trail: a screenshot it took, the view it built; A is mid-turn now
    for (kind, path) in [("reading", "/private/tmp/scratchpad/space-01-status.png"), ("reading", "static/js/views/space.js"), ("edit", "static/js/views/space.js")] {
        crate::presence::note_last_action(&store, SCOPE, "kevin@x.io", "A", kind, path, "claude-code", "", &crate::presence::Effort::default(), "", "", "", "");
    }
    record_trail(&store);
    let _ = store.eph_set(&crate::presence::focus_key(SCOPE, "kevin@x.io", "A"), &json!({"path": "static/js/views/space.js", "action": "running"}), Some(120.0));
    // two older sessions' handoffs: files, and "left" only in their replies
    for (session, ago, title, left) in [
        ("old1", 81.0 * 60.0, "Cork version number", "I haven't messaged the Instagram feature session; stop it in its own window if you want it stopped too."),
        ("old2", 11.0 * 3600.0, "Linear backend work", "I haven't looked at the frontend in a browser myself."),
    ] {
        store.kv_put(crate::handoff::BUCKET, &format!("{SCOPE}:{session}"), &json!({
            "user": "kevin@x.io", "session": session, "ended": stamp - ago, "title": title,
            "files": ["src/db.rs", "src/api.rs"], "symbols": ["SCHEMA"], "tests": {"ok": true, "command": "cargo test", "after_last_write": true},
            "left": [left], "deferred": [],
        }), stamp - ago).unwrap();
    }
    store
}

fn record_trail(store: &Store) {
    crate::trail::record(store, SCOPE, "kevin@x.io", "A", false);
}

fn input<'a>(ids: &'a [String], visible: &'a std::collections::BTreeSet<String>, session: &'a str) -> crate::brief::BriefInput<'a> {
    crate::brief::BriefInput {
        repo_id: "github.com/kevinveramd/cork-linear-test", since: 0.0, budget: 0, identifiers: ids, exclude: &[], session, resume: false,
        idle_after_s: 300.0, visible, viewer: "kevin@x.io", share_knowledge: true, verified: true,
    }
}

/// B's prompt briefing as the agent got it: the module page, then the
/// handoffs main.rs adds, under the briefing's ceiling.
fn prompt_page(store: &Store, ids: &[&str]) -> (Value, String) {
    let visible = std::collections::BTreeSet::new();
    let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
    let mut i = input(&ids, &visible, "B");
    i.budget = 2_400;
    let answer = crate::brief::brief(store, SCOPE, &i);
    let paths: Vec<String> = answer["paths"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
    let handoffs = crate::handoff::brief_lines(store, &crate::handoff::Receiver {
        scope: SCOPE, viewer: "kevin@x.io", session: "B", paths: &paths, identifiers: &ids, q: &[], share_knowledge: true,
    });
    let ceiling = answer["ceiling"].as_u64().unwrap_or(2_400) as usize;
    let text = crate::brief::with_extras(answer["text"].as_str().unwrap_or(""), &handoffs, ceiling);
    (answer, text)
}

/// What session B's hook sent for its spec (prompt_identifiers), the
/// sentence that put views/space.js off limits left out (prompt.rs `without_off_limits`).
const B_IDS: [&str; 13] = [
    "tests/api.rs", "src/db.rs", "src/api.rs", "API.md", "static/js/views/issue.js", "static/js/store.js",
    "kind", "start_date", "table_info", "ISSUE_SELECT", "due_date", "CARGO_TARGET_DIR", "update_issue",
];

#[test]
fn session_b_is_briefed_on_its_task_and_nothing_else() {
    let store = lattice();
    let visible = std::collections::BTreeSet::new();
    let mut start_input = input(&[], &visible, "");
    start_input.budget = 6_000;
    let start = crate::brief::brief(&store, SCOPE, &start_input);
    let start_text = start["text"].as_str().unwrap_or("");
    let (answer, prompt_text) = prompt_page(&store, &B_IDS);
    let ceiling = answer["ceiling"].as_u64().unwrap_or(0) as usize;
    println!(
        "A/B fixture: session start {} chars / {} tokens; prompt {} chars / {} tokens (ceiling {ceiling} for {} file(s))",
        start_text.len(), start_text.len().div_ceil(4), prompt_text.len(), prompt_text.len().div_ceil(4), answer["scope_files"],
    );
    println!("----- session start\n{start_text}\n----- prompt\n{prompt_text}");
    for text in [start_text, prompt_text.as_str()] {
        // another session's reply is never in a briefing
        for reply in ["The engine is done", "Your request was open", "I haven't messaged", "I haven't looked", "said:", "Left:"] {
            assert!(!text.contains(reply), "{reply}:\n{text}");
        }
        // nor a note on code that changed since it was written
        assert!(!text.contains("keep them idempotent"), "{text}");
        // nor a file outside the repo
        assert!(!text.contains("/private/tmp"), "{text}");
        // a module's constants are named, never listed one per line
        for constant in ["  pub const STATUSES", "  const COLS = 3", "  pub const BATCH_SECS"] {
            assert!(!text.contains(constant), "{constant}:\n{text}");
        }
    }
    // A is mid-turn: its trail is not "the last agent here"; its right-now line says where it is
    assert!(!start_text.contains("Map from the last agent here"), "{start_text}");
    // the prompt's page is its task: what it names, in full, first
    assert!(prompt_text.contains("  pub const ISSUE_SELECT: &str = \"SELECT id, title, due_date FROM issues\" ["), "{prompt_text}");
    assert!(prompt_text.contains("  pub async fn update_issue(me: Me, State(app): State<AppState>, body: Bytes) -> ApiResult<Json<db::Issue>> ["), "{prompt_text}");
    // A's live work is a "right now" line, never a module of B's page
    assert!(!prompt_text.lines().any(|l| l.starts_with("static/js/views/space.js")), "{prompt_text}");
    assert!(prompt_text.contains("  pub fn issue_from_row(r: &Row) -> rusqlite::Result<Issue> ["), "{prompt_text}");
    // and never past the ceiling its scope sets
    assert!(prompt_text.len() <= ceiling, "{} > {ceiling}", prompt_text.len());
    assert!(start_text.len() <= 4_600, "session start {} chars", start_text.len());
    // every symbol line goes to the hook to check against its copy
    let checks = answer["checks"].as_array().cloned().unwrap_or_default();
    assert!(checks.iter().any(|c| c["name"] == "update_issue" && c["span"].is_array()), "{checks:?}");
}

#[test]
fn a_short_module_leaves_its_share_to_a_long_one() {
    let sizes = vec![("db.rs".to_string(), 1_400), ("api.rs".to_string(), 300), ("test.rs".to_string(), 90), ("issue.js".to_string(), 200)];
    let room = super::water_fill(&sizes, 1_800);
    assert_eq!((room["test.rs"], room["issue.js"], room["api.rs"]), (90, 200, 300), "the short ones whole");
    assert_eq!(room["db.rs"], 1_210, "the long one the rest, not a quarter");
}

#[test]
fn the_prompt_ceiling_scales_with_the_task() {
    assert_eq!(crate::brief::prompt_ceiling(1), 1_400);
    assert_eq!(crate::brief::prompt_ceiling(2), 1_900);
    assert_eq!(crate::brief::prompt_ceiling(9), 2_400);
    let store = lattice();
    // a one-file task gets a one-file page
    let (one, text) = prompt_page(&store, &["src/api.rs", "update_issue"]);
    assert_eq!(one["scope_files"], json!(1));
    assert!(text.len() <= 1_400, "{}:\n{text}", text.len());
    assert!(text.contains("pub async fn update_issue("), "{text}");
}
