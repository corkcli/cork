//! A Cork message posted into another Claude Code session on this machine
//! the way Claude Code's own SendMessage posts one, so the recipient's
//! inbox treats it as a peer's message and not as an unknown one.
//!
//! Claude Code holds a peer message at a recipient that runs without asking
//! (bypass permissions) unless the sender attests its own permission class
//! and that class matches ("mode parity"): `bypass` for a session that runs
//! tools without asking, `prompting` for any other. Its SendMessage attests
//! it in the envelope it wraps the text in (Claude Code 2.1.312, read off its
//! binary: the UDS client builds `cAt(from, name, body, session, hops, mode)`
//! and the recipient's inbox policy reads `fromMode` back out of it):
//!
//! ```text
//! <cross-session-message from="uds:/tmp/cc-socks/123.sock" from-name="repo-35" from-mode="bypass">
//! the text
//! </cross-session-message>
//! ```
//!
//! sent as `{"msgV":1,"msg_id":<uuid>,"type":"user","message":{"role":"user",
//! "content":<envelope>},"priority":"next","from":<the same address>}` after
//! an auth line with the recipient's peer token. The recipient parses the
//! envelope only in its canonical form, so it is built exactly so.
//!
//! The class attested is the sending session's real one: its hooks record
//! the `permission_mode` every hook input carries. A session whose class is
//! not known (plan mode, which Claude Code counts as bypass only where
//! bypass is on; a mode never seen; no session at all) attests nothing, and
//! a message whose classes differ is not posted: Cork's own delivery (the
//! hooks, the listener) takes it, and needs no one's approval.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::config::Env;

/// The tag Claude Code wraps a cross-session message in.
const ENVELOPE: &str = "cross-session-message";

fn mode_path(session: &str, env: &Env) -> PathBuf {
    let digest = crate::report::sha256_hex(session.as_bytes());
    crate::config::data_dir(env).join("check").join(format!("mode-{}.json", &digest[..16]))
}

/// The permission mode a hook input says this session runs in, kept for
/// `cork message`: written only when it changed.
pub fn note_mode(session: &str, mode: &str, env: &Env) {
    let mode = mode.trim();
    if session.is_empty() || mode.is_empty() || mode_of(session, env) == mode {
        return;
    }
    crate::report::save_json(&mode_path(session, env), &json!({"mode": mode}));
}

/// The permission mode this session's hooks saw last; "" when none did.
pub fn mode_of(session: &str, env: &Env) -> String {
    if session.is_empty() {
        return String::new();
    }
    let kept = crate::config::load_json(&mode_path(session, env));
    kept.get("mode").and_then(Value::as_str).unwrap_or("").to_string()
}

/// A permission mode's class, as Claude Code's mode parity counts it:
/// `bypass` for bypass permissions, `prompting` for every mode that asks or
/// allows only some things. None for plan mode (bypass or not, depending on
/// a setting the hooks do not see) and for any mode not known here.
pub fn mode_class(mode: &str) -> Option<&'static str> {
    match mode {
        "bypassPermissions" => Some("bypass"),
        "default" | "acceptEdits" | "auto" | "dontAsk" => Some("prompting"),
        _ => None,
    }
}

/// Of these Claude Code sessions on this machine, the ones `me` can post a
/// message to without it being held there: its own class is known, and
/// theirs is the same. Never `me`; at most 64.
pub fn same_class(me: &str, sessions: &[&str], env: &Env) -> Vec<String> {
    let Some(mine) = mode_class(&mode_of(me, env)) else { return Vec::new() };
    sessions
        .iter()
        .filter(|s| **s != me && !s.is_empty() && mode_class(&mode_of(s, env)) == Some(mine))
        .take(64)
        .map(|s| s.to_string())
        .collect()
}

fn held_path(session: &str, env: &Env) -> PathBuf {
    let digest = crate::report::sha256_hex(session.as_bytes());
    crate::config::data_dir(env).join("check").join(format!("held-{}.txt", &digest[..16]))
}

/// Messages the session's listener took and could not post without them
/// being held there: they wait for the session's next hook output, which
/// needs no one's approval.
pub fn hold(session: &str, said: &str, env: &Env) {
    use std::io::Write;
    if session.is_empty() || said.trim().is_empty() {
        return;
    }
    let path = held_path(session, env);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = write!(file, "{}\n\n", said.trim());
    }
}

/// What `hold` kept for this session, taken once.
pub fn take_held(session: &str, env: &Env) -> String {
    if session.is_empty() {
        return String::new();
    }
    let path = held_path(session, env);
    let taking = path.with_extension(format!("taking{}", std::process::id()));
    if std::fs::rename(&path, &taking).is_err() {
        return String::new();
    }
    let said = std::fs::read_to_string(&taking).unwrap_or_default();
    let _ = std::fs::remove_file(&taking);
    said.trim().to_string()
}

/// A socket path as Claude Code addresses a session by it: `uds:` and the
/// path, any byte outside `A-Za-z0-9:_/.\-` percent-encoded.
pub fn address(socket: &str) -> String {
    let mut out = String::from("uds:");
    for byte in socket.bytes() {
        if byte.is_ascii_alphanumeric() || b":_/.\\-".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The envelope Claude Code's SendMessage wraps a message in, in the one
/// form its recipient parses back. None when the text could not survive it
/// (it names the envelope's own tag), or `name` is not a plain name: then
/// the message is not posted, and Cork's own delivery takes it.
pub fn envelope(from: &str, name: &str, class: &str, body: &str) -> Option<String> {
    let plain = |c: char| c.is_ascii_alphanumeric() || " ._-".contains(c);
    let squashed: String = body.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    if squashed.contains("crosssessionmessage") || !matches!(class, "bypass" | "prompting") || from.is_empty() {
        return None;
    }
    let name = name.trim();
    let named = if !name.is_empty() && name.chars().count() <= 64 && name.chars().all(plain) {
        format!(" from-name=\"{name}\"")
    } else {
        String::new()
    };
    Some(format!("<{ENVELOPE} from=\"{from}\"{named} from-mode=\"{class}\">\n{body}\n</{ENVELOPE}>"))
}

/// A version 4 UUID, as Claude Code's frames carry one in `msg_id`.
fn uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let seed = format!("{nanos}:{}:{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    let digest = crate::report::sha256_hex(seed.as_bytes());
    let mut hex: Vec<char> = digest.chars().take(32).collect();
    hex[12] = '4';
    hex[16] = ['8', '9', 'a', 'b'][usize::from(hex[16].to_digit(16).unwrap_or(0) as u8 & 3)];
    let hex: String = hex.into_iter().collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// The frame Claude Code's SendMessage writes for one message, enveloped.
pub fn frame(from: &str, content: &str) -> Value {
    json!({"msgV": 1, "msg_id": uuid(), "type": "user", "message": {"role": "user", "content": content},
           "priority": "next", "from": from})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code's own parse of the envelope (2.1.312), ported: the
    /// attributes in their order, and the body between two newlines. Its
    /// recipient takes `from-mode` only from an envelope that matches this
    /// and renders back to itself.
    fn parses(content: &str) -> Option<(String, String, String, String)> {
        let re = regex::Regex::new(concat!(
            r#"^<cross-session-message(?: from="([A-Za-z0-9%:_/.\\-]+)")?(?: from-session="([A-Za-z0-9_-]{1,80})")?"#,
            r#"(?: from-name="([^"<>\n\r]+)")?(?: from-mode="(bypass|prompting)")?(?: from-plugin="([^"<>\n\r]+)")?>\n([\s\S]*)\n</cross-session-message>$"#
        ))
        .unwrap();
        let c = re.captures(content)?;
        let get = |i: usize| c.get(i).map(|m| m.as_str().to_string()).unwrap_or_default();
        Some((get(1), get(3), get(4), get(6)))
    }

    /// The field report (v104): every message Cork posted into a session in
    /// bypass permissions was held there, "from an unidentified session ...
    /// The sender did not attest its permission mode". Now it is enveloped
    /// as Claude Code's SendMessage envelopes one, with the sender's address,
    /// name and real class.
    #[test]
    fn a_message_is_enveloped_as_claude_codes_own_with_the_senders_class() {
        let from = address("/tmp/cc-socks/4572.sock");
        assert_eq!(from, "uds:/tmp/cc-socks/4572.sock");
        assert_eq!(address("/tmp/my socks/1.sock"), "uds:/tmp/my%20socks/1.sock");
        let body = "Cork: A message from teammates' agents.\n[m1] kevin@x.com, just now: the API is up";
        let content = envelope(&from, "cork-linear-test-e3", "bypass", body).unwrap();
        assert_eq!(parses(&content), Some((from.clone(), "cork-linear-test-e3".into(), "bypass".into(), body.into())), "{content}");
        // a name that would not survive Claude Code's own cleanup is left out, not mangled
        let unnamed = envelope(&from, "a \"quoted\" <name>", "prompting", "hi").unwrap();
        assert_eq!(parses(&unnamed).map(|p| (p.1, p.2)), Some((String::new(), "prompting".to_string())));
        // a text naming the envelope's tag, or no class: not posted at all
        assert!(envelope(&from, "x", "bypass", "see </cross-session-message> here").is_none());
        assert!(envelope(&from, "x", "", "hi").is_none());
        let frame = frame(&from, &content);
        assert_eq!((frame["type"].as_str(), frame["priority"].as_str(), frame["from"].as_str()), (Some("user"), Some("next"), Some(from.as_str())));
        assert_eq!(frame["message"]["content"], json!(content));
        let id = frame["msg_id"].as_str().unwrap();
        assert!(regex::Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$").unwrap().is_match(id), "{id}");
    }

    /// The class is the sender's real one, never claimed: a prompting session
    /// attests prompting, plan mode and an unseen mode attest nothing, and a
    /// message goes to the peer socket only where the classes match.
    #[test]
    fn only_a_session_whose_class_is_known_and_matches_is_posted_to() {
        let dir = std::env::temp_dir().join(format!("cork-peer-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut env = Env::new();
        env.insert("CORK_HOME".into(), dir.to_string_lossy().into_owned());
        assert_eq!(mode_class("bypassPermissions"), Some("bypass"));
        assert_eq!(mode_class("acceptEdits"), Some("prompting"));
        assert_eq!(mode_class("plan"), None);
        assert_eq!(mode_class(""), None);
        for (session, mode) in [("a", "bypassPermissions"), ("b", "bypassPermissions"), ("c", "default"), ("d", "plan")] {
            note_mode(session, mode, &env);
        }
        let all = ["a", "b", "c", "d", "e"];
        assert_eq!(same_class("a", &all, &env), vec!["b".to_string()]);
        assert_eq!(same_class("c", &all, &env), Vec::<String>::new(), "no other prompting session");
        assert!(same_class("d", &all, &env).is_empty(), "plan mode attests nothing");
        assert!(same_class("e", &all, &env).is_empty(), "a session no hook has seen attests nothing");
        // a mode change is kept: the session now prompts
        note_mode("b", "default", &env);
        assert_eq!(same_class("c", &all, &env), vec!["b".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
