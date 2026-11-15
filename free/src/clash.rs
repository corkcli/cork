//! Two agents' work meeting on one step, linked on both.
//!
//! A write can meet another agent's work four ways: the gate stopped it
//! (a teammate's pushed rename, a copy behind their write, an owner's held
//! decision), its hunks overlap lines another agent wrote in the same file
//! minutes before, or the semantic lint found it using a name another
//! agent's change renamed, removed or re-signatured. Each is kept on the
//! write's step as `x.c` (presence::note_exchange) and mirrored onto the
//! other agent's step, so the dashboard marks both dots and opens both
//! sides from either: the other agent, the file, its line ranges, the
//! symbols, a signature before and after, why, when, and how it ended.
//!
//! Structure only, like the rest of a step's exchange: ranges and names,
//! never a line of a file. Held to `MAX_C_BYTES`.

use serde_json::{json, Map, Value};

use crate::presence::{self, Step};
use crate::store::{now, Store};

/// A collision's record on one step, at most.
pub const MAX_C_BYTES: usize = 400;
/// Two agents' writes of one file this close in time can overlap.
pub const OVERLAP_S: f64 = 1_200.0;
const MAX_RANGES: usize = 4;
const MAX_SYMBOLS: usize = 4;
const MAX_TEXT: usize = 90;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// What a write met, as its report knows it.
pub struct Met<'a> {
    /// the gate's stop on this write (gate::note_verdict's `link`)
    pub link: Option<&'a Value>,
    /// who wrote the file last before this write: (user, session, ts)
    pub prior: Option<&'a (String, String, f64)>,
    /// the semantic lint's findings on this write
    pub findings: &'a [Value],
}

/// A step's hunks as the lines they leave: `[start, length]`, new side.
pub fn ranges_of(x: &Value) -> Vec<[i64; 2]> {
    x.get("hunks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| {
            let h = h.as_array()?;
            Some([h.get(2)?.as_i64()?, h.get(3)?.as_i64()?])
        })
        .collect()
}

/// The first lines both sides touched, `[first, last]`: a pure insertion
/// counts as the line it sits on.
pub fn overlap(a: &[[i64; 2]], b: &[[i64; 2]]) -> Option<[i64; 2]> {
    let span = |[start, len]: [i64; 2]| (start, start + len.max(1) - 1);
    for &one in a {
        for &two in b {
            let ((s1, e1), (s2, e2)) = (span(one), span(two));
            let (from, to) = (s1.max(s2), e1.min(e2));
            if from <= to {
                return Some([from, to]);
            }
        }
    }
    None
}

/// What a step changed by name: added, removed, renamed from, re-signatured.
fn names_of(x: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: String| {
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    };
    for key in ["sig", "ren"] {
        for pair in x.get(key).and_then(Value::as_array).into_iter().flatten() {
            push(pair.get(0).and_then(Value::as_str).unwrap_or("").to_string());
        }
    }
    for key in ["add", "del"] {
        for name in x.get(key).and_then(Value::as_array).into_iter().flatten() {
            push(name.as_str().unwrap_or("").to_string());
        }
    }
    out.truncate(MAX_SYMBOLS);
    out
}

/// A step's first signature change, (before, after).
fn sig_of(x: &Value) -> Option<(String, String)> {
    let first = x.get("sig").and_then(Value::as_array)?.first()?.as_array()?;
    Some((first.get(1)?.as_str()?.to_string(), first.get(2)?.as_str()?.to_string()))
}

/// One side of a collision as the OTHER side's step keeps it.
struct Side {
    kind: String,
    user: String,
    session: String,
    path: String,
    ranges: Vec<[i64; 2]>,
    overlap: Option<[i64; 2]>,
    symbols: Vec<String>,
    sig: Option<(String, String)>,
    why: String,
    ts: f64,
    outcome: &'static str,
    rid: String,
    /// this step's change is what the other side met
    source: bool,
}

impl Side {
    fn record(&self) -> Value {
        let mut c = Map::new();
        c.insert("k".into(), json!(self.kind));
        if !self.user.is_empty() {
            c.insert("w".into(), json!(self.user));
        }
        if !self.session.is_empty() {
            c.insert("ws".into(), json!(self.session));
        }
        c.insert("p".into(), json!(clip(&self.path, 120)));
        if !self.ranges.is_empty() {
            c.insert("r".into(), json!(self.ranges.iter().take(MAX_RANGES).collect::<Vec<_>>()));
        }
        if let Some(o) = self.overlap {
            c.insert("o".into(), json!(o));
        }
        if !self.symbols.is_empty() {
            let names: Vec<String> = self.symbols.iter().take(MAX_SYMBOLS).map(|s| clip(s, 60)).collect();
            c.insert("s".into(), json!(names));
        }
        if let Some((before, after)) = &self.sig {
            c.insert("sig".into(), json!([clip(before, MAX_TEXT), clip(after, MAX_TEXT)]));
        }
        if !self.why.is_empty() {
            c.insert("why".into(), json!(clip(&self.why, MAX_TEXT)));
        }
        c.insert("ts".into(), json!(crate::compat::python_round(self.ts, 1)));
        c.insert("st".into(), json!(self.outcome));
        if !self.rid.is_empty() {
            c.insert("rid".into(), json!(self.rid));
        }
        if self.source {
            c.insert("src".into(), json!(true));
        }
        bound(Value::Object(c))
    }
}

/// A collision held to `MAX_C_BYTES`: the why goes first, then the
/// signature, the ranges past the first, the symbols past the first.
pub fn bound(c: Value) -> Value {
    let size = |v: &Value| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
    let Value::Object(mut map) = c else { return json!({}) };
    let fits = |map: &Map<String, Value>| size(&Value::Object(map.clone())) <= MAX_C_BYTES;
    for key in ["why", "sig"] {
        if fits(&map) {
            break;
        }
        map.remove(key);
    }
    for key in ["r", "s"] {
        if fits(&map) {
            break;
        }
        if let Some(list) = map.get_mut(key).and_then(Value::as_array_mut) {
            list.truncate(1);
        }
    }
    if !fits(&map) {
        map.insert("p".into(), json!(clip(&text(&Value::Object(map.clone()), "p"), 40)));
    }
    Value::Object(map)
}

/// The other agent's newest edit of `path`: its step's exchange and when.
fn their_step(store: &Store, scope: &str, user: &str, session: &str, path: &str) -> Option<(Value, f64)> {
    let step = presence::find_step(store, scope, user, session, "edit", path)?;
    let ts = step.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    Some((step.get("x").cloned().unwrap_or_else(|| json!({})), ts))
}

/// This write's collision, if it met another agent's work, for its own
/// step; the other agent's step gets the mirror. Old overlaps on this file
/// that either side has now built on are settled first ("merged").
pub fn on_write(store: &Store, scope: &str, user: &str, session: &str, path: &str, mine: &Value, met: &Met) -> Option<Value> {
    settle_on_write(store, scope, user, session, path);
    let stamp = now();
    let my_ranges = ranges_of(mine);
    let mut found: Option<(Side, String)> = None;

    // 1. the gate stopped this write before it went through
    if let Some(link) = met.link {
        let (w, ws) = (text(link, "w"), text(link, "ws"));
        let theirs = if ws.is_empty() { None } else { their_step(store, scope, &w, &ws, path) };
        let sig = link.get("sig").and_then(Value::as_array).and_then(|pair| {
            Some((pair.first()?.as_str()?.to_string(), pair.get(1)?.as_str()?.to_string()))
        });
        let symbol = text(link, "s");
        let their_ranges = theirs.as_ref().map(|(x, _)| ranges_of(x)).unwrap_or_default();
        found = Some((
            Side {
                kind: text(link, "k"),
                user: w,
                session: ws,
                path: path.to_string(),
                overlap: overlap(&my_ranges, &their_ranges),
                ranges: their_ranges,
                symbols: if symbol.is_empty() { Vec::new() } else { vec![symbol] },
                sig,
                why: text(link, "why"),
                ts: theirs.as_ref().map(|(_, ts)| *ts).unwrap_or(stamp),
                // it went through: this write is the agent adapting to it
                outcome: "resolved",
                rid: String::new(),
                source: false,
            },
            path.to_string(),
        ));
    }

    // 2. its lines overlap another agent's write of this file, minutes ago
    if found.is_none() {
        if let Some((w, ws, written)) = met.prior {
            let other = w != user || ws != session;
            if other && !ws.is_empty() && stamp - written <= OVERLAP_S {
                if let Some((x, ts)) = their_step(store, scope, w, ws, path) {
                    let their_ranges = ranges_of(&x);
                    if let Some(lines) = overlap(&my_ranges, &their_ranges) {
                        found = Some((
                            Side {
                                kind: "overlap".into(),
                                user: w.clone(),
                                session: ws.clone(),
                                path: path.to_string(),
                                ranges: their_ranges,
                                overlap: Some(lines),
                                symbols: names_of(&x),
                                sig: sig_of(&x),
                                why: String::new(),
                                ts,
                                outcome: "open",
                                rid: String::new(),
                                source: false,
                            },
                            path.to_string(),
                        ));
                    }
                }
            }
        }
    }

    // 3. it uses a name another agent's change renamed, removed or re-signatured
    if found.is_none() {
        let finding = met.findings.iter().find(|f| {
            let (by, by_session) = (text(f, "by"), text(f, "by_session"));
            !by.is_empty() && (by != user || (!by_session.is_empty() && by_session != session))
        });
        if let Some(f) = finding {
            let (w, ws, their_path) = (text(f, "by"), text(f, "by_session"), text(f, "in"));
            let symbol = text(f, "symbol");
            let theirs = if ws.is_empty() { None } else { their_step(store, scope, &w, &ws, &their_path) };
            let to = [text(f, "signature"), text(f, "renamed_to")].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
            let judgment = f.get("fix").map(|fix| text(fix, "class")) == Some("judgment".into());
            found = Some((
                Side {
                    kind: "lint".into(),
                    user: w,
                    session: ws,
                    path: their_path.clone(),
                    ranges: theirs.as_ref().map(|(x, _)| ranges_of(x)).unwrap_or_default(),
                    overlap: None,
                    symbols: vec![symbol.clone()],
                    sig: theirs.as_ref().and_then(|(x, _)| sig_of(x)).or_else(|| (!to.is_empty()).then(|| (String::new(), to))),
                    why: text(f, "rule"),
                    ts: theirs.as_ref().map(|(_, ts)| *ts).unwrap_or(stamp),
                    outcome: "open",
                    rid: if judgment { crate::lint::reconciliation_id(path, &symbol, &text(f, "rule"), user) } else { String::new() },
                    source: false,
                },
                their_path,
            ));
        }
    }

    let (side, their_path) = found?;
    // the mirror: this write, as the other agent's step keeps it
    if !side.session.is_empty() {
        let mirror = Side {
            kind: side.kind.clone(),
            user: user.to_string(),
            session: session.to_string(),
            path: path.to_string(),
            ranges: my_ranges.clone(),
            overlap: side.overlap,
            symbols: names_of(mine),
            sig: sig_of(mine),
            why: side.why.clone(),
            ts: stamp,
            outcome: side.outcome,
            rid: side.rid.clone(),
            source: true,
        };
        presence::note_exchange(
            store, scope, &side.user, &side.session, Step::Latest { kind: "edit", path: &their_path }, &json!({"c": mirror.record()}),
        );
    }
    Some(side.record())
}

/// An overlap is merged once either agent writes the file again: that
/// write was made on top of both. Settles this agent's open overlaps on
/// `path`, and their mirrors on the other agents' steps.
fn settle_on_write(store: &Store, scope: &str, user: &str, session: &str, path: &str) {
    let mut others: Vec<(String, String)> = Vec::new();
    let open_overlap = |record: &Value| {
        let c = record.get("x").and_then(|x| x.get("c"));
        text(record, "kind") == "edit"
            && text(record, "path") == path
            && c.is_some_and(|c| text(c, "k") == "overlap" && text(c, "st") == "open")
    };
    presence::edit_steps(store, scope, user, session, |record| {
        if !open_overlap(record) {
            return false;
        }
        let c = &mut record["x"]["c"];
        others.push((text(c, "w"), text(c, "ws")));
        c["st"] = json!("merged");
        true
    });
    for (w, ws) in others {
        if ws.is_empty() {
            continue;
        }
        presence::edit_steps(store, scope, &w, &ws, |record| {
            if !open_overlap(record) {
                return false;
            }
            let c = &mut record["x"]["c"];
            if text(c, "w") != user || text(c, "ws") != session {
                return false;
            }
            c["st"] = json!("merged");
            true
        });
    }
}

/// A broken reference's outcome is its reconciliation's: settled on the
/// steps as the roster reads them. `states` is `lint::reconciliation_states`.
pub fn settle_view(step: &mut Value, states: &std::collections::BTreeMap<String, String>) {
    let Some(c) = step.get_mut("x").and_then(|x| x.get_mut("c")) else { return };
    let rid = text(c, "rid");
    if rid.is_empty() {
        return;
    }
    if let Some(state) = states.get(&rid).filter(|s| !s.is_empty()) {
        c["st"] = json!(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn overlapping_lines_are_found_and_an_insertion_is_its_line() {
        assert_eq!(overlap(&[[10, 5]], &[[12, 2]]), Some([12, 13]));
        assert_eq!(overlap(&[[10, 5]], &[[15, 3]]), None);
        assert_eq!(overlap(&[[14, 0]], &[[10, 5]]), Some([14, 14]));
        assert_eq!(ranges_of(&json!({"hunks": [[1, 2, 3, 4], [9, 0, 9, 1]]})), vec![[3, 4], [9, 1]]);
    }

    #[test]
    fn a_collision_is_bounded() {
        let side = Side {
            kind: "overlap".into(), user: "a".repeat(80), session: "s".repeat(60), path: "p/".repeat(100),
            ranges: vec![[1, 2]; 9], overlap: Some([1, 2]), symbols: vec!["n".repeat(80); 9],
            sig: Some(("x".repeat(200), "y".repeat(200))), why: "w".repeat(200), ts: 1.0, outcome: "open",
            rid: "abc".into(), source: false,
        };
        let c = side.record();
        assert!(serde_json::to_string(&c).unwrap().len() <= MAX_C_BYTES, "{c}");
        assert_eq!(c["k"], "overlap");
        assert_eq!(c["o"], json!([1, 2]), "the overlap survives the shedding");
    }

    #[test]
    fn overlapping_writes_link_both_steps_and_a_later_write_merges_them() {
        let db = store();
        let (scope, path) = ("w:r", "shop/cart.py");
        // ann's agent edits lines 10-14
        presence::note_last_action(&db, scope, "ann@x.com", "a1", "edit", path, "claude", "opus", &crate::presence::Effort::default(), "dev", "", "", "");
        presence::note_exchange(&db, scope, "ann@x.com", "a1", Step::Current,
            &json!({"hunks": [[10, 3, 10, 5]], "sig": [["total", "total(a)", "total(a, b)"]]}));
        // bob's agent writes lines 12-13 of the same file a minute later
        presence::note_last_action(&db, scope, "bob@x.com", "b1", "edit", path, "codex", "gpt", &crate::presence::Effort::default(), "dev", "", "", "");
        let mine = json!({"hunks": [[12, 1, 12, 2]], "add": ["discount"]});
        let prior = ("ann@x.com".to_string(), "a1".to_string(), now() - 60.0);
        let met = Met { link: None, prior: Some(&prior), findings: &[] };
        let c = on_write(&db, scope, "bob@x.com", "b1", path, &mine, &met).expect("a collision");
        assert_eq!((c["k"].clone(), c["w"].clone(), c["o"].clone(), c["st"].clone()), (json!("overlap"), json!("ann@x.com"), json!([12, 13]), json!("open")));
        assert_eq!(c["r"], json!([[10, 5]]), "the other side's ranges");
        assert_eq!(c["sig"], json!(["total(a)", "total(a, b)"]));
        presence::note_exchange(&db, scope, "bob@x.com", "b1", Step::Latest { kind: "edit", path }, &json!({"c": c}));
        // ann's step carries the mirror: bob, his ranges, his names
        let theirs = presence::find_step(&db, scope, "ann@x.com", "a1", "edit", path).unwrap();
        let m = &theirs["x"]["c"];
        assert_eq!((m["w"].clone(), m["r"].clone(), m["s"].clone(), m["src"].clone()), (json!("bob@x.com"), json!([[12, 2]]), json!(["discount"]), json!(true)));
        // bob moves on, then writes the file again: both sides read merged
        presence::note_last_action(&db, scope, "bob@x.com", "b1", "reading", "x.py", "codex", "gpt", &crate::presence::Effort::default(), "dev", "", "", "");
        presence::note_last_action(&db, scope, "bob@x.com", "b1", "edit", path, "codex", "gpt", &crate::presence::Effort::default(), "dev", "", "", "");
        let none = Met { link: None, prior: None, findings: &[] };
        assert!(on_write(&db, scope, "bob@x.com", "b1", path, &json!({"hunks": [[40, 1, 40, 1]]}), &none).is_none());
        let history = presence::history(&db, scope, "bob@x.com", "b1");
        let earlier = history.iter().find(|h| h["path"] == path).unwrap();
        assert_eq!(earlier["x"]["c"]["st"], "merged");
        let theirs = presence::find_step(&db, scope, "ann@x.com", "a1", "edit", path).unwrap();
        assert_eq!(theirs["x"]["c"]["st"], "merged");
    }

    #[test]
    fn a_gate_stop_and_a_broken_reference_name_the_other_agent() {
        let db = store();
        let scope = "w:r";
        presence::note_last_action(&db, scope, "ann@x.com", "a1", "edit", "pay.py", "claude", "opus", &crate::presence::Effort::default(), "dev", "", "", "");
        presence::note_exchange(&db, scope, "ann@x.com", "a1", Step::Current, &json!({"hunks": [[5, 1, 5, 1]], "ren": [["charge", "charge_cents"]]}));
        // the gate held bob's write over ann's rename, then let the retry through
        let link = json!({"k": "gate", "w": "ann@x.com", "ws": "a1", "s": "charge", "sig": ["charge", "charge_cents"], "why": "ann renamed charge"});
        let met = Met { link: Some(&link), prior: None, findings: &[] };
        let c = on_write(&db, scope, "bob@x.com", "b1", "pay.py", &json!({"hunks": [[5, 2, 5, 2]]}), &met).unwrap();
        assert_eq!((c["k"].clone(), c["st"].clone(), c["s"].clone(), c["o"].clone()), (json!("gate"), json!("resolved"), json!(["charge"]), json!([5, 5])));
        // a broken reference: lint names ann's change, its reconciliation decides the outcome
        let findings = vec![json!({"rule": "stale_name", "symbol": "charge", "in": "pay.py", "renamed_to": "charge_cents",
            "by": "ann@x.com", "by_session": "a1", "fix": {"class": "judgment"}})];
        let met = Met { link: None, prior: None, findings: &findings };
        let mut c = on_write(&db, scope, "bob@x.com", "b1", "api.py", &json!({}), &met).unwrap();
        assert_eq!((c["k"].clone(), c["p"].clone(), c["st"].clone()), (json!("lint"), json!("pay.py"), json!("open")));
        let rid = text(&c, "rid");
        assert!(!rid.is_empty());
        let mut step = json!({"x": {"c": c.clone()}});
        settle_view(&mut step, &[(rid, "resolved".to_string())].into_iter().collect());
        c = step["x"]["c"].clone();
        assert_eq!(c["st"], "resolved");
    }
}
