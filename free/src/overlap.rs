//! Two tasks on the same code, said before either writes.
//!
//! Three agents on one repo: one fixing the revenue logic in
//! `settled_total`, one asked to rename it. The renamer was told only that
//! another session was "reading daybook.py" and went ahead; the fixer found
//! out from the rename itself and redid part of it. Neither needs to wait:
//! the rename is small and lands fast, the fix builds on it. What they need
//! is to know, up front, that the work is split. So when a task's code (what
//! its prompt names, or its closest match by meaning) is code another live
//! agent has read or written in the last minutes, both are told: do only
//! your part, make it fit theirs, leave theirs to them.

use serde_json::{json, Value};

use crate::store::Store;

/// A match by meaning this close counts as the task's code.
const STRONG_MEANING: f64 = 0.80;
/// How recently the other agent must have read or written the file.
const LIVE_S: f64 = 1_200.0;

/// Files the task is about: the prompt's named symbols and files, and its
/// closest match by meaning when that is strong.
fn targets(store: &Store, scope: &str, named: &[String], meant: &[Value]) -> Vec<(String, String)> {
    let graph = crate::graphview::snapshot(store, scope);
    let mut out: Vec<(String, String)> = Vec::new();
    for name in named {
        let found = if name.contains('/') || name.ends_with(".py") || name.ends_with(".rs") || name.ends_with(".ts") {
            crate::graphview::find_node(&graph, name, "").map(|nid| (crate::graphview::split_node(&nid).0.to_string(), String::new()))
        } else {
            crate::graphview::find_node(&graph, "", name).map(|nid| {
                let (p, s) = crate::graphview::split_node(&nid);
                (p.to_string(), s.to_string())
            })
        };
        if let Some(pair) = found.filter(|(p, _)| !p.is_empty() && !crate::graphview::is_test_path(p)) {
            if !out.contains(&pair) {
                out.push(pair);
            }
        }
    }
    if let Some(top) = meant.first() {
        let score = top.get("score").and_then(Value::as_f64).unwrap_or(0.0);
        let path = top.get("path").and_then(Value::as_str).unwrap_or("");
        let symbol = top.get("symbol").and_then(Value::as_str).unwrap_or("");
        if score >= STRONG_MEANING && !path.is_empty() && !crate::graphview::is_test_path(path) {
            let pair = (path.to_string(), if symbol == crate::embed::MODULE { String::new() } else { symbol.to_string() });
            if !out.iter().any(|(p, _)| p == path) {
                out.push(pair);
            }
        }
    }
    out
}

fn shown(path: &str, symbol: &str) -> String {
    if symbol.is_empty() { path.to_string() } else { format!("{path}::{symbol}") }
}

/// The line for this session's briefing, when its task overlaps another live
/// agent's work; that agent gets a notice on its next step.
pub fn announce(store: &Store, scope: &str, user: &str, session: &str, named: &[String], meant: &[Value]) -> Option<String> {
    if session.is_empty() {
        return None;
    }
    let targets = targets(store, scope, named, meant);
    if targets.is_empty() {
        return None;
    }
    let paths: Vec<String> = targets.iter().map(|(p, _)| p.clone()).collect();
    let others = crate::freshness::working_in(store, scope, user, session, &paths, LIVE_S);
    if others.is_empty() {
        return None;
    }
    let mut names: Vec<String> = Vec::new();
    let mut waiting: Vec<Value> = Vec::new();
    for (other_user, other_session, files) in &others {
        let who = crate::inflight::author_label(store, user, other_user);
        let about: Vec<Value> = targets.iter().filter(|(p, _)| files.contains(p)).map(|(p, s)| json!([p, s])).collect();
        // they hear of it once this session opens or edits one of those
        // files (`touched`): a file a prompt names is not yet anyone's work
        waiting.push(json!({"user": other_user, "session": other_session, "about": about}));
        names.push(format!("{who} ({})", files.join(", ")));
    }
    let _ = store.eph_set(&wait_key(scope, session), &json!({"user": user, "others": waiting}), Some(LIVE_S));
    Some(format!(
        "Overlap: {} {} working in the code your task is about, right now. Align with them: do only your task's part, \
make it fit theirs, and leave theirs to them; they have been told the same.",
        names.join("; "),
        if others.len() == 1 { "is" } else { "are" }
    ))
}

fn wait_key(scope: &str, session: &str) -> String {
    format!("overlapwait:{scope}:{session}")
}

/// This session opened or edited `paths`: the agents whose work there its
/// task overlaps (`announce`) hear of it now, each file once.
pub fn touched(store: &Store, scope: &str, session: &str, paths: &[String]) {
    if session.is_empty() || paths.is_empty() {
        return;
    }
    let key = wait_key(scope, session);
    let Some(mut waiting) = store.eph_get(&key) else { return };
    let user = waiting.get("user").and_then(Value::as_str).unwrap_or("").to_string();
    let mut heard: Vec<(String, String, Vec<(String, String)>)> = Vec::new();
    if let Some(others) = waiting.get_mut("others").and_then(Value::as_array_mut) {
        for other in others.iter_mut() {
            let Some(about) = other.get_mut("about").and_then(Value::as_array_mut) else { continue };
            let mut now_theirs: Vec<(String, String)> = Vec::new();
            about.retain(|pair| {
                let path = pair.get(0).and_then(Value::as_str).unwrap_or("");
                if paths.iter().any(|p| p == path) {
                    now_theirs.push((path.to_string(), pair.get(1).and_then(Value::as_str).unwrap_or("").to_string()));
                    false
                } else {
                    true
                }
            });
            if !now_theirs.is_empty() {
                let text = |k: &str| other.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                heard.push((text("user"), text("session"), now_theirs));
            }
        }
    }
    if heard.is_empty() {
        return;
    }
    let _ = store.eph_set(&key, &waiting, Some(LIVE_S));
    for (other_user, other_session, about) in heard {
        let me = crate::inflight::author_label(store, &other_user, &user);
        let shown_about: Vec<String> = about.iter().map(|(p, s)| shown(p, s)).collect();
        let files: Vec<String> = about.iter().map(|(p, _)| p.clone()).collect();
        crate::inflight::queue_notice(
            store, scope, &other_session, &format!("overlap|{user}|{session}|{}", shown_about.join(",")),
            &format!(
                "Cork: {me} is now working in code you are working in ({}). Align with them: finish only your \
part, make it fit theirs, and do not redo their part (Cork tells you when theirs lands).",
                shown_about.join(", ")
            ),
            &user, session, &files,
        );
    }
}

/// Code a prompt names only to keep away from it ("don't touch
/// views/space.js", "everything except the engine", "leave feed.js
/// alone"), lowercased, the way the hooks name it: not the task's targets.
/// Lattice test, round 2 (v104): a session was said to be working in
/// space.js because its task listed it among the files not to touch. A
/// name the prompt also uses outside such a clause still counts.
pub fn negated_names(prompt: &str) -> std::collections::BTreeSet<String> {
    const MARKERS: [&str; 22] = [
        "don't touch", "do not touch", "don't edit", "do not edit", "don't change", "do not change", "don't modify",
        "do not modify", "not to touch", "never touch", "without touching", "stay out of", "keep out of", "avoid",
        "except", "excluding", "other than", "off-limits", "off limits", "don't open", "do not open", "leave",
    ];
    let token = |w: &str| w.trim_matches(|c: char| !(c.is_alphanumeric() || "_./-".contains(c))).trim_end_matches('.').to_lowercase();
    let mut negated = std::collections::BTreeSet::new();
    let mut plain = std::collections::BTreeSet::new();
    let lower = prompt.to_lowercase();
    // sentences: a period ends one only before a space or the end (`a.rs` does not)
    for sentence in lower.split(['\n', '!', '?', ';']).flat_map(|s| s.split(". ")) {
        let at = MARKERS
            .iter()
            .filter_map(|m| {
                let found = sentence.find(m)?;
                // "leave X alone" only: "leave" alone is no negation
                (*m != "leave" || sentence[found..].contains(" alone")).then_some(found)
            })
            .min();
        let (before, after) = match at {
            Some(at) => sentence.split_at(at),
            None => (sentence, ""),
        };
        plain.extend(before.split(|c: char| c.is_whitespace() || c == ',' || c == '`').map(token).filter(|t| !t.is_empty()));
        negated.extend(after.split(|c: char| c.is_whitespace() || c == ',' || c == '`').map(token).filter(|t| !t.is_empty()));
    }
    negated.difference(&plain).cloned().collect()
}

/// Whether the code name `name` (as the hooks found it in the prompt) is
/// one of `negated`: the same name, a path ending in it, or a folder holding it.
pub fn is_negated(name: &str, negated: &std::collections::BTreeSet<String>) -> bool {
    let name = name.trim().to_lowercase();
    negated.iter().any(|n| {
        n == &name
            || n.ends_with(&format!("/{name}"))
            || name.ends_with(&format!("/{n}"))
            || (n.ends_with('/') && name.starts_with(n.as_str()))
            || (n.contains('/') && n.ends_with('/') && name.contains(n.as_str()))
    })
}

// ------------------------------------------------------------ the same task
//
// Two people ask two agents for the same thing ("a refunds-by-day report",
// "accounting needs refunds per day in a report"), minutes apart. Neither
// has touched code yet, so nothing above fires; by the time both write, one
// of the two jobs is waste. Each live session's task is kept as a vector (the
// prompt without its process sentences, the vector the worked examples
// already make; never its words) and the code names its prompt used; a new
// task that means the same as another live session's, or names the same
// code when there are no vectors (the free build), warns both before either
// writes, once per pair.

/// Two tasks this close in meaning are the same task. Calibrated with
/// `same_task_calibration` (bge-small, process sentences removed, Study 5's
/// tickets): one task phrased two ways scored 0.774 to 0.892; different
/// tasks 0.45 to 0.60; a different task of the same kind on the same code
/// (an export gaining the email vs gaining the region) up to 0.840, as
/// examples.rs's reuse study found (different kinds up to 0.83). Above all
/// of those: half the rephrasings are caught and no near task is, because a
/// false "someone is already on this" costs a coordination round trip, and
/// a near task on the same code is `announce`'s case once either reads it.
/// Rechecked on Scaling study 2's 32 prompts (2026-10-17, where #11 fired 0
/// times): its 496 different-task pairs score 0.367-0.761 (p99 0.735; the
/// top ones share code: a credits module vs its consumer), ten second
/// phrasings of its prompts 0.764-0.936. No two of its agents had the same
/// task, so 0 was right; 0.80 would also clear it, the near tasks (0.840)
/// would not, so the floor stays.
///
/// Lattice test, round 2 (v104): an issue `kind` and a `start_date`, each
/// "added end to end" through the same files, read as the same task. Two
/// prompts are the same task now only above a stricter floor AND naming the
/// same code (`SAME_NAMES`); above the old floor, or sharing code, they are
/// related, and said so.
pub const SAME_TASK: f32 = 0.88;
/// Above this but not the same task: related work on the same ground.
const RELATED_TASK: f32 = 0.86;
/// The share of code names two prompts must have in common to be the same
/// task (names a new field or function one of them adds count against it).
const SAME_NAMES: f64 = 0.8;
/// To be related work.
const RELATED_NAMES: f64 = 0.5;
/// A task stays live this long after its prompt.
const TASK_LIVE_S: f64 = 2.0 * 3600.0;
const MAX_NAMES: usize = 20;

fn task_key(scope: &str, session: &str) -> String {
    format!("dupetask:{scope}:{session}")
}

/// The code names a prompt used, as compared: lowercased, code-shaped
/// (snake_case, a path, a dotted or CamelCase name), at most MAX_NAMES.
fn code_names(identifiers: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for ident in identifiers {
        let i = ident.trim();
        let humps = i.chars().filter(|c| c.is_ascii_uppercase()).count();
        let shaped = i.contains('_') || i.contains('/') || i.contains('.') || (humps >= 2 && i.chars().any(|c| c.is_ascii_lowercase()));
        let name = i.to_lowercase();
        if shaped && name.len() >= 3 && !out.contains(&name) {
            out.push(name);
        }
        if out.len() >= MAX_NAMES {
            break;
        }
    }
    out
}

/// How two tasks relate.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
enum Kin {
    Related,
    Same,
}

/// The share of code names two prompts have in common; None when either
/// names none.
fn name_share(names: &[String], other_names: &[String]) -> Option<f64> {
    if names.is_empty() || other_names.is_empty() {
        return None;
    }
    let shared = names.iter().filter(|n| other_names.contains(n)).count();
    let union = names.len() + other_names.len() - shared;
    Some(shared as f64 / union.max(1) as f64)
}

/// How alike two tasks are, and how close: the same task by meaning above
/// the stricter floor with the same code named (or no code named on either
/// side); related above the old floor or with code in common. Without
/// vectors, by the code names alone.
fn same_task(q: &[f32], names: &[String], other_q: &[f32], other_names: &[String]) -> Option<(Kin, f64)> {
    let share = name_share(names, other_names);
    if !q.is_empty() && !other_q.is_empty() {
        let score = crate::embed::cosine(q, other_q);
        let same_code = match share {
            Some(share) => share >= SAME_NAMES,
            None => names.is_empty() && other_names.is_empty(),
        };
        if score >= SAME_TASK && same_code {
            return Some((Kin::Same, score as f64));
        }
        if score >= RELATED_TASK || share.is_some_and(|s| s >= RELATED_NAMES && score >= RELATED_TASK - 0.1) {
            return Some((Kin::Related, score as f64));
        }
        return None;
    }
    let share = share.filter(|s| *s > 0.0)?;
    if share >= SAME_NAMES {
        Some((Kin::Same, share))
    } else if share >= RELATED_NAMES {
        Some((Kin::Related, share))
    } else {
        None
    }
}

/// A new prompt for `session`: keep its task, and when another live
/// session's task is the same one, the line for this session's briefing
/// (that session hears it on its next step). `q` is the task's vector (empty
/// without embeddings), `identifiers` the code names the hook found in it.
pub fn duplicate_task(store: &Store, scope: &str, user: &str, session: &str, q: &[f32], identifiers: &[String]) -> Option<String> {
    if session.is_empty() {
        return None;
    }
    let stamp = crate::store::now();
    let names = code_names(identifiers);
    if q.is_empty() && names.is_empty() {
        return None;
    }
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let strings = |v: &Value, k: &str| -> Vec<String> {
        v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
    };
    // the same task again (a follow-up prompt) keeps when it started
    let key = task_key(scope, session);
    let started = store
        .eph_get(&key)
        .filter(|prior| same_task(q, &names, &crate::embed::decode_vec(&text(prior, "q")), &strings(prior, "names")).is_some_and(|(kin, _)| kin == Kin::Same))
        .and_then(|prior| prior.get("started").and_then(Value::as_f64))
        .unwrap_or(stamp);
    let _ = store.eph_set(
        &key,
        &serde_json::json!({"user": user, "session": session, "q": crate::embed::encode_vec(q), "names": names, "started": started, "ts": stamp}),
        Some(TASK_LIVE_S),
    );
    let mut best: Option<((Kin, f64), Value)> = None;
    for (_, other) in store.eph_scan(&format!("dupetask:{scope}:")) {
        let other_session = text(&other, "session");
        if other_session.is_empty() || other_session == session {
            continue;
        }
        if store.eph_get(&format!("sessended:{scope}:{other_session}")).is_some() {
            continue;
        }
        let Some(found) = same_task(q, &names, &crate::embed::decode_vec(&text(&other, "q")), &strings(&other, "names")) else { continue };
        if best.as_ref().is_some_and(|(b, _)| *b >= found) {
            continue;
        }
        best = Some((found, other));
    }
    let ((kin, _), other) = best?;
    let (other_user, other_session) = (text(&other, "user"), text(&other, "session"));
    // once per pair, whichever of the two asked second
    let pair = if session < other_session.as_str() { format!("{session}|{other_session}") } else { format!("{other_session}|{session}") };
    if !crate::access::tell_once(store, &format!("dupetold:{scope}:{pair}"), TASK_LIVE_S) {
        return None;
    }
    let label = |u: &str, s: &str, names: &[String]| -> String {
        let title = crate::freshness::title_of(store, scope, u, s);
        if !title.is_empty() {
            format!("'{}'", title.chars().take(80).collect::<String>())
        } else if !names.is_empty() {
            format!("a task naming {}", names.iter().take(3).cloned().collect::<Vec<_>>().join(", "))
        } else {
            "a task like yours".to_string()
        }
    };
    let age = crate::deltas::age_text(stamp - other.get("started").and_then(Value::as_f64).unwrap_or(stamp));
    let who = if other_user == user { "another session of yours".to_string() } else { format!("{}'s agent", crate::inflight::author_label(store, user, &other_user)) };
    let me = if other_user == user { "another session of yours".to_string() } else { format!("{}'s agent", crate::inflight::author_label(store, &other_user, user)) };
    if kin == Kin::Related {
        // the code both name, when they name any: where their work meets
        let other_names = strings(&other, "names");
        let shared: Vec<String> = names.iter().filter(|n| other_names.contains(n)).take(3).cloned().collect();
        let on = if shared.is_empty() { String::new() } else { format!(" ({})", shared.join(", ")) };
        crate::inflight::queue_notice(
            store, scope, &other_session, &format!("reltask|{user}|{session}"),
            &format!(
                "Cork: {me} just started {}, a related task on the same code{on}, not the same one: keep to your own \
change and build on theirs where the two meet.",
                label(user, session, &names)
            ),
            user, session, &[],
        );
        return Some(format!(
            "Related work: {who} is on {} (started {age}), a related task on the same code{on}, not the same one: keep to \
your own change and build on theirs where the two meet.",
            label(&other_user, &other_session, &other_names)
        ));
    }
    crate::inflight::queue_notice(
        store, scope, &other_session, &format!("dupetask|{user}|{session}"),
        &format!(
            "Cork: {me} just started {}, the same task as yours: coordinate before duplicating it.",
            label(user, session, &names)
        ),
        user, session, &[],
    );
    Some(format!(
        "Duplicate work: {who} is already on {} (started {age}): coordinate before duplicating it.",
        label(&other_user, &other_session, &strings(&other, "names"))
    ))
}

/// The session ended: its task is no longer anyone's to duplicate.
pub fn task_ended(store: &Store, scope: &str, session: &str) {
    if !session.is_empty() {
        store.eph_delete(&task_key(scope, session));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn unit(v: Vec<f32>) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / n).collect()
    }

    /// Lattice test, round 2 (v104): B's task ended "Don't touch
    /// static/js/engine/, views/space.js, views/studio.js, views/feed.js",
    /// and A was told B was working in space.js.
    #[test]
    fn code_a_prompt_says_to_keep_away_from_is_not_its_target() {
        let spec = "add an issue start_date field: src/db.rs, src/api.rs and static/js/views/issue.js.\n\
Don't commit. Don't touch static/js/engine/, views/space.js, views/studio.js, views/feed.js.";
        let negated = negated_names(spec);
        for name in ["static/js/views/space.js", "views/studio.js", "feed.js", "static/js/engine/core.js"] {
            assert!(is_negated(name, &negated), "{name}: {negated:?}");
        }
        for name in ["src/db.rs", "static/js/views/issue.js", "start_date"] {
            assert!(!is_negated(name, &negated), "{name}: {negated:?}");
        }
        assert!(is_negated("billing.py", &negated_names("Fix tax.py; leave billing.py alone.")));
        assert!(is_negated("api.rs", &negated_names("Update every module except api.rs")));
        assert!(!is_negated("db.rs", &negated_names("Avoid locking in db.rs. Then change db.rs to batch writes.")), "named outside the clause too");
    }

    /// A task whose prompt names a file another agent works in is told so
    /// at once; that agent hears of it only once this session opens the file.
    #[test]
    fn the_other_agent_hears_of_an_overlap_once_the_file_is_opened() {
        let db = store();
        let scope = "w:r";
        crate::codegraph::update_file(&db, scope, "src/db.rs", serde_json::json!({"path": "src/db.rs", "language": "rust",
            "symbols": {"Issue": {"kind": "struct", "edges": [], "span": [1, 4], "params": [], "sites": []}}, "imports": [], "ts": 1.0}), 1.0);
        crate::freshness::saw(&db, scope, "kevin@x.com", "A", "src/db.rs", "h1", true);
        let line = announce(&db, scope, "kevin@x.com", "B", &ids(&["src/db.rs"]), &[]).expect("B is told");
        assert!(line.starts_with("Overlap: another session of yours (src/db.rs)") || line.contains("src/db.rs"), "{line}");
        assert!(crate::inflight::take_notices(&db, scope, "A").is_empty(), "not before B opens it");
        crate::deltas::touch(&db, scope, "B", &["src/api.rs".to_string()], crate::store::now());
        assert!(crate::inflight::take_notices(&db, scope, "A").is_empty(), "another file");
        crate::deltas::touch(&db, scope, "B", &["src/db.rs".to_string()], crate::store::now());
        let told = crate::inflight::take_notices(&db, scope, "A");
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains("is now working in code you are working in (src/db.rs)"), "{}", told[0]);
        crate::deltas::touch(&db, scope, "B", &["src/db.rs".to_string()], crate::store::now());
        assert!(crate::inflight::take_notices(&db, scope, "A").is_empty(), "once");
    }

    /// Lattice test, round 2 (v104): an issue `kind` and a `start_date`,
    /// each through the same files, are related tasks, not the same one.
    #[test]
    fn tasks_on_the_same_files_adding_different_fields_are_related_not_the_same() {
        let db = store();
        let a = ids(&["src/db.rs", "src/api.rs", "tests/api.rs", "static/js/views/issue.js", "ISSUE_SELECT", "issue_kind"]);
        let b = ids(&["src/db.rs", "src/api.rs", "tests/api.rs", "static/js/views/issue.js", "ISSUE_SELECT", "start_date", "due_date"]);
        assert!(duplicate_task(&db, "w:r", "kevin@x.com", "A", &[], &a).is_none());
        let line = duplicate_task(&db, "w:r", "kevin@x.com", "B", &[], &b).expect("related");
        assert!(line.starts_with("Related work: another session of yours is on") && line.contains("not the same one"), "{line}");
        assert!(!line.contains("Duplicate"), "{line}");
        let told = crate::inflight::take_notices(&db, "w:r", "A");
        assert!(told[0].contains("a related task on the same code") && !told[0].contains("the same task as yours"), "{}", told[0]);
        // by meaning too (the cloud's vectors): close, but naming different code
        #[cfg(feature = "cloud")]
        {
            let base = unit(vec![1.0, 0.2, 0.1, 0.0]);
            let near = unit(vec![0.97, 0.22, 0.1, 0.05]);
            assert_eq!(same_task(&base, &a, &near, &b).map(|(kin, _)| kin), Some(Kin::Related));
            assert_eq!(same_task(&base, &a, &near, &a).map(|(kin, _)| kin), Some(Kin::Same));
        }
    }

    #[test]
    fn the_same_task_by_name_warns_both_once_and_a_different_one_does_not() {
        // the free build: no vectors, the names the prompts used
        let db = store();
        assert!(duplicate_task(&db, "w:r", "bob@x.com", "B", &[], &ids(&["refunds_by_day", "reports/daily.py"])).is_none());
        let line = duplicate_task(&db, "w:r", "ann@x.com", "A", &[], &ids(&["reports/daily.py", "refunds_by_day", "Refund"])).unwrap();
        assert!(line.starts_with("Duplicate work: bob@x.com's agent is already on a task naming refunds_by_day, reports/daily.py (started just now): coordinate before duplicating it."), "{line}");
        // bob hears it on his next step
        let told = crate::inflight::take_notices(&db, "w:r", "B");
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains("ann@x.com's agent just started a task naming") && told[0].contains("coordinate before duplicating it"), "{}", told[0]);
        // once per pair
        assert!(duplicate_task(&db, "w:r", "ann@x.com", "A", &[], &ids(&["refunds_by_day", "reports/daily.py"])).is_none());
        assert!(crate::inflight::take_notices(&db, "w:r", "B").is_empty());
        // a different task in the same repo
        assert!(duplicate_task(&db, "w:r", "cat@x.com", "C", &[], &ids(&["vat_for", "billing/tax.py"])).is_none());
        // a session that ended is no one to duplicate
        crate::freshness::note_ended(&db, "w:r", "B");
        crate::freshness::note_ended(&db, "w:r", "A");
        assert!(duplicate_task(&db, "w:r", "dee@x.com", "D", &[], &ids(&["refunds_by_day", "reports/daily.py"])).is_none());
    }

    /// The floor against the real model: `CORK_EMBEDDINGS=1 cargo test
    /// --release same_task_calibration -- --ignored --nocapture` (downloads
    /// bge-small once into $DATA_DIR/models). Prints every pair's score.
    #[test]
    #[ignore]
    fn same_task_calibration() {
        let same: [(&str, &str); 6] = [
            ("add a refunds by day report for accounting", "accounting needs a daily refunds report: total refunds per day"),
            ("uk customers aren't charged vat at checkout, it should be 20%", "we aren't adding tax for british customers when they check out"),
            ("the WELCOME10 code should be one use per customer but people keep reusing it", "make discount codes like WELCOME10 single-use per customer"),
            ("loyalty points are too high: cancelled or unpaid orders earn points", "don't give loyalty points for orders that were cancelled or never paid"),
            ("the orders export for accounting should include the customer's email", "add the customer email as a column in the accounting orders export"),
            ("french customers are getting free shipping by accident", "shipping to france is free when it shouldn't be, fix it"),
        ];
        let tasks: Vec<&str> = same.iter().map(|(a, _)| *a).collect();
        let vec = |t: &str| crate::embed::query_vector(&crate::examples::task_text(t)).expect("CORK_EMBEDDINGS=1 and the model");
        let mut lowest_same = f32::MAX;
        for (a, b) in same {
            let s = crate::embed::cosine(&vec(a), &vec(b));
            lowest_same = lowest_same.min(s);
            println!("same  {s:.3}  {a} | {b}");
        }
        let mut highest_other = f32::MIN;
        for (i, a) in tasks.iter().enumerate() {
            for b in tasks.iter().skip(i + 1) {
                let s = crate::embed::cosine(&vec(a), &vec(b));
                highest_other = highest_other.max(s);
                println!("other {s:.3}  {a} | {b}");
            }
        }
        // the hard ones: a different task of the same kind, in the same code
        let near: [(&str, &str); 6] = [
            ("add a refunds by day report for accounting", "add a revenue by month report for accounting"),
            ("uk customers aren't charged vat at checkout, it should be 20%", "german customers are charged the wrong vat rate at checkout"),
            ("the orders export for accounting should include the customer's email", "the orders export for accounting should include each customer's region"),
            ("the WELCOME10 code should be one use per customer but people keep reusing it", "the SUMMER20 code should stop working at the end of august"),
            ("loyalty points are too high: cancelled or unpaid orders earn points", "loyalty points should expire after a year"),
            ("french customers are getting free shipping by accident", "shipping to canada is charged twice"),
        ];
        let mut highest_near = f32::MIN;
        for (a, b) in near {
            let s = crate::embed::cosine(&vec(a), &vec(b));
            highest_near = highest_near.max(s);
            println!("near  {s:.3}  {a} | {b}");
        }
        println!("lowest same-task {lowest_same:.3}, highest different-task {highest_other:.3}, highest near-task {highest_near:.3}, floor {SAME_TASK}");
        assert!(highest_other < SAME_TASK && highest_near < SAME_TASK);
        // Scaling study 2: distinct tasks on one repo, the closest pairs on
        // the same code (a credits module and its consumer)
        let credits = "invoices should knock the member's leftover credit off what they owe (never below zero); the card fee is still worked out on the full amount. the invoice email should show a 'Credit applied: $X.XX' line when credit was used, and finance wants credit_liability() in the reports: all unused credit in cents. someone is writing a credits module right now, use it instead of rolling your own";
        let module = "we need a credits module in billing, credits.py with balance_for(member_id) giving the member's unused credit in cents. also GET /members/<id>/credit on the router answering {'cents': <balance>}, and a credit column at the end of the admin members table, formatted as money";
        let study: [(&str, &str); 2] = [
            ("the card fee on invoices rounds weird, a fee of 2.5 cents came out as 2, fees should round half up. also no card fee at all on invoices under $10. and the invoice email should show the card fee as its own line, like 'Card fee: $0.28'", credits),
            (module, credits),
        ];
        for (a, b) in study {
            let s = crate::embed::cosine(&vec(a), &vec(b));
            println!("study {s:.3}  {} | {}", &a[..40], &b[..40]);
            assert!(s < SAME_TASK, "a different study task read as the same one: {s:.3}");
        }
        let rephrased = "credits.py in billing with balance_for(member_id), GET /members/<id>/credit, and a credit column in the admin members table";
        println!("study rephrased {:.3}", crate::embed::cosine(&vec(rephrased), &vec(module)));
        // end to end on real vectors: one task phrased two ways warns, a
        // different report in the same place does not
        let db = store();
        assert!(duplicate_task(&db, "w:r", "bob@x.com", "B", &vec(same[0].0), &[]).is_none());
        assert!(duplicate_task(&db, "w:r", "cat@x.com", "C", &vec(near[0].1), &[]).is_none());
        let line = duplicate_task(&db, "w:r", "ann@x.com", "A", &vec(same[0].1), &[]).unwrap();
        assert!(line.contains("bob@x.com's agent is already on"), "{line}");
    }

    /// Meaning is the cloud's (embed.rs): the free build matches by names.
    #[cfg(feature = "cloud")]
    #[test]
    fn by_meaning_the_floor_decides() {
        // what bge-small gives two phrasings of one task vs two tasks: the
        // floor is what is tested here, the vectors are stand-ins
        let base = unit(vec![1.0, 0.2, 0.1, 0.0]);
        let paraphrase = unit(vec![0.95, 0.25, 0.12, 0.1]);
        let other = unit(vec![0.6, 0.7, 0.2, 0.3]);
        assert!(crate::embed::cosine(&base, &paraphrase) >= SAME_TASK);
        assert!(crate::embed::cosine(&base, &other) < SAME_TASK);
        let db = store();
        if crate::embed::encode_vec(&base).is_empty() {
            return; // the free build keeps no vectors: names only, tested above
        }
        assert!(duplicate_task(&db, "w:r", "bob@x.com", "B", &base, &[]).is_none());
        assert!(duplicate_task(&db, "w:r", "cat@x.com", "C", &other, &[]).is_none());
        let line = duplicate_task(&db, "w:r", "ann@x.com", "A", &paraphrase, &[]).unwrap();
        assert!(line.contains("bob@x.com's agent is already on a task like yours"), "{line}");
        // with vectors on both sides, sharing a name is not the same task
        let far = unit(vec![0.0, 1.0, 0.0, 0.0]);
        assert!(duplicate_task(&db, "w:r", "gus@x.com", "G", &far, &ids(&["refunds_by_day"])).is_none());
        let unrelated = unit(vec![0.0, 0.1, 1.0, 0.2]);
        assert!(duplicate_task(&db, "w:r", "eve@x.com", "E", &unrelated, &ids(&["refunds_by_day"])).is_none());
    }
}
