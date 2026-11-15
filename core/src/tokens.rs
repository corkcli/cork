//! Tokens that only look like code: the ids a harness or a tool prints (a
//! tool-use id, a background task's id, a digest), which a prompt carries
//! when the person pastes a notification or a log, and which name nothing in
//! any repository. The hooks do not send them for a briefing, and the
//! server does not list them as names it found no code for.

/// Whether `token` is an id rather than a name someone chose: a known id
/// prefix (`toolu_01…`, `msg_…`, `req_…`), hex with digits in it, or
/// letters and digits interleaved with no separator (`b6x30h54c`).
pub fn looks_like_id(token: &str) -> bool {
    let t = token.trim();
    if t.len() < 7 || !t.is_ascii() {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    for prefix in ["toolu_", "srvtoolu_", "msg_", "req_", "call_", "chatcmpl-", "run_", "wf_", "batch_", "file_"] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let rest_ok = rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if rest.len() >= 6 && rest_ok && rest.chars().any(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    if !t.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let digits = t.chars().filter(char::is_ascii_digit).count();
    if digits == 0 || digits == t.len() {
        return false;
    }
    // a digest or an id in hex
    if t.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    transitions(t) >= 4
}

/// How often a token switches between letters and digits: `b6x30h54c` six
/// times, `utf8` once, `sha256sum` twice.
fn transitions(token: &str) -> usize {
    let kinds: Vec<bool> = token.chars().map(|c| c.is_ascii_digit()).collect();
    kinds.windows(2).filter(|pair| pair[0] != pair[1]).count()
}

#[cfg(test)]
mod tests {
    use super::looks_like_id;

    #[test]
    fn ids_are_told_from_names() {
        for id in ["toolu_01NptqZqEmu3CZcyS7tLaxcs", "a4a12e608ecd81ce7", "b6x30h54c", "b0uw00pe6", "9d5dff3a", "msg_01XyZ9abc"] {
            assert!(looks_like_id(id), "{id}");
        }
        for name in ["mountIssue", "subagent_tokens", "utf8_decode", "sha256sum", "base64url", "x86_64", "h264Decoder",
                     "app.css", "toolu", "Collide", "v1beta2", "duration_ms", "test_rust_hooks"] {
            assert!(!looks_like_id(name), "{name}");
        }
    }
}
