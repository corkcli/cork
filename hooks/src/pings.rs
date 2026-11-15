//! What Cork tells the person, not only the agent: the running count of the
//! collisions it caught, a desktop ping when something matters, and an
//! optional sound when it blocks a write.
//!
//! A catch (a write held over a teammate's change, a numbered file moved to
//! a free number, a lint finding about a teammate's change) is counted once,
//! here, as the agent is told of it: [`caught`] bumps this month's count,
//! pings and plays the sound, and the caller asks the agent to tell the
//! person in one line, count included. The count is this machine's
//! (`~/.cork/caught.json`); the dashboard has the workspace's.
//!
//! Pings (`cork pings on|off`, kept in machine.json): left as installed,
//! only a catch and a finished handoff; `on` adds a handoff picked up, a
//! teammate's message to an idle session and a long run finishing; `off`,
//! none. One of a kind every ten minutes at most, and six an hour, so
//! routine work never pings. Each is shown by a detached `cork ping`, so no
//! hook waits on it, and says where it is from under its title: the repo's
//! short name and the branch checked out there ("cork-cloud · main"). The sound (`cork sound on|off`) is off unless switched
//! on, plays when a write is blocked, and is detached too.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::config::{self, Env};

/// One ping of a kind per this long.
const KIND_GAP_S: f64 = 600.0;
/// At most this many pings an hour, every kind together.
const PER_HOUR: usize = 6;
/// The sound plays at most this often.
const SOUND_GAP_S: f64 = 30.0;
/// A turn this long ending is a long run finishing.
pub const LONG_RUN_S: f64 = 20.0 * 60.0;
/// A handoff a session took is remembered this long.
const HELD_S: f64 = 3.0 * 86_400.0;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// What a ping is about.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Catch,
    HandoffPicked,
    HandoffDone,
    Message,
    LongRun,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Catch => "catch",
            Kind::HandoffPicked => "handoff_picked",
            Kind::HandoffDone => "handoff_done",
            Kind::Message => "message",
            Kind::LongRun => "long_run",
        }
    }
}

/// Whether the setting pings for `kind`: "on" every kind, "off" none, and
/// left unset only a catch and a finished handoff.
pub fn wants(setting: &str, kind: Kind) -> bool {
    match setting {
        "on" => true,
        "off" => false,
        _ => matches!(kind, Kind::Catch | Kind::HandoffDone),
    }
}

fn setting(env: &Env) -> String {
    text(&crate::machine::settings(env), "pings")
}

/// Whether the catch sound is on (off unless the person switched it on).
pub fn sound_on(env: &Env) -> bool {
    crate::machine::settings(env).get("sound").and_then(Value::as_bool).unwrap_or(false)
}

fn record_path(env: &Env) -> PathBuf {
    config::data_dir(env).join("pings.json")
}

/// Whether a ping of `kind` may go at `now`, given the record of those
/// sent; the record takes it when it may.
pub fn admit(record: &mut Value, kind: Kind, now: f64) -> bool {
    if !record.is_object() {
        *record = json!({});
    }
    let last = record.pointer(&format!("/last/{}", kind.key())).and_then(Value::as_f64).unwrap_or(0.0);
    if now - last < KIND_GAP_S {
        return false;
    }
    let mut sent: Vec<f64> = record.get("sent").and_then(Value::as_array).into_iter().flatten()
        .filter_map(Value::as_f64).filter(|t| now - t < 3600.0).collect();
    if sent.len() >= PER_HOUR {
        return false;
    }
    sent.push(now);
    record["sent"] = json!(sent);
    if !record.get("last").is_some_and(Value::is_object) {
        record["last"] = json!({});
    }
    record["last"][kind.key()] = json!(now);
    true
}

/// Whether the sound may play at `now`; the record takes it when it may.
pub fn admit_sound(record: &mut Value, now: f64) -> bool {
    if !record.is_object() {
        *record = json!({});
    }
    if now - record.get("sound").and_then(Value::as_f64).unwrap_or(0.0) < SOUND_GAP_S {
        return false;
    }
    record["sound"] = json!(now);
    true
}

/// Where there is a person at a desktop to see a ping: never on Windows, in
/// CI, over SSH or on a Linux box without a display.
fn desktop(env: &Env) -> bool {
    if cfg!(windows) {
        return false;
    }
    let markers = ["CI", "GITHUB_ACTIONS", "SSH_CONNECTION", "SSH_TTY", "CODESPACES", "CORK_WOKEN"];
    if markers.iter().any(|m| !config::get(env, m).is_empty()) {
        return false;
    }
    !(cfg!(target_os = "linux") && config::get(env, "DISPLAY").is_empty() && config::get(env, "WAYLAND_DISPLAY").is_empty())
}

/// This program, when it is Cork's (never a test binary, which would run
/// its tests on the arguments).
fn own_program() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let stem = exe.file_stem()?.to_str()?.to_string();
    (crate::machine::is_program_name(&stem) || matches!(stem.as_str(), "cork-hook" | "collide-hook")).then_some(exe)
}

fn detached(mut command: Command) {
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// A desktop ping, when the setting wants this kind and the rate allows it.
/// Shown by a detached `cork ping`, which names the repo and branch at `at`;
/// `true` when one went.
pub fn ping(env: &Env, kind: Kind, at: &Path, title: &str, body: &str) -> bool {
    if !wants(&setting(env), kind) || !desktop(env) {
        return false;
    }
    let Some(program) = own_program() else { return false };
    let path = record_path(env);
    let mut record = config::load_json(&path);
    if !admit(&mut record, kind, now_s()) {
        return false;
    }
    let _ = std::fs::write(&path, record.to_string());
    let mut command = Command::new(program);
    command.arg("ping").arg(title).arg(body).arg(at);
    detached(command);
    true
}

/// `cork ping <title> <body> [folder]`: the detached half of [`ping`].
pub fn ping_command(args: &[String], env: &Env) -> i32 {
    let title = args.get(1).map(String::as_str).unwrap_or("Cork");
    let body = args.get(2).map(String::as_str).unwrap_or("");
    let place = args.get(3).map(|dir| place(Path::new(dir))).unwrap_or_default();
    crate::wake::notify(title, &place, body, env);
    0
}

/// Where a ping is from: the repo's short name (its origin's, else its
/// folder's) and the branch checked out there, "cork-cloud · main".
pub fn place(dir: &Path) -> String {
    // the checkout's own root, where its .git is (a linked worktree's is a file)
    let Some(root) = dir.ancestors().find(|d| d.join(".git").exists()) else { return String::new() };
    let id = crate::machine::repo_id_for(root);
    let repo = id.rsplit('/').next().unwrap_or("").trim_end_matches(".git").to_string();
    [repo, crate::git::branch(root)].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ")
}

/// A teammate as a ping names them: "Kim" for "Kim (kim@x.com)", "kim" for
/// "kim@x.com"; empty when Cork does not know who.
pub fn short_name(label: &str) -> String {
    let label = label.trim();
    if label.is_empty() || label == "a teammate" {
        return String::new();
    }
    if label.starts_with("another session of yours") || label.starts_with("Another session of yours") {
        return "Your other session".to_string();
    }
    let name = label.split(" (").next().unwrap_or(label).split('#').next().unwrap_or(label);
    name.split('@').next().unwrap_or(name).to_string()
}

/// A catch's ping line: the file and whose change it met, when Cork knows
/// who; else the first line of what stopped it.
pub fn catch_line(path: &str, who: &str, reason: &str) -> String {
    let name = short_name(who);
    let line = if name.is_empty() {
        format!("{path}: {}", reason.lines().next().unwrap_or("a teammate's change affects this file"))
    } else if name == "Your other session" {
        format!("{path}: your other session's change affects this file")
    } else {
        format!("{path}: {name}'s change affects this file")
    };
    line.chars().take(160).collect()
}

/// The catch sound, when it is on: a short system sound, never waited on.
fn play_sound(env: &Env) {
    if !sound_on(env) || !desktop(env) {
        return;
    }
    let path = record_path(env);
    let mut record = config::load_json(&path);
    if !admit_sound(&mut record, now_s()) {
        return;
    }
    let _ = std::fs::write(&path, record.to_string());
    if cfg!(target_os = "macos") {
        let mut command = Command::new("afplay");
        command.arg("/System/Library/Sounds/Tink.aiff");
        detached(command);
    } else if cfg!(target_os = "linux") {
        let file = "/usr/share/sounds/freedesktop/stereo/complete.oga";
        let mut command = if std::path::Path::new(file).exists() {
            let mut paplay = Command::new("paplay");
            paplay.arg(file);
            paplay
        } else {
            let mut canberra = Command::new("canberra-gtk-play");
            canberra.args(["-i", "complete"]);
            canberra
        };
        command.stdin(Stdio::null());
        detached(command);
    }
}

// ------------------------------------------------------------ the catches

fn caught_path(env: &Env) -> PathBuf {
    config::data_dir(env).join("caught.json")
}

/// "2026-11": the month of a moment, in UTC.
pub fn month_of(secs: f64) -> String {
    let days = (secs / 86_400.0).floor() as i64;
    // civil from days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}")
}

/// This month's count after one more catch: a new month starts at one.
pub fn bump(record: &Value, month: &str) -> u64 {
    let so_far = if text(record, "month") == month { record.get("n").and_then(Value::as_u64).unwrap_or(0) } else { 0 };
    so_far + 1
}

/// The running count, as the agent says it.
pub fn count_line(n: u64) -> String {
    let collisions = if n == 1 { "1 collision".to_string() } else { format!("{n} collisions") };
    format!("Cork has caught {collisions} for you this month.")
}

/// One real catch: a write stopped that would have overwritten someone's
/// lines or broken a call, or a break Cork named that the agent fixed.
/// Counted, pinged and (when on) sounded; `what` is the ping's line
/// ([`catch_line`]), `at` the checkout it happened in. The
/// month's count, said to the agent once a session (`count_once`). Until
/// v105 every block counted, retries of a harmless addition and lint false
/// alarms among them, and the agent was asked to end its line with the
/// count each time (8, then 9, then 12 in one test).
pub fn caught(env: &Env, at: &Path, what: &str) -> u64 {
    let path = caught_path(env);
    let month = month_of(now_s());
    let n = bump(&config::load_json(&path), &month);
    let _ = std::fs::create_dir_all(config::data_dir(env));
    let _ = std::fs::write(&path, json!({"month": month, "n": n}).to_string());
    ping(env, Kind::Catch, at, "Cork caught a collision", what);
    play_sound(env);
    n
}

fn break_path(env: &Env, session: &str, rel: &str) -> PathBuf {
    let digest = crate::report::sha256_hex(format!("{session}\n{rel}").as_bytes());
    config::data_dir(env).join("check").join(format!("break-{}.json", &digest[..16]))
}

/// A teammate's change makes lines of this file wrong (the findings Cork
/// named them by): kept until a write of the file fixes them.
pub fn note_break(env: &Env, session: &str, rel: &str, findings: &[Value]) {
    if session.is_empty() || findings.is_empty() {
        return;
    }
    crate::report::save_json(&break_path(env, session, rel), &json!({"findings": findings}));
}

/// The findings `note_break` kept for this file; none when nothing is broken.
pub fn pending_break(env: &Env, session: &str, rel: &str) -> Vec<Value> {
    if session.is_empty() {
        return Vec::new();
    }
    config::load_json(&break_path(env, session, rel)).get("findings").and_then(Value::as_array).cloned().unwrap_or_default()
}

/// The break is fixed (or gone): nothing kept.
pub fn clear_break(env: &Env, session: &str, rel: &str) {
    let _ = std::fs::remove_file(break_path(env, session, rel));
}

/// The month's count, the first time a session is told of a catch; "" after.
pub fn count_once(env: &Env, session: &str, n: u64) -> String {
    if session.is_empty() || n == 0 {
        return String::new();
    }
    let digest = crate::report::sha256_hex(session.as_bytes());
    let mark = config::data_dir(env).join("check").join(format!("counted-{}", &digest[..16]));
    if mark.exists() {
        return String::new();
    }
    if let Some(dir) = mark.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&mark, "");
    count_line(n)
}

/// What the agent does about a stopped write: tell the person, in its own
/// words. The count, when this session has not heard it yet, as a fact.
pub fn tell_line(count: &str) -> String {
    let tell = "Tell the user in one line what Cork caught, who is on it and what you will do instead.";
    if count.is_empty() { tell.to_string() } else { format!("{tell}\n{count}") }
}

// ------------------------------------------------------------ turns

fn turn_path(env: &Env, session: &str) -> PathBuf {
    crate::proto::session_dir(env, session).join("turn.json")
}

/// A prompt started a turn of this session.
pub fn turn_started(session: &str, env: &Env) {
    if session.is_empty() {
        return;
    }
    let path = turn_path(env, session);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, json!({"start": now_s()}).to_string());
}

/// The turn ended: how long it ran, the first time it ends (0 when no
/// prompt started it here).
pub fn turn_ended(session: &str, env: &Env) -> f64 {
    if session.is_empty() {
        return 0.0;
    }
    let path = turn_path(env, session);
    let mut turn = config::load_json(&path);
    let start = turn.get("start").and_then(Value::as_f64).unwrap_or(0.0);
    if start <= 0.0 || turn.get("end").and_then(Value::as_f64).is_some_and(|end| end >= start) {
        return 0.0;
    }
    let now = now_s();
    turn["end"] = json!(now);
    let _ = std::fs::write(&path, turn.to_string());
    now - start
}

/// Whether the session is between turns: its last turn ended and no
/// prompt has started another. Unknown is not idle.
pub fn idle(session: &str, env: &Env) -> bool {
    let turn = config::load_json(&turn_path(env, session));
    match (turn.get("start").and_then(Value::as_f64), turn.get("end").and_then(Value::as_f64)) {
        (Some(start), Some(end)) => end >= start,
        _ => false,
    }
}

/// "42m", "1h 5m".
pub fn duration_words(seconds: f64) -> String {
    let minutes = (seconds.max(0.0) / 60.0).round() as u64;
    if minutes < 60 { format!("{minutes}m") } else { format!("{}h {}m", minutes / 60, minutes % 60) }
}

// ------------------------------------------------------------ handoffs

fn held_path(env: &Env) -> PathBuf {
    config::data_dir(env).join("handoffs-held.json")
}

/// A session took a handoff from `from` in the checkout at `at`:
/// remembered, so its reply to the sender says the handoff is done, and
/// pinged as picked up.
pub fn took_handoff(env: &Env, at: &Path, session: &str, from: &str, spec: &str) {
    if session.is_empty() || from.is_empty() {
        return;
    }
    let path = held_path(env);
    let mut held = config::load_json(&path);
    let now = now_s();
    if let Some(map) = held.as_object_mut() {
        map.retain(|_, list| list.as_array().is_some_and(|l| l.iter().any(|h| now - h.get("ts").and_then(Value::as_f64).unwrap_or(0.0) < HELD_S)));
    }
    let first: String = spec.lines().next().unwrap_or("").chars().take(120).collect();
    let mut list = held.get(session).and_then(Value::as_array).cloned().unwrap_or_default();
    list.push(json!({"from": from, "spec": first, "ts": now, "at": at}));
    held[session] = json!(list);
    let _ = std::fs::create_dir_all(config::data_dir(env));
    let _ = std::fs::write(&path, held.to_string());
    ping(env, Kind::HandoffPicked, at, "Cork: a handoff was picked up", &first);
}

/// The session sent `to` a message: when it took a handoff from them, that
/// is the reply saying it is done. Pinged, and forgotten.
pub fn replied(env: &Env, session: &str, to: &str) {
    if session.is_empty() || to.is_empty() {
        return;
    }
    let path = held_path(env);
    let mut held = config::load_json(&path);
    let mut list = held.get(session).and_then(Value::as_array).cloned().unwrap_or_default();
    let Some(at) = list.iter().position(|h| text(h, "from") == to) else { return };
    let done = list.remove(at);
    held[session] = json!(list);
    let _ = std::fs::write(&path, held.to_string());
    // where it was taken: the reply may come from another folder
    let taken = text(&done, "at");
    let at = if taken.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(taken) };
    ping(env, Kind::HandoffDone, &at, "Cork: a handoff is done", &text(&done, "spec"));
}

// ------------------------------------------------------------ the commands

/// `cork pings on|off|status`.
pub fn command(args: &[String], env: &Env) -> i32 {
    let value = match args.first().map(String::as_str).unwrap_or("status") {
        "on" => "on",
        "off" => "off",
        "status" => {
            println!("{}", match setting(env).as_str() {
                "on" => "Cork pings are on: collisions caught, handoffs picked up and done, teammates' messages to an idle \
session, and long runs finishing. `cork pings off` stops them.",
                "off" => "Cork pings are off. `cork pings on` turns them on.",
                _ => "Cork pings are on for collisions caught and finished handoffs (the default). `cork pings on` adds \
handoffs picked up, teammates' messages to an idle session and long runs finishing; `cork pings off` stops them.",
            });
            return 0;
        }
        _ => {
            println!("usage: cork pings on|off|status\n\nDesktop notices when something matters: a collision caught, a handoff done.");
            return 2;
        }
    };
    let mut machine = crate::machine::settings(env);
    machine["pings"] = json!(value);
    if !crate::machine::save_settings(env, &machine) {
        println!("Cork: could not save the setting.");
        return 1;
    }
    println!("{}", if value == "on" {
        "Cork pings are on: collisions caught, handoffs picked up and done, teammates' messages to an idle session, \
and long runs finishing. At most one of a kind every ten minutes."
    } else {
        "Cork pings are off."
    });
    0
}

/// `cork sound on|off|status`.
pub fn sound_command(args: &[String], env: &Env) -> i32 {
    let on = match args.first().map(String::as_str).unwrap_or("status") {
        "on" => true,
        "off" => false,
        "status" => {
            println!("Cork's catch sound is {}.", if sound_on(env) { "on" } else { "off (`cork sound on` plays a short sound when Cork blocks a collision)" });
            return 0;
        }
        _ => {
            println!("usage: cork sound on|off|status\n\nA short sound when Cork blocks a collision. Off unless you switch it on.");
            return 2;
        }
    };
    let mut machine = crate::machine::settings(env);
    machine["sound"] = json!(on);
    if !crate::machine::save_settings(env, &machine) {
        println!("Cork: could not save the setting.");
        return 1;
    }
    println!("{}", if on { "Cork will play a short sound when it blocks a collision." } else { "Cork's catch sound is off." });
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> (PathBuf, Env) {
        let home = std::env::temp_dir().join(format!("cork-pings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".cork")).unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().to_string());
        (home, env)
    }

    #[test]
    fn left_as_installed_only_catches_and_finished_handoffs_ping() {
        for kind in [Kind::Catch, Kind::HandoffDone] {
            assert!(wants("", kind));
        }
        for kind in [Kind::HandoffPicked, Kind::Message, Kind::LongRun] {
            assert!(!wants("", kind), "{kind:?}");
            assert!(wants("on", kind));
        }
        assert!(!wants("off", Kind::Catch) && !wants("off", Kind::HandoffDone));
    }

    #[test]
    fn pings_are_one_of_a_kind_per_ten_minutes_and_six_an_hour() {
        let mut record = json!({});
        let t = 1_000_000.0;
        assert!(admit(&mut record, Kind::Catch, t));
        assert!(!admit(&mut record, Kind::Catch, t + 60.0));
        // another kind is its own
        assert!(admit(&mut record, Kind::HandoffDone, t + 60.0));
        assert!(admit(&mut record, Kind::Catch, t + KIND_GAP_S));
        // the hour's cap holds across kinds
        assert!(admit(&mut record, Kind::Message, t + 700.0));
        assert!(admit(&mut record, Kind::LongRun, t + 800.0));
        assert!(admit(&mut record, Kind::HandoffPicked, t + 900.0));
        assert!(!admit(&mut record, Kind::HandoffDone, t + 1_300.0), "six an hour");
        // an hour after the first, room again
        assert!(admit(&mut record, Kind::HandoffDone, t + 3_601.0));
    }

    #[test]
    fn the_sound_is_off_until_switched_on() {
        let (home, env) = home("sound");
        assert!(!sound_on(&env));
        assert_eq!(sound_command(&["on".into()], &env), 0);
        assert!(sound_on(&env));
        assert_eq!(sound_command(&["off".into()], &env), 0);
        assert!(!sound_on(&env));
        // and at most once in a burst
        let mut record = json!({});
        assert!(admit_sound(&mut record, 100.0));
        assert!(!admit_sound(&mut record, 110.0));
        assert!(admit_sound(&mut record, 100.0 + SOUND_GAP_S));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_setting_is_kept_in_machine_json() {
        let (home, env) = home("setting");
        assert_eq!(setting(&env), "");
        assert_eq!(command(&["off".into()], &env), 0);
        assert_eq!(text(&crate::machine::settings(&env), "pings"), "off");
        assert_eq!(command(&["loud".into()], &env), 2);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_catch_count_runs_for_the_month_and_starts_over_with_the_next() {
        assert_eq!(month_of(1_794_555_673.0), "2026-11");
        assert_eq!(month_of(1_798_761_600.0), "2027-01");
        assert_eq!(month_of(951_782_400.0), "2000-02");
        assert_eq!(bump(&json!({}), "2026-11"), 1);
        assert_eq!(bump(&json!({"month": "2026-11", "n": 13}), "2026-11"), 14);
        assert_eq!(bump(&json!({"month": "2026-10", "n": 40}), "2026-11"), 1);
        assert_eq!(count_line(14), "Cork has caught 14 collisions for you this month.");
        assert_eq!(count_line(1), "Cork has caught 1 collision for you this month.");
        let (home, env) = home("caught");
        assert_eq!(caught(&env, &home, "a.py"), 1);
        assert_eq!(caught(&env, &home, "a.py"), 2);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// v104's test: the agent was asked to end its line with "Cork has caught
    /// N collisions for you this month" at every block (8, then 9, then 12).
    /// The count is said to a session once, as a fact; never as words it is
    /// asked to repeat.
    #[test]
    fn the_count_is_said_once_a_session_and_never_asked_for() {
        let (home, env) = home("count-once");
        assert_eq!(count_once(&env, "s1", 3), count_line(3));
        assert_eq!(count_once(&env, "s1", 4), "", "once a session");
        assert_eq!(count_once(&env, "s2", 4), count_line(4), "another session hears it once too");
        let tell = tell_line(&count_line(3));
        assert!(!tell.contains("ending with") && tell.ends_with("\nCork has caught 3 collisions for you this month."), "{tell}");
        assert_eq!(tell_line(""), "Tell the user in one line what Cork caught, who is on it and what you will do instead.");
        // a break kept until a write of the file fixes it
        note_break(&env, "s1", "app.py", &[json!({"rule": "signature_drift", "symbol": "f"})]);
        assert_eq!(pending_break(&env, "s1", "app.py").len(), 1);
        assert!(pending_break(&env, "s2", "app.py").is_empty());
        clear_break(&env, "s1", "app.py");
        assert!(pending_break(&env, "s1", "app.py").is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_ping_names_the_teammate_and_where_it_happened() {
        assert_eq!(short_name("Kim (kim@x.com)"), "Kim");
        assert_eq!(short_name("kim@x.com"), "kim");
        assert_eq!(short_name("kim@x.com#s2"), "kim");
        assert_eq!(short_name("a teammate"), "");
        assert_eq!(catch_line("static/js/app.js", "Kim (kim@x.com)", "Kim renamed `x`"),
            "static/js/app.js: Kim's change affects this file");
        assert_eq!(catch_line("db/007_x.sql", "", "006 in db/ is taken by bob@x.com.\nmore"),
            "db/007_x.sql: 006 in db/ is taken by bob@x.com.");
        assert_eq!(catch_line("a.py", "another session of yours", ""), "a.py: your other session's change affects this file");
        // a checkout: its origin's short name and its branch
        let (home, _) = home("place");
        let repo = home.join("app");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/fix-pay\n").unwrap();
        std::fs::write(repo.join(".git/config"), "[remote \"origin\"]\n\turl = git@github.com:acme/cork-cloud.git\n").unwrap();
        assert_eq!(place(&repo), "cork-cloud · fix-pay");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_turn_is_idle_once_it_ends_and_its_length_is_said_once() {
        let (home, env) = home("turns");
        assert!(!idle("s1", &env), "unknown is not idle");
        turn_started("s1", &env);
        assert!(!idle("s1", &env));
        assert!(turn_ended("s1", &env) >= 0.0);
        assert!(idle("s1", &env));
        assert_eq!(turn_ended("s1", &env), 0.0, "said once");
        assert_eq!(duration_words(42.0 * 60.0), "42m");
        assert_eq!(duration_words(65.0 * 60.0), "1h 5m");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_reply_to_the_sender_of_a_taken_handoff_is_its_end() {
        let (home, env) = home("handoffs");
        took_handoff(&env, &home, "s1", "kevin@x.com#s2", "Build the invite panel.\nMore.");
        let held = config::load_json(&held_path(&env));
        assert_eq!(held["s1"][0]["spec"], json!("Build the invite panel."));
        replied(&env, "s1", "bob@x.com");
        assert_eq!(config::load_json(&held_path(&env))["s1"].as_array().map(Vec::len), Some(1));
        replied(&env, "s1", "kevin@x.com#s2");
        assert_eq!(config::load_json(&held_path(&env))["s1"].as_array().map(Vec::len), Some(0));
        let _ = std::fs::remove_dir_all(&home);
    }
}
