//! `cork audit`: what this machine sent, request by request.
//!
//! Every outbound request the program makes (each HTTP post and get, and
//! each frame of the real-time protocol's WebSocket) is appended, as it
//! leaves, to a log on this machine: `~/.collide/audit/<date>.jsonl`, one
//! JSON line per request with the time, the endpoint, the body exactly as
//! sent (credentials masked) and the size of each field. It is never
//! uploaded. Kept 7 days, and at most 20 MB in all: the oldest go first.
//!
//! Writing it is fail-open and cheap (one append); a body over 1 MB keeps
//! its small fields whole and says how large each big one was.
//!
//!   cork audit                 the latest 20 requests, one line each
//!   cork audit -n 100          the latest 100
//!   cork audit --since 1h      only the last hour (30m, 2d, 90s)
//!   cork audit --full          with each body, pretty-printed
//!   cork audit --json          the records as JSON lines

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

pub const KEEP_DAYS: u64 = 7;
pub const MAX_TOTAL_BYTES: u64 = 20 * 1024 * 1024;
/// A record whose body is larger keeps its big fields as their sizes.
const MAX_BODY_BYTES: usize = 1024 * 1024;
const BIG_FIELD_BYTES: usize = 64 * 1024;
/// Never written to the log, whatever request carries them.
const MASKED: [&str; 7] = ["token", "access_token", "refresh_token", "code", "code_verifier", "client_secret", "password"];

/// Where the log lives: `CORK_AUDIT_DIR` (or `COLLIDE_AUDIT_DIR`), else in
/// the machine's data folder, beside the credential.
/// Unit tests write nowhere unless they ask to.
pub fn dir() -> Option<PathBuf> {
    let given = crate::config::var("CORK_AUDIT_DIR");
    if !given.is_empty() {
        return Some(PathBuf::from(given));
    }
    if cfg!(test) {
        return None;
    }
    Some(crate::config::data_dir(&crate::config::env_map()).join("audit"))
}

fn now_s() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Days since 1970-01-01 to (year, month, day).
pub(crate) fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `2026-10-15T09:30:01Z`
pub fn iso(ts: f64) -> String {
    let secs = ts as i64;
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn masked(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let v = if MASKED.contains(&k.as_str()) && v.is_string() { json!("[masked]") } else { masked(v) };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(masked).collect()),
        other => other.clone(),
    }
}

/// The size, in bytes as sent, of each top-level field.
fn sizes(body: &Value) -> Map<String, Value> {
    body.as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), json!(v.to_string().len()))).collect())
        .unwrap_or_default()
}

/// A body over the record limit: its big fields as their sizes (an array
/// keeps its first item, when that is small).
fn kept_body(body: &Value, bytes: usize) -> (Value, bool) {
    if bytes <= MAX_BODY_BYTES {
        return (body.clone(), false);
    }
    let Some(map) = body.as_object() else { return (json!({"elided_bytes": bytes}), true) };
    let kept = map
        .iter()
        .map(|(k, v)| {
            let size = v.to_string().len();
            if size <= BIG_FIELD_BYTES {
                return (k.clone(), v.clone());
            }
            let mut summary = json!({"elided_bytes": size});
            if let Value::Array(items) = v {
                summary["items"] = json!(items.len());
                if let Some(first) = items.first().filter(|f| f.to_string().len() <= BIG_FIELD_BYTES) {
                    summary["first"] = first.clone();
                }
            }
            (k.clone(), summary)
        })
        .collect();
    (Value::Object(kept), true)
}

/// Record one outbound request. `body` is what was sent: JSON, or for a
/// form or a frame the raw text (recorded as JSON when it parses).
pub fn record(channel: &str, method: &str, url: &str, body: Option<&Value>, raw_len: usize) {
    let _ = std::panic::catch_unwind(|| {
        let Some(dir) = dir() else { return };
        write_record(&dir, channel, method, url, body, raw_len, None);
    });
}

/// A request with how long it took (`ms`) and whether the server answered
/// it (`ok`): the hooks' own latency, per endpoint, for `cork status`.
pub fn record_timed(channel: &str, method: &str, url: &str, body: Option<&Value>, raw_len: usize, ms: u64, ok: bool) {
    let _ = std::panic::catch_unwind(|| {
        let Some(dir) = dir() else { return };
        write_record(&dir, channel, method, url, body, raw_len, Some((ms, ok)));
    });
}

/// The hooks' latency over the audit rows since `since` (epoch seconds):
/// per endpoint, the count, the median and the 90th percentile in
/// milliseconds, and how many went unanswered. Only rows that carry a
/// time (hooks v86+); None when there are none.
pub fn latency(rows: &[Value], since: f64) -> Option<Vec<(String, usize, u64, u64, usize)>> {
    let mut by: std::collections::BTreeMap<String, (Vec<u64>, usize)> = std::collections::BTreeMap::new();
    for row in rows {
        let Some(ms) = row.get("ms").and_then(Value::as_u64) else { continue };
        if row.get("ts").and_then(Value::as_f64).unwrap_or(0.0) < since || row.get("channel").and_then(Value::as_str) != Some("http") {
            continue;
        }
        let endpoint = row.get("endpoint").and_then(Value::as_str).unwrap_or("").to_string();
        let entry = by.entry(endpoint).or_insert_with(|| (Vec::new(), 0));
        entry.0.push(ms);
        if row.get("ok").and_then(Value::as_bool) == Some(false) {
            entry.1 += 1;
        }
    }
    if by.is_empty() {
        return None;
    }
    let mut out: Vec<(String, usize, u64, u64, usize)> = by
        .into_iter()
        .map(|(endpoint, (mut times, failed))| {
            times.sort_unstable();
            let at = |q: f64| times[((times.len() as f64 - 1.0) * q).round() as usize];
            (endpoint, times.len(), at(0.5), at(0.9), failed)
        })
        .collect();
    // the busiest first: the one that shapes what the agent feels
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Some(out)
}

/// The one line `cork status` says about the hooks' latency:
/// "Hooks (last day): /brief 42x p50 180 ms p90 410 ms · /gate 118x p50 60 ms p90 95 ms · 3 unanswered".
pub fn latency_line(since: f64) -> Option<String> {
    let dir = dir()?;
    let stats = latency(&records(&dir), since)?;
    let unanswered: usize = stats.iter().map(|s| s.4).sum();
    let parts: Vec<String> = stats
        .iter()
        .take(5)
        .map(|(endpoint, n, p50, p90, _)| format!("{endpoint} {n}x p50 {p50} ms p90 {p90} ms"))
        .collect();
    let tail = if unanswered > 0 { format!(" · {unanswered} unanswered") } else { String::new() };
    Some(format!("Hooks (last day): {}{tail}", parts.join(" · ")))
}

/// A WebSocket frame, as the protocol client sends it (ws_client's tap).
pub fn ws_tap(url: &str, frame: &str) {
    let parsed = serde_json::from_str::<Value>(frame).ok();
    let kind = parsed.as_ref().and_then(|v| v.get("t")).and_then(Value::as_str).unwrap_or("frame").to_string();
    record("ws", &kind, url, parsed.as_ref(), frame.len());
}

fn endpoint_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = rest.find('/').map(|n| &rest[n..]).unwrap_or("/");
    path.split('?').next().unwrap_or(path).to_string()
}

fn write_record(dir: &std::path::Path, channel: &str, method: &str, url: &str, body: Option<&Value>, raw_len: usize, timed: Option<(u64, bool)>) {
    let ts = now_s();
    let day = &iso(ts)[..10];
    let path = dir.join(format!("{day}.jsonl"));
    if !path.exists() {
        let _ = std::fs::create_dir_all(dir);
        prune(dir, ts);
    } else if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_TOTAL_BYTES / 2 {
        // a busy day rolls over, so pruning can drop its first half
        let rolled = dir.join(format!("{day}.{}.jsonl", ts as u64));
        let _ = std::fs::rename(&path, rolled);
        prune(dir, ts);
    }
    let shown = body.map(masked);
    let (kept, elided) = match &shown {
        Some(b) => {
            let (k, e) = kept_body(b, raw_len);
            (Some(k), e)
        }
        None => (None, false),
    };
    let base = url.split('?').next().unwrap_or(url);
    let mut row = json!({
        "ts": (ts * 1000.0).round() / 1000.0, "at": iso(ts), "channel": channel, "method": method,
        "url": base, "endpoint": endpoint_of(base), "bytes": raw_len, "pid": std::process::id(),
    });
    if let Some(b) = &shown {
        row["sizes"] = Value::Object(sizes(b));
    }
    if let Some(k) = kept {
        row["body"] = k;
    }
    if elided {
        row["elided"] = json!(true);
    }
    if let Some((ms, ok)) = timed {
        row["ms"] = json!(ms);
        row["ok"] = json!(ok);
    }
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let _ = file.write_all(format!("{row}\n").as_bytes());
}

/// Older than a week goes; then the oldest until the log is under 20 MB.
fn prune(dir: &std::path::Path, now: f64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(String, PathBuf, u64)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.ends_with(".jsonl").then(|| (name, e.path(), e.metadata().map(|m| m.len()).unwrap_or(0)))
        })
        .collect();
    files.sort();
    let cutoff = &iso(now - (KEEP_DAYS as f64) * 86_400.0)[..10];
    files.retain(|(name, path, _)| {
        if name.get(..10).is_some_and(|day| day < cutoff) {
            let _ = std::fs::remove_file(path);
            return false;
        }
        true
    });
    let mut total: u64 = files.iter().map(|f| f.2).sum();
    for (_, path, size) in &files {
        if total <= MAX_TOTAL_BYTES {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(*size);
        }
    }
}

/// Every record still on disk, oldest first.
pub fn records(dir: &std::path::Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect();
    names.sort();
    let mut out: Vec<Value> = Vec::new();
    for path in names {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        out.extend(text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()));
    }
    out.sort_by(|a, b| {
        let t = |v: &Value| v.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        t(a).partial_cmp(&t(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// `30m`, `1h`, `2d`, `90s`, `45` (seconds).
pub fn parse_since(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (number, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(text.len()));
    let n: f64 = number.parse().ok()?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        "d" => n * 86_400.0,
        "w" => n * 7.0 * 86_400.0,
        _ => return None,
    };
    Some(Duration::from_secs_f64(secs))
}

fn human(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    }
}

/// One line for a record: when, what, how much, and its largest fields.
pub fn summary_line(row: &Value) -> String {
    let get = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut fields: Vec<(String, u64)> = row
        .get("sizes")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0))).collect())
        .unwrap_or_default();
    fields.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let shown: Vec<String> = fields.iter().take(6).map(|(k, n)| format!("{k} {}", human(*n))).collect();
    let more = if fields.len() > 6 { format!(" +{} more", fields.len() - 6) } else { String::new() };
    let what = if get("channel") == "ws" { format!("ws {}", get("method")) } else { format!("{} {}", get("method"), get("endpoint")) };
    // how long it took, and a mark when the server never answered
    let took = match (row.get("ms").and_then(Value::as_u64), row.get("ok").and_then(Value::as_bool)) {
        (Some(ms), Some(false)) => format!("{ms:>6} ms ✗"),
        (Some(ms), _) => format!("{ms:>6} ms  "),
        _ => "          ".to_string(),
    };
    format!(
        "{}  {:<28} {:>9} {took} {}{}",
        get("at").replace('T', " ").trim_end_matches('Z'),
        what,
        human(row.get("bytes").and_then(Value::as_u64).unwrap_or(0)),
        shown.join(", "),
        more
    )
}

/// `cork audit [--full] [--json] [--since 1h] [-n N]`
pub fn command(args: &[String]) -> i32 {
    let mut full = false;
    let mut as_json = false;
    let mut since: Option<Duration> = None;
    let mut limit: usize = 20;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--full" => full = true,
            "--json" => as_json = true,
            "--since" => {
                i += 1;
                match args.get(i).and_then(|s| parse_since(s)) {
                    Some(d) => since = Some(d),
                    None => {
                        eprintln!("cork audit: --since takes a duration such as 30m, 1h or 2d");
                        return 2;
                    }
                }
            }
            "-n" | "--limit" => {
                i += 1;
                limit = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(limit);
            }
            "--all" => limit = usize::MAX,
            "-h" | "--help" => {
                println!("cork audit: what this machine sent to Cork, request by request (kept here only, 7 days / 20 MB).\n\n  cork audit               the latest 20 requests\n  cork audit -n 100        the latest 100 (--all: every one kept)\n  cork audit --since 1h    only the last hour (30m, 2d, 90s)\n  cork audit --full        with each body as it was sent\n  cork audit --json        the records as JSON lines");
                return 0;
            }
            other => {
                eprintln!("cork audit: unknown option {other} (try --help)");
                return 2;
            }
        }
        i += 1;
    }
    let Some(dir) = dir() else {
        eprintln!("cork audit: no home directory to keep the log in");
        return 1;
    };
    let mut rows = records(&dir);
    if let Some(window) = since {
        let floor = now_s() - window.as_secs_f64();
        rows.retain(|r| r.get("ts").and_then(Value::as_f64).unwrap_or(0.0) >= floor);
    }
    let skip = rows.len().saturating_sub(limit);
    let rows = &rows[skip..];
    if as_json {
        for row in rows {
            println!("{row}");
        }
        return 0;
    }
    if rows.is_empty() {
        println!("cork audit: nothing sent{} (log: {}).", if since.is_some() { " in that window" } else { "" }, dir.display());
        return 0;
    }
    let total: u64 = rows.iter().map(|r| r.get("bytes").and_then(Value::as_u64).unwrap_or(0)).sum();
    println!(
        "What this machine sent to Cork: {} request(s), {} ({}; kept here only, {KEEP_DAYS} days / 20 MB, never uploaded)",
        rows.len(),
        human(total),
        dir.display()
    );
    for row in rows {
        println!("{}", summary_line(row));
        if full {
            if let Some(body) = row.get("body") {
                let pretty = serde_json::to_string_pretty(body).unwrap_or_default();
                for line in pretty.lines() {
                    println!("    {line}");
                }
            }
            if row.get("elided").and_then(Value::as_bool) == Some(true) {
                println!("    (over 1 MB: fields over 64 KB are shown as their sizes)");
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("collide-audit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_request_is_recorded_with_its_body_and_field_sizes_and_credentials_masked() {
        let dir = temp("record");
        let body = json!({"repo_id": "r", "path": "a.py", "token": "cat_secret_value_123456", "nested": {"code": "abc"}});
        write_record(&dir, "http", "POST", "https://mcp.example/report?x=1", Some(&body), body.to_string().len(), None);
        let rows = records(&dir);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row["endpoint"], json!("/report"));
        assert_eq!(row["url"], json!("https://mcp.example/report"));
        assert_eq!(row["body"]["path"], json!("a.py"));
        assert_eq!(row["body"]["token"], json!("[masked]"));
        assert_eq!(row["body"]["nested"]["code"], json!("[masked]"));
        assert_eq!(row["sizes"]["path"], json!(6));
        assert!(summary_line(row).contains("POST /report"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_hooks_latency_is_summed_per_endpoint_with_the_unanswered_counted() {
        let rows: Vec<Value> = [("/brief", 180, true), ("/brief", 410, true), ("/brief", 5000, false), ("/gate", 60, true), ("/gate", 95, true), ("/gate", 70, true)]
            .iter()
            .map(|(e, ms, ok)| json!({"ts": 100.0, "channel": "http", "endpoint": e, "ms": ms, "ok": ok}))
            .chain([json!({"ts": 100.0, "channel": "http", "endpoint": "/presence"})]) // an older hook's row: no time
            .chain([json!({"ts": 1.0, "channel": "http", "endpoint": "/report", "ms": 9, "ok": true})]) // before `since`
            .collect();
        let stats = latency(&rows, 50.0).expect("timed rows");
        // the busiest first; a tie by name
        assert_eq!(stats[0], ("/brief".to_string(), 3, 410, 5000, 1));
        assert_eq!(stats[1], ("/gate".to_string(), 3, 70, 95, 0));
        assert_eq!(stats.len(), 2);
        assert!(latency(&rows, 1000.0).is_none(), "nothing in the window");
        let line = {
            let unanswered: usize = stats.iter().map(|s| s.4).sum();
            format!("{} unanswered", unanswered)
        };
        assert_eq!(line, "1 unanswered");
        let row = json!({"at": "2026-10-29T16:24:14Z", "channel": "http", "method": "POST", "endpoint": "/brief", "bytes": 916, "ms": 410, "ok": false, "sizes": {"prompt": 900}});
        assert!(summary_line(&row).contains("410 ms ✗"), "{}", summary_line(&row));
    }

    #[test]
    fn a_huge_body_keeps_its_small_fields_and_the_size_of_the_big_ones() {
        let big: Vec<Value> = (0..20_000).map(|i| json!({"path": format!("f{i}.py"), "line_hashes": vec!["abcdef12"; 8]})).collect();
        let body = json!({"repo_id": "r", "files": big});
        let (kept, elided) = kept_body(&body, body.to_string().len());
        assert!(elided);
        assert_eq!(kept["repo_id"], json!("r"));
        assert_eq!(kept["files"]["items"], json!(20_000));
        assert_eq!(kept["files"]["first"]["path"], json!("f0.py"));
    }

    #[test]
    fn old_days_and_overflow_are_pruned() {
        let dir = temp("prune");
        std::fs::write(dir.join("2020-01-01.jsonl"), "{}\n").unwrap();
        let today = &iso(now_s())[..10];
        std::fs::write(dir.join(format!("{today}.jsonl")), "{}\n").unwrap();
        prune(&dir, now_s());
        assert!(!dir.join("2020-01-01.jsonl").exists());
        assert!(dir.join(format!("{today}.jsonl")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn durations_and_dates_read_as_people_write_them() {
        assert_eq!(parse_since("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_since("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_since("2d"), Some(Duration::from_secs(172_800)));
        assert_eq!(parse_since("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_since("soon"), None);
        assert_eq!(iso(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_760_443_200.0), "2025-10-14T12:00:00Z");
    }
}
