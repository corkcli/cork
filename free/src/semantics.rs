//! Three-valued parse semantics: clean / partial / stale.
//!
//! The heart of the product, and the reason Cork can be trusted to say
//! "certain":
//!
//!   1. Information is only ever derived from clean parses. When the current
//!      parse is partial the last clean one is served, marked stale with its
//!      age.
//!   2. A symbol is *removed* only when absent from two consecutive clean
//!      parses. Absent from just the latest is "unknown / unconfirmed".
//!   3. A parse failure never converts a present symbol into a removed one.
//!
//! Rule 3 is what makes the gate safe to block on. An agent saving a file
//! mid-edit produces a broken parse constantly; if that read as "they deleted
//! everything", Cork would block the whole team on every keystroke.

use serde_json::{json, Map, Value};

/// One symbol out and one in used to be a rename, full stop — and "certain",
/// which is what the gate blocks teammates on. A five-day-old baseline once
/// paired a helper that had quietly disappeared with an unrelated route that
/// had appeared since, and the graph called it a live breaking change. So a
/// rename now needs evidence that the two are the same declaration under a
/// new name: a previous clean parse recent enough for the pair to have come
/// from one edit rather than an unknown number of them, the same kind of
/// declaration (what precedes the name — `def` is not `class`, `fn` is not
/// `async fn`), and a parameter list that mostly survived (at least half the
/// identifiers after the old name are still there, so a rename that adds a
/// parameter still counts). Anything short of this is what it looks like: a
/// symbol gone (pending removal) and a symbol added.
pub const RENAME_MAX_GAP_S: f64 = 6.0 * 3600.0;

/// `(head, tail)`: the signature before and after its own name, whitespace
/// collapsed; `None` when the name is not in it.
fn around(signature: &str, name: &str) -> Option<(String, String)> {
    if name.is_empty() {
        return None;
    }
    let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(name))).ok()?;
    let found = re.find(signature)?;
    let collapse = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    Some((collapse(&signature[..found.start()]), collapse(&signature[found.end()..])))
}

fn idents(text: &str) -> std::collections::BTreeSet<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("identifier pattern"));
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

/// Same kind of declaration, most of the same parameters, new name.
pub fn looks_like_rename(old_name: &str, old_signature: &str, new_name: &str, new_signature: &str) -> bool {
    let (Some(old), Some(new)) = (around(old_signature, old_name), around(new_signature, new_name)) else {
        return false;
    };
    if old.0 != new.0 {
        return false;
    }
    let old_idents = idents(&old.1);
    if old_idents.is_empty() {
        return old.1 == new.1;
    }
    let new_idents = idents(&new.1);
    old_idents.intersection(&new_idents).count() * 2 >= old_idents.len()
}

pub const CLEAN: &str = "clean";
pub const PARTIAL: &str = "partial";
pub const LIVE: &str = "live";
pub const STALE: &str = "stale";

pub const CERTAIN: &str = "certain";
pub const UNCONFIRMED: &str = "unconfirmed";
pub const UNKNOWN: &str = "unknown";

pub fn new_record() -> Value {
    json!({
        "status": CLEAN,
        "current_ts": 0.0,
        "last_clean": Value::Null,
        "prev_clean": Value::Null,
        "pending_removed": {},
    })
}

fn symbols_of(clean: Option<&Value>) -> Map<String, Value> {
    clean
        .and_then(|c| c.get("symbols"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn field(symbol: &Value, key: &str) -> String {
    symbol.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Fold a new parse into the record. Returns (record, events).
///
/// A partial parse yields no events and does not shift the clean history.
pub fn apply_parse(
    record: Option<&Value>, status: &str, new_symbols: Map<String, Value>, now: f64,
) -> (Value, Vec<Value>) {
    let mut record = record.cloned().unwrap_or_else(new_record);
    if !record.is_object() {
        record = new_record();
    }
    {
        let map = record.as_object_mut().expect("record is an object");
        map.insert("current_ts".into(), json!(now));
        if status == PARTIAL {
            map.insert("status".into(), json!(PARTIAL));
        }
    }
    if status == PARTIAL {
        return (record, Vec::new());
    }

    let prev = record.get("last_clean").cloned().filter(|v| !v.is_null());
    let prev_syms = symbols_of(prev.as_ref());
    {
        let map = record.as_object_mut().expect("record is an object");
        map.insert("prev_clean".into(), prev.clone().unwrap_or(Value::Null));
        map.insert("last_clean".into(), json!({"symbols": new_symbols, "ts": now}));
        map.insert("status".into(), json!(CLEAN));
    }

    let mut events: Vec<Value> = Vec::new();
    let mut pending: Map<String, Value> = record
        .get("pending_removed")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let removed_now: Vec<String> =
        prev_syms.keys().filter(|name| !new_symbols.contains_key(*name)).cloned().collect();
    let added_now: Vec<String> =
        new_symbols.keys().filter(|name| !prev_syms.contains_key(*name)).cloned().collect();
    let modified_now: Vec<String> = prev_syms
        .keys()
        .filter(|name| {
            new_symbols.get(*name).map(|new| field(new, "hash") != field(&prev_syms[*name], "hash"))
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    if let Some(prev) = prev.as_ref() {
        let prev_ts = prev.get("ts").and_then(Value::as_f64).unwrap_or(now);
        let recent = now - prev_ts <= RENAME_MAX_GAP_S;
        let is_rename = removed_now.len() == 1 && added_now.len() == 1 && recent && looks_like_rename(
            &removed_now[0], &field(&prev_syms[&removed_now[0]], "signature"),
            &added_now[0], &field(&new_symbols[&added_now[0]], "signature"),
        );
        if is_rename {
            // exactly one out, one in, the same declaration under a new name,
            // from a recent baseline: a rename, not a delete plus an add
            let (old_name, new_name) = (&removed_now[0], &added_now[0]);
            let new_signature = field(&new_symbols[new_name], "signature");
            events.push(json!({
                "symbol": old_name,
                "kind": "renamed",
                "detail": {"new_name": new_name, "signature": new_signature},
                "confidence": CERTAIN,
                // the interface as it was and as it is: enough to replay the
                // change without ever retaining source
                "signature_before": field(&prev_syms[old_name], "signature"),
                "signature_after": new_signature,
                "new_name": new_name,
            }));
            pending.remove(old_name);
        } else {
            for name in &removed_now {
                // first clean parse without the symbol: not yet a removal
                pending.insert(
                    name.clone(),
                    json!({"since": now, "symbol": prev_syms[name].clone()}),
                );
                events.push(json!({
                    "symbol": name,
                    "kind": UNKNOWN,
                    "detail": "absent from latest clean parse, not yet confirmed removed",
                    "confidence": UNCONFIRMED,
                }));
            }
            for name in &added_now {
                let signature = field(&new_symbols[name], "signature");
                events.push(json!({
                    "symbol": name,
                    "kind": "added",
                    "detail": signature,
                    "confidence": CERTAIN,
                    "signature_before": "",
                    "signature_after": signature,
                }));
            }
        }
        for name in &modified_now {
            let before = field(&prev_syms[name], "signature");
            let after = field(&new_symbols[name], "signature");
            let signature_changed = before != after;
            events.push(json!({
                "symbol": name,
                "kind": "modified",
                "detail": if signature_changed {
                    format!("signature changed to: {after}")
                } else {
                    "implementation changed, signature unchanged".to_string()
                },
                "confidence": CERTAIN,
                "signature_change": signature_changed,
                "signature_before": before,
                "signature_after": after,
            }));
        }
    }

    // promote pending removals absent from two consecutive clean parses
    for name in pending.keys().cloned().collect::<Vec<_>>() {
        if new_symbols.contains_key(&name) {
            pending.remove(&name); // reappeared: never was a removal
        } else if !removed_now.contains(&name) {
            let before = pending
                .get(&name)
                .and_then(|entry| entry.get("symbol"))
                .map(|symbol| field(symbol, "signature"))
                .unwrap_or_default();
            events.push(json!({
                "symbol": name,
                "kind": "removed",
                "detail": "absent from two consecutive clean parses",
                "confidence": CERTAIN,
                "signature_before": before,
                "signature_after": "",
            }));
            pending.remove(&name);
        }
    }

    if let Some(map) = record.as_object_mut() {
        map.insert("pending_removed".into(), Value::Object(pending));
    }
    (record, events)
}

/// The events of a write whose baseline another session wrote. One person's
/// agents share a file record, but not always a checkout: what the baseline
/// has and this agent's own copy never had is a teammate's work it has not
/// pulled, not something it took out. `own` is that copy as the agent last
/// read or wrote it, name to signature. A "rename" of a name it never had is
/// its new name added; an absence of one is no removal; a signature the copy
/// already had is the other copy's change, unconfirmed, and blocks no one.
pub fn across_copies(events: Vec<Value>, own: &Map<String, Value>) -> Vec<Value> {
    events
        .into_iter()
        .filter_map(|event| {
            let symbol = field(&event, "symbol");
            match event.get("kind").and_then(Value::as_str) {
                Some("renamed") if !own.contains_key(&symbol) => {
                    let new_name = field(&event, "new_name");
                    let signature = field(&event, "signature_after");
                    Some(json!({
                        "symbol": new_name, "kind": "added", "detail": signature,
                        "confidence": CERTAIN, "signature_before": "", "signature_after": signature,
                    }))
                }
                Some("removed") | Some(UNKNOWN) if !own.contains_key(&symbol) => None,
                Some("modified")
                    if event.get("signature_change").and_then(Value::as_bool).unwrap_or(false)
                        && own.get(&symbol).and_then(Value::as_str) == Some(field(&event, "signature_after").as_str()) =>
                {
                    let mut event = event;
                    event["confidence"] = json!(UNCONFIRMED);
                    Some(event)
                }
                _ => Some(event),
            }
        })
        .collect()
}

pub struct Served {
    pub symbols: Map<String, Value>,
    pub freshness: &'static str,
    pub age_s: f64,
}

/// The freshest servable symbols for a file. Never derives anything from a
/// partial parse: if the current state is partial, the last clean parse is
/// served marked stale with its age. `None` when no clean parse was ever seen.
pub fn serve(record: Option<&Value>, now: f64) -> Option<Served> {
    let record = record?;
    let last_clean = record.get("last_clean").filter(|v| !v.is_null())?;
    let symbols = last_clean.get("symbols").and_then(Value::as_object).cloned().unwrap_or_default();
    if record.get("status").and_then(Value::as_str) == Some(PARTIAL) {
        let ts = last_clean.get("ts").and_then(Value::as_f64).unwrap_or(now);
        let age = (now - ts).max(0.0);
        let age_s = crate::compat::python_round(age, 1);
        return Some(Served { symbols, freshness: STALE, age_s });
    }
    Some(Served { symbols, freshness: LIVE, age_s: 0.0 })
}

#[cfg(test)]
mod semantics_ported_tests {
    use super::*;

    const CLEAN_PY: &str = "import os\n\ndef greet(name: str) -> str:\n    return f\"hi {name}\"\n\nclass Widget:\n    def method(self):\n        pass\n";
    const BROKEN_PY: &str = "def greet(name: str) -> str\n    return f\"hi {name\"\n\ndef half_typed(\n";

    /// One parse as the report pipeline folds it: (status, name -> symbol).
    fn parsed(src: &str) -> (&'static str, Map<String, Value>) {
        let out = cork_core::engine::parse("a.py", src).expect("python parses");
        let symbols = out.symbols.iter().map(|s| (s.name.clone(), json!({
            "name": s.name, "kind": s.kind, "signature": s.signature, "hash": s.hash,
        }))).collect();
        (out.status, symbols)
    }

    fn apply(record: Option<&Value>, src: &str, now: f64) -> (Value, Vec<Value>) {
        let (status, symbols) = parsed(src);
        apply_parse(record, status, symbols, now)
    }

    fn kinds_of(events: &[Value], symbol: &str) -> Vec<(String, String)> {
        events.iter().filter(|e| e["symbol"] == symbol)
            .map(|e| (e["kind"].as_str().unwrap_or("").to_string(), e["confidence"].as_str().unwrap_or("").to_string()))
            .collect()
    }

    #[test]
    fn a_partial_parse_serves_the_last_clean_one_as_stale() {
        let (record, _) = apply(None, CLEAN_PY, 100.0);
        let (record, events) = apply(Some(&record), BROKEN_PY, 160.0);
        assert!(events.is_empty(), "nothing is derived from a partial parse");
        let served = serve(Some(&record), 160.0).expect("the clean parse is served");
        assert_eq!((served.freshness, served.age_s), (STALE, 60.0));
        assert!(served.symbols.contains_key("greet"), "a parse failure never removes a symbol");
    }

    #[test]
    fn no_clean_parse_serves_nothing() {
        let (record, events) = apply(None, BROKEN_PY, 1.0);
        assert!(events.is_empty());
        assert!(serve(Some(&record), 2.0).is_none());
        assert!(serve(None, 2.0).is_none());
    }

    #[test]
    fn a_rename_is_one_certain_event() {
        let (record, _) = apply(None, "def old_name(a, b):\n    return a\n", 1.0);
        let (_, events) = apply(Some(&record), "def new_name(a, b, c=None):\n    return a\n", 2.0);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!((events[0]["kind"].as_str(), events[0]["symbol"].as_str()), (Some("renamed"), Some("old_name")));
        assert_eq!(events[0]["detail"]["new_name"], "new_name");
        assert!(events[0]["detail"]["signature"].as_str().unwrap().contains("c=None"));
        assert_eq!(events[0]["confidence"], CERTAIN);
    }

    #[test]
    fn a_broken_interlude_then_a_rename_is_still_one_event() {
        let (record, _) = apply(None, "def old_name():\n    return 1\n", 1.0);
        let (record, mid) = apply(Some(&record), "def new_na(\n", 2.0);
        assert!(mid.is_empty());
        let (_, events) = apply(Some(&record), "def new_name():\n    return 1\n", 3.0);
        let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
        assert_eq!(kinds, vec!["renamed"]);
    }

    #[test]
    fn a_removal_needs_two_consecutive_clean_parses() {
        let two = "def keep():\n    pass\n\ndef gone():\n    pass\n\ndef also():\n    pass\n";
        let one = "def keep():\n    pass\n\ndef also():\n    pass\n";
        let (record, _) = apply(None, two, 1.0);
        let (record, events) = apply(Some(&record), one, 2.0);
        assert_eq!(kinds_of(&events, "gone"), vec![(UNKNOWN.to_string(), UNCONFIRMED.to_string())]);
        let (_, events) = apply(Some(&record), one, 3.0);
        assert_eq!(kinds_of(&events, "gone"), vec![("removed".to_string(), CERTAIN.to_string())]);
    }

    #[test]
    fn a_partial_between_cleans_does_not_confirm_a_removal() {
        let two = "def keep():\n    pass\n\ndef gone():\n    pass\n\ndef x():\n    pass\n";
        let one = "def keep():\n    pass\n\ndef x():\n    pass\n";
        let (record, _) = apply(None, two, 1.0);
        let (record, _) = apply(Some(&record), one, 2.0);
        let (record, events) = apply(Some(&record), "def keep(\n", 3.0);
        assert!(events.is_empty());
        // the symbol reappears: it never was a removal
        let (record, events) = apply(Some(&record), two, 4.0);
        assert_eq!(kinds_of(&events, "gone").first().map(|k| k.0.as_str()), Some("added"));
        assert!(!events.iter().any(|e| e["kind"] == "removed"));
        assert_eq!(record["pending_removed"], json!({}));
    }

    #[test]
    fn a_docstring_change_modifies_only_that_symbol() {
        let (record, _) = apply(None, "def a():\n    pass\n\ndef b():\n    pass\n", 1.0);
        let (_, events) = apply(Some(&record), "def a():\n    \"\"\"doc\"\"\"\n    pass\n\ndef b():\n    pass\n", 2.0);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!((events[0]["symbol"].as_str(), events[0]["kind"].as_str()), (Some("a"), Some("modified")));
        assert_eq!(events[0]["signature_change"], false);
    }

    #[test]
    fn a_signature_change_is_flagged() {
        let (record, _) = apply(None, "def f(a):\n    pass\n\ndef g():\n    pass\n", 1.0);
        let (_, events) = apply(Some(&record), "def f(a, b):\n    pass\n\ndef g():\n    pass\n", 2.0);
        assert_eq!(events[0]["kind"], "modified");
        assert_eq!(events[0]["signature_change"], true);
    }
}
