//! What `setup`, `connect_agents` and `sync_agents_block` hand out: the
//! machine install, the canonical AGENTS.md block, the rules files for
//! agent tools without hooks, and the MCP configs.
//!
//! Cork is installed once per machine (machines.rs): the `cork` program in
//! `~/.cork/bin` (`~/.collide/bin` on a machine installed before the
//! rename), its hooks registered at the user level of
//! each agent tool. Nothing is committed into a repo for it any more. The
//! old per-repo setup (hook scripts and a pinned binary under `.collide/`,
//! wired from the repo's own `.claude/settings.json`) is retired: the hooks
//! report a repo still carrying it (a non-empty `settings_digest`), the
//! server answers with `repo_hooks_retired` (envelope.rs), and the machine
//! install run at the repo's root takes it out and stages the removal.
//!
//! None of these functions edit a file themselves. The AGENTS block text is
//! written into developers' `AGENTS.md`, so a reworded byte is a version
//! bump every repo re-syncs; it is kept verbatim in `block_body.txt`
//! (`{version}` / `{repo_id}` substituted at call time) and checked against
//! `blocks_golden.json`.
//!
//! `setup_tool_response` / `install_hooks_response` / `connect_agents_response`
//! take an already-resolved [`ArtifactRequest`]: workspace routing, scope
//! resolution and the read-only check are the MCP dispatch layer's job.

use serde_json::{json, Map, Value};

use crate::store::{now, Store};

// ------------------------------------------------------------- versioning

// Kept in `envelope.rs` (it already rides these on every `/report`); reused
// here rather than redeclared. HOOK_ARTIFACT_VERSION lives there and in
// `blocks_golden.json`, which the tests below hold it to.
pub use crate::envelope::BLOCK_VERSION;
#[cfg(test)]
use crate::envelope::HOOK_ARTIFACT_VERSION;

pub const START_MARKER: &str = "<!-- cork:start -->";
pub const END_MARKER: &str = "<!-- cork:end -->";
/// The markers blocks were written between before the rename to Cork. A
/// file still carrying them is read as having the block, and re-syncing
/// replaces that old block with the new one (never a second block).
pub const OLD_START_MARKER: &str = "<!-- collide:start -->";
pub const OLD_END_MARKER: &str = "<!-- collide:end -->";

/// Where the block sits in `content`: `(start, end)` with `end` just past
/// the end marker, or `None` for the end when the block is unterminated.
/// The new cork markers are looked for first, then the old collide ones.
fn find_block(content: &str) -> Option<(usize, Option<usize>)> {
    for (open, close) in [(START_MARKER, END_MARKER), (OLD_START_MARKER, OLD_END_MARKER)] {
        if let Some(start) = content.find(open) {
            let end = content[start + open.len()..].find(close).map(|rel| start + open.len() + rel + close.len());
            return Some((start, end));
        }
    }
    None
}

// ---------------------------------------------------------- the block text

/// The un-rendered template, with `{version}` / `{repo_id}` as the two
/// literal tokens this module substitutes.
const BLOCK_BODY_TEMPLATE: &str = include_str!("block_body.txt");

/// The AGENTS block body for `repo_id`.
pub fn block_body(repo_id: &str) -> String {
    BLOCK_BODY_TEMPLATE
        .replace("{version}", &BLOCK_VERSION.to_string())
        .replace("{repo_id}", repo_id)
}

/// The block version an AGENTS.md file ACTUALLY carries, or `None` when the
/// file has no Cork block (under the cork or the old collide markers).
/// Ground truth for the setup ticker — see
/// `envelope::reconcile_setup_version`, which this feeds.
pub fn parse_block_version(content: &str) -> Option<i64> {
    let (start, end) = find_block(content)?;
    let body = &content[start..end.unwrap_or(content.len())];
    let needle = "cross-agent collision awareness (v";
    let digits_start = body.find(needle)? + needle.len();
    let rest = &body[digits_start..];
    let end = rest.find(')')?;
    let digits = &rest[..end];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

pub fn canonical_block(repo_id: &str) -> String {
    format!("{START_MARKER}\n{}\n{END_MARKER}", block_body(repo_id))
}

/// The mandatory protocol written into every agent's native rules file.
/// Shared files (AGENTS.md, windsurf/copilot) carry the marker-wrapped
/// block so re-syncing never clobbers a teammate's own instructions;
/// dedicated files are self-contained.
pub fn agent_rules_files(repo_id: &str) -> Value {
    let body = block_body(repo_id);
    let block = canonical_block(repo_id);
    let cursor_mdc = format!(
        "---\ndescription: Cork cross-agent collision protocol (required)\nalwaysApply: true\n---\n\n{body}\n"
    );
    let clinerules = format!("{body}\n");
    let mut map = Map::new();
    map.insert("AGENTS.md".into(), json!(block));
    map.insert(".cursor/rules/collide.mdc".into(), json!(cursor_mdc));
    map.insert(".clinerules/collide.md".into(), json!(clinerules));
    map.insert(".windsurfrules".into(), json!(block));
    map.insert(".github/copilot-instructions.md".into(), json!(block));
    Value::Object(map)
}

/// The URL agents connect to. Canonical when the server names one
/// (CORK_MCP_URL / COLLIDE_MCP_URL); else the classic `{server}/mcp`; else a placeholder.
pub fn mcp_endpoint(server_url: &str, mcp_url: &str) -> String {
    if !mcp_url.is_empty() {
        return mcp_url.trim_end_matches('/').to_string();
    }
    if server_url.is_empty() {
        "https://<cork-server>/mcp".to_string()
    } else {
        format!("{}/mcp", server_url.trim_end_matches('/'))
    }
}

/// Repo-committable MCP configs so each client discovers the server.
pub fn agent_mcp_files(server_url: &str, mcp_url: &str) -> Value {
    let cfg = dumps_with_newline(&mcp_json(server_url, mcp_url));
    json!({".mcp.json": cfg.clone(), ".cursor/mcp.json": cfg})
}

/// Config that lives outside the repo (user-global) or is a shell command,
/// returned as copy-paste snippets rather than committable files. The hooks
/// of every agent tool that has them come from the machine install.
pub fn agent_snippets(server_url: &str, repo_id: &str, mcp_url: &str) -> Value {
    let url = mcp_endpoint(server_url, mcp_url);
    let stripped = server_url.trim_end_matches('/');
    let srv = if stripped.is_empty() { "https://<cork-server>" } else { stripped };
    let codex = format!(
        "Codex reads AGENTS.md for the protocol. Its report and gate hooks come from the machine \
install (setup's install.command writes them into ~/.codex/hooks.json for every repo), so it gets \
real per-edit blocking, not just advisory. Codex pins hook trust to a hash of the hook definition \
and does not reload it mid-session: after the install, start a new Codex session and approve \
Cork's hooks in /hooks. For the MCP tools, add to ~/.codex/config.toml (in place of an \
[mcp_servers.collide] table, if one is there):\n\
[mcp_servers.cork]\nurl = \"{url}\"\n"
    );
    let cursor = format!(
        "Cursor reads .cursor/rules/collide.mdc for the protocol. Its report and gate hooks come from \
the machine install (~/.cursor/hooks.json, for every repo), so it gets real per-edit blocking too. Its \
preToolUse answer carries allow/deny/ask, and its postToolUse carries the briefing notes back into \
the agent. There is no batch event, so the repo's own check is spawned after a write and its \
verdict rides the next tool result. MCP tools come from the committed .cursor/mcp.json: \
{{\"mcpServers\": {{\"cork\": {{\"type\": \"http\", \"url\": \"{url}\"}}}}}}\n"
    );
    let hermes = format!(
        "Hermes runs Cork through a plugin that the machine install writes under \
~/.hermes/plugins and enables: it hands Hermes's pre_llm_call, pre_tool_call and \
post_tool_call to the same cork program the other agents' hooks run (cork \
--harness hermes), and delivers teammates' messages into the open conversation. Gating and \
reporting both work. Hermes discards a post_tool_call return value by design, so notes that would \
ride a tool result arrive with the next turn's briefing instead. MCP tools: \
{{\"mcpServers\": {{\"cork\": {{\"type\": \"http\", \"url\": \"{url}\"}}}}}}\n"
    );
    let cline_windsurf = format!(
        "Add the server to your Cline/Windsurf MCP settings (VS Code):\n\
{{\"mcpServers\": {{\"cork\": {{\"type\": \"http\", \"url\": \"{url}\"}}}}}}\n"
    );
    let watcher = format!(
        "Enforcement floor for ANY agent — reports every edit from disk, no hooks needed:\n\
pipx install collide-watch   # or: pip install collide-watch\n\
collide-watch --repo-id \"{repo_id}\" --server \"{srv}\" --token \"$CORK_TOKEN\"\n"
    );
    json!({"codex": codex, "cursor": cursor, "hermes": hermes, "cline_windsurf": cline_windsurf, "watcher": watcher})
}

/// Merge target for the repo's `.mcp.json`.
pub fn mcp_json(server_url: &str, mcp_url: &str) -> Value {
    json!({"mcpServers": {"cork": {"type": "http", "url": mcp_endpoint(server_url, mcp_url)}}})
}

/// Return `(new_content, markers_were_present)`. Only the region between
/// the markers (cork, or the old collide ones, which the new block
/// replaces) is replaced; everything outside is byte-identical.
/// Absent markers -> the wrapped block is appended.
pub fn sync_agents_content(file_content: &str, repo_id: &str) -> (String, bool) {
    let block = canonical_block(repo_id);
    match find_block(file_content) {
        Some((start, Some(end))) => {
            let prefix = &file_content[..start];
            let suffix = &file_content[end..];
            (format!("{prefix}{block}{suffix}"), true)
        }
        _ => {
            let mut prefix = file_content.to_string();
            if !prefix.is_empty() && !prefix.ends_with('\n') {
                prefix.push('\n');
            }
            let sep = if prefix.is_empty() { "" } else { "\n" };
            (format!("{prefix}{sep}{block}\n"), false)
        }
    }
}

/// Pretty JSON (2-space indent, `": "`) plus a trailing newline, for
/// artifact content that is genuinely written to a file
/// (`~/.cork/credentials.json`, `.mcp.json`, `.cursor/mcp.json`). This
/// crate's `Map` preserves insertion order (`serde_json` pulls in `indexmap`
/// here), so keys come out in the order the `Value` was built.
pub(crate) fn dumps_with_newline(value: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into()))
}

// ---------------------------------------------------- sync_agents_block() tool

/// The `sync_agents_block` tool. Text transform plus the ticker
/// flip: calling this IS the setup act, so the scope is marked applied at
/// the current version. `verified=true` when the content was already
/// canonical (the one physical check, performed once).
pub fn sync_agents_block(store: &Store, scope: &str, user_id: &str, repo_id: &str, file_content: &str) -> Value {
    let (content, markers_found) = sync_agents_content(file_content, repo_id);
    let changed = content != file_content;
    let stamp = now();
    let state = json!({
        "applied_version": BLOCK_VERSION,
        "applied_by": user_id,
        "verified": !changed,
        "ts": stamp,
    });
    let _ = store.kv_put("setup_state", scope, &state, stamp);
    let _ = store.ledger_append(
        scope,
        "setup_applied",
        &json!({"user": user_id, "version": BLOCK_VERSION, "verified": !changed}),
        stamp,
    );
    json!({"content": content, "markers_found": markers_found, "changed": changed})
}

// --------------------------------------------------- hook credential minting

/// Mirrors `auth::token_key` (SHA256 hex of the raw token — tokens are
/// stored hashed so a DB leak never leaks a usable credential). Duplicated
/// rather than made `pub` on `auth.rs`: that module is being actively
/// extended by a concurrent port (OAuth 2.1 flow), and this is three lines
/// of the same standard algorithm, not a design decision to share.
fn token_key(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// `n` bytes of OS-sourced randomness with no extra dependency, built the
/// same way `compat::new_id` gets its uniqueness: `RandomState::new()`
/// draws fresh SipHash keys from the OS RNG on every construction (not
/// cached), so hashing a counter under a freshly-constructed `RandomState`
/// each round is an OS-randomness extractor, not a clock-seeded PRNG.
pub(crate) fn random_bytes(n: usize) -> Vec<u8> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = Vec::with_capacity(n + 8);
    let mut counter: u64 = 0;
    while bytes.len() < n {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(counter);
        counter += 1;
        bytes.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes.truncate(n);
    bytes
}

/// 32 random bytes, base64url without padding, behind `ACCESS_TOKEN_PREFIX`.
fn mint_raw_token() -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    format!("{}{}", crate::auth::ACCESS_TOKEN_PREFIX, URL_SAFE_NO_PAD.encode(random_bytes(32)))
}

/// The identity a minted hook credential is bound to.
pub struct HookIdentity<'a> {
    /// `auth.uid` — becomes the token record's `"uid"`.
    pub auth_uid: &'a str,
    /// `principal.email`.
    pub email: &'a str,
    /// `principal.name`.
    pub name: &'a str,
    /// `workspace_for(auth, repo_id)` — the repo's ROUTED workspace, which
    /// may differ from the caller's own default workspace.
    pub workspace: &'a str,
    /// The workspaces the credential it is minted from may touch (its
    /// allowlist; empty: all of the person's). Carried over, so a machine
    /// credential never reaches more than the token that asked for it.
    pub workspaces: &'a [String],
}

/// A fresh 90-day workspace-bound hook token, shaped as the
/// `~/.cork/credentials.json` artifact, and written into `TOKEN_BUCKET`
/// in the shape `auth::principal_for_bearer` already reads (`kind`,
/// `expires`, `uid`, `email`, `workspace`). `client_id` distinguishes
/// `setup`/`install_hooks` (`"collide-hook"`) from `connect_agents`
/// (`"collide-connect"`).
pub fn mint_hook_credential(store: &Store, identity: &HookIdentity, client_id: &str, public_url: &str) -> Value {
    let created = now();
    let token = mint_raw_token();
    let mut record = json!({
        "uid": identity.auth_uid, "email": identity.email, "name": identity.name,
        "workspace": identity.workspace, "client_id": client_id,
        "created": created, "kind": "access", "expires": created + 90.0 * 86_400.0,
    });
    if !identity.workspaces.is_empty() {
        record["workspaces"] = json!(identity.workspaces);
    }
    let _ = store.kv_put(crate::auth::TOKEN_BUCKET, &token_key(&token), &record, created);

    let mut credentials = Map::new();
    credentials.insert("token".into(), json!(token));
    let server_url = public_url.trim_end_matches('/');
    if !server_url.is_empty() {
        credentials.insert(server_url.to_string(), json!(token));
        credentials.insert(format!("{server_url}#{}", identity.workspace), json!(token));
    }
    Value::Object(credentials)
}

// ------------------------------------------------------- full tool responses

/// Everything `setup_tool_response` / `install_hooks_response` /
/// `connect_agents_response` need, already resolved by the caller — see the
/// module doc's scope boundary. The MCP layer still owns auth, workspace
/// routing, and (for install_hooks/connect_agents) the read-only refusal.
pub struct ArtifactRequest<'a> {
    pub repo_id: &'a str,
    pub public_url: &'a str,
    pub mcp_url: &'a str,
    /// `workspace_for(auth, repo_id)`.
    pub workspace: &'a str,
    /// `auth.uid`.
    pub auth_uid: &'a str,
    /// `principal.email`.
    pub email: &'a str,
    /// `principal.name`.
    pub name: &'a str,
    /// The caller's credential's allowlist (`Caller.workspaces`), carried
    /// into the machine credential `setup` leads to.
    pub workspaces: &'a [String],
}

/// What a viewer's agent is told: its agents get nothing from Cork, so
/// there is nothing for it to install.
pub const VIEWER_SETUP: &str = "read-only access: a viewer's agents get nothing from Cork, so there \
is nothing to install on this machine. Ask the workspace owner for write access, then call setup again.";

/// What setup says while this server has no build of the program to hand
/// out (a release on its way, or a server that never syncs releases).
pub const NO_BUILD_SETUP: &str = "this Cork server has no build of Cork to install right now \
(a release may still be on its way): call setup again in a few minutes. Until then call get_briefing \
before you change anything and report_edit after each write.";

/// The `setup` MCP tool: Cork installed once for the whole machine, linked
/// to the caller's account by a single-use code (`machines::offer_free`),
/// never as files in the repo. Run at a repo's root, the install also
/// retires that repo's old per-repo setup and stages the removal. `mode` is
/// accepted for older callers and ignored: the machine's hooks always gate.
pub fn setup_tool_response(store: &Store, _mode: &str, can_write: bool, req: &ArtifactRequest) -> Value {
    if !can_write {
        return json!({"ok": false, "error": VIEWER_SETUP});
    }
    crate::machines::offer_free(store, req.public_url, req.workspace, req.workspaces, req.auth_uid, req.email, req.name)
        .unwrap_or_else(|| json!({"ok": false, "error": NO_BUILD_SETUP}))
}

/// The hidden `install_hooks` tool: what `setup` answers (there is one way
/// to install the hooks now). Errs with the read-only message when the
/// caller cannot record, which MCP dispatch surfaces as a refusal.
pub fn install_hooks_response(store: &Store, can_write: bool, req: &ArtifactRequest) -> Result<Value, String> {
    if !can_write {
        return Err("read-only access: the hook reports edits; ask the workspace owner for write access".into());
    }
    Ok(setup_tool_response(store, "", can_write, req))
}

/// `connect_agents` with mode "files": the protocol written into each agent
/// tool's own rules file and the MCP configs, for tools the machine install
/// does not cover (Windsurf, Cline, GitHub Copilot). Hooks are never among
/// them: every agent tool that has hooks gets them from the machine install
/// (`setup_tool_response`, which `connect_agents` answers without a mode).
/// `repo_files` order: `agent_rules_files` (5 entries), then
/// `agent_mcp_files` (2 entries).
pub fn connect_agents_response(can_write: bool, req: &ArtifactRequest) -> Result<Value, String> {
    if !can_write {
        return Err(
            "read-only access: connecting agents is for members who record; ask the owner for write access".into(),
        );
    }
    let mut repo_files = Map::new();
    if let Value::Object(rules) = agent_rules_files(req.repo_id) {
        repo_files.extend(rules);
    }
    if let Value::Object(mcp_files) = agent_mcp_files(req.public_url, req.mcp_url) {
        repo_files.extend(mcp_files);
    }
    Ok(json!({
        "repo_files": Value::Object(repo_files),
        "snippets": agent_snippets(req.public_url, req.repo_id, req.mcp_url),
        "notes": [
            "MERGE the marker-wrapped files (AGENTS.md, .windsurfrules, .github/copilot-instructions.md) \
so you never clobber existing instructions — use sync_agents_block, or paste the block \
between its <!-- cork:start/end --> markers (it replaces an old block between \
<!-- collide:start/end -->). Write the dedicated files \
(.cursor/rules/collide.mdc, .clinerules/collide.md, .mcp.json, .cursor/mcp.json) as-is. \
Commit all of them so every teammate's agent inherits the protocol.",
            "These are the protocol and the MCP configs only. The hooks (Claude Code, Codex, Cursor, \
Hermes) come from the machine install: call setup and run the one command it returns.",
            "snippets.codex and snippets.cline_windsurf are user-global MCP config, not repo files.",
            "snippets.watcher is the enforcement floor for any agent without hooks: it reports every \
edit from disk (source: disk).",
            "The server's own MCP instructions push the protocol to any connected MCP agent, so tools \
beyond this list are covered the moment they connect.",
        ],
    }))
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden values for the text Cork writes into developers' files: the
    /// AGENTS block, the rules files, the MCP configs and the snippets.
    fn golden() -> Value {
        serde_json::from_str(include_str!("blocks_golden.json")).expect("golden fixture is valid JSON")
    }

    #[test]
    fn versions_match_envelope() {
        assert_eq!(BLOCK_VERSION, golden()["BLOCK_VERSION"].as_i64().unwrap());
        assert_eq!(HOOK_ARTIFACT_VERSION, golden()["HOOK_ARTIFACT_VERSION"].as_i64().unwrap());
    }

    #[test]
    fn canonical_block_and_parse_round_trip() {
        let g = golden();
        let block = canonical_block("acme/repo");
        assert_eq!(block, g["canonical_block"].as_str().unwrap());
        assert_eq!(parse_block_version(&block), Some(BLOCK_VERSION));
        assert_eq!(parse_block_version("no markers"), None);
    }

    #[test]
    fn agent_rules_files_match() {
        assert_eq!(agent_rules_files("acme/repo"), golden()["agent_rules_files"]);
    }

    #[test]
    fn agent_mcp_files_and_snippets_match() {
        let g = golden();
        assert_eq!(agent_mcp_files("https://x.test", ""), g["agent_mcp_files"]);
        assert_eq!(agent_snippets("https://x.test", "acme/repo", ""), g["agent_snippets"]);
    }

    /// Every agent tool with hooks gets them from the machine install: no
    /// snippet points at a script or a hook file in the repo.
    #[test]
    fn no_snippet_points_at_the_retired_per_repo_hooks() {
        let snippets = agent_snippets("https://x.test", "acme/repo", "").to_string();
        for retired in [".collide/report_hook.py", ".collide/gate_hook.py", "python3 .collide", "committed by connect_agents"] {
            assert!(!snippets.contains(retired), "{retired}");
        }
        assert!(snippets.contains("~/.hermes/plugins") && snippets.contains("cork --harness hermes"));
    }

    #[test]
    fn mcp_endpoint_matches() {
        let g = golden();
        assert_eq!(mcp_endpoint("https://x.test/", ""), g["mcp_endpoint"].as_str().unwrap());
        assert_eq!(mcp_endpoint("", "https://mcp.x.test/mcp/"), g["mcp_endpoint_mcpurl"].as_str().unwrap());
        assert_eq!(mcp_endpoint("", ""), g["mcp_endpoint_default"].as_str().unwrap());
    }

    #[test]
    fn mcp_json_matches() {
        let g = golden();
        assert_eq!(mcp_json("https://x.test", ""), g["mcp_json"]);
        assert_eq!(mcp_json("", "https://mcp.x.test/mcp"), g["mcp_json_mcpurl"]);
        assert_eq!(mcp_json("", ""), g["mcp_json_default"]);
    }

    #[test]
    fn sync_agents_content_matches() {
        let g = golden();
        let (content, markers) = sync_agents_content("hello world\nno markers here\n", "acme/repo");
        assert_eq!(content, g["sync_agents_content_absent"]["content"].as_str().unwrap());
        assert_eq!(markers, g["sync_agents_content_absent"]["markers_found"].as_bool().unwrap());

        let existing = format!("before\n{}\nafter\n", canonical_block("old/repo"));
        let (content, markers) = sync_agents_content(&existing, "acme/repo");
        assert_eq!(content, g["sync_agents_content_present"]["content"].as_str().unwrap());
        assert_eq!(markers, g["sync_agents_content_present"]["markers_found"].as_bool().unwrap());
    }

    fn request<'a>(public_url: &'a str) -> ArtifactRequest<'a> {
        ArtifactRequest {
            repo_id: "github.com/acme/repo", public_url, mcp_url: "", workspace: "w1", auth_uid: "alice",
            email: "alice@example.com", name: "Alice", workspaces: &[],
        }
    }

    /// Setup never hands out files for the repo: a viewer is told its agents
    /// get nothing, and a server with no build says so instead of falling
    /// back to the retired per-repo files.
    #[test]
    fn setup_is_the_machine_install_or_an_honest_no() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let viewer = setup_tool_response(&db, "enforce", false, &request("https://x.test"));
        assert_eq!(viewer, json!({"ok": false, "error": VIEWER_SETUP}));
        // no public URL: nothing a machine could be linked to
        let none = setup_tool_response(&db, "files", true, &request(""));
        assert_eq!(none, json!({"ok": false, "error": NO_BUILD_SETUP}));
        for answer in [&viewer, &none] {
            let text = answer.to_string();
            assert!(!text.contains("repo_files") && !text.contains(".collide/"), "{text}");
        }
        assert!(install_hooks_response(&db, false, &request("https://x.test")).is_err());
    }

    #[test]
    fn connect_agents_files_are_rules_and_mcp_configs_only() {
        let answer = connect_agents_response(true, &request("https://x.test")).unwrap();
        let files: Vec<&String> = answer["repo_files"].as_object().unwrap().keys().collect();
        assert_eq!(files, [
            "AGENTS.md", ".cursor/rules/collide.mdc", ".clinerules/collide.md", ".windsurfrules",
            ".github/copilot-instructions.md", ".mcp.json", ".cursor/mcp.json",
        ]);
        assert!(answer.get("user_files").is_none(), "no credential: nothing here reports");
        assert!(connect_agents_response(false, &request("https://x.test")).is_err());
    }

    /// A file synced before the rename carries the block between the old
    /// collide markers: it still counts as having the block, and a re-sync
    /// swaps it for the new one in place (one block, never two).
    #[test]
    fn an_old_collide_block_is_read_and_replaced() {
        let old = canonical_block("old/repo")
            .replace(START_MARKER, OLD_START_MARKER)
            .replace(END_MARKER, OLD_END_MARKER)
            .replace("(v27)", "(v26)");
        let existing = format!("before\n{old}\nafter\n");
        assert_eq!(parse_block_version(&existing), Some(26));
        let (content, markers) = sync_agents_content(&existing, "acme/repo");
        assert!(markers);
        assert_eq!(content, format!("before\n{}\nafter\n", canonical_block("acme/repo")));
        assert!(!content.contains("collide:start") && content.matches(START_MARKER).count() == 1);
    }

    #[test]
    fn mint_raw_token_is_unique_and_shaped() {
        // no golden equivalent (fresh secret material every call) — just the
        // invariants a hook credential needs: prefixed, non-empty, distinct.
        let a = mint_raw_token();
        let b = mint_raw_token();
        assert!(a.starts_with(crate::auth::ACCESS_TOKEN_PREFIX));
        assert_ne!(a, b);
        assert!(a.len() > crate::auth::ACCESS_TOKEN_PREFIX.len() + 32);
    }
}
