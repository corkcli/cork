//! The day's rhythm, read off the ledger: what greets a person in the
//! morning, and what their day came to by the evening.
//!
//! **Since you left.** A prompt typed by a person (the hook says `here`)
//! marks them present in the repo. Their first session start there after
//! [`AWAY_S`] away, at most once in [`MORNING_EVERY_S`], is told what
//! happened meanwhile, in at most [`MAX_MORNING`] lines: what teammates
//! changed, handoffs waiting, messages for their agents, and how each of
//! their own runs that went on without them ended. The first start ever
//! seen here only marks them present: "since you left" needs a leaving.
//!
//! **Today.** `cork today` and the dashboard's card: the person's own
//! sessions since a moment they name (their local midnight), what Cork
//! caught for them, what it saved, and what is left open.
//!
//! On the local server every agent session is its own identity, and every
//! one of them is the person's: there are no teammates, and every run is
//! theirs.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::store::Store;

/// Away at least this long makes the next start a morning.
pub const AWAY_S: f64 = 6.0 * 3600.0;
/// At most one morning briefing in this long.
pub const MORNING_EVERY_S: f64 = 16.0 * 3600.0;
/// The most lines a morning briefing has, its first included.
pub const MAX_MORNING: usize = 5;
/// The most of the person's own runs it names.
const MAX_RUNS: usize = 2;
/// How far back "since you left" reaches at most.
const LONGEST_AWAY_S: f64 = 4.0 * 86_400.0;
/// How long a person's presence is remembered.
const KEEP_S: f64 = 14.0 * 86_400.0;
/// The rows that are a collision caught for the person: a write held, a
/// numbered file moved to a free number, a planned group held until it was
/// clear.
pub const CATCH_KINDS: [&str; 3] = ["gate_block", "claim_prevented", "collision_prevented"];
/// The rows that say an agent of the person's worked in a repo.
const ACTIVITY_KINDS: [&str; 2] = ["edit_reported", "turn_usage"];

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Whose work is the person's: theirs by name, or on the local server,
/// every agent's.
#[derive(Clone, Copy)]
pub struct Viewer<'a> {
    pub user: &'a str,
    pub local: bool,
}

impl<'a> Viewer<'a> {
    pub fn mine(&self, user: &str) -> bool {
        self.local || user == self.user
    }

    /// The person, as their presence is kept: on the local server each
    /// session has its own identity, and the machine is one person.
    fn who(&self) -> &'a str {
        if self.local { "" } else { self.user }
    }
}

fn here_key(scope: &str, viewer: Viewer) -> String {
    format!("here:{scope}:{}", viewer.who())
}

fn morning_key(scope: &str, viewer: Viewer) -> String {
    format!("morning:{scope}:{}", viewer.who())
}

/// The person typed a prompt here (or started a session): present now.
pub fn note_here(store: &Store, scope: &str, viewer: Viewer, at: f64) {
    let _ = store.eph_set(&here_key(scope, viewer), &json!({"ts": at}), Some(KEEP_S));
}

/// When the person was last here, and when they last had a morning
/// briefing: whether this start is a morning, and since when.
pub fn morning_since(last_here: Option<f64>, last_morning: Option<f64>, at: f64) -> Option<f64> {
    let here = last_here?;
    if at - here < AWAY_S || last_morning.is_some_and(|m| at - m < MORNING_EVERY_S) {
        return None;
    }
    Some(here.max(at - LONGEST_AWAY_S))
}

/// A session with a person at it starts: the lines of what happened since
/// they left, on their first start after a while away; empty otherwise.
/// Either way they are here now. `picked` is the handoffs the start took.
pub fn morning(store: &Store, scope: &str, viewer: Viewer, session: &str, picked: usize, at: f64) -> Vec<String> {
    let stamp = |key: &str| store.eph_get(key).and_then(|v| v.get("ts").and_then(Value::as_f64));
    let since = morning_since(stamp(&here_key(scope, viewer)), stamp(&morning_key(scope, viewer)), at);
    note_here(store, scope, viewer, at);
    let Some(since) = since else { return Vec::new() };
    let _ = store.eph_set(&morning_key(scope, viewer), &json!({"ts": at}), Some(KEEP_S));
    morning_lines(store, scope, viewer, session, picked, since, at)
}

/// What happened in the repo since `since`, at most [`MAX_MORNING`] lines,
/// the first saying how long the person was away.
pub fn morning_lines(store: &Store, scope: &str, viewer: Viewer, session: &str, picked: usize, since: f64, at: f64) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    // teammates' changes, by person
    let mut by_person: BTreeMap<String, (u64, BTreeSet<String>)> = BTreeMap::new();
    for (_, _, fields) in store.ledger_fields_since(scope, since, &["edit_reported"], &["user", "path"], 5_000) {
        let (user, path) = (&fields[0], &fields[1]);
        if user.is_empty() || viewer.mine(user) {
            continue;
        }
        let entry = by_person.entry(user.clone()).or_default();
        entry.0 += 1;
        if !path.is_empty() {
            entry.1.insert(path.clone());
        }
    }
    let mut people: Vec<(String, (u64, BTreeSet<String>))> = by_person.into_iter().collect();
    people.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    if !people.is_empty() {
        let named: Vec<String> = people.iter().take(2).map(|(who, (_, files))| format!("{who} changed {}", files_words(files))).collect();
        let more = if people.len() > 2 { format!(", and {} more", people.len() - 2) } else { String::new() };
        lines.push(format!("{}{more}.", named.join("; ")));
    }
    // handoffs: the ones this start took, and any still waiting
    let waiting = picked + store.kv_list(crate::local::HANDOFF_BUCKET, &format!("{scope}:{}:", handoff_owner(viewer)))
        .into_iter()
        .filter(|(_, h)| !crate::agenttools::is_expired(h.get("ts").and_then(Value::as_f64).unwrap_or(0.0), at))
        .count();
    if waiting > 0 {
        lines.push(if picked > 0 {
            format!("{} waiting for you; this session took {}.", plural(waiting, "handoff was", "handoffs were"), if picked == 1 { "it".to_string() } else { picked.to_string() })
        } else {
            format!("{} waiting for you.", plural(waiting, "handoff is", "handoffs are"))
        });
    }
    // messages for the person's agents no session has taken yet
    if !viewer.local {
        let mut from: BTreeSet<String> = BTreeSet::new();
        let mut count = 0usize;
        for (_, message) in store.kv_list("inbox", &format!("{scope}:{}:", viewer.user)) {
            let sender = text(&message, "from");
            if crate::agenttools::is_expired(message.get("ts").and_then(Value::as_f64).unwrap_or(0.0), at)
                || sender.split('#').next() == Some(viewer.user)
            {
                continue;
            }
            count += 1;
            from.insert(sender.split('#').next().unwrap_or("").to_string());
        }
        if count > 0 {
            let who: Vec<String> = from.into_iter().filter(|f| !f.is_empty()).take(3).collect();
            let who = if who.is_empty() { String::new() } else { format!(", from {}", who.join(", ")) };
            lines.push(format!("{} for your agents{who}.", plural(count, "unread message", "unread messages")));
        }
    }
    // the person's own runs that went on while they were away
    let room = MAX_MORNING.saturating_sub(lines.len() + 1).min(MAX_RUNS);
    lines.extend(runs(store, scope, viewer, session, since, at).into_iter().take(room));
    if lines.is_empty() {
        return Vec::new();
    }
    let mut out = vec![format!("Cork, since the user was last here ({} ago):", age_words(at - since))];
    out.extend(lines.into_iter().map(|l| format!("- {l}")));
    out.truncate(MAX_MORNING);
    out
}

fn handoff_owner<'a>(viewer: Viewer<'a>) -> &'a str {
    viewer.who()
}

/// One line per run of the person's that worked after `since`: how long,
/// what it changed, and how it ended.
fn runs(store: &Store, scope: &str, viewer: Viewer, session: &str, since: f64, at: f64) -> Vec<String> {
    let journal = crate::insights::journal(store, scope, ((at - since) / 86_400.0).max(0.02));
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for stretch in journal.get("sessions").and_then(Value::as_array).into_iter().flatten() {
        let (user, id) = (text(stretch, "user"), text(stretch, "session"));
        if !viewer.mine(&user) || id.is_empty() || id == session || !seen.insert(format!("{user}#{id}")) {
            continue;
        }
        let whole = stretch.get("whole").filter(|w| w.is_object()).unwrap_or(stretch);
        let end = whole.get("end").and_then(Value::as_f64).unwrap_or(0.0);
        let edits = whole.get("edits").and_then(Value::as_u64).unwrap_or(0);
        if end <= since || edits == 0 {
            continue;
        }
        let start = whole.get("start").and_then(Value::as_f64).unwrap_or(end).max(since);
        let title = crate::freshness::title_of(store, scope, &user, &id);
        let name = if title.is_empty() { format!("A run of yours ({})", short(&id)) } else { format!("\"{title}\"") };
        let files = whole.get("files").and_then(Value::as_u64).unwrap_or(0);
        out.push(format!(
            "{name} worked {} while you were away: {} in {}, last active {} ago; {}.",
            span_words(end - start), plural(edits as usize, "edit", "edits"), plural(files as usize, "file", "files"),
            age_words(at - end), ending(store, scope, &id),
        ));
    }
    out
}

/// How a run ended, from the handoff it left and the last tests it ran.
fn ending(store: &Store, scope: &str, session: &str) -> String {
    let left = store.kv_get(crate::handoff::BUCKET, &format!("{scope}:{session}"));
    let first_left = left.as_ref()
        .and_then(|h| h.get("left").and_then(Value::as_array).and_then(|l| l.first()).and_then(Value::as_str).map(str::to_string));
    let tests = store.eph_get(&format!("extests:{scope}:{session}")).and_then(|t| t.get("ok").and_then(Value::as_bool));
    let tests = match tests {
        Some(true) => "tests passing",
        Some(false) => "tests failing",
        None => "no test run seen",
    };
    match first_left {
        Some(item) => format!("{tests}, left open: {}", item.chars().take(100).collect::<String>()),
        None => tests.to_string(),
    }
}

/// The person's day across `scopes` since `since`: what their agents did,
/// what Cork caught and saved for them, and what is left open, as numbers
/// and as plain lines.
pub fn day(store: &Store, scopes: &[String], viewer: Viewer, since: f64, at: f64) -> Value {
    let days = ((at - since) / 86_400.0).max(0.02);
    let (mut sessions, mut edits, mut caught, mut tokens_saved) = (0usize, 0u64, 0usize, 0u64);
    let mut usd_saved = 0.0f64;
    let mut files: BTreeMap<String, u64> = BTreeMap::new();
    let mut repos: BTreeSet<String> = BTreeSet::new();
    let mut left: Vec<String> = Vec::new();
    let (mut handoffs, mut deferred) = (0usize, 0usize);
    for scope in scopes {
        let repo = scope.split_once(':').map(|(_, r)| r).unwrap_or(scope).to_string();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        // a repo the person's agents did nothing in since is read no further
        // than what is left open in it
        let active = store.ledger_fields_since(scope, since, &ACTIVITY_KINDS, &["user"], 5_000).iter().any(|(_, _, f)| viewer.mine(&f[0]));
        let journal = if active { crate::insights::journal(store, scope, days) } else { json!({}) };
        for stretch in journal.get("sessions").and_then(Value::as_array).into_iter().flatten() {
            let user = text(stretch, "user");
            if !viewer.mine(&user) || stretch.get("end").and_then(Value::as_f64).unwrap_or(0.0) < since {
                continue;
            }
            let id = text(stretch, "session");
            if seen.insert(format!("{user}#{id}#{}", text(stretch, "agent"))) {
                sessions += 1;
            }
            let n = stretch.get("edits").and_then(Value::as_u64).unwrap_or(0);
            edits += n;
            if n > 0 {
                repos.insert(repo.clone());
            }
            // each run's most-written files, most first
            for (rank, path) in stretch.get("top_paths").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).enumerate() {
                *files.entry(path.to_string()).or_default() += 4 - rank.min(3) as u64;
            }
        }
        caught += store.ledger_fields_since(scope, since, &CATCH_KINDS, &["user"], 10_000)
            .into_iter().filter(|(_, _, f)| viewer.mine(&f[0])).count();
        let kinds: Vec<&str> = crate::briefstat::SOURCE_KINDS.iter().map(|(k, _)| *k).collect();
        for row in if active { store.ledger_since_kinds(scope, since, &kinds) } else { Vec::new() } {
            if !viewer.mine(&text(&row.payload, "user")) {
                continue;
            }
            if let Some(saving) = crate::briefstat::row_saving(&row.kind, &row.payload) {
                tokens_saved += saving.tokens_saved;
                usd_saved += saving.est_usd - saving.cost_usd;
            }
        }
        // what is left open: runs that handed work on, handoffs waiting,
        // work deferred
        for (_, h) in store.kv_list(crate::handoff::BUCKET, &format!("{scope}:")) {
            if viewer.mine(&text(&h, "user")) {
                if let Some(item) = h.get("left").and_then(Value::as_array).and_then(|l| l.first()).and_then(Value::as_str) {
                    left.push(item.chars().take(100).collect());
                }
            }
        }
        handoffs += store.kv_list(crate::local::HANDOFF_BUCKET, &format!("{scope}:{}:", handoff_owner(viewer)))
            .into_iter()
            .filter(|(_, h)| !crate::agenttools::is_expired(h.get("ts").and_then(Value::as_f64).unwrap_or(0.0), at))
            .count();
        deferred += store.kv_list("tripwire", &format!("{scope}:"))
            .into_iter()
            .filter(|(_, t)| viewer.mine(&text(t, "owner")) && t.get("fired").map(Value::is_null).unwrap_or(true))
            .count();
    }
    let mut top: Vec<(String, u64)> = files.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    let mut lines: Vec<String> = Vec::new();
    if sessions == 0 {
        lines.push("No agent of yours worked today.".to_string());
    } else {
        let most: Vec<String> = top.iter().take(3).map(|(p, _)| p.clone()).collect();
        let most = if most.is_empty() { String::new() } else { format!(", most in {}", most.join(", ")) };
        let across = if repos.len() > 1 { format!(" across {} repos", repos.len()) } else { String::new() };
        lines.push(format!("Your agents ran {}{across}: {}{most}.", plural(sessions, "session", "sessions"), plural(edits as usize, "edit", "edits")));
    }
    lines.push(match caught {
        0 => "Nothing collided.".to_string(),
        n => format!("Cork caught {} before it landed.", plural(n, "collision", "collisions")),
    });
    if tokens_saved > 0 {
        lines.push(format!("Its briefings saved about {} tokens (~${:.2}).", tokens_words(tokens_saved), usd_saved.max(0.0)));
    }
    let mut open: Vec<String> = Vec::new();
    if handoffs > 0 {
        open.push(format!("{} waiting", plural(handoffs, "handoff", "handoffs")));
    }
    if deferred > 0 {
        open.push(format!("{} deferred", plural(deferred, "task", "tasks")));
    }
    for item in left.iter().take(2) {
        open.push(format!("\"{item}\""));
    }
    lines.push(if open.is_empty() { "Nothing is left open.".to_string() } else { format!("Left open: {}.", open.join("; ")) });
    json!({
        "since": since, "sessions": sessions, "edits": edits, "repos": repos.len(), "caught": caught,
        "tokens_saved": tokens_saved, "est_usd_saved": (usd_saved.max(0.0) * 100.0).round() / 100.0,
        "top_files": top.iter().take(5).map(|(p, n)| json!([p, n])).collect::<Vec<_>>(),
        "open": {"handoffs": handoffs, "deferred": deferred, "left": left.iter().take(5).collect::<Vec<_>>()},
        "lines": lines,
    })
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

fn short(session: &str) -> String {
    session.chars().filter(char::is_ascii_alphanumeric).take(8).collect()
}

/// "a.py", "a.py and b.py", "a.py, b.py and 3 more files".
fn files_words(files: &BTreeSet<String>) -> String {
    let names: Vec<&str> = files.iter().map(|f| f.rsplit('/').next().unwrap_or(f)).collect();
    match names.len() {
        0 => "code".to_string(),
        1 => names[0].to_string(),
        2 => format!("{} and {}", names[0], names[1]),
        n => format!("{}, {} and {} more files", names[0], names[1], n - 2),
    }
}

/// "40m", "9h", "2 days".
fn age_words(seconds: f64) -> String {
    let s = seconds.max(0.0);
    if s < 3600.0 {
        format!("{}m", (s / 60.0).round().max(1.0))
    } else if s < 48.0 * 3600.0 {
        format!("{}h", (s / 3600.0).round())
    } else {
        format!("{} days", (s / 86_400.0).round())
    }
}

/// "25m", "1h 40m".
fn span_words(seconds: f64) -> String {
    let minutes = (seconds.max(60.0) / 60.0).round() as u64;
    if minutes < 60 { format!("{minutes}m") } else { format!("{}h {}m", minutes / 60, minutes % 60) }
}

fn tokens_words(n: u64) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1_000_000.0) } else if n >= 1_000 { format!("{}k", n / 1_000) } else { n.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPE: &str = "w:github.com/acme/app";

    fn db() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn me() -> Viewer<'static> {
        Viewer { user: "kevin@x.com", local: false }
    }

    #[test]
    fn a_morning_is_the_first_start_after_a_while_away_once_a_day() {
        let at = 2_000_000.0;
        // never here before: nothing to say since, only here now
        assert_eq!(morning_since(None, None, at), None);
        // back after a short break: not a morning
        assert_eq!(morning_since(Some(at - 3600.0), None, at), None);
        // back after a night away: since they left
        assert_eq!(morning_since(Some(at - 10.0 * 3600.0), None, at), Some(at - 10.0 * 3600.0));
        // a second long break the same day: no second morning
        assert_eq!(morning_since(Some(at - 7.0 * 3600.0), Some(at - 8.0 * 3600.0), at), None);
        // the next day, again
        assert_eq!(morning_since(Some(at - 7.0 * 3600.0), Some(at - 20.0 * 3600.0), at), Some(at - 7.0 * 3600.0));
        // a long absence reaches back four days at most
        assert_eq!(morning_since(Some(at - 30.0 * 86_400.0), None, at), Some(at - LONGEST_AWAY_S));
    }

    #[test]
    fn the_morning_says_what_teammates_did_and_comes_once() {
        let store = db();
        let now = crate::store::now();
        // first start ever: only marks the person here
        assert!(morning(&store, SCOPE, me(), "s1", 0, now - 10.0 * 3600.0).is_empty());
        for (path, user) in [("api/pay.py", "bob@x.com"), ("api/db.py", "bob@x.com"), ("web/ui.tsx", "ana@x.com"), ("mine.py", "kevin@x.com")] {
            store.ledger_append(SCOPE, "edit_reported", &json!({"user": user, "session": "t", "path": path}), now - 3600.0).unwrap();
        }
        store.kv_put("inbox", &format!("{SCOPE}:kevin@x.com:m1"), &json!({"id": "m1", "from": "bob@x.com#t", "ts": now - 60.0}), now).unwrap();
        let said = morning(&store, SCOPE, me(), "s2", 1, now);
        assert!(said[0].starts_with("Cork, since the user was last here (10h ago):"), "{said:?}");
        assert!(said.iter().any(|l| l == "- bob@x.com changed db.py and pay.py; ana@x.com changed ui.tsx."), "{said:?}");
        assert!(said.iter().any(|l| l == "- 1 handoff was waiting for you; this session took it."), "{said:?}");
        assert!(said.iter().any(|l| l == "- 1 unread message for your agents, from bob@x.com."), "{said:?}");
        assert!(!said.iter().any(|l| l.contains("mine.py")));
        assert!(said.len() <= MAX_MORNING);
        // the next start the same morning hears nothing
        assert!(morning(&store, SCOPE, me(), "s3", 0, now + 60.0).is_empty());
    }

    #[test]
    fn a_run_that_went_on_alone_says_how_it_ended() {
        let store = db();
        let now = crate::store::now();
        for i in 0..3 {
            store.ledger_append(SCOPE, "edit_reported", &json!({"user": "kevin@x.com", "session": "night", "path": format!("f{i}.py")}),
                now - 5.0 * 3600.0 + f64::from(i) * 600.0).unwrap();
        }
        store.eph_set("extests:w:github.com/acme/app:night", &json!({"ok": false, "ts": now - 4.0 * 3600.0}), None).unwrap();
        let said = morning_lines(&store, SCOPE, me(), "today", 0, now - 9.0 * 3600.0, now);
        let run = said.iter().find(|l| l.contains("A run of yours (night)")).cloned().unwrap_or_default();
        assert!(run.contains("worked 20m while you were away: 3 edits in 3 files") && run.ends_with("tests failing."), "{said:?}");
        // the session starting now is not a run that went on without them
        assert!(morning_lines(&store, SCOPE, me(), "night", 0, now - 9.0 * 3600.0, now).is_empty());
    }

    #[test]
    fn the_day_counts_the_persons_own_work_and_catches() {
        let store = db();
        let now = crate::store::now();
        store.ledger_append(SCOPE, "edit_reported", &json!({"user": "kevin@x.com", "session": "a", "path": "x.py"}), now - 600.0).unwrap();
        store.ledger_append(SCOPE, "edit_reported", &json!({"user": "bob@x.com", "session": "b", "path": "y.py"}), now - 600.0).unwrap();
        store.ledger_append(SCOPE, "gate_block", &json!({"user": "kevin@x.com", "session": "a", "path": "x.py"}), now - 500.0).unwrap();
        store.ledger_append(SCOPE, "claim_prevented", &json!({"user": "bob@x.com", "session": "b"}), now - 500.0).unwrap();
        let mine = day(&store, &[SCOPE.to_string()], me(), now - 3600.0, now);
        assert_eq!(mine["sessions"], json!(1));
        assert_eq!(mine["edits"], json!(1));
        assert_eq!(mine["caught"], json!(1));
        let lines: Vec<&str> = mine["lines"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert_eq!(lines[0], "Your agents ran 1 session: 1 edit, most in x.py.");
        assert_eq!(lines[1], "Cork caught 1 collision before it landed.");
        assert_eq!(*lines.last().unwrap(), "Nothing is left open.");
        // on the local server every agent is the person's
        let local = day(&store, &[SCOPE.to_string()], Viewer { user: "agent-x", local: true }, now - 3600.0, now);
        assert_eq!(local["sessions"], json!(2));
        assert_eq!(local["caught"], json!(2));
    }
}
