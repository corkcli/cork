//! The one network call each hook makes, with the same hard budget the
//! Python hooks enforce: any error, timeout or non-200 is swallowed and the
//! agent proceeds. Reporting must never disturb the work it reports on.
//!
//! It is also the door everything leaves by: the body is made smaller
//! first (privacy.rs: paths, commands, file text, the workspace's
//! switches), then written to this machine's audit log (audit.rs) exactly
//! as it is sent.

use std::time::Duration;

use serde_json::Value;

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(timeout).build()
}

/// POST JSON, returning the decoded body. `Err(())` means "could not talk to
/// the server" — every caller treats that as allow / skip.
pub fn post(
    server: &str,
    endpoint: &str,
    token: &str,
    user_agent: &str,
    payload: &Value,
    remaining: Duration,
) -> Result<Value, ()> {
    if remaining.is_zero() {
        return Err(());
    }
    let started = std::time::Instant::now();
    // a body that cannot be made smaller is not sent as it was; the agent
    // proceeds either way
    let sent = std::panic::catch_unwind(|| crate::privacy::outbound(server, endpoint, payload)).map_err(|_| ())?;
    let body = sent.to_string();
    let url = format!("{server}{endpoint}");
    // the audit row carries how long the request took and whether it was
    // answered: what `cork status` sums into the hooks' latency, so a
    // slow prompt is a number per endpoint and not a feeling
    let audit = |ok: bool| {
        crate::audit::record_timed("http", "POST", &url, Some(&sent), body.len(), started.elapsed().as_millis() as u64, ok)
    };
    // both inside the caller's budget
    let remaining = remaining.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        audit(false);
        return Err(());
    }
    let response = match agent(remaining)
        .post(&url)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", user_agent)
        .send_string(&body)
    {
        Ok(response) => response,
        Err(_) => {
            audit(false);
            return Err(());
        }
    };
    if response.status() != 200 {
        audit(false);
        return Err(());
    }
    let Ok(body) = response.into_string() else {
        audit(false);
        return Err(());
    };
    audit(true);
    let answer = serde_json::from_str(&body).unwrap_or(Value::Object(Default::default()));
    // the workspace's switches ride the answers: kept for the next request
    let _ = std::panic::catch_unwind(|| crate::privacy::learn(server, &sent, &answer));
    // so does the repo's flaky-test registry, for the next check (flaky.rs)
    let _ = std::panic::catch_unwind(|| crate::flaky::learn(server, &sent, &answer));
    Ok(answer)
}

/// GET JSON, returning the decoded body: `post`'s twin for a read, with the
/// same budget and the same audit row. `Err(())` means "could not talk to
/// the server".
pub fn get(server: &str, endpoint: &str, token: &str, user_agent: &str, remaining: Duration) -> Result<Value, ()> {
    if remaining.is_zero() {
        return Err(());
    }
    let started = std::time::Instant::now();
    let url = format!("{server}{endpoint}");
    let audit = |ok: bool| crate::audit::record_timed("http", "GET", &url, None, 0, started.elapsed().as_millis() as u64, ok);
    let response = agent(remaining)
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", user_agent)
        .call();
    let Ok(response) = response else {
        audit(false);
        return Err(());
    };
    let body = response.into_string().map_err(|_| audit(false))?;
    audit(true);
    Ok(serde_json::from_str(&body).unwrap_or(Value::Object(Default::default())))
}

/// POST a body as it is (`content_type` says what it is): a file the user
/// shares, or a JSON body whose answer matters when it is a refusal. The
/// server's status and JSON answer come back whatever the status, so the
/// caller can say why; `Err(())` is "could not talk to the server". The
/// audit row names the request, never the bytes.
pub fn send(
    server: &str, endpoint: &str, token: &str, user_agent: &str, content_type: &str, body: &[u8], remaining: Duration,
) -> Result<(u16, Value), ()> {
    if remaining.is_zero() {
        return Err(());
    }
    let started = std::time::Instant::now();
    let url = format!("{server}{endpoint}");
    let audit = |ok: bool| crate::audit::record_timed("http", "POST", &url, None, body.len(), started.elapsed().as_millis() as u64, ok);
    let response = agent(remaining)
        .post(&url)
        .set("Content-Type", content_type)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", user_agent)
        .send_bytes(body);
    let (status, response) = match response {
        Ok(response) => (response.status(), response),
        Err(ureq::Error::Status(status, response)) => (status, response),
        Err(_) => {
            audit(false);
            return Err(());
        }
    };
    audit(status < 400);
    let text = response.into_string().unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Object(Default::default()))))
}

/// A request made without `post` (sign-in, the self-update's downloads,
/// the MCP bridge): recorded in the audit log all the same. `body` is the
/// text sent, JSON or a form.
pub fn audit_raw(method: &str, url: &str, body: &str) {
    let parsed = serde_json::from_str::<Value>(body).ok().or_else(|| {
        // a form: its fields, so the log can mask the credentials in it
        (!body.is_empty() && body.contains('=')).then(|| {
            Value::Object(
                body.split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
                    .collect(),
            )
        })
    });
    crate::audit::record("http", method, url, parsed.as_ref(), body.len());
}
