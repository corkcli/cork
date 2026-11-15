//! Web Bot Auth: the requests this server sends to other sites carry an
//! HTTP Message Signature (RFC 9421) made with Cork's Ed25519 key, so a site
//! can tell a request that says it is Cork really is. The public keys are at
//! https://corkcli.com/.well-known/http-message-signatures-directory (the
//! website serves them: nextjs-app/src/lib/web-bot-auth.ts, which also says
//! how a key is rotated).
//!
//! The private key is CORK_BOT_SIGNING_KEY: the 32-byte seed in base64url
//! (a private JWK's `d`); with several, comma-separated, the first signs.
//! Unset, nothing is signed and the log says so once.
//!
//! Each signature covers `@authority` and `Signature-Agent`, carries
//! created, keyid (the key's JWK thumbprint), alg, expires (five minutes
//! on), a nonce and tag="web-bot-auth", and Signature-Agent is the quoted
//! origin whose directory lists the key. That is the form Cloudflare's
//! verifier documents (and draft-ietf-webbotauth-httpsig-protocol-01's
//! Appendix E.2.2); the draft's newer dictionary form (sig1="...", covered
//! as "signature-agent";key="sig1") is refused by Cloudflare's edge today.
//!
//! Signed: the status checks' agent (status.rs), which reads Resend's status
//! page as well as Cork's own addresses. Calls to API providers with their
//! own credentials (Stripe, GitHub, Resend's API, PostHog, S3) are not.

use std::io::Read;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

/// The origin whose key directory lists the keys.
pub const SIGNATURE_AGENT: &str = "https://corkcli.com";
pub const TAG: &str = "web-bot-auth";
/// How long a signature is good for.
const EXPIRES_S: u64 = 300;

pub struct Key {
    signing: SigningKey,
    /// the public key's JWK thumbprint, the keyid signatures name
    pub kid: String,
}

impl Key {
    /// A key from its 32-byte seed in base64url.
    pub fn from_seed(seed: &str) -> Result<Key, String> {
        let raw = URL_SAFE_NO_PAD
            .decode(seed.trim().trim_end_matches('='))
            .map_err(|_| "not base64url".to_string())?;
        let bytes: [u8; 32] = raw.try_into().map_err(|_| "an Ed25519 private key is 32 bytes".to_string())?;
        let signing = SigningKey::from_bytes(&bytes);
        let kid = thumbprint(signing.verifying_key().as_bytes());
        Ok(Key { signing, kid })
    }
}

/// RFC 7638 thumbprint of an Ed25519 public key (RFC 8037 §2, A.3).
pub fn thumbprint(public: &[u8; 32]) -> String {
    let x = URL_SAFE_NO_PAD.encode(public);
    URL_SAFE_NO_PAD.encode(Sha256::digest(format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x}"}}"#).as_bytes()))
}

/// The key requests are signed with, read once.
pub fn key() -> Option<&'static Key> {
    static KEY: OnceLock<Option<Key>> = OnceLock::new();
    KEY.get_or_init(|| {
        let value = crate::env::var("CORK_BOT_SIGNING_KEY").unwrap_or_default();
        let Some(first) = value.split(',').map(str::trim).find(|seed| !seed.is_empty()) else {
            tracing::info!("web bot auth: CORK_BOT_SIGNING_KEY is not set; requests to other sites go unsigned");
            return None;
        };
        match Key::from_seed(first) {
            Ok(key) => Some(key),
            Err(why) => {
                tracing::warn!("web bot auth: CORK_BOT_SIGNING_KEY is not a key ({why}); requests to other sites go unsigned");
                None
            }
        }
    })
    .as_ref()
}

/// A URL's authority as RFC 9421 §2.2.3 has it: the host, lowercase, and the
/// port when it is not the scheme's default.
pub fn authority(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    Some(match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

/// An sf-string (RFC 9651 §3.3.3).
fn sf_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The three headers a signed request carries.
#[derive(Debug, PartialEq)]
pub struct Signed {
    pub signature_agent: String,
    pub signature_input: String,
    pub signature: String,
}

/// Sign a request to `authority` as `agent`, under `label`. An empty nonce
/// is left out.
pub fn sign(key: &Key, label: &str, authority: &str, agent: &str, created: u64, expires: u64, nonce: &str) -> Signed {
    let agent = sf_string(agent);
    let nonce = if nonce.is_empty() { String::new() } else { format!(";nonce={}", sf_string(nonce)) };
    let params = format!(
        r#"("@authority" "signature-agent");created={created};keyid={};alg="ed25519";expires={expires}{nonce};tag="{TAG}""#,
        sf_string(&key.kid)
    );
    let base = format!("\"@authority\": {authority}\n\"signature-agent\": {agent}\n\"@signature-params\": {params}");
    let signature = STANDARD.encode(key.signing.sign(base.as_bytes()).to_bytes());
    Signed { signature_agent: agent, signature_input: format!("{label}={params}"), signature: format!("{label}=:{signature}:") }
}

/// 64 random bytes in base64, or empty when the OS will not give them.
fn nonce() -> String {
    let mut bytes = [0u8; 64];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)) {
        Ok(()) => STANDARD.encode(bytes),
        Err(_) => String::new(),
    }
}

/// The headers that sign a request to `url` now: none when there is no key
/// or the URL has no host. For any HTTP client.
pub fn headers_for(url: &str) -> Vec<(&'static str, String)> {
    key().map(|key| headers_with(key, url)).unwrap_or_default()
}

fn headers_with(key: &Key, url: &str) -> Vec<(&'static str, String)> {
    let Some(authority) = authority(url) else { return Vec::new() };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let signed = sign(key, "sig1", &authority, SIGNATURE_AGENT, now, now + EXPIRES_S, &nonce());
    vec![("Signature-Agent", signed.signature_agent), ("Signature-Input", signed.signature_input), ("Signature", signed.signature)]
}

fn with_headers(mut request: ureq::Request, headers: Vec<(&'static str, String)>) -> ureq::Request {
    for (name, value) in headers {
        request = request.set(name, &value);
    }
    request
}

/// ureq middleware that signs every request an agent sends:
/// `AgentBuilder::new().middleware(webbotauth::sign_ureq)`.
pub fn sign_ureq(request: ureq::Request, next: ureq::MiddlewareNext) -> Result<ureq::Response, ureq::Error> {
    let headers = headers_for(request.url());
    next.handle(with_headers(request, headers))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 9421 Appendix B.1.4's test-key-ed25519, as its seed (the JWK's d).
    const TEST_KEY: &str = "n4Ni-HpISpVObnQMW0wOhCKROaIKqKtW_2ZYb2p9KcU";

    #[test]
    fn the_thumbprint_is_rfc_8037s() {
        // RFC 8037 A.3
        let x: [u8; 32] = URL_SAFE_NO_PAD.decode("11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo").unwrap().try_into().unwrap();
        assert_eq!(thumbprint(&x), "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k");
        assert_eq!(Key::from_seed(TEST_KEY).unwrap().kid, "poqkLGiymh_W0uP6PZFw-dvez3QJT5SolqXBCW38r0U");
    }

    #[test]
    fn the_key_signs_rfc_9421s_ed25519_example() {
        // RFC 9421 B.2.6: the signature base and the signature it gives
        let base = "\"date\": Tue, 20 Apr 2021 02:07:55 GMT\n\"@method\": POST\n\"@path\": /foo\n\"@authority\": example.com\n\
                    \"content-type\": application/json\n\"content-length\": 18\n\
                    \"@signature-params\": (\"date\" \"@method\" \"@path\" \"@authority\" \"content-type\" \"content-length\");created=1618884473;keyid=\"test-key-ed25519\"";
        let key = Key::from_seed(TEST_KEY).unwrap();
        assert_eq!(
            STANDARD.encode(key.signing.sign(base.as_bytes()).to_bytes()),
            "wqcAqbmYJ2ji2glfAMaRy4gruYYnx2nEFN2HN6jrnDnQCK1u02Gb04v9EDgwUPiu4A0w6vuQv5lIp5WPpBKRCw=="
        );
    }

    #[test]
    fn a_request_is_signed_as_the_web_bot_auth_draft_shows() {
        // draft-ietf-webbotauth-httpsig-protocol-01, Appendix E.2.2
        let key = Key::from_seed(TEST_KEY).unwrap();
        let nonce = "e8N7S2MFd/qrd6T2R3tdfAuuANngKI7LFtKYI/vowzk4lAZYadIX6wW25MwG7DCT9RUKAJ0qVkU0mEeLElW1qg==";
        let signed = sign(&key, "sig2", "example.com", "https://signature-agent.test", 1735689600, 1735693200, nonce);
        assert_eq!(signed.signature_agent, "\"https://signature-agent.test\"");
        assert_eq!(
            signed.signature_input,
            format!(
                "sig2=(\"@authority\" \"signature-agent\");created=1735689600;keyid=\"poqkLGiymh_W0uP6PZFw-dvez3QJT5SolqXBCW38r0U\";\
                 alg=\"ed25519\";expires=1735693200;nonce=\"{nonce}\";tag=\"web-bot-auth\""
            )
        );
        assert_eq!(
            signed.signature,
            "sig2=:jdq0SqOwHdyHr9+r5jw3iYZH6aNGKijYp/EstF4RQTQdi5N5YYKrD+mCT1HA1nZDsi6nJKuHxUi/5Syp3rLWBA==:"
        );
    }

    #[test]
    fn the_authority_drops_a_default_port_and_keeps_another() {
        assert_eq!(authority("https://API.IndexNow.org/indexnow?x=1").as_deref(), Some("api.indexnow.org"));
        assert_eq!(authority("https://example.com:443/").as_deref(), Some("example.com"));
        assert_eq!(authority("http://127.0.0.1:8080/health").as_deref(), Some("127.0.0.1:8080"));
        assert_eq!(authority("not a url"), None);
    }

    #[test]
    fn an_agent_with_the_middleware_sends_a_signature_that_verifies() {
        use std::io::Write;
        static KEY: OnceLock<Key> = OnceLock::new();
        let key = KEY.get_or_init(|| Key::from_seed(TEST_KEY).unwrap());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while !seen.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                seen.push(byte[0]);
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            String::from_utf8(seen).unwrap()
        });
        let agent = ureq::AgentBuilder::new()
            .middleware(move |request: ureq::Request, next: ureq::MiddlewareNext| {
                let headers = headers_with(key, request.url());
                next.handle(with_headers(request, headers))
            })
            .build();
        agent.get(&format!("http://127.0.0.1:{port}/health")).call().unwrap();
        let head = server.join().unwrap();
        let header = |name: &str| {
            head.lines()
                .find_map(|line| line.split_once(": ").filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.to_string()))
                .unwrap_or_else(|| panic!("no {name} in {head}"))
        };
        assert_eq!(header("Signature-Agent"), "\"https://corkcli.com\"");
        let input = header("Signature-Input");
        let params = input.strip_prefix("sig1=").unwrap();
        assert!(params.starts_with("(\"@authority\" \"signature-agent\");created="), "{params}");
        assert!(params.ends_with(";tag=\"web-bot-auth\""), "{params}");
        let base = format!("\"@authority\": 127.0.0.1:{port}\n\"signature-agent\": \"https://corkcli.com\"\n\"@signature-params\": {params}");
        let signature = STANDARD.decode(header("Signature").strip_prefix("sig1=:").unwrap().strip_suffix(':').unwrap()).unwrap();
        let signature = ed25519_dalek::Signature::from_slice(&signature).unwrap();
        key.signing.verifying_key().verify_strict(base.as_bytes(), &signature).unwrap();
    }

    #[test]
    fn a_bad_key_is_refused() {
        assert!(Key::from_seed("short").is_err());
        assert!(Key::from_seed("!!!").is_err());
    }
}
