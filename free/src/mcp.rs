//! MCP over Streamable HTTP: the surface agents actually connect to.
//!
//! Every other module here answers a plain JSON POST, which is what the
//! hooks call. An agent speaks MCP, and this is where it connects.
//!
//! The protocol itself is the official SDK's job. What lives here is the part
//! that is Cork's: which tools exist, what they are called, the text an
//! agent reads to decide when to call them, who is allowed to see which, and
//! the dispatch into the engine.
//!
//! One rule governs the tool table below: a description is not
//! documentation here — it is the input an agent uses to choose a tool, so a
//! reworded one is a different tool. Change the text only on purpose.

use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use serde_json::{json, Map, Value};

use crate::App;

const ACTIVITY_URI_PREFIX: &str = "cork://activity/";
/// The prefix the resource was listed under before the rename; still read.
const OLD_ACTIVITY_URI_PREFIX: &str = "collide://activity/";

/// How long a client may keep a list result before asking again. Clients on
/// the 2026-07-28 revision (Claude Code 2.1.267 among them) REJECT a list that
/// omits its cache directive — `ttlMs` and `cacheScope` became required there,
/// and a rejected tools/list is a server with no tools. So every list this
/// server returns carries one. Private, because the menu depends on who is
/// asking: a read-only member does not see the write tools.
const LIST_TTL_MS: u64 = 300_000;

/// Tools that change state. A read-only member sees the rest of the menu and
/// not these: showing an agent a tool it will be refused teaches it to ignore
/// refusals. Filtering is MENU ONLY; call-level authorisation is still the
/// gate, because a menu is a hint and a gate is a guarantee.
const WRITE_TOOLS: [&str; 8] = [
    "report_edit", "declare_intent", "heartbeat", "complete_intent",
    "defer_intent", "remember", "send_agent_message", "claim",
];
/// Hidden once this user has produced hook traffic: the hooks report and
/// gate every write and `apply` declares and completes. Menu only — every
/// call still works — but a menu an agent never needs is 5K tokens on every
/// message. The setup tools are NOT here: hook traffic is per account, setup
/// is per machine, and the server cannot tell which machine is asking — an
/// account with hooks on one laptop was told "call setup" on a new one by
/// the briefing while the menu hid `setup`, and the agent was stuck.
const CEREMONY_TOOLS: [&str; 6] = [
    "report_edit", "check_collisions", "heartbeat", "declare_intent", "complete_intent",
    "sync_agents_block",
];

/// All an unclaimed anonymous agent may call (`crate::authorize_sandbox`):
/// the tools that read and write its own sandbox repo and nothing beyond
/// it. Not `setup` or `connect_agents` (they mint a machine credential and
/// would put hooks on real repos), not `send_agent_message` (it reaches
/// people in any account), and none of the hidden ones.
pub const SANDBOX_TOOLS: [&str; 16] = [
    "check_collisions", "report_edit", "declare_intent", "heartbeat", "complete_intent", "defer_intent",
    "get_briefing", "remember", "recall", "get_symbol", "blast_radius", "recap", "claim", "inbox_ack",
    "pending_reconciliations", "explain_code",
];

/// A sandbox agent's refusal for every other tool.
const SANDBOX_TOOL_LINE: &str = "This is a sandbox registration: it can use the tools that read and write its own \
    sandbox/ repos, and not this one. Claim it (https://corkcli.com/auth.md) to use Cork in full.";

/// Off the menu, still answered. Each of these is covered by the briefing,
/// by another tool, or by a dashboard page; listing them cost every client
/// that loads full schemas ~2K tokens a message and made the right tool
/// harder to pick. A call by name (an older AGENTS.md, a script) still works.
pub const HIDDEN_TOOLS: [&str; 16] = ["plan_work", "differential_check", "simulate_merge", "blind_spots", "graph_neighbors", "graph_path", "repo_map", "list_activity", "compliance_report", "graph_export", "graph_import", "open_dashboard", "move_repo", "remove_repo", "switch_workspace", "install_hooks"];

/// What each listed tool does to the world, for clients that decide by it.
/// Every tool states all three of read-only, destructive and open-world:
/// ChatGPT's app review requires each one explicitly, even where the spec
/// would let destructive go unsaid for a read-only tool.
pub fn tool_annotations(name: &str) -> Option<ToolAnnotations> {
    match name {
        "get_briefing" => Some(ToolAnnotations::new().read_only(true).destructive(false).open_world(false)),
        "get_symbol" => Some(ToolAnnotations::new().read_only(true).destructive(false).open_world(false)),
        "blast_radius" => Some(ToolAnnotations::new().read_only(true).destructive(false).open_world(false)),
        "explain_code" => Some(ToolAnnotations::new().read_only(true).destructive(false).open_world(false)),
        "recap" => Some(ToolAnnotations::new().read_only(true).destructive(false).open_world(false)),
        "recall" => Some(ToolAnnotations::new().read_only(true).destructive(false).open_world(false)),
        "remember" => Some(ToolAnnotations::new().read_only(false).destructive(true).open_world(true)),
        // open world: a connected CRM gets the task too (crm::sync_deferred)
        "defer_intent" => Some(ToolAnnotations::new().read_only(false).destructive(false).open_world(true)),
        "send_agent_message" => Some(ToolAnnotations::new().read_only(false).destructive(false).open_world(true)),
        "inbox_ack" => Some(ToolAnnotations::new().read_only(false).destructive(false).idempotent(true).open_world(false)),
        "claim" => Some(ToolAnnotations::new().read_only(false).destructive(false).open_world(true)),
        "pending_reconciliations" => Some(ToolAnnotations::new().read_only(false).destructive(false).open_world(true)),
        "setup" => Some(ToolAnnotations::new().read_only(false).destructive(false).open_world(false)),
        "connect_agents" => Some(ToolAnnotations::new().read_only(false).destructive(false).open_world(false)),
        "check_collisions" => Some(ToolAnnotations::new().read_only(true).destructive(false).open_world(false)),
        "report_edit" => Some(ToolAnnotations::new().read_only(false).destructive(false).open_world(true)),
        "declare_intent" => Some(ToolAnnotations::new().read_only(false).destructive(false).open_world(true)),
        "heartbeat" => Some(ToolAnnotations::new().read_only(false).destructive(false).idempotent(true).open_world(true)),
        "complete_intent" => Some(ToolAnnotations::new().read_only(false).destructive(false).open_world(true)),
        // it records the repo's setup state, so it is not read-only
        "sync_agents_block" => Some(ToolAnnotations::new().read_only(false).destructive(false).idempotent(true).open_world(false)),
        _ => None,
    }
}

/// The tool menu: name, description and input schema. The description is
/// the text an agent reads to decide WHEN to call something; a retyped
/// description is a different tool.
pub fn tool_table() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("check_collisions",
         r#"The main call. Empty list means no collisions (root fast path). Otherwise: symbol, kind (renamed | modified | removed | added | unknown), detail, confidence (certain | likely | unconfirmed | unknown), author, freshness (live | stale + age), matching intents. Exact path and symbol matching only."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "paths": {"type": "array", "items": {"type": "string"}, "description": "Files you are about to write."}, "symbols_referenced": {"type": "array", "items": {"type": "string"}, "description": "Symbols your change calls or depends on."}}, "required": ["repo_id"]})),
        ("report_edit",
         r#"Submit a file you wrote. The server parses in memory, updates your workspace tree, and discards the source; returns parse confidence and your new root. draft=true: presence only, no tree advance. expected_hashes ({symbol: hash | null}) makes the write compare-and-swap: pass the hashes you read; if any moved, the write is REJECTED with the current state. Not needed for files you edit with your editor tools when hooks are live."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "path": {"type": "string", "description": "Repo-relative path of the file you wrote."}, "content": {"type": "string", "description": "The file's full new content."}, "idempotency_key": {"type": "string", "description": "Any string; a retry with the same key is applied once."}, "draft": {"type": "boolean", "description": "true: broadcast what is in motion without recording the edit."}, "expected_hashes": {"type": "object", "description": "{symbol: hash you read, or null}: reject the write if any moved."}, "why": {"type": "string", "description": "One line, at most 200 characters: why this change. With content, it rides on the edit; alone (no content), it is the rationale for the edit the hook already recorded from disk."}}, "required": ["repo_id", "path"]})),
        ("declare_intent",
         r#"Register an in-flight change. PREFER typed `operations`: {op:"rename", symbol, new_name} | {op:"add_param", symbol, name, type?, default?} | {op:"remove_param", symbol, name} | {op:"change_return", symbol, type} | {op:"extract", symbol, new_name} | {op:"move", symbol, new_path} | {op:"delete", symbol} | {op:"add", symbol} | {op:"modify", symbol}. Typed intents get conflict prediction against other live intents (intent_conflicts: COMMUTE / CONFLICT / UNKNOWN). ref links an issue or PR. TTL 15 minutes; extend with heartbeat. With the native hook installed, `cork apply` (JSON op or list on stdin) performs rename / add_param / remove_param / pass_arg on this checkout — definition and every call site — verified with the repo's check, declared as one intent, reported, completed."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "paths": {"type": "array", "items": {"type": "string"}, "description": "Files the change will touch."}, "symbols": {"type": "array", "items": {"type": "string"}, "description": "Symbols the change will touch."}, "change_type": {"type": "string", "description": "Legacy prose kind of change; prefer operations."}, "before": {"type": "string", "description": "The signature or name before the change."}, "after": {"type": "string", "description": "The signature or name after it."}, "summary": {"type": "string", "description": "One line on what you are about to do."}, "idempotency_key": {"type": "string", "description": "Any string; a retry with the same key declares once."}, "operations": {"type": "array", "items": {"type": "object"}, "description": "Typed operations, e.g. {op: \"rename\", symbol, new_name}."}, "ref": {"type": "string", "description": "An issue or pull request this work belongs to."}}, "required": ["repo_id", "paths"]})),
        ("heartbeat",
         r#"Extend an active intent's TTL. The response carries since_your_last_call — cross-agent events since your previous call — so heartbeating regularly keeps you aware without polling."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "intent_id": {"type": "string", "description": "The id declare_intent returned."}}, "required": ["repo_id", "intent_id"]})),
        ("complete_intent",
         r#"Mark an intent completed (otherwise it expires to abandoned). ALWAYS pass rationale for non-trivial changes: why this change was made and what was rejected. It's anchored to the changed symbols and fires as a blocking-level notice if anyone later declares an intent that would reverse this one (rename back, re-add a deleted symbol) — settled decisions defend themselves."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "intent_id": {"type": "string", "description": "The id declare_intent returned."}, "rationale": {"type": "string", "description": "Why this change was made and what was rejected."}}, "required": ["repo_id", "intent_id"]})),
        ("defer_intent",
         r#"Leave work unfinished safely: a tripwire on paths or symbols that warns whoever next edits them (for example "webhooks in payments/ are half-done; finish them before changing this"). It fires once into their next tool result and briefing, and becomes a task in the team's CRM when one is connected. Returns the tripwire id and expiry."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "note": {"type": "string", "description": "What is unfinished and what the next person should do."}, "paths": {"type": "array", "items": {"type": "string"}, "description": "Files whose next edit triggers the warning."}, "symbols": {"type": "array", "items": {"type": "string"}, "description": "Symbol names whose next edit triggers it."}, "expires_days": {"type": "number", "description": "Days until the tripwire expires (default 14)."}}, "required": ["repo_id", "note"]})),
        ("get_briefing",
         r#"Only when no `Cork briefing` is in your context (Cork's hooks are not installed): the recent history of this repo — who changed what, renames to adapt to, hotspots, abandoned work and who is working right now. Where the hooks run, the same briefing already arrived with your prompt; do not call this again for it. Pass path (and symbol) to also get the blast radius of what you are about to change. Read-only. Returns summary lines, changed modules with exact signatures, anchored notes and live agents."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "days": {"type": "number", "description": "How far back to look, in days (default 7)."}, "path": {"type": "string", "description": "Optional file to also compute the blast radius for."}, "symbol": {"type": "string", "description": "Optional symbol in path to narrow that blast radius."}, "depth": {"type": "integer", "description": "Hops of dependents to include with path (default 3)."}}, "required": ["repo_id"]})),
        ("remember",
         r#"Save a durable team fact: a decision, constraint or gotcha, not code state. Anchor facts about specific code (anchor="auth.py::validate_token", "auth.py" or "src/auth/"): anchored notes reach whoever touches that code and are marked stale when it is rewritten. When the human states a standing decision ("remove X", "never Y", "only Z"), save it with kind="decision" anchored to the code it covers: it never goes stale, and each agent's first write there is held once with it. Writes the note(s) and returns their ids."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "fact": {"type": "string", "description": "The note, one or two sentences."}, "anchor": {"type": "string", "description": "Where it applies: path::symbol, a file, or dir/."}, "tags": {"type": "array", "items": {"type": "string"}, "description": "Labels to find it by with recall."}, "supersedes": {"type": "string", "description": "Id of a note or decision this one replaces; the old one is retired. kind=\"decision\" with no fact withdraws that decision."}, "kind": {"type": "string", "enum": ["note", "decision"], "description": "note (default), or decision: the owner's standing decision; needs an anchor."}, "facts": {"type": "array", "items": {"type": "object"}, "description": "Several notes at once: [{fact, anchor?, tags?, kind?}]. Decision items with the same fact are one decision over all their anchors."}}, "required": ["repo_id"]})),
        ("recall",
         r#"Search the team's saved notes (made with remember) by text and tags, newest first; an empty query returns the most recent. The hook briefing already carries the notes anchored to the code you touch: use this only to search beyond them, before re-deciding something the team may already have settled. Read-only. Returns each note with its anchor, confidence and whether it may be stale."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "query": {"type": "string", "description": "Text to find in the note; empty returns the most recent."}, "tags": {"type": "array", "items": {"type": "string"}, "description": "Only notes carrying all of these tags."}, "limit": {"type": "integer", "description": "Maximum notes to return (default 25)."}}, "required": ["repo_id"]})),
        ("simulate_merge",
         "Counterfactual dry-run: if the given workspaces (default: all) landed together right now, which symbols would disagree? Reports only same-symbol divergence (signature or implementation), before any real merge exists.",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "users": {"type": "array", "items": {"type": "string"}}},
                "required": ["repo_id"]})),
        ("differential_check",
         "Narrow differential verification: does YOUR workspace preserve the interface assumptions OTHER_USER's code relies on (and vice versa)? Interface oracle only — signature preservation over the parsed reference graph. Returns PASS / FAIL / UNKNOWN with findings, and states exactly what was and was not checked; never claims beyond the oracle (no typecheck, no tests — the server retains no source).",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "other_user": {"type": "string"}},
                "required": ["repo_id", "other_user"]})),
        ("blind_spots",
         "Metacognition: where Cork's map is unreliable — files never cleanly parsed, files serving stale state, workspaces gone quiet. Treat answers about these as unverified.",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}},
                "required": ["repo_id"]})),
        ("get_symbol",
         r#"Exact facts about code without opening the file: the signature, parameters, calls, span and hash of one symbol, or, omitting symbol, of every symbol in the module, with the notes anchored to it and whether the file passed the repo's check. Use it when a briefing lacks a fact; use blast_radius for who depends on it. Read-only."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "path": {"type": "string", "description": "Repo-relative file path, e.g. src/auth.py."}, "symbol": {"type": "string", "description": "Function, class or method name; omit for the whole module."}}, "required": ["repo_id", "path"]})),
        ("blast_radius",
         r#"Before renaming, changing a signature or deleting: every symbol that depends on this one, level by level, with each dependent's file, owner and any in-flight work over it. Answers "what breaks if I change this" from the live code graph; skip it when the briefing's Scope line says every dependent is listed. Read-only."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "path": {"type": "string", "description": "Repo-relative file path of the symbol."}, "symbol": {"type": "string", "description": "The symbol whose dependents to list; omit for the whole file."}, "depth": {"type": "integer", "description": "How many hops of dependents to follow, 1 to 6 (default 3)."}}, "required": ["repo_id", "path"]})),
        ("plan_work",
         "BEFORE dispatching agents, and the live gate while they run. Collapse a list of work units — {path, symbol, op?} each — into the fewest independent groups, then check every group against what is open right now: a group nobody else has a claim on is run_now; one that overlaps another agent's intent, a hot file, or whose typed ops cannot be proven to commute is hold, with who blocks it. dispatch=true claims each run_now group as an intent (pass its intent_id to the agent you spawn, to heartbeat and complete) and queues the held ones: when the agents in their way finish, a release line arrives in your context; wait for it rather than guessing. Units whose blast radii (to `depth`) touch the same files are one group: two agents there would collide, and one agent already has the context. Units that are the same transform (same op on the same symbol across files) are one group: six call sites needing the same change is one agent doing six ops, not six agents paying the fixed cost. Returns the groups with their union of files; the suggested agent count is the group count.",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "units": {"type": "array", "items": {"type": "object"}}, "depth": {"type": "integer"},
                                                 "dispatch": {"type": "boolean", "description": "claim every run_now group now and queue the held ones for release"}},
                "required": ["repo_id", "units"]})),
        ("graph_neighbors",
         "Orient in unfamiliar code without opening it: what this symbol (or file) depends on, what depends on it, and the relation on each edge (calls / inherits / uses_type / references).",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "path": {"type": "string"}, "symbol": {"type": "string"}, "depth": {"type": "integer"}},
                "required": ["repo_id", "path"]})),
        ("graph_path",
         "How two symbols are connected: the shortest chain of relations between them, or an honest \"not connected\".",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "from_path": {"type": "string"}, "to_path": {"type": "string"}, "from_symbol": {"type": "string"}, "to_symbol": {"type": "string"}},
                "required": ["repo_id", "from_path", "to_path"]})),
        ("repo_map",
         "The repo in one response: subsystems and their hub symbols, the god nodes everything leans on, what is being edited right now, and where past attempts left scars. Read this instead of grepping — it is built from every teammate's reported edits, so it gets sharper (and cheaper) the more agents work the repo.",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "budget": {"type": "integer"}},
                "required": ["repo_id"]})),
        ("list_activity",
         "Active intents, workspace roots, last-update timestamps, recent events.",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}},
                "required": ["repo_id"]})),
        ("compliance_report",
         "From the ledger: check-before-write rate per user, intents completed vs abandoned, collisions vs blocks vs overrides.",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "since": {"type": "number"}},
                "required": ["repo_id"]})),
        ("recap",
         r#"What each agent did in this repo, session by session: edits, lines, tokens, model, estimated cost, rationales and top paths. Use it to answer "what happened" or "what did this cost". Read-only. Returns one entry per session and a dashboard link."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "days": {"type": "number", "description": "How many days back, up to 90 (default 1)."}}, "required": ["repo_id"]})),
        ("setup",
         r#"Set up Cork on this machine, once for every repo on it. The answer has install.command: run it exactly as given, in a shell at the repo root. It installs Cork's hooks for Claude Code, Codex, Cursor and Hermes, links the machine to your account, and writes nothing into the repo; if the repo still has Cork's old per-repo setup (.collide/ hook files), it removes that and stages the removal for you to commit. Returns the command and what it installs."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (its git origin)."}, "mode": {"type": "string", "description": "Ignored: kept for older callers. The machine's hooks always gate writes."}}, "required": ["repo_id"]})),
        ("sync_agents_block",
         r#"Replace ONLY the region between the cork markers (or the old collide markers, which it replaces) in the given AGENTS.md content with the current canonical block (byte-identical outside the markers; appended when markers are absent). Apply the returned content yourself so it lands in your own diff."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "file_content": {"type": "string", "description": "The current AGENTS.md content (empty when the file does not exist)."}}, "required": ["repo_id", "file_content"]})),
        ("install_hooks",
         "Install or refresh Cork's hooks on this machine: the same one command as setup; idempotent.",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}},
                "required": ["repo_id"]})),
        ("connect_agents",
         r#"Set up Cork for the agent tools on this machine (Claude Code, Codex, Cursor, Hermes): returns the same one install command as setup, which installs Cork's hooks once for every repo on the machine and writes nothing into the repo. mode "files" returns the protocol rules files and MCP configs to commit for tools without hooks (Windsurf, Cline, GitHub Copilot) instead."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "mode": {"type": "string", "description": "Leave empty for the machine install; \"files\" for the rules files and MCP configs."}}, "required": ["repo_id"]})),
        ("send_agent_message",
         r#"Use this whenever the user wants work handed to another agent or session ("send this to Cork", "hand this off", "give this to the other session", a spec or task they would otherwise copy and paste), and whenever another agent needs to know or do something. Never ask the user to copy and paste between agents. For a handoff, set handoff=true and write the message as a self-contained spec (the goal, the context and decisions so far, the files involved, what done looks like): it goes to exactly ONE agent, the user's other open session in this repo (or the one `to` names), which starts on it by itself; with none open it waits, and the next session started in the repo picks it up. In a shell, `cork handoff "..."` and `cork message "..."` do the same with no tool to load. Without handoff, it is a message: with neither to nor anchor, it goes to the agents at work in this repo whose files the message names (your own other sessions included), and to everyone at work only when it names no code. to = one account email; or anchor = the code it is about ("path.py::symbol", "path.py" or "dir/") with no `to`, which reaches whoever has an open intent on, or edited in the last day, that code or its callers. With an anchor the recipient also sees whether the code changed since you sent it and whether its tests have passed since. It arrives with their next tool result. to = "@user" (or "@user/2", one of their agents) messages any Cork user once they accept you as a contact; hub = a hub's name or id (a new name with to="@a, @b" starts it), or a channel of your workspace (#general, or its id), seen by every teammate who can see it. Returns the message id and who received it. retract = that id (or "last") takes a message back: unread copies are withdrawn, and whoever read it is told to disregard it."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "message": {"type": "string", "description": "What you need them to do or know."}, "to": {"type": "string", "description": "A teammate's account email, or @user / @user/2; omit it when anchor is given."}, "anchor": {"type": "string", "description": "The code the message is about: path.py::symbol, path.py, or dir/."}, "hub": {"type": "string", "description": "A hub's name or id, or a workspace channel: \"#general\" or its id."}, "retract": {"type": "string", "description": "The id of a message you sent (or \"last\"), to take it back instead of sending one."}, "handoff": {"type": "boolean", "description": "true: work for ONE agent to do, the message being its spec. It reaches the user's other open session in this repo (or the agent to names), or waits for the next session started here."}}, "required": ["repo_id"]})),
        ("claim",
         r#"Reserve the next number in a sequence (a migration, an ADR, a release) so no other agent takes it; two agents asking at once never get the same number. The hooks already claim a numbered file's number when you create it, so call this only for a number you need before writing, or for a named sequence. Returns the number, zero-padded like the files already there."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "sequence": {"type": "string", "description": "A directory such as db/migrations/, or a name such as release."}, "title": {"type": "string", "description": "What the number is for; teammates see it."}, "after": {"type": "integer", "description": "The highest number you already see in use in that directory, if any."}}, "required": ["repo_id", "sequence"]})),
        ("inbox_ack",
         r#"Mark messages from teammates' agents as handled so they stop arriving: pass the ids from messages_pending once you have acted on them. Unacknowledged messages repeat on every response. Returns the messages still pending."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "ids": {"type": "array", "items": {"type": "string"}, "description": "Message ids from messages_pending."}}, "required": ["repo_id", "ids"]})),
        ("pending_reconciliations",
         r#"When a response says two agents' work collides (reconciliations_pending): claim the open collisions, with both sides' intents, changed lines and notes, to merge them. Merge by keeping both sides' work; never by deleting either. After the merge passes the tests, call again with resolve=[id]. Returns the claimed reconciliations and how many stay open."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "resolve": {"type": "array", "items": {"type": "string"}, "description": "Ids of reconciliations you have merged and verified."}}, "required": ["repo_id"]})),
        ("explain_code",
         r#"Why the code is the way it is: the history behind a path or symbol, oldest first — each edit (who, when, what changed), completed intents with their rationales, reverted attempts and anchored notes. It includes uncommitted work and the reasons given. Read-only."#,
         json!({"type": "object", "properties": {"repo_id": {"type": "string", "description": "The repository id, e.g. github.com/org/repo (repo_id in .cork/config.json, or .collide/config.json in a repo set up before the rename)."}, "path": {"type": "string", "description": "File to explain; with no symbol, the whole file."}, "symbol": {"type": "string", "description": "Narrow the history to one symbol."}}, "required": ["repo_id"]})),
        ("graph_export",
         "Export the code graph for other tools: \"graphify\" (graph.json, the shape Graphify's viewer and Obsidian export read), \"graphml\" (Gephi, yEd) or \"cypher\" (Neo4j, FalkorDB).",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "format": {"type": "string"}},
                "required": ["repo_id"]})),
        ("graph_import",
         "Bootstrap the graph from a Graphify graph.json (pass its parsed object). Useful for languages Cork has no grammar for yet: the imported edges fill the gaps, and files Cork parses itself keep their live parse.",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "graph": {"type": "object"}},
                "required": ["repo_id", "graph"]})),
        ("open_dashboard",
         "The dashboard URL for this repo's workspace, to hand the human: live activity, the graph, the ledger.",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "section": {"type": "string"}, "path": {"type": "string"}, "seq": {"type": "integer"}, "seats": {"type": "integer"}, "interval": {"type": "string"}, "plan": {"type": "string"}},
                "required": ["repo_id"]})),
        ("move_repo",
         "Move a watched repo to another workspace you belong to; agents route by the repo's workspace from the next call. new_workspace=<name> instead creates that workspace (you own it) and moves the repo there.",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "workspace": {"type": "string"}, "new_workspace": {"type": "string"}},
                "required": ["repo_id"]})),
        ("remove_repo",
         "Stop watching a repo in this workspace. Its history stays in the ledger; nothing is deleted.",
         json!({"type": "object", "properties": {"repo_id": {"type": "string"}, "workspace": {"type": "string"}},
                "required": ["repo_id"]})),
        ("switch_workspace",
         "Without an argument: list every workspace this account belongs to and which one THIS credential is currently bound to — present the options to the human when they ask to switch. With a workspace id or name: re-bind this credential to it immediately (membership-checked, no re-authentication). Scope note: repos already established in another workspace keep auto-routing to their home; switching governs where BRAND-NEW repos and defaults land.",
         json!({"type": "object", "properties": {"workspace": {"type": "string"}},
                "required": []})),
    ]
}

/// The handler the SDK drives. It holds the same `App` the HTTP routes do, so
/// a tool call and a POST to the equivalent endpoint run identical code
/// against identical state — which is what lets one differential test suite
/// cover both.
#[derive(Clone)]
pub struct CorkMcp {
    pub app: Arc<App>,
}

/// The bearer credential for this call, pulled off the HTTP request the SDK
/// carried through. MCP has no notion of a caller, so the transport's headers
/// are the only place identity lives.
fn bearer(context: &RequestContext<RoleServer>) -> Option<String> {
    let parts = context.extensions.get::<http::request::Parts>()?;
    parts
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// The address this call came in on: the
/// gate's `x-collide-via` header — "host path", with the ORIGINAL path the
/// root mount otherwise hides — else the Host header and "/mcp". Returned as
/// the "host path" string every route builds, so `presence::via_view` shapes
/// the stored `{host, path, mcp}` the same way for a tool call as for a POST.
fn via_of(context: &RequestContext<RoleServer>) -> String {
    let headers = context.extensions.get::<http::request::Parts>().map(|parts| &parts.headers);
    let header = |name: &str| -> String {
        headers
            .and_then(|map| map.get(name))
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let stamped = header("x-collide-via");
    if !stamped.is_empty() {
        let (host, path) = stamped.split_once(' ').unwrap_or((stamped.as_str(), ""));
        let path = if path.is_empty() { "/mcp" } else { path };
        return format!("{host} {path}");
    }
    format!("{} /mcp", header("host"))
}

/// How long an MCP session's own repo is remembered for the calls after.
const MCP_REPO_TTL_S: f64 = 4.0 * 3_600.0;

/// Tools that act on a repo the caller must choose on purpose: never defaulted.
const EXPLICIT_REPO_TOOLS: [&str; 6] = ["setup", "install_hooks", "connect_agents", "move_repo", "remove_repo", "switch_workspace"];

/// Whether a call to `tool` may leave `repo_id` out (`default_repo_id`).
fn defaults_repo_id(tool: &str) -> bool {
    static TAKES: std::sync::OnceLock<std::collections::BTreeSet<&'static str>> = std::sync::OnceLock::new();
    let takes = TAKES.get_or_init(|| {
        tool_table()
            .into_iter()
            .filter(|(_, _, schema)| schema.pointer("/properties/repo_id").is_some())
            .map(|(name, _, _)| name)
            .collect()
    });
    takes.contains(tool) && !EXPLICIT_REPO_TOOLS.contains(&tool)
}

/// A tool's schema as listed: `repo_id` optional where `default_repo_id`
/// fills it in, and said so.
fn optional_repo_id(name: &str, schema: Value) -> Value {
    if !defaults_repo_id(name) {
        return schema;
    }
    let mut schema = schema;
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|field| field.as_str() != Some("repo_id"));
    }
    if let Some(property) = schema.pointer_mut("/properties/repo_id").and_then(Value::as_object_mut) {
        property.insert("description".into(), json!(
            "The repository id, e.g. github.com/org/repo (the Cork briefing names it). Omit it to use the repo your session works in."));
    }
    schema
}

/// Arguments as a JSON object. An absent `arguments` is an empty call, not an
/// error — several tools take only `repo_id` and some take nothing.
fn arguments(request: &CallToolRequestParams) -> Value {
    Value::Object(request.arguments.clone().unwrap_or_default())
}

/// Which conversation is calling: the transport's
/// `Mcp-Session-Id` header, which the streamable HTTP client sends on every
/// call after the handshake. That is what makes an MCP-only agent one agent
/// per SESSION rather than one per person, which is what the multi-agent
/// Now card counts. A `session` argument, when a client passes one, wins.
fn session_of(context: &RequestContext<RoleServer>, args: &Value) -> String {
    let named = text_of(args, "session");
    if !named.is_empty() {
        return named;
    }
    context
        .extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.headers.get("mcp-session-id"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// The agent's self-identification from the MCP initialize handshake (e.g.
/// "claude-code 2.x"): the client's title, else its
/// name, then its version. Attribution only, never authorization. An `agent`
/// argument, when a client passes one, wins.
fn agent_of(context: &RequestContext<RoleServer>, args: &Value) -> String {
    let named = text_of(args, "agent");
    if !named.is_empty() {
        return named;
    }
    let Some(info) = context.peer.peer_info() else { return String::new() };
    let client = &info.client_info;
    let label = client.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or(client.name.trim());
    let version = client.version.trim();
    if version.is_empty() { label.to_string() } else { format!("{label} {version}").trim().to_string() }
}

/// The tools that RECORD, plus the two that
/// mint a reporting credential. A viewer (effective access "read") is
/// refused these, in the same shape as any other access denial.
const RECORDING_TOOLS: [&str; 12] = [
    "sync_agents_block", "declare_intent", "heartbeat", "complete_intent", "report_edit",
    "defer_intent", "remember", "send_agent_message", "inbox_ack", "claim",
    "install_hooks", "connect_agents",
];

/// The result goes into the first content block as JSON text. Clients —
/// including this repo's own test client — read `content[0].text` and
/// parse it, so that shape is load
/// bearing. The structured field is populated too, for clients that prefer
/// it, but the text block is what has to be there.
fn respond(value: Value) -> CallToolResult {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(value);
    result
}

fn text_of(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

impl CorkMcp {
    /// Placement by elicitation: the form a capable client shows when an
    /// agent's call touches a repo none of the caller's granted workspaces
    /// holds by the human's choice. Options: each workspace with its plan
    /// and repo count, and "A new workspace…" (with a name) when the plan
    /// allows one. The answer is applied through the same `move_repo` /
    /// `create_and_move` an agent would call — history moves with it — and
    /// the 7-day `wschoice` stamp is set first, so neither this form nor the
    /// text question (`advisory::workspace_choice_notice`, the fallback for
    /// clients without forms) asks twice. Returns whether the repo was
    /// placed, so the caller is re-resolved into its new home. Declining,
    /// cancelling, a client without forms or any failure: false, and the
    /// call runs where it would have.
    async fn ask_placement(
        &self, context: &RequestContext<RoleServer>, caller: &crate::Caller, args: &Value, tool: &str,
    ) -> bool {
        const NOT_FOR: [&str; 9] = [
            "move_repo", "remove_repo", "switch_workspace", "setup", "install_hooks", "connect_agents",
            "sync_agents_block", "open_dashboard", "recap",
        ];
        let repo_id = text_of(args, "repo_id");
        let repo_id = repo_id.trim();
        if repo_id.is_empty() || caller.read_only || caller.uid.is_empty() || NOT_FOR.contains(&tool) {
            return false;
        }
        // the free version keeps one workspace on the machine: nothing to place
        if crate::local::active() {
            return false;
        }
        if !context.peer.supported_elicitation_modes().contains(&rmcp::service::ElicitationMode::Form) {
            return false;
        }
        let store = &self.app.store;
        let aliases = &self.app.aliases;
        let mine: Vec<Value> = crate::workspaces::list_for(store, &caller.uid)
            .into_iter()
            .filter(|w| caller.workspaces.is_empty() || caller.workspaces.contains(&text_of(w, "id")))
            .collect();
        // placed by a human anywhere they may work: nothing to ask
        let explicit = mine.iter().any(|w| {
            crate::watch::binding_of(store, aliases, &text_of(w, "id"), repo_id)
                .is_some_and(|(_, binding)| !crate::compat::truthy(binding.get("auto")))
        });
        if explicit {
            return false;
        }
        let org = crate::repo::repo_org(repo_id);
        let home = mine.iter().find(|w| text_of(w, "id") == caller.workspace);
        if let Some(home) = home {
            let named = |key: &str| text_of(home, key).trim().to_lowercase();
            if !org.is_empty() && (named("github_org") == org || named("name") == org) {
                return false; // the org workspace IS the accurate home
            }
        }
        let owned = crate::workspaces::owned_by(store, &caller.uid);
        let can_create = crate::billing::workspace_limit_for(store, &owned).is_none_or(|cap| (owned.len() as i64) < cap);
        if mine.len() < 2 && !can_create {
            return false; // one workspace and no room for another: no choice to make
        }
        let stamp = format!("wschoice:{}:{}:{}", caller.workspace, crate::repo::repo_key(repo_id), caller.uid);
        if store.eph_get(&stamp).is_some() {
            return false;
        }
        let _ = store.eph_set(&stamp, &json!({"t": crate::store::now()}), Some(7.0 * 86_400.0));

        let label = |w: &Value| -> String {
            let id = text_of(w, "id");
            let plan = crate::billing::plan_of(store, &id);
            let name = text_of(&plan, "plan");
            let pretty = {
                let mut chars = name.chars();
                chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
            };
            let tier = if text_of(&plan, "status") == "trialing" {
                format!("{pretty} trial")
            } else if name == "free" || name.is_empty() {
                "Free".to_string()
            } else {
                pretty
            };
            let repos = store.kv_list("ghrepo", &format!("{id}:")).len();
            format!("{} · {tier} · {repos} repo{}", text_of(w, "name"), if repos == 1 { "" } else { "s" })
        };
        let mut choices: Vec<Value> = mine.iter().map(|w| json!({"const": text_of(w, "id"), "title": label(w)})).collect();
        if can_create {
            choices.push(json!({"const": "__new__", "title": "A new workspace…"}));
        }
        let mut properties = json!({
            "workspace": {"type": "string", "title": "Workspace", "oneOf": choices, "default": caller.workspace},
        });
        if can_create {
            properties["new_workspace_name"] = json!({
                "type": "string", "title": "New workspace name",
                "description": "Only if you chose a new workspace",
            });
        }
        let schema = json!({"type": "object", "properties": properties, "required": ["workspace"]});
        let Ok(requested_schema) = serde_json::from_value(schema) else { return false };
        let asked = context
            .peer
            .create_elicitation(rmcp::model::ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: format!(
                    "'{repo_id}' isn't in a Cork workspace yet. Where should this repo's agents \
report? Teammates in that workspace see this work; nobody else does."
                ),
                requested_schema,
            })
            .await;
        let Ok(answer) = asked else { return false };
        if answer.action != rmcp::model::ElicitationAction::Accept {
            return false;
        }
        let content = answer.content.unwrap_or(Value::Null);
        let chosen = text_of(&content, "workspace");
        let routing = crate::auth::AuthUser {
            user_id: caller.user_id.clone(),
            workspace: caller.workspace.clone(),
            uid: caller.uid.clone(),
            workspaces: caller.workspaces.clone(),
        };
        let result = if chosen == "__new__" {
            let name = text_of(&content, "new_workspace_name");
            crate::watch::create_and_move(
                store, aliases, &routing, &caller.email, &caller.name, repo_id, &name, &caller.uid, "elicitation")
        } else if mine.iter().any(|w| text_of(w, "id") == chosen) {
            // choosing the current workspace pins it, exactly like the
            // agent-side "keep" answer
            crate::watch::move_repo(store, aliases, &routing, repo_id, &chosen, &caller.uid, "elicitation")
        } else {
            return false;
        };
        crate::compat::truthy(result.get("ok"))
    }

    /// Resolve the caller the way every HTTP route does, so authorisation is
    /// one implementation rather than two that drift.
    fn caller(
        &self, context: &RequestContext<RoleServer>, args: &Value,
    ) -> Result<crate::Caller, Value> {
        if let Some(sandbox) = self.sandbox(context) {
            return crate::authorize_sandbox(&self.app, &sandbox, args, json!({"ok": false})).map_err(|refusal| refusal.0);
        }
        let mut headers = axum::http::HeaderMap::new();
        if let Some(token) = bearer(context) {
            if let Ok(value) = axum::http::HeaderValue::from_str(&token) {
                headers.insert("authorization", value);
            }
        }
        crate::authorize_full(&self.app, &headers, args, json!({"ok": false}))
            .map_err(|refusal| refusal.0)
    }

    /// The unclaimed anonymous agent behind this connection, if that is who
    /// it is (`auth::sandbox_principal`).
    fn sandbox(&self, context: &RequestContext<RoleServer>) -> Option<crate::auth::Principal> {
        crate::auth::sandbox_principal(&self.app.store, bearer(context).as_deref(), crate::store::now())
    }
}

impl CorkMcp {
    /// Fill in `repo_id` when a call leaves it out: the repo this MCP
    /// session named on an earlier call, else the one repo this person's
    /// hooks reported from in the last half hour (`watch::hook_repos`).
    /// Agents in v76 went hunting for it (`cat .collide/config.json`,
    /// `collide status`, as the names were then): 18 commands a run in the
    /// scaling study. With no repo to go on the call runs as before; with
    /// several, it is refused
    /// with their names, never guessed.
    fn default_repo_id(&self, context: &RequestContext<RoleServer>, args: &mut Value, tool: &str) -> Result<(), Value> {
        let store = &self.app.store;
        let mcp_session = session_of(context, &Value::Null);
        let given = text_of(args, "repo_id").trim().to_string();
        if !given.is_empty() {
            if !mcp_session.is_empty() {
                let _ = store.eph_set(&format!("mcprepo:{mcp_session}"), &json!({"repo_id": given}), Some(MCP_REPO_TTL_S));
            }
            return Ok(());
        }
        if !defaults_repo_id(tool) {
            return Ok(());
        }
        let Some(map) = args.as_object_mut() else { return Ok(()) };
        // a sandbox has one repo unless it names another
        if self.sandbox(context).is_some() {
            map.insert("repo_id".into(), json!("sandbox/demo"));
            return Ok(());
        }
        if !mcp_session.is_empty() {
            if let Some(named) = store.eph_get(&format!("mcprepo:{mcp_session}")).and_then(|v| v.get("repo_id").and_then(Value::as_str).map(str::to_string)) {
                map.insert("repo_id".into(), json!(named));
                return Ok(());
            }
        }
        let uid = bearer(context)
            .and_then(|token| crate::auth::principal_for_bearer(store, Some(token.as_str()), crate::store::now()))
            .map(|principal| principal.uid)
            .unwrap_or_default();
        let repos = crate::watch::hook_repos(store, &uid);
        match repos.as_slice() {
            [] => Ok(()),
            [one] => {
                map.insert("repo_id".into(), json!(one));
                Ok(())
            }
            several => Err(json!({
                "ok": false,
                "reason": format!(
                    "repo_id is missing, and your agents work in {} repos right now ({}): pass the one this call is about.",
                    several.len(), several.join(", ")),
            })),
        }
    }

    async fn dispatch(
        &self, request: &CallToolRequestParams, context: &RequestContext<RoleServer>, caller: crate::Caller,
        args: &Value, via: &str, session: &str, agent: &str,
    ) -> Result<CallToolResponse, McpError> {
        let store = &self.app.store;
        let args = args.clone();
        let repo_id = text_of(&args, "repo_id");
        let tool = request.name.as_ref();
        if caller.sandbox && !SANDBOX_TOOLS.contains(&tool) {
            return Ok(respond(json!({"ok": false, "access_denied": true, "reason": SANDBOX_TOOL_LINE})).into());
        }
        // a viewer's agents record nothing: the refusal is a RESULT, the
        // same shape and the same line the hook endpoints answer with
        // hubs are free on every plan and record nothing of the repo: a
        // viewer's agents may message people through them
        let hub_send = tool == "send_agent_message" && crate::hubs::is_hub_send(&args);
        if caller.read_only && RECORDING_TOOLS.contains(&tool) && !hub_send {
            return Ok(respond(json!({
                "ok": false, "access_denied": true,
                "reason": crate::access::viewer_notice(
                    store, &caller.workspace, &caller.user_id, &self.app.dashboard_url),
            }))
            .into());
        }
        // the first call IS the watch; the (N+1)th repo is refused as a
        // RESULT — the repo-cap payload — not bound, not recorded
        // setup is the explicit way in: the one call that adds a repo the
        // workspace does not have yet (every other tool asks first)
        let explicit = matches!(tool, "setup" | "install_hooks" | "connect_agents");
        if let Err(refused) = crate::bind_scope_as(&self.app, &caller, explicit) {
            return Ok(respond(refused).into());
        }

        let value = match request.name.as_ref() {
            // a hub or a Cork user by @username: people and agents in
            // any account, reached by the message text alone
            // taking a message back, whichever way it was sent in this repo
            "send_agent_message" if !text_of(&args, "retract").trim().is_empty() => crate::agenttools::retract_message(
                store, &caller.scope, &caller.user_id, session, &text_of(&args, "retract"), agent),
            "send_agent_message" if crate::hubs::is_hub_send(&args) => {
                let sender = self.hub_session(&caller, session);
                crate::hubs::agent_send_live(&self.app, &caller.uid, &sender, agent, &args)
            }
            "get_briefing" if caller.read_only => {
                // a viewer's agents get no briefing: the notice IS the
                // briefing — and nothing rides along with it (no advisory
                // layer)
                let notice = crate::access::viewer_notice(
                    store, &caller.workspace, &caller.user_id, &self.app.dashboard_url);
                return Ok(respond(json!({"repo_id": repo_id, "viewer": true, "access_denied": true,
                                         "reason": notice, "summary": [notice]})).into());
            }
            "get_briefing" => {
                // the fourth agent at once on Free: recorded and on Now,
                // briefed nothing — the notice once, then an empty briefing
                let (over_cap, cap_notice) = crate::access::agent_cap_state(
                    store, &caller.workspace, &caller.user_id, session, &self.app.dashboard_url);
                if over_cap {
                    let summary: Vec<Value> = if cap_notice.is_empty() { Vec::new() } else { vec![json!(cap_notice)] };
                    return Ok(respond(json!({"repo_id": repo_id, "over_cap": true, "summary": summary})).into());
                }
                // a briefing is the first thing an agent does — seat it on
                // the dashboard's Now panel right away, before any edit is
                // reported. Sessionless: the briefing names the person, not
                // one agent.
                crate::presence::touch_presence(
                    store, &caller.scope, &caller.user_id, "", agent, via);
                let days = args.get("days").and_then(Value::as_f64).unwrap_or(7.0);
                let plan = crate::access::plan_for(store, &caller.workspace);
                let mut briefing = crate::briefing::activity(
                    store, &caller.scope, &repo_id,
                    crate::access::clamp_since((days * 86_400.0).max(1.0), plan.history_window_s()),
                    self.app.idle_after_s, &caller.user_id, plan.flag("knowledge_sharing", true));
                // one orientation call: the briefing and, when a path is
                // given, the blast radius of the symbol about to change —
                // the two calls every session opened with, now one message
                let path = text_of(&args, "path");
                if !path.is_empty() {
                    let graph = crate::graphview::snapshot(store, &caller.scope);
                    let overlays = crate::graphview::overlays_for(
                        store, &caller.scope, &graph, self.app.idle_after_s, 30.0, true, true);
                    let radius = crate::graphview::blast_radius_view(
                        &graph, &overlays, &path, &text_of(&args, "symbol"),
                        args.get("depth").and_then(Value::as_u64).unwrap_or(3) as usize);
                    if let Some(map) = briefing.as_object_mut() {
                        map.insert("radius".into(), radius);
                    }
                }
                // the owner's standing decisions: those over the given path,
                // else every live one in the repo (the briefing is about all
                // of it), one compact line each, once per session
                {
                    let decisions: Vec<Value> = crate::decisions::live(store, &caller.scope)
                        .into_iter()
                        .filter(|d| path.is_empty() || crate::decisions::covers_path(d, &path))
                        .take(5)
                        .collect();
                    let told_key = |id: &str| format!("decisiontold:{}:{}:{id}", caller.scope,
                                                      crate::presence::agent_id(&caller.user_id, session));
                    let mut lines: Vec<Value> = Vec::new();
                    for d in &decisions {
                        let id = d.get("id").and_then(Value::as_str).unwrap_or("");
                        if !session.is_empty() && store.eph_get(&told_key(id)).is_some() {
                            continue;
                        }
                        let on: Vec<String> = crate::decisions::anchors_of(d).iter().map(crate::decisions::anchor_text).collect();
                        lines.push(json!(crate::decisions::line_for(store, &caller.user_id, d, &on)));
                        if !session.is_empty() {
                            let _ = store.eph_set(&told_key(id), &json!({"ts": crate::store::now()}), Some(crate::decisions::HELD_TTL_S));
                        }
                    }
                    if !lines.is_empty() {
                        if let Some(map) = briefing.as_object_mut() {
                            if let Some(summary) = map.get_mut("summary").and_then(Value::as_array_mut) {
                                for (i, line) in lines.iter().enumerate() {
                                    summary.insert(i, line.clone());
                                }
                            }
                            map.insert("decisions".into(), json!(lines));
                        }
                    }
                }
                // once per session: who Cork cannot see in this room, and
                // where to add them
                if let Some(nudge) = crate::access::viewers_nudge(
                    store, &caller.workspace, &caller.user_id, session, &self.app.dashboard_url) {
                    if let Some(map) = briefing.as_object_mut() {
                        map.insert("viewers_notice".into(), json!(nudge));
                    }
                }
                briefing
            }
            "blind_spots" => crate::merge::blind_spots(
                store, &caller.scope, &repo_id, self.app.idle_after_s, self.app.hot_ttl_s),
            "simulate_merge" => crate::merge::simulate_merge(
                store, &caller.scope, &repo_id, &strings(&args, "users")),
            "differential_check" => crate::merge::differential_check(
                store, &caller.scope, &caller.user_id, &text_of(&args, "other_user")),
            "recall" => {
                let plan = crate::access::plan_for(store, &caller.workspace);
                crate::memory::recall(
                    store, &caller.workspace, &caller.uid, &caller.user_id, &caller.scope,
                    &text_of(&args, "query"), &strings(&args, "tags"),
                    args.get("limit").and_then(Value::as_i64).unwrap_or(25),
                    crate::access::oldest_ts(&plan), plan.flag("knowledge_sharing", true))
            }
            "remember" if crate::decisions::wants_decision(&args)
                || args.get("facts").and_then(Value::as_array).is_some_and(|f| f.iter().any(crate::decisions::wants_decision)) => {
                // the owner's standing decision: anchored, never stale, and
                // the first write under it in any session is held once
                let (decided, notes) = crate::decisions::remember(
                    store, &caller.scope, &caller.user_id, session, "", agent, &args);
                let mut saved = Vec::new();
                for item in notes.iter().filter(|item| !text_of(item, "fact").trim().is_empty()) {
                    let tags = strings(item, "tags");
                    let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
                    saved.push(crate::memory::save(store, &crate::memory::SaveInput {
                        scope: &caller.scope,
                        user_id: &caller.user_id,
                        fact: &text_of(item, "fact"),
                        tags: &tag_refs,
                        agent: agent,
                        anchor: &text_of(item, "anchor"),
                        auto: "",
                        supersedes: &text_of(item, "supersedes"),
                    }));
                }
                if saved.is_empty() { decided } else { json!({"ok": true, "decisions": decided, "count": saved.len(), "saved": saved}) }
            }
            "remember" if args.get("facts").and_then(Value::as_array).is_some_and(|f| !f.is_empty()) => {
                // several anchored facts in one call — the decisions a stage
                // settled, recorded together instead of one message each
                let mut saved = Vec::new();
                for item in args.get("facts").and_then(Value::as_array).into_iter().flatten() {
                    let fact = text_of(item, "fact");
                    if fact.trim().is_empty() {
                        continue;
                    }
                    let tags = strings(item, "tags");
                    let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
                    saved.push(crate::memory::save(store, &crate::memory::SaveInput {
                        scope: &caller.scope,
                        user_id: &caller.user_id,
                        fact: &fact,
                        tags: &tag_refs,
                        agent: agent,
                        anchor: &text_of(item, "anchor"),
                        auto: "",
                        supersedes: &text_of(item, "supersedes"),
                    }));
                }
                json!({"ok": true, "count": saved.len(), "saved": saved})
            }
            "remember" => {
                let tags = strings(&args, "tags");
                let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
                crate::memory::save(store, &crate::memory::SaveInput {
                    scope: &caller.scope,
                    user_id: &caller.user_id,
                    fact: &text_of(&args, "fact"),
                    tags: &tag_refs,
                    agent: agent,
                    anchor: &text_of(&args, "anchor"),
                    auto: "",
                    supersedes: &text_of(&args, "supersedes"),
                })
            }
            "heartbeat" => crate::intents::heartbeat(
                store, &caller.scope, &caller.user_id, &text_of(&args, "intent_id"),
                self.app.intent_ttl_s),
            "complete_intent" => crate::intents::complete(store, &crate::intents::CompleteInput {
                scope: &caller.scope,
                user_id: &caller.user_id,
                intent_id: &text_of(&args, "intent_id"),
                session: session,
                rationale: &text_of(&args, "rationale"),
                via,
            }),
            "defer_intent" => {
                let deferred = crate::intents::defer(
                    store, &caller.scope, &caller.user_id, session, agent,
                    &strings(&args, "paths"), &strings(&args, "symbols"),
                    &text_of(&args, "note"),
                    args.get("expires_days").and_then(Value::as_f64).unwrap_or(14.0));
                // deferred work is the task list: a workspace with a CRM
                // connected gets it filed there by the server, off the
                // agent's path — no CRM tool in any agent's menu
                if crate::compat::truthy(deferred.get("ok")) {
                    crate::crm::sync_deferred(
                        store, &caller.workspace, &caller.scope, &repo_id, &caller.user_id, agent,
                        &text_of(&args, "note"), &strings(&args, "paths"), &strings(&args, "symbols"), via);
                }
                deferred
            }
            "declare_intent" => {
                // presence from any protocol call, before the lease is even
                // validated: declaring seats the agent first
                crate::presence::touch_presence(
                    store, &caller.scope, &caller.user_id, session,
                    agent, via);
                crate::intents::declare(store, &crate::intents::DeclareInput {
                    scope: &caller.scope,
                    user_id: &caller.user_id,
                    repo_id: &repo_id,
                    paths: strings(&args, "paths"),
                    symbols: strings(&args, "symbols"),
                    change_type: &text_of(&args, "change_type"),
                    before: &text_of(&args, "before"),
                    after: &text_of(&args, "after"),
                    summary: &text_of(&args, "summary"),
                    agent: agent,
                    session: session,
                    reference: &text_of(&args, "ref"),
                    operations: args.get("operations").cloned(),
                    idempotency_key: &text_of(&args, "idempotency_key"),
                    ttl_s: self.app.intent_ttl_s,
                    idle_after_s: self.app.idle_after_s,
                    via,
                })
            }
            "move_repo" | "remove_repo" | "switch_workspace" => {
                let auth = crate::auth::AuthUser {
                    user_id: caller.user_id.clone(),
                    workspace: caller.workspace.clone(),
                    uid: caller.uid.clone(),
                    workspaces: Vec::new(),
                };
                match request.name.as_ref() {
                    "move_repo" if !text_of(&args, "new_workspace").trim().is_empty() => crate::watch::create_and_move(
                        store, &self.app.aliases, &auth, &caller.email, &caller.name, &repo_id,
                        &text_of(&args, "new_workspace"), &caller.uid, via),
                    "move_repo" => crate::watch::move_repo(
                        store, &self.app.aliases, &auth, &repo_id, &text_of(&args, "workspace"),
                        &caller.uid, via),
                    "remove_repo" => crate::watch::remove_repo(
                        store, &self.app.aliases, &auth, &repo_id, &text_of(&args, "workspace"),
                        &caller.uid),
                    // rebinding needs the credential itself, which only the
                    // transport has — it is re-read here, never stored
                    _ => crate::watch::switch_workspace(
                        store, &caller.uid, &caller.workspace,
                        &bearer(context).unwrap_or_default(), &text_of(&args, "workspace")),
                }
            }
            // work for one agent: the user's other open session here (or
            // the one `to` names), else it waits for their next session
            "send_agent_message" if args.get("handoff").and_then(Value::as_bool) == Some(true) => crate::local::handoff(
                store, &caller.scope, &caller.workspace, &caller.user_id, &text_of(&args, "to"), session,
                &text_of(&args, "message"), agent, self.app.idle_after_s),
            "send_agent_message" if !text_of(&args, "anchor").trim().is_empty() => {
                // `to` names one agent here, by any name Cork shows it under
                let to = text_of(&args, "to");
                let to = if to.trim().is_empty() {
                    Ok(String::new())
                } else {
                    crate::local::resolve_to(store, &caller.scope, &caller.user_id, session, "", &to, self.app.idle_after_s)
                };
                match to {
                    Ok(to) => crate::agenttools::send_anchored_message_in(
                        store, &caller.scope, &caller.workspace, &caller.user_id, session, &to,
                        &text_of(&args, "message"), &text_of(&args, "anchor"), agent, &self.app.dashboard_url),
                    Err(why) => json!({"ok": false, "error": why}),
                }
            }
            // no recipient and no anchor: everyone at work in this repo, the
            // sender's own other sessions included, as `cork message` does
            "send_agent_message" if text_of(&args, "to").trim().is_empty() && !text_of(&args, "message").trim().is_empty() => {
                crate::local::broadcast(
                    store, &caller.scope, &caller.workspace, &caller.user_id, session, &text_of(&args, "message"), agent,
                    self.app.idle_after_s)
            }
            "send_agent_message" => {
                let message = text_of(&args, "message");
                // the agent `to` names here (a session's title, a helper's
                // name, an id...), or nothing is sent and the answer says who is here
                match crate::local::resolve_to(
                    store, &caller.scope, &caller.user_id, session, "", &text_of(&args, "to"), self.app.idle_after_s) {
                    Err(why) => json!({"ok": false, "error": why}),
                    // limits.messaging "own": only the sender's own agents
                    Ok(to) => match crate::access::messaging_refusal(
                        store, &caller.workspace, &caller.user_id, &to, &self.app.dashboard_url) {
                        Some(refusal) if !message.trim().is_empty() => json!({"ok": false, "error": refusal}),
                        _ => crate::agenttools::send_agent_message_in(
                            store, &caller.scope, &caller.user_id, session, &to, &message, agent, false),
                    },
                }
            }
            "claim" => crate::claims::claim(
                store, &caller.scope, &caller.user_id, session, agent,
                &text_of(&args, "sequence"), &text_of(&args, "title"),
                args.get("after").and_then(Value::as_i64).unwrap_or(0), via),
            "inbox_ack" => {
                let inbox = crate::agenttools::inbox_ack(
                    store, &caller.scope, &caller.user_id, &crate::visible_for(&self.app, &caller),
                    &strings(&args, "ids"));
                let inbox = crate::access::own_messages_only(
                    store, &caller.workspace, &caller.user_id, inbox, &self.app.dashboard_url);
                crate::agenttools::mark_delivered(
                    store, inbox.get("messages").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]), session);
                inbox
            }
            "pending_reconciliations" => crate::agenttools::claim_reconciliations(
                store, &caller.scope, &caller.user_id, &strings(&args, "resolve"),
                self.app.idle_after_s),
            "explain_code" => crate::agenttools::explain_code(
                store, &caller.scope, &text_of(&args, "path"), &text_of(&args, "symbol"),
                crate::access::oldest_ts(&crate::access::plan_for(store, &caller.workspace))),
            "graph_export" => crate::agenttools::graph_export(
                store, &caller.scope, &repo_id, &text_of(&args, "format"), self.app.idle_after_s),
            "graph_import" => crate::agenttools::graph_import(
                store, &caller.scope, args.get("graph").unwrap_or(&Value::Null),
                crate::store::now()),
            "open_dashboard" => crate::agenttools::open_dashboard(
                &self.app.dashboard_url, &text_of(&args, "section"), &text_of(&args, "path"),
                args.get("seq").and_then(Value::as_i64).unwrap_or(0),
                args.get("seats").and_then(Value::as_i64).unwrap_or(0),
                &text_of(&args, "interval"), &text_of(&args, "plan")),
            "setup" | "sync_agents_block" | "install_hooks" | "connect_agents" => {
                let artifact = crate::blocks::ArtifactRequest {
                    repo_id: &repo_id,
                    public_url: &self.app.public_url,
                    mcp_url: &self.app.mcp_url,
                    workspace: &caller.workspace,
                    auth_uid: &caller.uid,
                    email: &caller.email,
                    name: &caller.name,
                    workspaces: &caller.workspaces,
                };
                let can_write = !caller.read_only;
                match request.name.as_ref() {
                    "setup" => crate::blocks::setup_tool_response(
                        store, &text_of(&args, "mode"), can_write, &artifact),
                    "sync_agents_block" => crate::blocks::sync_agents_block(
                        store, &caller.scope, &caller.user_id, &repo_id,
                        &text_of(&args, "file_content")),
                    "install_hooks" => match crate::blocks::install_hooks_response(
                        store, can_write, &artifact) {
                        Ok(value) => value,
                        // the read-only refusal is a RESULT the agent reads
                        Err(message) => json!({"ok": false, "error": message}),
                    },
                    // connect_agents is setup's machine install: one command for
                    // every repo on the machine. Mode "files" returns the rules
                    // files and MCP configs for tools without hooks
                    _ if can_write && text_of(&args, "mode") != "files" => crate::blocks::setup_tool_response(
                        store, "", can_write, &artifact),
                    _ => match crate::blocks::connect_agents_response(can_write, &artifact) {
                        Ok(value) => value,
                        Err(message) => json!({"ok": false, "error": message}),
                    },
                }
            }
            "recap" => {
                let days = args.get("days").and_then(Value::as_f64).unwrap_or(1.0);
                // recap clamps to 90 days itself; the plan's window sits inside that
                let window = crate::access::plan_for(store, &caller.workspace).history_window_s();
                let days = crate::access::clamp_days(days.max(0.02).min(90.0), window);
                let mut view = crate::insights::recap(store, &caller.scope, days);
                // the deep link is dashboard wiring, not insight logic, so it is
                // attached here
                let origin = self.app.dashboard_url.trim_end_matches('/');
                if let Some(map) = view.as_object_mut() {
                    map.insert("dashboard".into(), json!(format!("{origin}/dashboard/activity")));
                }
                view
            }
            "compliance_report" => match crate::access::compliance_refusal(
                store, &caller.workspace, &self.app.dashboard_url) {
                Some(gated) => gated,
                None => crate::briefing::compliance_report(
                    store, &caller.scope, &repo_id,
                    args.get("since").and_then(Value::as_f64).unwrap_or(0.0)),
            },
            "list_activity" => {
                let visible = crate::visible_for(&self.app, &caller);
                let mut view = crate::activity::list_activity(
                    store, &caller.scope, &repo_id, self.app.idle_after_s, &visible);
                // who-did-it, resolved for THIS caller: nicknames are private
                // to whoever chose them
                crate::activity::label_actors(store, &caller.user_id, &mut view);
                // the deep link is dashboard wiring, attached here
                let origin = self.app.dashboard_url.trim_end_matches('/');
                if let Some(map) = view.as_object_mut() {
                    map.insert("dashboard".into(), json!(format!("{origin}/dashboard/activity")));
                }
                view
            }
            "get_symbol" => crate::memory::get_symbol(
                store, &caller.scope, &text_of(&args, "path"), &text_of(&args, "symbol")),
            "blast_radius" | "graph_neighbors" | "graph_path" | "repo_map" | "plan_work" => {
                // one snapshot and one overlay pass serve all four; assembling
                // the graph is the expensive step and it is cached besides
                let graph = crate::graphview::snapshot(store, &caller.scope);
                let overlays = crate::graphview::overlays_for(
                    store, &caller.scope, &graph, self.app.idle_after_s, 30.0, true, true);
                match request.name.as_ref() {
                    "plan_work" => {
                        let units: Vec<Value> = args.get("units").and_then(Value::as_array).cloned().unwrap_or_default();
                        let plan = crate::graphview::plan_work_view(
                            &graph, &overlays, &units, args.get("depth").and_then(Value::as_u64).unwrap_or(2) as usize);
                        // the live gate: verdicts against everything open right now,
                        // and with dispatch the runnable groups are claimed on the spot
                        crate::traffic::gate(
                            store, &caller.scope, &caller.repo_id, &caller.user_id, &session, plan, &units,
                            args.get("dispatch").and_then(Value::as_bool).unwrap_or(false), self.app.idle_after_s)
                    }
                    "blast_radius" => crate::graphview::blast_radius_view(
                        &graph, &overlays, &text_of(&args, "path"), &text_of(&args, "symbol"),
                        args.get("depth").and_then(Value::as_u64).unwrap_or(3) as usize),
                    "graph_neighbors" => crate::graphview::neighbors_view(
                        &graph, &overlays, &text_of(&args, "path"), &text_of(&args, "symbol"),
                        args.get("depth").and_then(Value::as_u64).unwrap_or(1) as usize),
                    "graph_path" => crate::graphview::path_view(
                        &graph, &text_of(&args, "from_path"), &text_of(&args, "from_symbol"),
                        &text_of(&args, "to_path"), &text_of(&args, "to_symbol")),
                    _ => {
                        let budget = args
                            .get("budget")
                            .and_then(Value::as_u64)
                            .unwrap_or(12)
                            .clamp(3, 40) as usize;
                        let comms = crate::graphview::communities_cached(
                            &graph, &caller.scope, &overlays.co_change);
                        let mut map =
                            crate::graphview::repo_map(&graph, &comms, &overlays, budget);
                        if let Some(object) = map.as_object_mut() {
                            object.insert("ok".into(), json!(true));
                            object.insert("repo_id".into(), json!(repo_id));
                        }
                        map
                    }
                }
            }
            "check_collisions" => {
                let paths = strings(&args, "paths");
                let symbols = strings(&args, "symbols_referenced");
                let agent = agent.to_string();
                // presence from any protocol call, so an agent appears the
                // moment it does anything rather than only after its first
                // reported edit
                crate::presence::touch_presence(
                    store, &caller.scope, &caller.user_id, session, &agent, via);
                // the check IS the evidence compliance grades on, so it is
                // recorded here rather than inferred later
                let _ = store.ledger_append(
                    &caller.scope,
                    "check_performed",
                    &json!({"user": caller.user_id, "session": session,
                            "agent": agent, "paths": paths, "symbols": symbols}),
                    crate::store::now(),
                );
                let response = crate::collisions::check_collisions(
                    store, &crate::collisions::CheckInput {
                        scope: &caller.scope,
                        user_id: &caller.user_id,
                        paths,
                        symbols,
                        idle_after_s: self.app.idle_after_s,
                        visible: crate::visible_for(&self.app, &caller),
                    });
                crate::events::with_deltas(store, &caller.scope, &caller.user_id, response)
            }
            "report_edit" => {
                let path = text_of(&args, "path");
                let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                let why = text_of(&args, "why");
                if path.is_empty() || repo_id.is_empty() {
                    return Ok(respond(
                        json!({"ok": false, "reason": "missing repo_id, path, or content"}))
                        .into());
                }
                // a reason with no content: the rationale for an edit the hook
                // already recorded from disk, a few dozen bytes and no parse
                if content.is_empty() && !why.trim().is_empty() {
                    return Ok(respond(crate::report::record_rationale(
                        store, &caller.scope, &caller.user_id, &session, &text_of(&args, "agent"), &path, &why,
                    )).into());
                }
                if content.len() > 2_000_000 {
                    return Ok(respond(json!({"ok": false, "reason": "file too large"})).into());
                }
                let agent = {
                    let named = agent.to_string();
                    if named.is_empty() { "mcp".to_string() } else { named }
                };
                let member = crate::auth::member(store, &caller.workspace, &caller.uid);
                let renamed_to = crate::rename::adopt_origin(
                    store, &self.app.aliases, &caller.workspace, &repo_id,
                    &text_of(&args, "origin"), &caller.user_id, member.as_ref(),
                );
                let response = crate::report::report_edit(store, &crate::report::ReportInput {
                    scope: &caller.scope,
                    user_id: &caller.user_id,
                    path: &path,
                    content,
                    local: None,
                    agent: &agent,
                    model: &text_of(&args, "model"),
                    effort: crate::presence::Effort::of(&args),
                    branch: &text_of(&args, "branch"),
                    session: session,
                    tokens: args.get("tokens").and_then(Value::as_i64).unwrap_or(0),
                    // the MCP tool is the model-facing path and has no
                    // transcript to read usage out of; only the hook fills these
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    turn_id: &text_of(&args, "turn_id"),
                    context: 0,
                    why: &why,
                    hot_ttl_s: self.app.hot_ttl_s,
                    via,
                    workspace: &caller.workspace,
                    draft: args.get("draft").and_then(Value::as_bool).unwrap_or(false),
                    auto: false,
                    verified: None,
                    worker: "",
                    resent: false,
                });
                crate::envelope::decorate(
                    store,
                    &self.app.aliases,
                    &crate::envelope::EnvelopeInput {
                        renamed_to: &renamed_to,
                        scope: &caller.scope,
                        workspace: &caller.workspace,
                        repo_id: &repo_id,
                        user_id: &caller.user_id,
                        hook_version: crate::envelope::as_int(args.get("hook_version")),
                        agents_version: crate::envelope::as_int(args.get("agents_version")),
                        settings_digest: args.get("settings_digest").and_then(|v| v.as_str()).unwrap_or(""),
                    },
                    response,
                )
            }
            unknown => {
                return Err(McpError::invalid_params(
                    format!("unknown tool: {unknown}"), None));
            }
        };
        // The advisory layer around a result: the
        // workspace-name hint, the footer with seats and the dashboard, and
        // the setup / hooks-pending / restart notices. Applied per tool to
        // a fixed set of tools, so the same sentences ride along the same
        // calls.
        let mut value = value;
        let tool = request.name.as_ref();
        let advisory = crate::advisory::Advisory { dashboard_url: self.app.dashboard_url.clone(), public_url: self.app.public_url.clone() };
        if crate::advisory::wants_hint(tool) {
            crate::advisory::apply_hint(store, &caller, &repo_id, &mut value);
        }
        if crate::advisory::wants_footer(tool) {
            crate::advisory::apply_footer(store, &advisory, &caller, &mut value);
        }
        if crate::advisory::wants_setup_hint(tool) {
            crate::advisory::apply_setup_hint(store, &self.app.aliases, &advisory, &caller, &repo_id, &mut value);
        }
        if tool == "get_briefing" {
            crate::advisory::apply_hooks_live(store, &caller, &mut value);
        }
        crate::advisory::apply_messages_without_hooks(store, &advisory, &caller, tool, &mut value);
        self.attach_hub_messages(&caller, session, tool, &mut value);
        Ok(respond(value).into())
    }
}

impl CorkMcp {
    /// The session a hub message is signed with: this MCP session when it
    /// is the agent's only identity (no hooks in this repo) or already
    /// numbered; for an agent with hooks, whose number belongs to its hook
    /// session, none — it signs as its person (`@user`), never as a phantom
    /// agent number.
    fn hub_session(&self, caller: &crate::Caller, session: &str) -> String {
        let store = &self.app.store;
        let seated = store.kv_get("agentno", &caller.uid)
            .is_some_and(|r| r.get("sessions").and_then(|s| s.get(session)).is_some());
        let hooks = crate::compat::truthy(store.kv_get("hookseen", &format!("{}:{}", caller.scope, caller.user_id)).as_ref());
        if seated || !hooks { session.to_string() } else { String::new() }
    }

    /// Hub messages waiting for this agent, on its reply (`hub_messages`):
    /// for a session that named itself (`session`, as a hub message tells
    /// it to), one that already used the hub tools, or an agent with no
    /// hooks, whose MCP replies are the only way a message reaches it. An
    /// agent with hooks gets them from its hooks, under its own session.
    fn attach_hub_messages(&self, caller: &crate::Caller, session: &str, tool: &str, value: &mut Value) {
        if crate::local::active() || caller.uid.is_empty() || !value.is_object() {
            return;
        }
        let store = &self.app.store;
        let session = self.hub_session(caller, session);
        // the briefing names the caller's hubs, in one line, when there are any
        if tool == "get_briefing" {
            if let Some(line) = crate::hubs::brief_line(store, &caller.uid, &session) {
                value["hubs"] = json!(line);
            }
        }
        if session.is_empty() {
            return;
        }
        crate::hubs::note_session(store, &caller.uid, &session, &caller.scope, &caller.user_id);
        if let Some(note) = crate::hubs::hook_note(store, &caller.uid, &session) {
            value["hub_messages"] = json!(note);
        }
    }
}

impl ServerHandler for CorkMcp {
    fn get_info(&self) -> ServerConfig {
        // Tools change when a member's access changes or an integration is
        // connected, and resources change when a repo's root moves, so both
        // are declared list-changed capable rather than static. A client not
        // told the menu can change will cache a stale one for the session.
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_resources_list_changed()
                .enable_resources_subscribe()
                .build(),
        )
        .with_server_info(Implementation::new("Cork", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            // What an agent is told the moment it connects, before any hook,
            // AGENTS.md or config exists — the whole of "zero install". It
            // leads with WHEN, since a model that has the rule first applies
            // it to the first request; the mechanics come after.
            "Cork shows you what every other agent and person editing this codebase is changing, \
             live, so you never overwrite their work. Where Cork's hooks are installed they do the \
             protocol for you: a `Cork briefing` arrives in your context, every write is checked and \
             reported, and the repo check's verdict arrives in your next tool result; build on the \
             briefing and do not call declare_intent, check_collisions, report_edit or heartbeat, nor \
             get_briefing or recall for what the briefing already gave you. When the user wants work \
             handed to another agent or session (\"send this to Cork\", a spec, a task, findings), hand \
             it off through Cork, never by asking them to copy and paste: in a shell run `cork handoff \
             \"<self-contained spec>\"`; with no shell, call send_agent_message with handoff=true. It \
             reaches the user's other open session in the repo, which starts on it by itself, or waits \
             for their next one. When you are asked to tell the other agents something, run `cork \
             message --about path.py::symbol \"...\"` in your shell (no tool to load): it reaches the \
             agents on that code. Do not announce your plan. \
             Without hooks, call get_briefing first for any coding request, then declare_intent before \
             changing anything, check_collisions before writing and report_edit after. Lines starting \
             with Δ in a result are teammates' changes since your last step: adapt to them, never undo \
             them. When a result asks where a repo belongs or to run setup, act on it (asking the human \
             where it says to) before carrying on. Load every Cork tool you need in ONE ToolSearch \
             call (select:<tool>,<tool>): each separate load costs a message.",
        )
    }

    async fn list_tools(
        &self, _request: Option<PaginatedRequestParams>, context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // An unauthenticated listing gets the whole menu: discovery happens
        // before a credential exists, and hiding tools from a client that is
        // only looking would make Cork look smaller than it is.
        let mut hide_ceremony = false;
        let hide_writes = match bearer(&context) {
            None => false,
            Some(token) => {
                let mut headers = axum::http::HeaderMap::new();
                match axum::http::HeaderValue::from_str(&token) {
                    Err(_) => false,
                    Ok(value) => {
                        headers.insert("authorization", value);
                        let read_only = crate::auth::principal_for_bearer(
                            &self.app.store, Some(token.as_str()), crate::store::now())
                            .filter(|principal| !principal.workspace.is_empty())
                            .and_then(|principal| {
                                crate::auth::member(
                                    &self.app.store, &principal.workspace, &principal.uid)
                            })
                            .map(|member| {
                                // the EFFECTIVE access: on Free every non-owner is a viewer
                                let principal = crate::auth::principal_for_bearer(
                                    &self.app.store, Some(token.as_str()), crate::store::now());
                                let workspace = principal.map(|p| p.workspace).unwrap_or_default();
                                crate::access::access_of(&self.app.store, &workspace, Some(&member)) == "read"
                            })
                            .unwrap_or(false);
                        if let Some(principal) = crate::auth::principal_for_bearer(
                            &self.app.store, Some(token.as_str()), crate::store::now())
                        {
                            let suffix = format!(":{}", principal.label());
                            hide_ceremony = self.app.store.kv_list("hookseen", "")
                                .iter()
                                .any(|(key, _)| key.ends_with(&suffix));
                        }
                        read_only
                    }
                }
            }
        };

        let sandbox = self.sandbox(&context).is_some();
        let tools = tool_table()
            .into_iter()
            .filter(|(name, _, _)| {
                (!sandbox || SANDBOX_TOOLS.contains(name))
                    && !HIDDEN_TOOLS.contains(name)
                    && !(hide_writes && WRITE_TOOLS.contains(name))
                    && !(hide_ceremony && CEREMONY_TOOLS.contains(name))
            })
            .map(|(name, description, schema)| {
                let object: Map<String, Value> = optional_repo_id(name, schema).as_object().cloned().unwrap_or_default();
                let tool = Tool::new(name, description, Arc::new(object));
                match tool_annotations(name) {
                    Some(annotations) => tool.annotate(annotations),
                    None => tool,
                }
            })
            .collect();
        let mut result = ListToolsResult::with_all_items(tools);
        result.ttl_ms = Some(LIST_TTL_MS);
        result.cache_scope = Some(CacheScope::Private);
        Ok(result)
    }

    async fn list_prompts(
        &self, _request: Option<PaginatedRequestParams>, _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        let mut result = ListPromptsResult::default();
        result.ttl_ms = Some(LIST_TTL_MS);
        result.cache_scope = Some(CacheScope::Private);
        Ok(result)
    }

    async fn list_resource_templates(
        &self, _request: Option<PaginatedRequestParams>, _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let mut result = ListResourceTemplatesResult::default();
        result.ttl_ms = Some(LIST_TTL_MS);
        result.cache_scope = Some(CacheScope::Private);
        Ok(result)
    }

    async fn call_tool(
        &self, request: CallToolRequestParams, context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let mut args = arguments(&request);
        // no repo_id: the repo this MCP session named before, else the one
        // this person's hooks work in; several, and the call must say which
        if let Err(refusal) = self.default_repo_id(&context, &mut args, request.name.as_ref()) {
            return Ok(respond(refusal).into());
        }
        let mut caller = match self.caller(&context, &args) {
            Ok(caller) => caller,
            // a refusal is a RESULT, not a protocol error: the agent is meant
            // to read it and adapt, and an error would hide it behind the
            // client's own retry logic
            Err(refusal) => return Ok(respond(refusal).into()),
        };
        // a repo no workspace of theirs holds yet: a client that can show a
        // form asks the human where it goes BEFORE this call runs, and the
        // call then runs in the workspace they chose
        if !caller.sandbox && self.ask_placement(&context, &caller, &args, request.name.as_ref()).await {
            if let Ok(placed) = self.caller(&context, &args) {
                caller = placed;
            }
        }
        crate::billing::count_call(&self.app.store, &caller.workspace);
        // the transport's identity
        let via = via_of(&context);
        let session = session_of(&context, &args);
        let agent = agent_of(&context, &args);
        // an agent with no hooks (a chat app, an MCP-only client) is metered
        // by the gaps between its calls, the way a transcript's lines are;
        // one whose hooks report is billed from its transcript instead
        // a sandbox holds no agent hours and is no one's account: never
        // metered, never counted as a person's activity
        if !caller.sandbox && !crate::hooks_seen(&self.app.store, &caller.scope, &caller.user_id) {
            crate::billing::meter_mcp_call(&self.app.store, &caller.workspace, &caller.uid, &session, &self.app.dashboard_url);
        }
        if !caller.sandbox {
            // activation: the first tool call is the moment a connect becomes a user
            crate::analytics::capture_once(
                &self.app.store, &caller.uid, &caller.uid, "first_tool_call", &caller.workspace,
                json!({"tool": request.name.as_ref(), "repo_id": caller.repo_id, "agent": agent, "via": via}),
            );
            crate::analytics::capture_daily(&self.app.store, &caller.uid, "agent_active", &caller.workspace, json!({"via": "mcp"}));
        }
        // every row this call writes carries the address
        crate::store::VIA
            .scope(via.clone(), self.dispatch(&request, &context, caller, &args, &via, &session, &agent))
            .await
    }

    async fn list_resources(
        &self, _request: Option<PaginatedRequestParams>, _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let mut result = ListResourcesResult::with_all_items(vec![
            Resource::new(format!("{ACTIVITY_URI_PREFIX}{{repo_id}}"), "activity")
                .with_description(
                    "Live activity for a repo: who is here, what is claimed, recent edits.")
                .with_mime_type("application/json"),
        ]);
        result.ttl_ms = Some(LIST_TTL_MS);
        result.cache_scope = Some(CacheScope::Private);
        Ok(result)
    }

    async fn read_resource(
        &self, request: ReadResourceRequestParams, context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let Some(repo_id) = request.uri.strip_prefix(ACTIVITY_URI_PREFIX).or_else(|| request.uri.strip_prefix(OLD_ACTIVITY_URI_PREFIX)) else {
            return Err(McpError::invalid_params(
                format!("unknown resource: {}", request.uri), None));
        };
        let args = json!({"repo_id": repo_id});
        let caller = match self.caller(&context, &args) {
            Ok(caller) => caller,
            Err(refusal) => {
                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    serde_json::to_string(&refusal).unwrap_or_default(),
                    request.uri.clone(),
                )])
                .into())
            }
        };
        if let Err(refused) = crate::bind_scope(&self.app, &caller) {
            return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                serde_json::to_string(&refused).unwrap_or_default(),
                request.uri.clone(),
            )])
            .into());
        }
        let visible = crate::visible_for(&self.app, &caller);
        let mut activity = crate::activity::list_activity(
            &self.app.store, &caller.scope, repo_id, self.app.idle_after_s, &visible);
        // who-did-it, resolved for THIS caller: nicknames are private to
        // whoever chose them, so the labels depend on who is asking
        crate::activity::label_actors(&self.app.store, &caller.user_id, &mut activity);
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            serde_json::to_string(&activity).unwrap_or_default(),
            request.uri.clone(),
        )])
        .into())
    }
}

/// The menu an unauthenticated `tools/list` would get, as JSON: the tool
/// table less the hidden tools, each with its listed schema and its
/// annotations. The public MCP manifest (`/.well-known/mcp.json`) is built
/// from this, so what it shows is exactly what the server lists.
pub fn public_tools() -> Vec<Value> {
    tool_table()
        .into_iter()
        .filter(|(name, _, _)| !HIDDEN_TOOLS.contains(name))
        .map(|(name, description, schema)| {
            let mut tool = json!({
                "name": name,
                "description": description,
                "inputSchema": optional_repo_id(name, schema),
            });
            if let Some(annotations) = tool_annotations(name) {
                tool["annotations"] = serde_json::to_value(annotations).unwrap_or(Value::Null);
            }
            if WRITE_TOOLS.contains(&name) {
                tool["_meta"] = json!({"com.corkcli/write": true});
            }
            tool
        })
        .collect()
}

/// The MCP protocol revisions this server's SDK negotiates, newest first.
pub fn protocol_versions() -> Vec<Value> {
    rmcp::model::ProtocolVersion::KNOWN_VERSIONS
        .iter()
        .rev()
        .filter_map(|v| serde_json::to_value(v).ok())
        .collect()
}

/// The MCP Server Card (SEP-2127, `server-card.schema.json` v1): identity,
/// the remote endpoint and the protocol versions, and by that spec's rule no
/// tools — those are in [`manifest`].
pub fn server_card(mcp_url: &str) -> Value {
    json!({
        "$schema": "https://static.modelcontextprotocol.io/schemas/v1/server-card.schema.json",
        "name": "com.corkcli/mcp",
        "title": "Cork MCP",
        "description": "Keeps AI coding agents on one codebase from overwriting each other, and briefs each on its code.",
        "version": env!("CARGO_PKG_VERSION"),
        "websiteUrl": "https://corkcli.com/docs/mcp-tools",
        "repository": {"url": "https://github.com/corkcli/cork", "source": "github"},
        "icons": [{"src": "https://corkcli.com/brand/cork-mark-black.svg", "mimeType": "image/svg+xml"}],
        "remotes": [{
            "type": "streamable-http",
            "url": mcp_url,
            "supportedProtocolVersions": protocol_versions(),
        }],
    })
}

/// The public MCP manifest: where the server is, how to get a credential,
/// and the tools it lists, so a client or a scanner can inspect them before
/// signing in. Listing needs no credential here; every call does.
pub fn manifest(mcp_url: &str) -> Value {
    let tools = public_tools();
    json!({
        "name": "Cork MCP",
        "server_id": "com.corkcli/mcp",
        "kind": "product",
        "description": "Cork shows each AI coding agent what every other agent and person on the same codebase is changing, live: briefings before work, collision checks before writes, notes and messages between agents.",
        "version": env!("CARGO_PKG_VERSION"),
        "url": mcp_url,
        "transport": "streamable-http",
        "supportedProtocolVersions": protocol_versions(),
        "websiteUrl": "https://corkcli.com",
        "documentation": "https://corkcli.com/docs/mcp-tools",
        "server_card": "https://corkcli.com/.well-known/mcp/server-card.json",
        // whether the service is up, for people and for agents
        "status_page": "https://corkcli.com/status",
        "status": "https://corkcli.com/status.json",
        "capabilities": {"tools": true, "resources": true, "prompts": false},
        "authentication": {
            "required": true,
            "type": "oauth2",
            "flows": ["authorization_code"],
            "pkce": "S256",
            "scopes": {"collide": "Act for the signed-in person in their workspaces: read briefings and activity, report edits, message teammates' agents. A viewer seat reads only."},
            "protected_resource_metadata": format!("{mcp_url}/.well-known/oauth-protected-resource"),
            "authorization_server_metadata": format!("{mcp_url}/.well-known/oauth-authorization-server"),
            "registration_endpoint": format!("{mcp_url}/register"),
            "authorization_endpoint": format!("{mcp_url}/authorize"),
            "token_endpoint": format!("{mcp_url}/token"),
            // or the agent registers itself (auth.md's identity endpoint);
            // identity_assertion too when a provider is trusted, per the
            // authorization server metadata
            "agent_registration": {
                "identity_endpoint": format!("{mcp_url}/agent/identity"),
                "claim_endpoint": format!("{mcp_url}/agent/identity/claim"),
                "identity_types": ["anonymous", "service_auth"],
                "sandbox": "anonymous: a sandbox token at once, with no account: sandbox/<name> repos, the sandbox's tools, no agent hours, 1,000 tool calls at 2 a second, 24 hours. Claiming it with a person's email makes it their agent.",
            },
            "docs": "https://corkcli.com/auth.md",
        },
        "tools_note": "The menu an unauthenticated client is shown. Calling any tool needs a credential: a person's, or an anonymous registration's sandbox token for the sandbox's tools. A viewer seat is not shown the write tools (marked _meta.com.corkcli/write), and an account whose hooks already run is not shown report_edit, check_collisions, heartbeat, declare_intent, complete_intent or sync_agents_block; those still answer when called.",
        "tool_count": tools.len(),
        "tools": tools,
    })
}

#[cfg(test)]
mod public_listing_tests {
    use super::*;

    const SITE: &str = "https://mcp.corkcli.com";

    /// The website's copies (nextjs-app/public/.well-known) are this
    /// server's own answer, written by this test: a tool added, renamed or
    /// re-described here fails the test until the files are regenerated with
    /// CORK_WRITE_MCP_CARDS=1.
    #[test]
    fn the_websites_manifest_and_card_match_the_tool_table() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../nextjs-app/public/.well-known");
        if !root.is_dir() {
            return; // built outside the monorepo
        }
        let files = [
            (root.join("mcp.json"), manifest(SITE)),
            (root.join("mcp/server-card.json"), server_card(SITE)),
        ];
        for (path, expected) in files {
            let text = serde_json::to_string_pretty(&expected).unwrap() + "\n";
            if crate::env::var("CORK_WRITE_MCP_CARDS").as_deref() == Ok("1") {
                std::fs::write(&path, &text).unwrap();
                continue;
            }
            let found = std::fs::read_to_string(&path).unwrap_or_default();
            assert_eq!(found, text, "{} is stale: rerun with CORK_WRITE_MCP_CARDS=1", path.display());
        }
    }

    #[test]
    fn the_public_menu_is_the_unauthenticated_list() {
        let tools = public_tools();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"get_briefing") && names.contains(&"remember") && names.contains(&"claim"));
        assert!(!names.iter().any(|n| HIDDEN_TOOLS.contains(n)), "{names:?}");
        for tool in &tools {
            assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
            assert!(tool["annotations"].get("readOnlyHint").is_some(), "{tool}");
        }
        let card = server_card(SITE);
        assert!(card.get("tools").is_none(), "SEP-2127: a card lists no primitives");
        assert!(card["remotes"][0]["supportedProtocolVersions"].as_array().is_some_and(|v| !v.is_empty()));
        assert!(card["description"].as_str().unwrap().len() <= 100);
    }
}

#[cfg(test)]
mod repo_default_tests {
    use super::*;

    #[test]
    fn repo_id_is_optional_where_the_server_fills_it_in() {
        assert!(defaults_repo_id("get_symbol") && defaults_repo_id("send_agent_message") && defaults_repo_id("claim"));
        assert!(!defaults_repo_id("setup") && !defaults_repo_id("move_repo"));
        for (name, _, schema) in tool_table() {
            let listed = optional_repo_id(name, schema.clone());
            let required: Vec<&str> = listed.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            if defaults_repo_id(name) {
                assert!(!required.contains(&"repo_id"), "{name}");
                assert!(listed.pointer("/properties/repo_id/description").and_then(Value::as_str).unwrap_or("").contains("Omit it"), "{name}");
            } else {
                assert_eq!(listed, schema, "{name}");
            }
        }
    }
}
