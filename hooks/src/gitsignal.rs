//! Git's own record of trouble, for the waste report's baseline. Counts
//! only: no output, no commit message and no code ever leaves the machine.
//!
//! * Live: a pull, rebase, merge, cherry-pick or stash pop whose output
//!   reported conflicts, a push git rejected, and a `git revert`, seen in an
//!   agent's shell after the command ran (`observe`).
//! * History: the repo's own `git log`, per ISO week, for the last 26 weeks:
//!   commits, commits whose subject starts with "Revert", and commits whose
//!   subject says they resolved a conflict or fixed a merge. Counted here and
//!   sent as numbers, at most once a day per checkout, detached from the
//!   session (`spawn_baseline_if_due`, `baseline_command`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::{self, Env};

const WEEKS: u64 = 26;
const MAX_COMMITS: usize = 20_000;
const BASELINE_EVERY_S: u64 = 86_400;

fn git_command(verbs: &str) -> Regex {
    // `git`, any global options (-C dir, -c k=v, --no-pager), then the verb
    // at the start of a command (after `;`, `&&`, `|`, `(`, env assignments)
    Regex::new(&format!(
        r"(?:^|[;&|(])\s*(?:\w+=\S*\s+)*git(?:\s+(?:-C\s+\S+|-c\s+\S+|--[\w-]+(?:=\S+)?))*\s+(?:{verbs})\b"
    ))
    .expect("static regex")
}

fn conflicting() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| git_command(r"pull|rebase|merge|cherry-pick|stash\s+(?:pop|apply)|am|revert"))
}

fn pushing() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| git_command("push"))
}

fn reverting() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| git_command("revert"))
}

/// The signals a shell command's output carries: `merge_conflict`,
/// `push_rejected`, `revert`. `failed` is whether the call itself failed.
pub fn classify(command: &str, output: &str, failed: bool) -> Vec<&'static str> {
    let mut out = Vec::new();
    let conflict_said = ["CONFLICT (", "Automatic merge failed", "could not apply", "Resolve all conflicts manually", "Merge conflict in"]
        .iter()
        .any(|w| output.contains(w));
    if conflicting().is_match(command) && conflict_said {
        out.push("merge_conflict");
    }
    let rejected = ["[rejected]", "[remote rejected]", "Updates were rejected", "(non-fast-forward)", "(fetch first)"]
        .iter()
        .any(|w| output.contains(w));
    if pushing().is_match(command) && rejected {
        out.push("push_rejected");
    }
    let finishing = ["--abort", "--quit", "--skip"].iter().any(|f| command.contains(f));
    if reverting().is_match(command) && !finishing && !failed && !conflict_said {
        out.push("revert");
    }
    out
}

/// Send what a shell command's output said, as counts. Quiet on failure:
/// a signal is never worth holding the agent's step for.
pub fn observe(command: &str, output: &str, failed: bool, cfg: &config::Config, session: &str, budget: Duration) {
    if !cfg.usable() || budget.is_zero() {
        return;
    }
    for signal in classify(command, output, failed) {
        let body = json!({"action": "signal", "repo_id": cfg.repo_id, "session": session, "signal": signal, "source": "shell",
                          "agent": "collide-hook"});
        let _ = crate::http::post(&cfg.server, "/land", &cfg.token, &crate::report::user_agent(), &body, budget.min(Duration::from_secs(2)));
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// `YYYY-MM-DD` of the Monday (UTC) of the week `ts` falls in.
pub fn monday_of(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    // 1970-01-01 was a Thursday: three days after a Monday
    let from_monday = (days + 3).rem_euclid(7);
    let (y, m, d) = civil(days - from_monday);
    format!("{y:04}-{m:02}-{d:02}")
}

fn conflict_fix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:resolv\w*|fix\w*)\s+(?:the\s+|a\s+)?(?:merge\s+)?conflicts?\b|\bmerge\s+conflicts?\b|\bfix\w*\s+(?:the\s+|a\s+|bad\s+|broken\s+)?merge\b")
            .expect("static regex")
    })
}

/// Per week, from `git log` lines of `<unix time>\t<subject>`: (commits,
/// reverts, conflict fixes). The subjects are read here and dropped.
pub fn weekly_counts(log: &str) -> BTreeMap<String, (u64, u64, u64)> {
    let mut weeks: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
    for line in log.lines().take(MAX_COMMITS) {
        let Some((ts, subject)) = line.split_once('\t') else { continue };
        let Ok(ts) = ts.trim().parse::<i64>() else { continue };
        let slot = weeks.entry(monday_of(ts)).or_insert((0, 0, 0));
        slot.0 += 1;
        if subject.starts_with("Revert") {
            slot.1 += 1;
        }
        if conflict_fix().is_match(subject) {
            slot.2 += 1;
        }
    }
    weeks
}

fn stamp_path(root: &Path, env: &Env) -> PathBuf {
    let tag = format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes()));
    config::credentials_path(env).with_file_name("gitbaseline").join(format!("{}.json", &tag[..16]))
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// At most once a day per checkout, count the history in a detached
/// process: the session never waits on `git log`.
pub fn spawn_baseline_if_due(root: &Path, env: &Env) {
    if config::get(env, "CORK_GIT_BASELINE").trim() == "0" {
        return;
    }
    let stamp = stamp_path(root, env);
    let last = config::load_json(&stamp).get("ts").and_then(Value::as_u64).unwrap_or(0);
    if now_s().saturating_sub(last) < BASELINE_EVERY_S {
        return;
    }
    crate::report::save_json(&stamp, &json!({"ts": now_s()}));
    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = std::process::Command::new(exe);
    command
        .arg("git-baseline")
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

/// `collide-hook git-baseline <root>`: count the checkout's last 26 weeks of
/// history and send the counts.
pub fn baseline_command(root: &str, env: &Env) -> i32 {
    let root = if root.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(root) };
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let output = std::process::Command::new("git")
        .args(["log", &format!("--since={WEEKS}.weeks"), "-n", &MAX_COMMITS.to_string(), "--format=%ct%x09%s", "HEAD"])
        .current_dir(&root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(output) = output else { return 0 };
    if !output.status.success() {
        return 0;
    }
    let weeks: serde_json::Map<String, Value> = weekly_counts(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .map(|(monday, (commits, reverts, fixes))| (monday, json!({"commits": commits, "reverts": reverts, "conflict_fixes": fixes})))
        .collect();
    let body = json!({"action": "baseline", "repo_id": cfg.repo_id, "session": "", "agent": "collide-hook", "weeks": weeks});
    let _ = crate::http::post(&cfg.server, "/land", &cfg.token, &crate::report::user_agent(), &body, Duration::from_secs(10));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflicts_rejected_pushes_and_reverts_are_read_from_git_output() {
        assert_eq!(classify("git pull --rebase", "CONFLICT (content): Merge conflict in a.py\n", true), vec!["merge_conflict"]);
        assert_eq!(classify("cd x && git -C sub merge main", "Automatic merge failed; fix conflicts", true), vec!["merge_conflict"]);
        assert_eq!(classify("git rebase origin/main", "Successfully rebased", false), Vec::<&str>::new());
        // a conflict word in some other command's output is not git's
        assert_eq!(classify("cat notes.md", "CONFLICT (content)", false), Vec::<&str>::new());
        assert_eq!(classify("git push origin HEAD", " ! [rejected]        main -> main (fetch first)", true), vec!["push_rejected"]);
        assert_eq!(classify("git push", "Everything up-to-date", false), Vec::<&str>::new());
        assert_eq!(classify("git revert abc123 --no-edit", "[main 1234] Revert \"x\"", false), vec!["revert"]);
        assert_eq!(classify("git revert --abort", "", false), Vec::<&str>::new());
        assert_eq!(classify("git revert abc", "CONFLICT (content): Merge conflict in a.py", true), vec!["merge_conflict"]);
        // git named as an argument is not git run
        assert_eq!(classify("echo git revert", "", false), Vec::<&str>::new());
        assert_eq!(classify("GIT_EDITOR=true git revert HEAD", "", false), vec!["revert"]);
    }

    #[test]
    fn history_is_counted_per_monday_and_only_numbers_come_out() {
        // 2026-10-12 is a Monday; 2026-10-18 the Sunday after; 2026-10-19 Monday
        assert_eq!(monday_of(1_791_763_200), "2026-10-12");
        assert_eq!(monday_of(1_791_763_200 + 6 * 86_400 + 86_399), "2026-10-12");
        assert_eq!(monday_of(1_791_763_200 + 7 * 86_400), "2026-10-19");
        assert_eq!(monday_of(0), "1969-12-29");
        let log = "1791763200\tAdd the parser\n\
                   1791763300\tRevert \"Add the parser\"\n\
                   1791763400\tResolve merge conflicts in calc.py\n\
                   1791763500\tfix bad merge\n\
                   1792368000\tMerge branch 'main'\n\
                   garbage line\n";
        let weeks = weekly_counts(log);
        assert_eq!(weeks.get("2026-10-12"), Some(&(4, 1, 2)));
        assert_eq!(weeks.get("2026-10-19"), Some(&(1, 0, 0)));
        assert_eq!(weeks.len(), 2);
    }
}
