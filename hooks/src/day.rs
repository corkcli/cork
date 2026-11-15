//! `cork today`: the person's day in plain English, read off Cork's ledger
//! by the server (cork-rs today.rs): what their agents did, what Cork
//! caught and saved for them, and what is left open. The dashboard's Today
//! card says the same.

use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Env;

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Seconds east of UTC from `date +%z`'s "+0100", "-0530".
pub fn offset_of(zone: &str) -> Option<f64> {
    let zone = zone.trim();
    let (sign, digits) = match (zone.strip_prefix('+'), zone.strip_prefix('-')) {
        (Some(rest), _) => (1.0, rest),
        (_, Some(rest)) => (-1.0, rest),
        _ => return None,
    };
    if digits.len() != 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let hours: f64 = digits[..2].parse().ok()?;
    let minutes: f64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600.0 + minutes * 60.0))
}

/// The last local midnight before `at`, for a zone `offset` seconds east.
pub fn midnight(at: f64, offset: f64) -> f64 {
    ((at + offset) / 86_400.0).floor() * 86_400.0 - offset
}

/// This machine's offset from UTC, as its clock says (0 where it cannot).
fn local_offset() -> f64 {
    if cfg!(windows) {
        return 0.0;
    }
    Command::new("date").arg("+%z").output().ok()
        .and_then(|out| offset_of(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or(0.0)
}

/// `cork today [--json]`.
pub fn command(args: &[String], env: &Env) -> i32 {
    let (server, token) = match crate::machine::cloud_account(env) {
        Some(found) => found,
        None if !crate::machine::installed(env) => {
            println!("Cork is not installed for this machine. Install it with: curl -fsSL https://corkcli.com/install.sh | sh");
            return 1;
        }
        None => {
            if !crate::machine::ensure_local(env, Duration::from_secs(3)) {
                println!("Cork: its local server is not running; it starts with your next agent session.");
                return 1;
            }
            crate::machine::local_endpoint(env)
        }
    };
    let since = midnight(now_s(), local_offset());
    let answer = crate::http::post(&server, "/today", &token, &crate::report::user_agent(), &json!({"since": since}),
        Duration::from_secs(10));
    let Ok(answer) = answer.map_err(|_| ()).and_then(|a| if a.get("ok").and_then(Value::as_bool) == Some(true) { Ok(a) } else { Err(()) }) else {
        println!("Cork: could not reach Cork for today's summary. Try again in a moment.");
        return 1;
    };
    if args.iter().any(|a| a == "--json") {
        println!("{answer}");
        return 0;
    }
    println!("Cork today:");
    for line in answer.get("lines").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        println!("  {line}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_starts_at_the_machines_own_midnight() {
        assert_eq!(offset_of("+0100"), Some(3600.0));
        assert_eq!(offset_of("-0530\n"), Some(-19_800.0));
        assert_eq!(offset_of("UTC"), None);
        // 2026-11-12 10:00 UTC
        let at = 1_794_484_800.0;
        assert_eq!(midnight(at, 0.0), 1_794_441_600.0);
        // in New York (UTC-5) it is 05:00, after its own midnight at 05:00 UTC
        assert_eq!(midnight(at, -18_000.0), 1_794_441_600.0 + 18_000.0);
        // in Tokyo (UTC+9) it is 19:00, after midnight at 15:00 UTC the day before
        assert_eq!(midnight(at, 32_400.0), 1_794_441_600.0 - 32_400.0);
    }
}
