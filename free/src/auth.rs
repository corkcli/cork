//! Resolving a bearer token to the principal behind it.
//!
//! The hot endpoints are reached with a hook credential (`cat_…`) rather than
//! a Firebase ID token, and those resolve with one KV read: tokens are stored
//! hashed, so a database leak never leaks a usable credential.
//!
//! `principal_for_bearer` below only ever resolves the `cat_…` shape. The
//! dashboard's identity (Firebase ID tokens, and `test:` tokens when test
//! auth is on) is `DashboardAuth`, further down.

use serde_json::Value;

use crate::store::Store;

pub const TOKEN_BUCKET: &str = "oauth_token";
pub const MEMBER_BUCKET: &str = "member";
/// Prefixes that make a bearer token self-describing at a glance (in logs,
/// in `oauth::token`'s grant-type branches) without a KV lookup. Minted by
/// `oauth::mint_tokens`.
pub const ACCESS_TOKEN_PREFIX: &str = "cat_";
pub const REFRESH_TOKEN_PREFIX: &str = "crt_";
/// An unclaimed anonymous agent's token (`oauth::agent_identity`): kind
/// `sandbox` in the token bucket, so `principal_for_bearer`, and every
/// surface that resolves a credential through it, refuses it outright. Only
/// the MCP endpoint asks `sandbox_principal` for one.
pub const SANDBOX_TOKEN_PREFIX: &str = "csb_";
/// Agent registrations (`oauth::agent_identity`), by registration id.
pub const AGENT_REG_BUCKET: &str = "agent_reg";
/// The only repos a sandbox token may name: `sandbox/<anything>`.
pub const SANDBOX_REPO_PREFIX: &str = "sandbox/";

/// Tokens are stored hashed: a DB leak never leaks a usable credential.
/// `pub` (not `pub(crate)`-only-by-accident) because `oauth.rs` — a sibling
/// module, not this one — hashes both the token it just minted (to write
/// the record) and one it is looking up (`/token`'s refresh-token grant),
/// so it needs this to agree byte for byte with lookups done here.
pub fn token_key(token: &str) -> String {
    crate::hashing::sha256_hex(token)
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub uid: String,
    pub email: String,
    /// Display name from the identity provider; not used for access control,
    /// only echoed back into records and dashboards. Populated by
    /// `oauth::mint_tokens` from the Firebase principal that approved the
    /// grant, so `token`'s refresh path can rebuild an equivalent `Principal`
    /// from the stored record without a fresh Firebase round trip.
    pub name: String,
    pub workspace: String,
    /// every workspace this credential may touch; empty means an unrestricted
    /// legacy binding. Read by the endpoints still to move across, which is
    /// why it is resolved here rather than where it is first needed.
    #[allow(dead_code)]
    pub workspaces: Vec<String>,
    /// The numeric id of the GitHub account linked to this Firebase login
    /// (`firebase.identities["github.com"]` in the ID token), empty when the
    /// login has none or the credential is not a Firebase one. GitHub is a
    /// sign-in only and Cork keeps no GitHub token, so nothing gates on
    /// this today; it stays resolved because it is the sign-in's own record
    /// of which GitHub account it is.
    #[allow(dead_code)]
    pub github_id: String,
    /// Firebase's own `email_verified` claim: the address was proven (a Google
    /// sign-in, a verified email link). False for every non-Firebase
    /// credential. An email/password account can claim ANY address, so a
    /// domain check without this proves nothing — the admin gate needs both.
    pub email_verified: bool,
}

impl Principal {
    /// The identity written into records and shown on dashboards.
    pub fn label(&self) -> String {
        if self.email.is_empty() { self.uid.clone() } else { self.email.clone() }
    }
}

pub fn principal_for_bearer(store: &Store, header: Option<&str>, now: f64) -> Option<Principal> {
    let header = header?;
    let (scheme, token) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
        return None;
    }
    let record = store.kv_get(TOKEN_BUCKET, &token_key(token.trim()))?;
    if record.get("kind").and_then(Value::as_str) != Some("access") {
        return None;
    }
    if record.get("expires").and_then(Value::as_f64).unwrap_or(0.0) <= now {
        return None;
    }
    let text = |key: &str| record.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    Some(Principal {
        uid: text("uid"),
        email: text("email"),
        name: text("name"),
        workspace: text("workspace"),
        workspaces: record
            .get("workspaces")
            .and_then(Value::as_array)
            .map(|items| {
                items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()
            })
            .unwrap_or_default(),
        // a hook / MCP credential is not a Firebase login: no GitHub link
        github_id: String::new(), email_verified: false,
    })
}

/// An unclaimed anonymous agent behind a sandbox token: its own sandbox
/// workspace and nothing else. None unless the token is live AND its
/// registration is still unclaimed and unexpired, so claiming or expiring a
/// registration ends every token it was given at once, whatever their own
/// expiry says.
pub fn sandbox_principal(store: &Store, header: Option<&str>, now: f64) -> Option<Principal> {
    let (scheme, token) = header?.split_once(' ')?;
    let token = token.trim();
    if !scheme.eq_ignore_ascii_case("bearer") || !token.starts_with(SANDBOX_TOKEN_PREFIX) {
        return None;
    }
    let record = store.kv_get(TOKEN_BUCKET, &token_key(token))?;
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    if text(&record, "kind") != "sandbox" || record.get("expires").and_then(Value::as_f64).unwrap_or(0.0) <= now {
        return None;
    }
    let registration = store.kv_get(AGENT_REG_BUCKET, &text(&record, "registration"))?;
    let workspace = text(&record, "workspace");
    if text(&registration, "status") != "unclaimed"
        || registration.get("expires").and_then(Value::as_f64).unwrap_or(0.0) <= now
        || workspace.is_empty()
        || text(&registration, "sandbox_ws") != workspace
    {
        return None;
    }
    Some(Principal {
        uid: text(&record, "uid"),
        email: String::new(),
        name: "sandbox agent".to_string(),
        workspaces: vec![workspace.clone()],
        workspace,
        github_id: String::new(),
        email_verified: false,
    })
}

/// A repo a sandbox token may name: `sandbox/<name>`, nothing else.
pub fn is_sandbox_repo(repo_id: &str) -> bool {
    let repo = repo_id.trim().to_ascii_lowercase();
    repo.len() > SANDBOX_REPO_PREFIX.len() && repo.starts_with(SANDBOX_REPO_PREFIX) && !repo.contains("..")
}

/// The dashboard page where a person does what an agent was refused:
/// `<dashboard>/dashboard/settings?workspace=<id>&section=<section>`.
pub fn settings_link(dashboard: &str, workspace: &str, section: &str) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if !workspace.is_empty() {
        query.append_pair("workspace", workspace);
    }
    query.append_pair("section", section);
    format!("{}/dashboard/settings?{}", dashboard.trim_end_matches('/'), query.finish())
}

/// An agent's credential by its shape: every token this server mints for an
/// agent or a machine (`cat_` access, `crt_` refresh, `csb_` sandbox) and
/// any other `<letters>_` token it may mint later. A person's dashboard
/// session is a Firebase ID token (a JWT, `eyJ…`) or, under the test suite,
/// `test:…`.
pub fn is_agent_token(token: &str) -> bool {
    let token = token.trim();
    token.starts_with(ACCESS_TOKEN_PREFIX)
        || token.starts_with(REFRESH_TOKEN_PREFIX)
        || token.starts_with(SANDBOX_TOKEN_PREFIX)
        || token.split_once('_').is_some_and(|(prefix, _)| (2..=4).contains(&prefix.len()) && prefix.chars().all(|c| c.is_ascii_lowercase()))
}

/// The membership record, which also carries the read/write access level.
/// A per-repo access list comes back rename-aware (see
/// [`with_repo_renames`]), so every access check reads the same list.
pub fn member(store: &Store, workspace_id: &str, uid: &str) -> Option<Value> {
    if workspace_id.is_empty() || uid.is_empty() {
        return None;
    }
    store
        .kv_get(MEMBER_BUCKET, &format!("{workspace_id}/{uid}"))
        .map(|record| with_repo_renames(store, workspace_id, record))
}

/// A member's repo list, widened by the workspace's renames. A renamed repo
/// has two names — the one its data is stored under (`agentmemory`) and the
/// one everyone sees (`collide`) — linked by a `repo_alias` row. The owner
/// ticks the name they see; reads that walk the workspace's scopes see the
/// stored one. Without this, a member given "collide" was refused its own
/// repo everywhere but a direct call, while "every repo" worked. Both names
/// (and any chain of renames) count as the same repo. "*" and a missing
/// list are untouched — nothing to widen, and no extra read on that path.
pub fn with_repo_renames(store: &Store, workspace_id: &str, mut record: Value) -> Value {
    let Some(list) = record.get("repos").and_then(Value::as_array).cloned() else { return record };
    let prefix = format!("{workspace_id}:");
    let links: Vec<(String, String)> = store
        .kv_list("repo_alias", &prefix)
        .into_iter()
        .filter_map(|(key, row)| {
            let from = key.strip_prefix(&prefix).unwrap_or(&key);
            let to = row.get("canonical").and_then(Value::as_str)?;
            Some((crate::repo::repo_key(from), crate::repo::repo_key(to)))
        })
        .collect();
    if links.is_empty() {
        return record;
    }
    let text = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    let mut keys: std::collections::BTreeSet<String> = list.iter().map(|v| crate::repo::repo_key(&text(v))).collect();
    loop {
        let before = keys.len();
        for (from, to) in &links {
            if keys.contains(from) || keys.contains(to) {
                keys.insert(from.clone());
                keys.insert(to.clone());
            }
        }
        if keys.len() == before {
            break;
        }
    }
    let original: std::collections::BTreeSet<String> = list.iter().map(|v| crate::repo::repo_key(&text(v))).collect();
    let mut widened = list;
    widened.extend(keys.difference(&original).map(|k| Value::String(k.clone())));
    record["repos"] = Value::Array(widened);
    record
}

pub fn is_read_only(member: Option<&Value>) -> bool {
    member
        .and_then(|m| m.get("access"))
        .and_then(Value::as_str)
        .map(|access| access == "read")
        .unwrap_or(false)
}

// ------------------------------------------------- the dashboard's identity

/// Who is asking, resolved in this order: a
/// workspace-bound `cat_` token, then a `test:` token when test auth is on,
/// then a Firebase ID token. The order is load-bearing — a `cat_` token is
/// looked up, never decoded, and must never fall through to the JWT path.
#[derive(Clone, Debug)]
pub struct DashboardAuth {
    pub test_mode: bool,
    pub firebase_project_id: String,
    /// The emulator signs nothing, so signature verification is skipped and
    /// audience, expiry and issuer are still checked. Never on in production.
    pub firebase_emulator: bool,
}

impl DashboardAuth {
    pub fn from_env() -> Self {
        let env = |key: &str| crate::env::var(key).unwrap_or_default().trim().to_string();
        Self {
            test_mode: env("CORK_TEST_AUTH") == "1",
            firebase_project_id: env("FIREBASE_PROJECT_ID"),
            firebase_emulator: !env("FIREBASE_AUTH_EMULATOR_HOST").is_empty(),
        }
    }

    pub fn principal_for(&self, store: &Store, token: &str, now: f64) -> Option<Principal> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        if token.starts_with(ACCESS_TOKEN_PREFIX) {
            return principal_for_bearer(store, Some(&format!("Bearer {token}")), now);
        }
        if self.test_mode && token.starts_with("test:") {
            // "test:<uid>[:<workspace>[:<email>]]" — the moral equivalent of
            // a real credential for the suite, and nothing else ever
            let mut parts = token.split(':');
            parts.next();
            let uid = parts.next().unwrap_or("").to_string();
            if uid.is_empty() {
                return None;
            }
            let workspace = parts.next().unwrap_or("").to_string();
            let email = parts.next().unwrap_or("").to_string();
            return Some(Principal {
                uid: uid.clone(),
                email_verified: !email.is_empty(),
                email,
                name: uid,
                workspace,
                workspaces: Vec::new(),
                github_id: String::new(),
            });
        }
        if !self.firebase_project_id.is_empty() {
            return self.verify_firebase(token, now);
        }
        None
    }

    /// The person behind a dashboard session, and never an agent: a bearer
    /// that is an agent's credential (an OAuth access or refresh token, a
    /// hook or machine credential, all `cat_`/`crt_`) is refused here though
    /// `principal_of` accepts it. For the routes that mint or widen a
    /// credential: an agent holding a token narrowed to one workspace must
    /// not approve itself one for all of them.
    pub fn person_of(&self, store: &Store, headers: &axum::http::HeaderMap, now: f64) -> Option<Principal> {
        let bearer = headers.get("authorization").and_then(|value| value.to_str().ok()).unwrap_or("");
        let token = bearer.split_once(' ').map(|(_, token)| token.trim()).unwrap_or("");
        if is_agent_token(token) {
            return None;
        }
        self.principal_of(store, headers, "", now)
    }

    /// An owner power (inviting people, changing a member's access or
    /// removing them, transferring the workspace, answering seat requests,
    /// PR checks' reading mode, the privacy switches): the person, signed in
    /// at the dashboard. An agent's token, however much it may otherwise do,
    /// gets a 403 naming the page (`page`) where its person does this; no
    /// credential at all, a 401.
    pub fn owner_power(&self, store: &Store, headers: &axum::http::HeaderMap, now: f64, page: &str) -> Result<Principal, Refusal> {
        let bearer = headers.get("authorization").and_then(|value| value.to_str().ok()).unwrap_or("");
        let token = bearer.split_once(' ').map(|(_, token)| token.trim()).unwrap_or("");
        if is_agent_token(token) {
            return Err((403, serde_json::json!({
                "ok": false,
                "code": "person_required",
                "error": format!("an agent cannot do this: only a person, signed in to Cork, can. Ask your person to do it at {page}"),
                "dashboard": page,
            })));
        }
        self.person_of(store, headers, now).ok_or_else(|| (401, serde_json::json!({"error": "unauthorized"})))
    }

    /// Bearer header first; explicit `?token=` second, for WebSocket-style
    /// clients that cannot set headers.
    pub fn principal_of(
        &self, store: &Store, headers: &axum::http::HeaderMap, token_param: &str, now: f64,
    ) -> Option<Principal> {
        let from_header = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|header| {
                let (scheme, token) = header.split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then(|| token.to_string())
            })
            .and_then(|token| self.principal_for(store, &token, now));
        match from_header {
            Some(principal) => Some(principal),
            None if !token_param.is_empty() => self.principal_for(store, token_param, now),
            None => None,
        }
    }

    /// A Firebase ID token: RS256 against Google's published keys, audience
    /// the project, issuer `securetoken.google.com/<project>`. The keys come
    /// from the JWK endpoint, in a form the verifier consumes directly.
    fn verify_firebase(&self, token: &str, now: f64) -> Option<Principal> {
        use jsonwebtoken::{decode, decode_header, Algorithm, Validation};
        let issuer = format!("https://securetoken.google.com/{}", self.firebase_project_id);
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[self.firebase_project_id.as_str()]);
        validation.set_issuer(&[issuer.as_str()]);

        let claims: Value = if self.firebase_emulator {
            // The emulator signs nothing, so the payload is read without a
            // signature check and audience, issuer and expiry are checked by
            // hand — the same three PyJWT checks with verify_signature off.
            let payload = token.split('.').nth(1)?;
            use base64::Engine;
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
            let claims: Value = serde_json::from_slice(&bytes).ok()?;
            let aud_ok = match claims.get("aud") {
                Some(Value::String(aud)) => aud == &self.firebase_project_id,
                Some(Value::Array(auds)) => auds.iter().any(|a| a.as_str() == Some(&self.firebase_project_id)),
                _ => false,
            };
            let exp_ok = claims.get("exp").and_then(Value::as_f64).is_some_and(|exp| exp > now);
            let iss_ok = claims.get("iss").and_then(Value::as_str) == Some(issuer.as_str());
            if !(aud_ok && exp_ok && iss_ok) {
                return None;
            }
            claims
        } else {
            let kid = decode_header(token).ok()?.kid?;
            let key = firebase_key(&kid, now)?;
            decode::<Value>(token, &key, &validation).ok()?.claims
        };
        let uid = claims
            .get("sub")
            .or_else(|| claims.get("user_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if uid.is_empty() {
            return None;
        }
        let text = |key: &str| claims.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        Some(Principal {
            uid, email: text("email"), name: text("name"),
            workspace: String::new(), workspaces: Vec::new(),
            github_id: linked_github_id(&claims),
            email_verified: claims.get("email_verified").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

/// The GitHub account linked to a Firebase login, from the verified ID
/// token: `firebase.identities["github.com"][0]`, a numeric id (string or
/// number in the claim). Empty when no GitHub account is linked.
pub fn linked_github_id(claims: &Value) -> String {
    match claims.pointer("/firebase/identities/github.com/0") {
        Some(Value::String(id)) => id.clone(),
        Some(Value::Number(id)) => id.to_string(),
        _ => String::new(),
    }
}

type JwkCache = std::sync::Mutex<(f64, std::collections::BTreeMap<String, jsonwebtoken::jwk::Jwk>)>;

fn jwk_cache() -> &'static JwkCache {
    static KEYS: std::sync::OnceLock<JwkCache> = std::sync::OnceLock::new();
    KEYS.get_or_init(|| std::sync::Mutex::new((0.0, Default::default())))
}

const FIREBASE_JWKS: &str =
    "https://www.googleapis.com/service_accounts/v1/jwk/securetoken@system.gserviceaccount.com";

/// Fetch Google's signing keys. Network only — the lock is taken afterwards,
/// for the swap, so a slow fetch never holds every other request behind it.
fn refresh_firebase_keys(now: f64) -> bool {
    let fetched: Option<jsonwebtoken::jwk::JwkSet> = ureq::get(FIREBASE_JWKS)
        .timeout(std::time::Duration::from_secs(5))
        .call()
        .ok()
        .and_then(|response| response.into_string().ok())
        .and_then(|text| serde_json::from_str(&text).ok());
    let Some(set) = fetched else { return false };
    if let Ok(mut guard) = jwk_cache().lock() {
        guard.1 = set.keys.into_iter().filter_map(|k| Some((k.common.key_id.clone()?, k))).collect();
        guard.0 = now + 3600.0;
    }
    true
}

/// Keep the key cache warm from a blocking thread, at startup and hourly, so
/// the request path only ever reads it. A worker thread blocked on Google
/// under a mutex is how one slow fetch stalls every dashboard call at once.
pub fn spawn_firebase_key_refresh() {
    tokio::task::spawn_blocking(|| loop {
        refresh_firebase_keys(crate::store::now());
        std::thread::sleep(std::time::Duration::from_secs(3600));
    });
}

/// A key by id from the cache. An unknown id means rotation, which is rare;
/// one fetch is allowed then, still outside the lock.
fn firebase_key(kid: &str, now: f64) -> Option<jsonwebtoken::DecodingKey> {
    let cached = jwk_cache().lock().ok().and_then(|guard| guard.1.get(kid).cloned());
    let jwk = match cached {
        Some(jwk) => jwk,
        None => {
            if !refresh_firebase_keys(now) {
                return None;
            }
            jwk_cache().lock().ok()?.1.get(kid).cloned()?
        }
    };
    jsonwebtoken::DecodingKey::from_jwk(&jwk).ok()
}

#[cfg(test)]
mod firebase_tests {
    use super::*;

    fn auth(emulator: bool) -> DashboardAuth {
        DashboardAuth { test_mode: false, firebase_project_id: "demo-project".into(), firebase_emulator: emulator }
    }

    /// The crate panics at verify time unless a crypto provider feature is
    /// on, and a panic in a handler is a hung browser rather than a 401. A
    /// garbage token must come back as None, quietly.
    #[test]
    fn a_garbage_token_is_refused_not_panicked() {
        assert!(auth(false).verify_firebase("not.a.token", crate::store::now()).is_none());
        assert!(auth(false).verify_firebase("", crate::store::now()).is_none());
    }

    #[test]
    fn an_emulator_token_is_checked_for_audience_issuer_and_expiry() {
        use base64::Engine;
        let now = crate::store::now();
        let claims = |aud: &str, iss: &str, exp: f64| {
            let payload = serde_json::json!({"aud": aud, "iss": iss, "exp": exp, "sub": "uid-1",
                                             "email": "a@b.c", "name": "A"});
            let seg = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            format!("{}.{}.sig", seg.encode(b"{}"), seg.encode(payload.to_string()))
        };
        let good = claims("demo-project", "https://securetoken.google.com/demo-project", now + 60.0);
        let principal = auth(true).verify_firebase(&good, now).expect("emulator token accepted");
        assert_eq!(principal.uid, "uid-1");
        assert!(auth(true).verify_firebase(&claims("other", "https://securetoken.google.com/demo-project", now + 60.0), now).is_none());
        assert!(auth(true).verify_firebase(&claims("demo-project", "https://securetoken.google.com/other", now + 60.0), now).is_none());
        assert!(auth(true).verify_firebase(&claims("demo-project", "https://securetoken.google.com/demo-project", now - 1.0), now).is_none());
    }
}

/// The identity a dashboard route acts as. `user_id` is the display id —
/// email, falling back to uid — which is what the ledger records.
#[derive(Clone, Debug)]
pub struct AuthUser {
    pub user_id: String,
    pub workspace: String,
    pub uid: String,
    pub workspaces: Vec<String>,
}

/// A refusal a route returns as-is: status and body.
pub type Refusal = (u16, Value);

/// Resolve a principal against one workspace, enforcing membership. A
/// workspace-bound credential pins the workspace; an identity credential must
/// name one. The error strings are the dashboard's, verbatim.
pub fn workspace_auth(
    store: &Store, principal: Option<Principal>, workspace_id: &str,
) -> Result<AuthUser, Refusal> {
    let Some(principal) = principal else {
        return Err((401, serde_json::json!({"error": "unauthorized"})));
    };
    let mut workspace_id = workspace_id.to_string();
    if !principal.workspace.is_empty() {
        if !workspace_id.is_empty() && workspace_id != principal.workspace {
            return Err((403, serde_json::json!({"error": "credential is bound to another workspace"})));
        }
        workspace_id = principal.workspace.clone();
    }
    if workspace_id.is_empty() {
        return Err((400, serde_json::json!({"error": "workspace parameter required"})));
    }
    if member(store, &workspace_id, &principal.uid).is_none() {
        return Err((403, serde_json::json!({"error": "not a member of this workspace"})));
    }
    Ok(AuthUser {
        user_id: principal.label(),
        workspace: workspace_id,
        uid: principal.uid,
        workspaces: principal.workspaces,
    })
}

/// `workspace_auth` plus the per-repo access list — the same rule the MCP
/// tools and the hooks apply. The GitHub "your account was removed from this
/// repo" check belongs to the GitHub integration and is applied where that
/// integration is wired.
pub fn repo_auth(
    store: &Store, principal: Option<Principal>, workspace_id: &str, repo_id: &str,
) -> Result<AuthUser, Refusal> {
    let auth = workspace_auth(store, principal, workspace_id)?;
    let current = member(store, &auth.workspace, &auth.uid);
    if !crate::access::has_repo_access(current.as_ref(), repo_id) {
        return Err((403, serde_json::json!({"error": "no access to this repo in the workspace"})));
    }
    Ok(auth)
}

#[cfg(test)]
mod github_link_tests {
    use super::linked_github_id;
    use serde_json::json;

    #[test]
    fn the_linked_github_account_comes_from_the_id_tokens_identities() {
        let linked = json!({"sub": "u1", "firebase": {"identities": {"github.com": ["583231"], "email": ["a@b.c"]}}});
        assert_eq!(linked_github_id(&linked), "583231");
        let numeric = json!({"firebase": {"identities": {"github.com": [583231]}}});
        assert_eq!(linked_github_id(&numeric), "583231");
        let google_only = json!({"firebase": {"identities": {"google.com": ["1"]}}});
        assert_eq!(linked_github_id(&google_only), "");
        assert_eq!(linked_github_id(&json!({"sub": "u1"})), "");
    }
}
