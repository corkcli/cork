//! The write gate.
//!
//! PreToolUse asks this whether a write may proceed. It blocks only on
//! confidence-certain collisions — a rename, a removal or a signature change
//! by someone else — touching the exact path being written, and it fails open
//! on anything else at all. A gate that wrongly blocks is worse than no gate:
//! agents learn to route around it.
//!
//! An enforce-mode check IS a check before write, performed mechanically on
//! every Edit. Recording only the blocks left the ledger unable to tell "the
//! gate cleared this path" from "nobody looked", which is what made the
//! strictest repos score worst on check-before-write. So the clear is
//! recorded too.

use serde_json::{json, Value};

use crate::repo::path_key;
use crate::store::{now, Store};

const BLOCKING_KINDS: [&str; 3] = ["renamed", "removed", "signature"];
const CERTAIN: &str = "certain";
const BLOCKED_TTL_S: f64 = 600.0;
/// How long the gate's "editing" presence marker stands. PreToolUse fires
/// before EVERY Edit/Write/MultiEdit/apply_patch/Bash, so an agent that is
/// actually writing re-arms this many times a minute; 60s is long enough to
/// bridge one slow tool call (a big MultiEdit, a long build) without the line
/// flickering, and short enough that an agent which stopped writing — or
/// whose write was blocked and abandoned — stops claiming to edit within a
/// minute. A stale "editing" line is worse than a missing one, and
/// report_edit deletes this marker the moment the write actually lands.
const EDITING_TTL_S: f64 = 60.0;
const MAX_PATH: usize = 300;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Collisions, blocks and overrides leave scar tissue on a path: a running
/// score in the shared KV, so nothing has to scan the ledger on the request
/// path.
fn bump_salience(store: &Store, scope: &str, path: &str, field: &str) {
    let key = format!("{scope}:{}", path_key(path));
    let mut record = store.kv_get("salience", &key).unwrap_or_else(|| {
        json!({"path": path, "collisions": 0, "overrides": 0, "blocks": 0})
    });
    let Some(map) = record.as_object_mut() else { return };
    let current = map.get(field).and_then(Value::as_i64).unwrap_or(0);
    map.insert(field.to_string(), json!(current + 1));
    let read = |name: &str| map.get(name).and_then(Value::as_i64).unwrap_or(0);
    let score = 3 * read("collisions") + 4 * read("overrides") + 2 * read("blocks");
    map.insert("score".into(), json!(score));
    let stamp = now();
    map.insert("updated".into(), json!(stamp));
    let _ = store.kv_put("salience", &key, &record, stamp);
}

/// The pre-write gate IS presence. It fires before EVERY Edit/Write, which
/// makes it the earliest honest moment the dashboard can say "editing
/// <path>" — earlier than any report, which only arrives once the write has
/// already landed. (The other source of that line is the `draft:` marker, and
/// no hook has ever sent draft=true.) `report_edit` deletes this marker when
/// the write lands, flipping the row from "editing <path>" to the last-action
/// line with no extra bookkeeping.
///
/// Written DIRECTLY, not through `touch_presence`: that one returns early
/// when any focus marker is under 60s old, so a read half a minute earlier
/// would silently swallow this.
///
/// Per agent: the gate carries the session, so the marker is THIS agent's
/// and it inherits from THIS agent's standing marker — a second agent of the
/// same person editing elsewhere neither overwrites this line nor lends it
/// its model.
fn mark_editing(store: &Store, scope: &str, user_id: &str, session: &str, path: &str, via: &str) {
    let key = crate::presence::focus_key(scope, user_id, session);
    let existing = store.eph_get(&key).unwrap_or_else(|| json!({}));
    // the gate payload carries only repo_id, path and session on purpose:
    // agent/model/branch would mean reading the transcript tail, which the
    // hook's fail-open budget cannot afford. So they are INHERITED from the
    // marker already standing — an empty model BLANKS the dashboard's agent
    // chip, because model resolution takes whichever of focus/lastedit is
    // newer and returns its model verbatim.
    let agent = text(&existing, "agent");
    let model = text(&existing, "model");
    // and the effort that model ran at, inherited with it
    let effort = crate::presence::Effort::of(&existing);
    let shown = crate::presence::clip(path, MAX_PATH);
    let agent_id = crate::presence::agent_id(user_id, session);
    let mut marker = json!({"user": user_id, "session": session, "agent_id": &agent_id,
                            "repo_id": crate::presence::split_scope(scope).1,
                            "path": &shown, "action": "editing",
                            "agent": &agent, "model": &model, "branch": text(&existing, "branch"),
                            "ts": now()});
    effort.tag_value(&mut marker);
    let _ = store.eph_set(&key, &marker, Some(EDITING_TTL_S));
    // liveness: the dashboard polls every 10s anyway, but any event refreshes
    // it immediately. NOTE for the dashboard: the Activity feed renders a
    // focus event that is not running/searching as "read <path>", so it needs
    // an "editing" case of its own.
    let mut event = json!({"kind": "focus", "user": user_id, "session": session,
                           "agent_id": &agent_id, "path": &shown,
                           "action": "editing", "agent": &agent});
    if !model.is_empty() {
        if let Some(map) = event.as_object_mut() {
            map.insert("model".into(), json!(model));
            effort.tag(map);
        }
    }
    crate::events::publish(store, scope, event, via);
    crate::presence::nudge_workspace(store, scope);
}

pub struct Decision {
    pub body: Value,
}

/// One ephemeral scan, then a verdict. Any internal failure allows the write.
/// Whether any agent holds a hot marker on this path: the cheap test before a
/// read is parsed for `acknowledge_read`.
pub fn has_blocking_marker(store: &Store, scope: &str, path: &str) -> bool {
    !store.eph_scan(&format!("hot:{scope}:{}:", path_key(path))).is_empty()
}

/// The agent whose write a hot marker is: its session, or one of the
/// session's helpers (presence::agent_session).
fn marker_agent(marker: &Value) -> String {
    crate::presence::agent_session(&text(marker, "session"), &text(marker, "worker"))
}

/// The key that records one agent having one blocking change in its copy.
fn ack_key(scope: &str, user_id: &str, session: &str, path: &str, marker: &Value, event: &Value) -> String {
    // fixed decimals, so the key never depends on float formatting
    let what = format!(
        "{}|{}|{}|{}|{:.6}",
        text(marker, "user"), text(marker, "session"), text(event, "kind"), text(event, "symbol"),
        marker.get("ts").and_then(Value::as_f64).unwrap_or(0.0)
    );
    format!("gateack:{scope}:{}:{}:{}", crate::presence::agent_id(user_id, session), path_key(path), crate::hashing::sha256_hex(&what).get(..16).unwrap_or(""))
}

/// Whether a copy whose symbols are `symbols` (name to signature) already
/// holds a teammate's blocking change `event`: what lets a read clear a
/// block, and what tells an adapted write from one that overrode it.
pub fn reflected(event: &Value, symbols: &std::collections::BTreeMap<String, String>) -> bool {
    let symbol = text(event, "symbol");
    match text(event, "kind").as_str() {
        // the copy has the new name: it has the teammate's change. The
        // old name may rightly still be there when the "rename" was two
        // different symbols; demanding it gone held an agent that had
        // pulled everything, five times (Study 5, medium)
        "renamed" => {
            let new_name = text(event, "new_name");
            !new_name.is_empty() && symbols.contains_key(&new_name)
        }
        "removed" => !symbols.contains_key(&symbol),
        "signature" => {
            let after = text(event, "signature_after");
            !after.is_empty() && symbols.get(&symbol).map(String::as_str) == Some(after.as_str())
        }
        _ => false,
    }
}

/// A read of `path` by this agent: `symbols` is what ITS copy holds, name to
/// signature. Each certain change another agent made there that the copy
/// already reflects is acknowledged, so the gate stops blocking it. Without
/// this a block lasted until the marker expired, whatever the agent did: in
/// the staging demo an agent pulled the rename, re-read the file, and was
/// still blocked twelve times, then fixed the bug somewhere worse.
pub fn acknowledge_read(
    store: &Store, scope: &str, user_id: &str, session: &str, path: &str,
    symbols: &std::collections::BTreeMap<String, String>,
) -> usize {
    let mut acked = 0;
    for (_key, marker) in store.eph_scan(&format!("hot:{scope}:{}:", path_key(path))) {
        if text(&marker, "path") != path {
            continue;
        }
        let same_user = text(&marker, "user") == user_id;
        let marker_session = text(&marker, "session");
        if same_user && (session.is_empty() || marker_session.is_empty() || marker_agent(&marker) == session) {
            continue;
        }
        for event in marker.get("events").and_then(Value::as_array).unwrap_or(&Vec::new()) {
            if reflected(event, symbols) {
                let _ = store.eph_set(
                    &ack_key(scope, user_id, session, path, &marker, event),
                    &json!({"ts": now()}),
                    Some(BLOCKED_TTL_S.max(7_200.0)),
                );
                acked += 1;
            }
        }
    }
    acked
}

#[allow(clippy::too_many_arguments)]
pub fn gate(
    store: &Store, scope: &str, user_id: &str, session: &str, worker: &str, path: &str, dashboard_url: &str, via: &str,
) -> Decision {
    gate_with(store, scope, user_id, session, worker, path, dashboard_url, via, None, false)
}

/// How the reader knows a marker's author: "another session of yours", or
/// the teammate as `nickname (email)` (never a hook's own name).
fn author_label(store: &Store, user_id: &str, author: &str) -> String {
    if author == user_id {
        return "another session of yours".to_string();
    }
    let users: std::collections::BTreeSet<String> = [author.to_string()].into_iter().filter(|u| !u.is_empty()).collect();
    crate::activity::identity_labels(store, user_id, &users)
        .get(author)
        .and_then(|l| l.get("label"))
        .and_then(Value::as_str)
        .unwrap_or(author)
        .to_string()
}

/// A teammate's certain change that is still only in their checkout, as a
/// line: what to write against now. Never "pull": there is nothing on
/// origin to pull, and an agent sent to pull it polls, retries, and in Study
/// 8 bypassed the gate with `sed`.
pub fn unpushed_line(author: &str, path: &str, event: &Value) -> String {
    let symbol = text(event, "symbol");
    // older markers carry the new name and signature only in `detail`
    let detail = event.get("detail").cloned().unwrap_or(Value::Null);
    let or_detail = |own: String, key: &str| if own.is_empty() { text(&detail, key) } else { own };
    let new_name = or_detail(text(event, "new_name"), "new_name");
    let after = {
        let given = or_detail(text(event, "signature_after"), "signature");
        let said = detail.as_str().and_then(|d| d.strip_prefix("signature changed to: ")).unwrap_or("").to_string();
        if given.is_empty() { said } else { given }
    };
    match text(event, "kind").as_str() {
        "renamed" if !new_name.is_empty() => format!(
            "Cork: {author} renamed `{symbol}` to `{new_name}` in {path} (not pushed yet): write against `{new_name}`; landing will rebase onto their change."
        ),
        "removed" => format!(
            "Cork: {author} removed `{symbol}` from {path} (not pushed yet): add no new use of it; landing will rebase onto their change."
        ),
        _ if !after.is_empty() => format!(
            "Cork: {author} changed `{symbol}` in {path} to `{after}` (not pushed yet): write against `{after}`; landing will rebase onto their change."
        ),
        kind => format!("Cork: {author} {kind} `{symbol}` in {path} (not pushed yet); landing will rebase onto their change."),
    }
}

/// What to write against after a teammate's change: the new name, else the
/// new signature ("" for a removal).
fn unpushed_target(event: &Value) -> String {
    let detail = event.get("detail").cloned().unwrap_or(Value::Null);
    let pick = |own: &str, key: &str| {
        let given = text(event, own);
        if given.is_empty() { text(&detail, key) } else { given }
    };
    let new_name = pick("new_name", "new_name");
    if !new_name.is_empty() {
        return new_name;
    }
    let after = pick("signature_after", "signature");
    if !after.is_empty() {
        return after;
    }
    detail.as_str().and_then(|d| d.strip_prefix("signature changed to: ")).unwrap_or("").to_string()
}

/// The gate, with what the hook knows about the write (`write`: the names it
/// adds and changes, hooks v75+) and whether this session lands its pushes.
///
/// A teammate's rename, removal or new signature blocks only when it is
/// certain AND on origin (their session landed after it): then a pull and a
/// re-read clear it, and it blocks once. One still only in their checkout
/// is a line, never a block: no pull can bring it in, and a block nothing
/// can clear is the phantom block of Study 8.
#[allow(clippy::too_many_arguments)]
pub fn gate_with(
    store: &Store, scope: &str, user_id: &str, session: &str, worker: &str, path: &str, dashboard_url: &str, via: &str,
    write: Option<&crate::freshness::WriteFacts>, lands: bool,
) -> Decision {
    let prefix = format!("hot:{scope}:{}:", path_key(path));
    let mut blocking: Vec<Value> = Vec::new();
    let mut notices: Vec<String> = Vec::new();
    // the facts those lines state, as deltas and the hook's told list name
    // them: said here, so not again by a delta or the lint
    let mut facts: Vec<String> = Vec::new();
    let mut unpushed: Vec<Value> = Vec::new();
    // this agent: the session, or the helper of it the hook names. Its copy,
    // branch and what it was told are its own; a sibling helper's live work
    // is a teammate's (presence::agent_session)
    let me = crate::presence::agent_session(session, worker);
    // the branch this session's work lands on (branches.rs): a teammate on
    // another branch meets it only when the branches merge
    let mine = crate::branches::of(store, scope, user_id, &me);
    for (_key, marker) in store.eph_scan(&prefix) {
        if text(&marker, "path") != path {
            continue;
        }
        // my own marker is mine only when it is my agent's: another
        // session of the same user, or another helper of mine, is another agent
        let same_user = text(&marker, "user") == user_id;
        let marker_session = text(&marker, "session");
        let theirs = marker_agent(&marker);
        if same_user && (session.is_empty() || marker_session.is_empty() || theirs == me) {
            continue;
        }
        // one working copy (an earlier session of this person in this
        // folder, a helper in the same folder): its change is already on
        // this disk, not a teammate's unpushed work
        if crate::freshness::same_copy(store, scope, (user_id, &me), (&text(&marker, "user"), &theirs)) {
            continue;
        }
        let author = if same_user {
            let sibling = crate::presence::sibling_label(store, scope, user_id, &theirs, &me);
            if sibling.is_empty() { author_label(store, user_id, &text(&marker, "user")) } else { sibling }
        } else {
            author_label(store, user_id, &text(&marker, "user"))
        };
        // another branch: never a block, one line per file pair
        let marked = text(&marker, "branch");
        let elsewhere = crate::branches::apart(store, scope, &mine, (&text(&marker, "user"), &theirs))
            .or_else(|| (!mine.is_empty() && !marked.is_empty() && marked != mine).then_some(marked));
        if let Some(on) = elsewhere {
            let symbols: Vec<String> = marker.get("events").and_then(Value::as_array).into_iter().flatten()
                .filter(|e| BLOCKING_KINDS.contains(&text(e, "kind").as_str()) && text(e, "confidence") == CERTAIN)
                .map(|e| format!("`{}`", text(e, "symbol"))).take(3).collect();
            if !symbols.is_empty()
                && crate::branches::first_note(store, scope, (user_id, &me), &on, path)
            {
                notices.push(crate::branches::note_line(&author, &symbols.join(", "), path, &on));
            }
            continue;
        }
        let marker_ts = marker.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        let pushed = crate::freshness::landed_since(store, scope, &text(&marker, "user"), &marker_session, marker_ts);
        for event in marker.get("events").and_then(Value::as_array).unwrap_or(&Vec::new()) {
            let kind = text(event, "kind");
            if !BLOCKING_KINDS.contains(&kind.as_str()) || text(event, "confidence") != CERTAIN {
                continue;
            }
            let acked = ack_key(scope, user_id, &me, path, &marker, event);
            // this agent already has the change in its copy (it pulled and
            // read the file): the conflict the block prevents is gone
            if store.eph_get(&acked).is_some() {
                continue;
            }
            if !pushed {
                // said once, as what to write against
                if access_once(store, &format!("gatenote:{acked}")) {
                    notices.push(unpushed_line(&author, path, event));
                    // the hook looks at origin for it: pushed some other
                    // way than landing, it is still a pull (gate.rs, hooks)
                    let mut annotated = event.clone();
                    if let Some(map) = annotated.as_object_mut() {
                        map.insert("author".into(), json!(author));
                    }
                    unpushed.push(annotated);
                    let kind = if kind == "signature" { "modified".to_string() } else { kind.clone() };
                    facts.extend(crate::deltas::fact_id(
                        path, &text(event, "symbol"), &kind, &text(event, "signature_before"),
                        &text(event, "signature_after"), &text(event, "new_name")));
                }
                continue;
            }
            // pushed: certain, and on origin. Held once per (agent, file,
            // teammate session); the retry goes through whatever it holds
            // (Study 8, second study: an agent mid-rebase, whose copy no read
            // could acknowledge, was held fifteen times). The hook checks it
            // against this checkout's origin before it stands.
            let once = format!("gateonce:{scope}:{}:{}:{}#{theirs}",
                crate::presence::agent_id(user_id, &me), path_key(path), text(&marker, "user"));
            if store.eph_get(&once).is_some() {
                continue;
            }
            let mut annotated = event.clone();
            if let Some(map) = annotated.as_object_mut() {
                map.insert("author".into(), json!(author));
                map.insert("pushed".into(), json!(true));
                map.insert("once".into(), json!(once));
                // whose change it is, by agent: the step it held keeps a
                // link to theirs (clash.rs)
                map.insert("by_user".into(), json!(text(&marker, "user")));
                map.insert("by_session".into(), json!(marker_session));
            }
            blocking.push(annotated);
        }
    }

    // the clear is evidence too: this is what lets compliance tell a gated
    // write from an unobserved one
    store.ledger_append_later(
        scope,
        "check_performed",
        &json!({
            "user": user_id, "session": session, "agent": "collide-gate-hook",
            "paths": [path], "symbols": [], "auto": true,
        }),
        now(),
    );
    if worker.is_empty() {
        mark_editing(store, scope, user_id, session, path, via);
    } else {
        // a subagent's write: its own marker says so, and the session's
        // focus (a command of its own still running) stands
        crate::presence::note_worker_activity(store, scope, user_id, session, worker, "", "", "editing", path, false);
    }

    // an owner's standing decision over this path, met for the first time by
    // this agent (or this subagent, when the hook names it): held once, with
    // the decision as the reason; the retry goes through. Fails open.
    if let Some(reason) = crate::decisions::hold(store, scope, user_id, session, worker, path, via) {
        return Decision { body: json!({"allow": false, "path": path, "reason": reason, "collisions": [], "decision": true}) };
    }

    if blocking.is_empty() {
        // no live rename/removal here, but the copy may still be behind a
        // teammate's change to this very file: said once, before the write
        let mut behind = crate::freshness::behind(store, scope, user_id, &me, path);
        let who = behind.as_ref().map(|b| {
            let sibling = if b.user == user_id { crate::presence::sibling_label(store, scope, user_id, &b.session, &me) } else { String::new() };
            if !sibling.is_empty() {
                sibling
            } else if b.user == user_id {
                "Another session of yours".to_string()
            } else {
                author_label(store, user_id, &b.user)
            }
        });
        // a copy behind a teammate on another branch is not behind: their
        // write meets this one when the branches merge, said once
        if let (Some(b), Some(who)) = (&behind, &who) {
            if let Some(on) = crate::branches::apart(store, scope, &mine, (&b.user, &b.session)) {
                if crate::branches::first_note(store, scope, (user_id, &me), &on, path) {
                    notices.push(crate::branches::note_line(who, "", path, &on));
                }
                behind = None;
            }
        }
        let who = if behind.is_some() { who } else { None };
        let said = who.and_then(|who| crate::freshness::gate_note(store, scope, user_id, &me, path, &who, write, lands));
        match said {
            Some(crate::freshness::Say::Block(reason)) => {
                // what was said once about a teammate's unpushed change rides
                // the block, or it would be lost with the write
                let reason = if notices.is_empty() { reason } else { format!("{reason}\n{}", notices.join("\n")) };
                // the stale write it stopped, on the ledger like every other
                // block: the waste report counts it from this row
                let mut body = json!({"allow": false, "path": path, "reason": reason, "collisions": [], "stale_copy": true, "facts": facts});
                if let Some(b) = &behind {
                    body["pushed"] = json!(b.pushed);
                    body["by_user"] = json!(b.user);
                    body["by_session"] = json!(b.session);
                    // the agent held: a helper's catch is its own, not its session's
                    let _ = store.ledger_append(scope, "gate_block", &json!({
                        "user": user_id, "session": session, "worker": worker, "path": path, "events": [],
                        "stale_copy": true, "by": b.user, "pushed": b.pushed, "written_at": b.written_at,
                    }), now());
                    // what the agent does next settles it (outcomes.rs)
                    crate::outcomes::open(store, scope, "stale_copy", user_id, session, &[path.to_string()],
                        json!({"by": b.user, "written_at": b.written_at}), now());
                }
                crate::events::publish(store, scope, json!({"kind": "gate", "action": "block", "user": user_id, "path": path, "reason": reason}), via);
                return Decision { body };
            }
            Some(crate::freshness::Say::Notice(line)) => notices.push(line),
            None => {}
        }
        let mut body = json!({"allow": true, "path": path});
        if !notices.is_empty() {
            body["notice"] = json!(notices.join("\n"));
            body["facts"] = json!(facts);
            body["unpushed"] = json!(unpushed);
        }
        return Decision { body };
    }

    let first = &blocking[0];
    let detail = match first.get("detail") {
        Some(Value::Object(map)) => format!(
            "now `{}` — {}",
            map.get("new_name").and_then(Value::as_str).unwrap_or(""),
            map.get("signature").and_then(Value::as_str).unwrap_or("")
        ),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let see = if dashboard_url.is_empty() {
        String::new()
    } else {
        format!(
            " Cork just prevented a blind conflict — the human can see it at {dashboard_url}/dashboard."
        )
    };
    let kind = text(first, "kind");
    let what = match kind.as_str() {
        "renamed" => "renamed",
        "removed" => "removed",
        "signature" => "changed the signature of",
        other => other,
    };
    let reason = if lands {
        // landing rebases onto it: no pull, only what to write against
        let against = unpushed_target(first);
        let to = if against.is_empty() { "their change".to_string() } else { format!("`{against}`") };
        format!(
            "{} {what} `{}` in {path} ({detail}) and pushed it; your copy does not have it yet. Write against {to}: landing \
will rebase onto it when you push. Retry the write and it goes through.{see}",
            text(first, "author"),
            text(first, "symbol"),
        )
    } else {
        format!(
            "{} {what} `{}` in {path} ({detail}) and pushed it. Pull that change and re-read {path}: once your copy has it, this write goes through.{see}",
            text(first, "author"),
            text(first, "symbol"),
        )
    };
    // held once per teammate session: the retry goes through
    for event in &blocking {
        let once = text(event, "once");
        if !once.is_empty() {
            let _ = store.eph_set(&once, &json!({"ts": now()}), Some(BLOCKED_TTL_S.max(7_200.0)));
        }
    }
    let blocking: Vec<Value> = blocking.into_iter().map(|mut e| {
        if let Some(map) = e.as_object_mut() {
            map.remove("once");
        }
        e
    }).collect();

    let stamp = now();
    // repo-scoped still — a block is about this repo's code — but keyed by
    // AGENT, so two agents of one person blocked on the same path no longer
    // share (and overwrite) one marker
    let _ = store.eph_set(
        &format!("blocked:{scope}:{}:{}", crate::presence::agent_id(user_id, session), path_key(path)),
        &json!({"path": path, "ts": stamp, "reason": reason}),
        Some(BLOCKED_TTL_S),
    );
    bump_salience(store, scope, path, "blocks");
    let _ = store.ledger_append(
        scope,
        "gate_block",
        &json!({"user": user_id, "session": session, "worker": worker, "path": path, "events": blocking}),
        stamp,
    );
    crate::outcomes::open(store, scope, "teammate_change", user_id, session, &[path.to_string()],
        json!({"events": blocking}), stamp);

    crate::events::publish(
        store,
        scope,
        json!({"kind": "gate", "action": "block", "user": user_id,
               "path": path, "reason": reason}),
        via,
    );

    let mut body = json!({
        "allow": false, "path": path, "reason": reason, "collisions": blocking,
    });
    if !notices.is_empty() {
        body["notice"] = json!(notices.join("\n"));
        body["facts"] = json!(facts);
        body["unpushed"] = json!(unpushed);
    }
    Decision { body }
}

/// True the first time `key` is asked about (kept as long as a block is).
fn access_once(store: &Store, key: &str) -> bool {
    if store.eph_get(key).is_some() {
        return false;
    }
    let _ = store.eph_set(key, &json!({"ts": now()}), Some(BLOCKED_TTL_S.max(7_200.0)));
    true
}

/// How long a gate verdict waits for its write's report.
const VERDICT_TTL_S: f64 = 900.0;

fn verdict_key(scope: &str, user_id: &str, session: &str, path: &str) -> String {
    format!("gatex:{scope}:{}:{}", crate::presence::agent_id(user_id, session), path_key(path))
}

/// The first line of a gate's words, short: what the dropdown says why.
fn first_line(text: &str) -> String {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(120).collect()
}

/// The gate's verdict on a write, kept for that write's report to put on
/// its step (report.rs, presence::note_exchange): allowed, held (an owner's
/// standing decision) or blocked, the first line of why, and how many
/// times this agent was stopped on the path before the write went through.
pub fn note_verdict(store: &Store, scope: &str, user_id: &str, session: &str, path: &str, decision: &Value) {
    let key = verdict_key(scope, user_id, session, path);
    let allowed = decision.get("allow").and_then(Value::as_bool).unwrap_or(true);
    let verdict = if allowed {
        "allowed"
    } else if decision.get("decision").and_then(Value::as_bool) == Some(true) {
        "held"
    } else {
        "blocked"
    };
    let earlier = store.eph_get(&key);
    let stops = earlier.as_ref().and_then(|e| e.get("stops")).and_then(Value::as_u64).unwrap_or(0)
        + u64::from(earlier.as_ref().is_some_and(|e| e.get("v").and_then(Value::as_str) != Some("allowed")));
    let why = first_line(
        decision.get("reason").and_then(Value::as_str).or_else(|| decision.get("notice").and_then(Value::as_str)).unwrap_or(""),
    );
    let mut record = json!({"v": verdict});
    if !why.is_empty() {
        record["why"] = json!(why);
    }
    if stops > 0 {
        record["stops"] = json!(stops);
    }
    // a stop names whose agent's change it met (a held decision names none):
    // kept through the retry that goes through, for the collision on the
    // write's step (clash.rs)
    let link = stop_link(decision, verdict, &why).or_else(|| earlier.as_ref().and_then(|e| e.get("link")).cloned());
    if let Some(link) = link {
        record["link"] = link;
    }
    let _ = store.eph_set(&key, &record, Some(VERDICT_TTL_S));
}

/// What a stop said about the other side: whose agent (`w`, `ws`), the
/// kind of stop, the symbol and its signature before and after, and why.
fn stop_link(decision: &Value, verdict: &str, why: &str) -> Option<Value> {
    if verdict == "allowed" {
        return None;
    }
    let first = decision.get("collisions").and_then(Value::as_array).and_then(|c| c.first());
    let pick = |key: &str| first.map(|e| text(e, key)).filter(|v| !v.is_empty()).unwrap_or_else(|| text(decision, key));
    let kind = if verdict == "held" {
        "held"
    } else if decision.get("stale_copy").and_then(Value::as_bool) == Some(true) {
        "stale"
    } else {
        "gate"
    };
    let mut link = json!({"k": kind, "why": why});
    for (from, to) in [("by_user", "w"), ("by_session", "ws"), ("symbol", "s")] {
        let value = pick(from);
        if !value.is_empty() {
            link[to] = json!(value);
        }
    }
    if let Some(event) = first {
        let (before, after) = (text(event, "signature_before"), text(event, "signature_after"));
        let after = if after.is_empty() { text(event, "new_name") } else { after };
        if !before.is_empty() || !after.is_empty() {
            link["sig"] = json!([before, after]);
        }
    }
    Some(link)
}

/// The links `note_verdict` keeps are the dashboard's: the hook's answer
/// goes without them.
pub fn strip_links(decision: &mut Value) {
    if let Some(map) = decision.as_object_mut() {
        map.remove("by_user");
        map.remove("by_session");
    }
    if let Some(list) = decision.get_mut("collisions").and_then(Value::as_array_mut) {
        for event in list.iter_mut().filter_map(Value::as_object_mut) {
            event.remove("by_user");
            event.remove("by_session");
        }
    }
}

/// The verdict `note_verdict` kept for this write, taken (once).
pub fn take_verdict(store: &Store, scope: &str, user_id: &str, session: &str, path: &str) -> Option<Value> {
    let key = verdict_key(scope, user_id, session, path);
    let verdict = store.eph_get(&key)?;
    store.eph_delete(&key);
    Some(verdict)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn rename_marker(db: &Store, scope: &str, path: &str, user: &str, session: &str, ts: f64) {
        let _ = db.eph_set(
            &format!("hot:{scope}:{}:{user}#{session}", path_key(path)),
            &json!({"path": path, "user": user, "session": session, "ts": ts,
                    "events": [{"symbol": "amount_due", "kind": "renamed", "confidence": "certain",
                                "new_name": "checkout_total", "detail": {"new_name": "checkout_total", "signature": "def checkout_total(lines)"}}]}),
            Some(600.0),
        );
    }

    /// Study 8's phantom block: a rename recorded by a teammate's session
    /// that never landed held another agent four times ("pull that change"),
    /// with nothing on origin to pull. Unpushed, it is a line saying what to
    /// write against, said once; landed, it blocks once and then lets the
    /// retry through.
    #[test]
    fn an_unpushed_rename_is_never_a_block_and_never_a_pull() {
        let db = store();
        let (scope, path) = ("w:r", "shop/total.py");
        rename_marker(&db, scope, path, "kevin@x.com", "a1", now());
        let first = gate_with(&db, scope, "kevin@x.com", "b1", "", path, "", "t", None, true).body;
        assert_eq!(first["allow"], json!(true), "{first}");
        let notice = text(&first, "notice");
        assert!(notice.contains("another session of yours renamed `amount_due` to `checkout_total`")
            && notice.contains("not pushed yet): write against `checkout_total`; landing will rebase"), "{notice}");
        assert!(!notice.to_lowercase().contains("pull"), "{notice}");
        let again = gate_with(&db, scope, "kevin@x.com", "b1", "", path, "", "t", None, true).body;
        assert_eq!(again, json!({"allow": true, "path": path}), "said once");

        // the same rename, landed: held once, and with landing on it says
        // what to write against, never "pull"; the retry goes through
        let db = store();
        rename_marker(&db, scope, path, "kevin@x.com", "a1", now() - 1.0);
        db.ledger_append(scope, "landed", &json!({"user": "kevin@x.com", "session": "a1"}), now()).unwrap();
        let first = gate_with(&db, scope, "kevin@x.com", "b1", "", path, "", "t", None, true).body;
        assert_eq!(first["allow"], json!(false), "{first}");
        let reason = text(&first, "reason");
        assert!(reason.contains("and pushed it") && reason.contains("Write against `checkout_total`: landing will rebase"), "{reason}");
        assert!(!reason.to_lowercase().contains("pull"), "{reason}");
        assert_eq!(first["collisions"][0]["pushed"], json!(true));
        assert!(first["collisions"][0].get("once").is_none(), "the bookkeeping stays on the server");
        let again = gate_with(&db, scope, "kevin@x.com", "b1", "", path, "", "t", None, true).body;
        assert_eq!(again["allow"], json!(true), "held once: {again}");
        // a session that does not land is told to pull, once
        let first = gate_with(&db, scope, "kevin@x.com", "c1", "", path, "", "t", None, false).body;
        assert!(text(&first, "reason").contains("and pushed it. Pull that change"), "{first}");
        assert_eq!(gate_with(&db, scope, "kevin@x.com", "c1", "", path, "", "t", None, false).body["allow"], json!(true));
    }


    /// With landing on, no notice the server writes tells an agent to pull
    /// (Study 8, second study: pull nudges drove 34 manual pulls a round and
    /// most of its remaining conflicts). Every template that can name a
    /// teammate's pushed or unpushed change is rendered with landing on.
    #[test]
    fn no_server_notice_says_pull_when_landing_is_on() {
        let db = store();
        let (scope, path) = ("w:r", "shop/total.py");
        let mut said: Vec<(&str, String)> = Vec::new();
        // the gate on a pushed rename
        rename_marker(&db, scope, path, "bob@x.com", "b1", now() - 1.0);
        db.ledger_append(scope, "landed", &json!({"user": "bob@x.com", "session": "b1"}), now()).unwrap();
        said.push(("gate, pushed", text(&gate_with(&db, scope, "kevin@x.com", "k1", "", path, "", "t", None, true).body, "reason")));
        // the gate on an unpushed rename
        let db2 = store();
        rename_marker(&db2, scope, path, "bob@x.com", "b1", now());
        said.push(("gate, unpushed", text(&gate_with(&db2, scope, "kevin@x.com", "k1", "", path, "", "t", None, true).body, "notice")));
        // a copy behind a teammate's write, pushed and not, and the landing itself
        for pushed in [true, false] {
            let db = store();
            let (v1, v2) = (crate::freshness::fingerprint_content("a\n"), crate::freshness::fingerprint_content("b\n"));
            crate::freshness::note_checkout(&db, scope, "kevin@x.com", "k1", "clone-k");
            crate::freshness::note_checkout(&db, scope, "kevin@x.com", "b1", "clone-b");
            crate::freshness::note_lands(&db, scope, "kevin@x.com", "k1", Some(true));
            crate::freshness::saw(&db, scope, "kevin@x.com", "k1", path, &v1, true);
            crate::deltas::touch_as(&db, scope, "k1", &[path.to_string()], true, now());
            // k1 changed `total` itself: a teammate's change to it is news
            db.ledger_append(scope, "edit_reported", &json!({"path": path, "user": "kevin@x.com", "session": "k1",
                "events": [{"symbol": "total", "kind": "modified", "signature_before": "def total(a)", "signature_after": "def total(a)"}]}), now() - 1.0).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
            crate::freshness::saw(&db, scope, "kevin@x.com", "b1", path, &v2, true);
            if pushed {
                db.ledger_append(scope, "landed", &json!({"user": "kevin@x.com", "session": "b1"}), now()).unwrap();
                crate::freshness::announce_landing(&db, scope, "kevin@x.com", "b1");
                said.extend(crate::inflight::take_notices(&db, scope, "k1").into_iter().map(|t| ("landing", t)));
            }
            if let Some(crate::freshness::Say::Block(t) | crate::freshness::Say::Notice(t)) =
                crate::freshness::gate_note(&db, scope, "kevin@x.com", "k1", path, "Bob", None, true)
            {
                said.push(("stale copy", t));
            }
            // the delta's tail
            crate::deltas::touch_as(&db, scope, "k1", &[path.to_string()], true, now());
            db.ledger_append(scope, "edit_reported", &json!({"path": path, "user": "bob@x.com", "session": "b2",
                "events": [{"symbol": "total", "kind": "modified", "signature_before": "def total(a)", "signature_after": "def total(a, b)"}]}), now()).unwrap();
            let lines = crate::deltas::deltas_told(&db, scope, "kevin@x.com", "k1", now() - 10.0, 0, &Default::default());
            said.extend(lines["lines"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|t| ("delta", t.to_string())));
        }
        // a module in flight that this session imports
        let db3 = store();
        crate::freshness::note_lands(&db3, scope, "kevin@x.com", "k1", Some(true));
        crate::inflight::queue(&db3, scope, "kevin@x.com", "k1", "shop/credits.py", "bob@x.com", "b1", true, "def balance(member)");
        said.extend(crate::inflight::take_notices(&db3, scope, "k1").into_iter().map(|t| ("in flight", t)));
        // the briefing's finish line
        said.extend(crate::brief::finish_line("commit and push when done", "cork commit --mine").map(|t| ("finish line", t)));
        // branch-aware: the gate's line, the in-flight notice and the delta
        // for a teammate on another branch, and the note itself
        let db4 = store();
        rename_marker(&db4, scope, path, "bob@x.com", "b1", now() - 1.0);
        db4.ledger_append(scope, "landed", &json!({"user": "bob@x.com", "session": "b1"}), now()).unwrap();
        crate::branches::note(&db4, scope, "bob@x.com", "b1", "feat/x", "");
        crate::branches::note(&db4, scope, "kevin@x.com", "k1", "agent-k", "main");
        crate::freshness::note_lands(&db4, scope, "kevin@x.com", "k1", Some(true));
        said.push(("branch, gate", text(&gate_with(&db4, scope, "kevin@x.com", "k1", "", path, "", "t", None, true).body, "notice")));
        crate::inflight::queue(&db4, scope, "kevin@x.com", "k1", "shop/credits.py", "bob@x.com", "b1", true, "def balance(member)");
        said.extend(crate::inflight::take_notices(&db4, scope, "k1").into_iter().map(|t| ("branch, in flight", t)));
        crate::deltas::touch_as(&db4, scope, "k1", &[path.to_string()], true, now());
        db4.ledger_append(scope, "edit_reported", &json!({"path": path, "user": "bob@x.com", "session": "b1",
            "events": [{"symbol": "total", "kind": "modified", "signature_before": "def total(a)", "signature_after": "def total(a, b)"}]}), now()).unwrap();
        let lines = crate::deltas::deltas_told(&db4, scope, "kevin@x.com", "k1", now() - 10.0, 0, &Default::default());
        said.extend(lines["lines"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|t| ("branch, delta", t.to_string())));
        said.push(("branch note", crate::branches::note_line("Bob (bob@x.com)", "`amount_due`", path, "feat/x")));
        // a handoff left by a session that stopped mid-task
        let db5 = store();
        db5.ledger_append(scope, "edit_reported", &json!({"user": "bob@x.com", "session": "b1", "path": path,
            "events": [{"symbol": "total", "kind": "modified"}]}), now()).unwrap();
        crate::examples::note_tests(&db5, scope, "bob@x.com", "b1", false, "pytest -q");
        crate::handoff::note_closing(&db5, scope, "b1", "Changed total. Still to do: apply the fee in total_due.");
        let _ = crate::handoff::save(&db5, scope, "bob@x.com", "b1");
        let paths = vec![path.to_string()];
        let receiver = crate::handoff::Receiver { scope, viewer: "kevin@x.com", session: "k1", paths: &paths, identifiers: &[], q: &[], share_knowledge: true };
        said.extend(crate::handoff::brief_lines(&db5, &receiver).into_iter().map(|t| ("handoff", t)));
        // duplicate work: the briefing's line and the other agent's notice
        let db6 = store();
        let names = vec!["refunds_by_day".to_string(), "reports/daily.py".to_string()];
        let _ = crate::overlap::duplicate_task(&db6, scope, "bob@x.com", "b1", &[], &names);
        said.extend(crate::overlap::duplicate_task(&db6, scope, "kevin@x.com", "k1", &[], &names).map(|t| ("duplicate work", t)));
        said.extend(crate::inflight::take_notices(&db6, scope, "b1").into_iter().map(|t| ("duplicate work, notice", t)));
        for (what, t) in &said {
            assert!(!t.to_lowercase().contains("pull"), "{what} says pull with landing on: {t}");
        }
        assert!(said.iter().filter(|(_, t)| !t.is_empty()).count() >= 7, "{said:?}");
        // the round-3 wordings were rendered, not skipped
        assert!(said.iter().any(|(w, t)| *w == "landing" && t.contains("Your next land rebases onto it")), "{said:?}");
        assert!(said.iter().any(|(w, t)| *w == "delta" && t.contains("landing rebases onto them when you push")), "{said:?}");
        // and v75's new texts were rendered too
        for what in ["branch, gate", "branch, in flight", "branch, delta", "branch note", "handoff", "duplicate work", "duplicate work, notice"] {
            assert!(said.iter().any(|(w, t)| *w == what && !t.is_empty()), "{what} was not rendered: {said:?}");
        }
    }

    /// Branch-aware: a landed rename on another branch is never a block, one
    /// line per file and branch; on the same branch (or landing onto it) it
    /// still holds once. An in-flight notice from another branch says when
    /// the work meets, not "landing will rebase".
    #[test]
    fn another_branch_is_a_line_never_a_block() {
        let (scope, path) = ("w:r", "shop/total.py");
        let db = store();
        rename_marker(&db, scope, path, "bob@x.com", "b1", now() - 1.0);
        db.ledger_append(scope, "landed", &json!({"user": "bob@x.com", "session": "b1"}), now()).unwrap();
        crate::branches::note(&db, scope, "bob@x.com", "b1", "feat/x", "");
        crate::branches::note(&db, scope, "kevin@x.com", "k1", "worktree-agent-7", "main");
        let first = gate_with(&db, scope, "kevin@x.com", "k1", "", path, "", "t", None, true).body;
        assert_eq!(first["allow"], json!(true), "{first}");
        assert_eq!(text(&first, "notice"),
            "Cork: bob@x.com is changing `amount_due` in shop/total.py on branch feat/x; this will conflict when the branches merge. No need to stop now.");
        assert_eq!(gate_with(&db, scope, "kevin@x.com", "k1", "", path, "", "t", None, true).body, json!({"allow": true, "path": path}), "said once");
        // the marker's own branch serves once the session record is gone
        let db = store();
        rename_marker(&db, scope, path, "bob@x.com", "b1", now() - 1.0);
        let key = format!("hot:{scope}:{}:bob@x.com#b1", path_key(path));
        let mut marker = db.eph_get(&key).unwrap();
        marker["branch"] = json!("feat/x");
        let _ = db.eph_set(&key, &marker, Some(600.0));
        db.ledger_append(scope, "landed", &json!({"user": "bob@x.com", "session": "b1"}), now()).unwrap();
        crate::branches::note(&db, scope, "kevin@x.com", "k1", "main", "");
        assert_eq!(gate_with(&db, scope, "kevin@x.com", "k1", "", path, "", "t", None, true).body["allow"], json!(true));
        // the same branch, by landing onto it: held once
        let db = store();
        rename_marker(&db, scope, path, "bob@x.com", "b1", now() - 1.0);
        db.ledger_append(scope, "landed", &json!({"user": "bob@x.com", "session": "b1"}), now()).unwrap();
        crate::branches::note(&db, scope, "bob@x.com", "b1", "agent-b", "main");
        crate::branches::note(&db, scope, "kevin@x.com", "k1", "main", "");
        assert_eq!(gate_with(&db, scope, "kevin@x.com", "k1", "", path, "", "t", None, true).body["allow"], json!(false));
        assert_eq!(gate_with(&db, scope, "kevin@x.com", "k1", "", path, "", "t", None, true).body["allow"], json!(true));
        // in flight from another branch
        let db = store();
        crate::branches::note(&db, scope, "bob@x.com", "b1", "feat/x", "");
        crate::branches::note(&db, scope, "kevin@x.com", "k1", "main", "");
        crate::freshness::note_lands(&db, scope, "kevin@x.com", "k1", Some(true));
        crate::inflight::queue(&db, scope, "kevin@x.com", "k1", "shop/credits.py", "bob@x.com", "b1", true, "def balance(member)");
        let told = crate::inflight::take_notices(&db, scope, "k1");
        assert!(told[0].contains("new work by bob@x.com (on branch feat/x)") && told[0].contains("it reaches your branch when the branches merge"), "{told:?}");
        // and a delta names the branch and when it meets this work
        crate::deltas::touch_as(&db, scope, "k1", &[path.to_string()], true, now());
        db.ledger_append(scope, "edit_reported", &json!({"path": path, "user": "bob@x.com", "session": "b1",
            "events": [{"symbol": "total", "kind": "modified", "signature_before": "def total(a)", "signature_after": "def total(a, b)"}]}), now()).unwrap();
        let lines = crate::deltas::deltas_told(&db, scope, "kevin@x.com", "k1", now() - 10.0, 0, &Default::default());
        let said = lines["lines"].to_string();
        assert!(said.contains("bob@x.com on branch feat/x") && said.contains("conflicts only when the branches merge"), "{said}");
        // landing never rebases onto another branch's change, and nothing here is a pull
        assert!(said.contains("on other branches") && !said.contains("landing rebases") && !said.to_lowercase().contains("pull"), "{said}");
    }

    /// A teammate is named as the reader knows them, never by the hook that
    /// reported their edit (Study 8: "another session of yours
    /// (collide-report-hook)").
    #[test]
    fn a_teammate_is_named_by_their_label() {
        let db = store();
        let _ = db.kv_put("nicknames", "kevin@x.com", &json!({"bob@x.com": "Bob"}), now());
        assert_eq!(author_label(&db, "kevin@x.com", "bob@x.com"), "Bob (bob@x.com)");
        assert_eq!(author_label(&db, "kevin@x.com", "kevin@x.com"), "another session of yours");
        assert_eq!(crate::presence::agent_name("collide-report-hook/75"), "");
        assert_eq!(crate::presence::agent_name("cork-report-hook/89"), "");
        assert_eq!(crate::presence::agent_name("collide-hook land"), "");
        assert_eq!(crate::presence::agent_name("claude"), "claude");
    }

    /// A helper's rename, live in its own worktree, is a teammate's change
    /// to its siblings and its lead: said once, by name, as what to write
    /// against. In the same folder it is already on disk, and to itself it
    /// is nothing. Before, one session's helpers skipped each other's marks.
    #[test]
    fn a_sibling_helpers_rename_is_a_teammates_change() {
        let db = store();
        let (scope, path) = ("w:r", "shop/total.py");
        crate::freshness::note_session(&db, scope, "kevin@x.com", "s", "cork-cloud-19", "claude", "m");
        crate::presence::note_worker(&db, scope, "kevin@x.com", "s", "wA", "general-purpose", "repo-names");
        let _ = db.eph_set(
            &format!("hot:{scope}:{}:kevin@x.com#s", path_key(path)),
            &json!({"path": path, "user": "kevin@x.com", "session": "s", "worker": "wA", "ts": now(),
                    "events": [{"symbol": "amount_due", "kind": "renamed", "confidence": "certain", "new_name": "checkout_total"}]}),
            Some(600.0),
        );
        for (agent, checkout) in [("s/wA", "wt-a"), ("s/wB", "wt-b"), ("s/wC", "wt-a"), ("s", "main-copy")] {
            crate::freshness::note_checkout(&db, scope, "kevin@x.com", agent, checkout);
        }
        let notice = |worker: &str| text(&gate_with(&db, scope, "kevin@x.com", "s", worker, path, "", "t", None, true).body, "notice");
        let sibling = notice("wB");
        assert!(sibling.contains("cork-cloud-19's helper repo-names renamed `amount_due` to `checkout_total`"), "{sibling}");
        assert!(notice("wB").is_empty(), "said once");
        assert!(notice("").contains("renamed `amount_due`"), "the lead, in its own copy, hears it too");
        assert!(notice("wA").is_empty(), "never its own");
        assert!(notice("wC").is_empty(), "a helper in the same folder has it on disk");
    }
}
