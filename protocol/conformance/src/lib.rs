//! Conformance: a harness around the Engine with the code, notes and tasks domains,
//! and a runner for the golden scenarios in `schema/vectors/*.json`.
//!
//! A vector is a list of steps. Each step does one thing (connect a named
//! connection, send it a frame, disconnect it, tick the clock, add graph
//! edges) at a time `at` (ms), and lists exactly the frames the engine must
//! emit, in order:
//!
//! ```json
//! {"at": 0, "conn": "a", "send": {"t": "op", ...},
//!  "expect": [{"conn": "a", "recv": {"t": "ack", "version": 1}}]}
//! ```
//!
//! `recv` matches as a subset: every field it names must match, others are
//! ignored. Strings starting with `$` are placeholders: `"$any"` matches
//! anything present, `"$bind:x"` matches anything and remembers it as `x`,
//! `"$ref:x"` must equal what `x` was bound to.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use cork_protocol::{ClientFrame, Config, ConnId, Engine, MemGraph, MemLog, Outgoing, ServerFrame};
use cork_protocol_code::CodeDomain;
use cork_protocol_notes::NotesDomain;
use cork_protocol_tasks::TasksDomain;
use serde_json::Value;

pub struct Harness {
    pub engine: Engine,
    pub code_graph: Arc<MemGraph>,
    pub notes_graph: Arc<MemGraph>,
    pub tasks_graph: Arc<MemGraph>,
    pub log: Arc<Mutex<MemLog>>,
    conns: HashMap<String, ConnId>,
    names: HashMap<ConnId, String>,
    next: ConnId,
}

impl Harness {
    pub fn new(cfg: Config) -> Self {
        let code = CodeDomain::new();
        let code_graph = code.graph_handle().unwrap();
        let notes = NotesDomain::new();
        let notes_graph = notes.graph_handle();
        let tasks = TasksDomain::new();
        let tasks_graph = tasks.graph_handle();
        let log = Arc::new(Mutex::new(MemLog::new()));
        let engine = Engine::builder().config(cfg).domain(code).domain(notes).domain(tasks).log(log.clone()).build();
        Harness { engine, code_graph, notes_graph, tasks_graph, log, conns: HashMap::new(), names: HashMap::new(), next: 1 }
    }

    /// Open a new connection under `name` (replacing an earlier one).
    pub fn connect(&mut self, name: &str, now: u64) -> Vec<(String, ServerFrame)> {
        let id = self.next;
        self.next += 1;
        self.conns.insert(name.to_string(), id);
        self.names.insert(id, name.to_string());
        let outs = self.engine.connected(id, now);
        self.name(outs)
    }

    pub fn disconnect(&mut self, name: &str, now: u64) -> Vec<(String, ServerFrame)> {
        let id = self.conns[name];
        let outs = self.engine.disconnected(id, now);
        self.name(outs)
    }

    pub fn send(&mut self, name: &str, frame: ClientFrame, now: u64) -> Vec<(String, ServerFrame)> {
        if !self.conns.contains_key(name) {
            self.connect(name, now);
        }
        let id = self.conns[name];
        let outs = self.engine.handle(id, frame, now);
        self.name(outs)
    }

    pub fn send_json(&mut self, name: &str, frame: &Value, now: u64) -> Result<Vec<(String, ServerFrame)>, String> {
        let f: ClientFrame = serde_json::from_value(frame.clone()).map_err(|e| format!("send frame does not decode: {e}: {frame}"))?;
        Ok(self.send(name, f, now))
    }

    pub fn tick(&mut self, now: u64) -> Vec<(String, ServerFrame)> {
        let outs = self.engine.tick(now);
        self.name(outs)
    }

    fn name(&self, outs: Vec<Outgoing>) -> Vec<(String, ServerFrame)> {
        outs.into_iter().map(|o| (self.names.get(&o.conn).cloned().unwrap_or_else(|| o.conn.to_string()), o.frame)).collect()
    }
}

/// Does `actual` match the partial `expected`? Binds placeholders.
pub fn matches(expected: &Value, actual: &Value, binds: &mut HashMap<String, Value>, path: &str) -> Result<(), String> {
    match expected {
        Value::String(s) if s == "$any" => Ok(()),
        Value::String(s) if s.starts_with("$bind:") => {
            binds.insert(s[6..].to_string(), actual.clone());
            Ok(())
        }
        Value::String(s) if s.starts_with("$ref:") => {
            let want = binds.get(&s[5..]).ok_or_else(|| format!("{path}: {s} was never bound"))?;
            if want == actual {
                Ok(())
            } else {
                Err(format!("{path}: expected {s} = {want}, got {actual}"))
            }
        }
        Value::Object(m) => {
            let a = actual.as_object().ok_or_else(|| format!("{path}: expected an object, got {actual}"))?;
            for (k, v) in m {
                let av = a.get(k).ok_or_else(|| format!("{path}.{k}: missing in {actual}"))?;
                matches(v, av, binds, &format!("{path}.{k}"))?;
            }
            Ok(())
        }
        Value::Array(xs) => {
            let a = actual.as_array().ok_or_else(|| format!("{path}: expected an array, got {actual}"))?;
            if xs.len() != a.len() {
                return Err(format!("{path}: expected {} items, got {}: {actual}", xs.len(), a.len()));
            }
            for (i, (x, y)) in xs.iter().zip(a).enumerate() {
                matches(x, y, binds, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        other => {
            if other == actual {
                Ok(())
            } else {
                Err(format!("{path}: expected {other}, got {actual}"))
            }
        }
    }
}

/// Replace `"$ref:x"` strings in a frame to send with what `x` was bound to
/// (a hash from an earlier ack, say).
pub fn substitute(v: &Value, binds: &HashMap<String, Value>) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) if s.starts_with("$ref:") => binds.get(&s[5..]).cloned().ok_or_else(|| format!("{s} was never bound"))?,
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| Ok((k.clone(), substitute(x, binds)?))).collect::<Result<_, String>>()?),
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, binds)).collect::<Result<_, _>>()?),
        other => other.clone(),
    })
}

/// Replay one vector; Err names the first step that diverged.
pub fn run_vector(v: &Value) -> Result<(), String> {
    run_vector_collect(v).map(|_| ())
}

/// Replay one vector and return every frame the engine sent, as JSON.
pub fn run_vector_collect(v: &Value) -> Result<Vec<Value>, String> {
    let mut all = Vec::new();
    let mut cfg = Config::default();
    if let Some(c) = v.get("config") {
        if let Some(g) = c.get("grace_ms").and_then(Value::as_u64) {
            cfg.grace_ms = g;
        }
        if let Some(d) = c.get("max_depth").and_then(Value::as_u64) {
            cfg.max_depth = d as u32;
        }
        if let Some(h) = c.get("history_per_key").and_then(Value::as_u64) {
            cfg.history_per_key = h as usize;
        }
    }
    let mut h = Harness::new(cfg);
    let mut binds = HashMap::new();
    let steps = v.get("steps").and_then(Value::as_array).ok_or("vector has no steps")?;
    let mut now = 0u64;
    for (i, step) in steps.iter().enumerate() {
        if let Some(at) = step.get("at").and_then(Value::as_u64) {
            now = at;
        }
        let conn = step.get("conn").and_then(Value::as_str).unwrap_or("");
        let outs = if let Some(edges) = step.get("graph") {
            for e in edges.as_array().ok_or("graph must be a list")? {
                let scope = e[0].as_str().ok_or("graph edge: [scope, dependent, dependency]")?;
                let (a, b) = (e[1].as_str().unwrap_or(""), e[2].as_str().unwrap_or(""));
                let g = if a.starts_with("note:") {
                    &h.notes_graph
                } else if a.starts_with("task:") {
                    &h.tasks_graph
                } else {
                    &h.code_graph
                };
                g.add_edge(scope, a, b);
            }
            Vec::new()
        } else if step.get("connect").is_some() {
            h.connect(conn, now)
        } else if step.get("disconnect").is_some() {
            h.disconnect(conn, now)
        } else if step.get("tick").is_some() {
            h.tick(now)
        } else if let Some(f) = step.get("send") {
            let f = substitute(f, &binds).map_err(|e| format!("step {i}: {e}"))?;
            h.send_json(conn, &f, now).map_err(|e| format!("step {i}: {e}"))?
        } else {
            return Err(format!("step {i}: nothing to do"));
        };
        let expect = step.get("expect").and_then(Value::as_array).cloned().unwrap_or_default();
        let actual: Vec<Value> =
            outs.iter().map(|(c, f)| serde_json::json!({"conn": c, "recv": serde_json::to_value(f).unwrap()})).collect();
        let show = || serde_json::to_string_pretty(&actual).unwrap();
        if expect.len() != actual.len() {
            return Err(format!("step {i}: expected {} frames, got {}:\n{}", expect.len(), actual.len(), show()));
        }
        for (j, (e, a)) in expect.iter().zip(&actual).enumerate() {
            matches(e, a, &mut binds, &format!("step {i} frame {j}")).map_err(|m| format!("{m}\nactual:\n{}", show()))?;
        }
        all.extend(actual.into_iter().map(|mut a| a["recv"].take()));
    }
    let log = h.log.lock().unwrap();
    log.verify().map_err(|e| format!("log chain: {e}"))?;
    Ok(all)
}

/// A small JSON Schema checker: enough of draft 2020-12 for
/// `frames.schema.json` ($ref, oneOf, const, enum, type, properties,
/// required, additionalProperties: false, items, minimum, maxLength).
pub fn validate(schema: &Value, root: &Value, v: &Value, path: &str) -> Result<(), String> {
    let Some(s) = schema.as_object() else { return Ok(()) };
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let name = r.trim_start_matches("#/$defs/");
        let target = root["$defs"].get(name).ok_or_else(|| format!("{path}: unknown $ref {r}"))?;
        return validate(target, root, v, path);
    }
    if let Some(alts) = s.get("oneOf").and_then(Value::as_array) {
        let ok: Vec<String> = alts.iter().filter(|a| validate(a, root, v, path).is_ok()).map(|a| a.to_string()).collect();
        if ok.len() != 1 {
            let why: Vec<String> =
                alts.iter().filter_map(|a| validate(a, root, v, path).err()).filter(|e| !e.contains(".t: expected")).collect();
            return Err(format!("{path}: matches {} of oneOf: {}", ok.len(), why.join(" | ")));
        }
        return Ok(());
    }
    if let Some(c) = s.get("const") {
        if c != v {
            return Err(format!("{path}: expected {c}, got {v}"));
        }
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array) {
        if !e.contains(v) {
            return Err(format!("{path}: {v} not in {e:?}"));
        }
    }
    match s.get("type").and_then(Value::as_str) {
        Some("object") if !v.is_object() => return Err(format!("{path}: expected object, got {v}")),
        Some("array") if !v.is_array() => return Err(format!("{path}: expected array, got {v}")),
        Some("string") if !v.is_string() => return Err(format!("{path}: expected string, got {v}")),
        Some("boolean") if !v.is_boolean() => return Err(format!("{path}: expected boolean, got {v}")),
        Some("integer") if !(v.is_u64() || v.is_i64()) => return Err(format!("{path}: expected integer, got {v}")),
        _ => {}
    }
    if let (Some(m), Some(n)) = (s.get("minimum").and_then(Value::as_i64), v.as_i64()) {
        if n < m {
            return Err(format!("{path}: {n} < {m}"));
        }
    }
    if let (Some(m), Some(t)) = (s.get("maxLength").and_then(Value::as_u64), v.as_str()) {
        if t.len() as u64 > m {
            return Err(format!("{path}: longer than {m}"));
        }
    }
    if let Some(o) = v.as_object() {
        let props = s.get("properties").and_then(Value::as_object);
        for r in s.get("required").and_then(Value::as_array).into_iter().flatten() {
            let r = r.as_str().unwrap();
            if !o.contains_key(r) {
                return Err(format!("{path}: missing required `{r}`"));
            }
        }
        for (k, x) in o {
            match props.and_then(|p| p.get(k)) {
                Some(ps) => validate(ps, root, x, &format!("{path}.{k}"))?,
                None if s.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    return Err(format!("{path}: unexpected field `{k}`"))
                }
                None => {}
            }
        }
    }
    if let (Some(items), Some(a)) = (s.get("items"), v.as_array()) {
        for (i, x) in a.iter().enumerate() {
            validate(items, root, x, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

/// Every `*.json` in a directory, sorted, with its parsed content.
pub fn load_vectors(dir: &Path) -> Vec<(String, Value)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    v.sort();
    v.into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p).unwrap();
            (p.file_name().unwrap().to_string_lossy().into_owned(), serde_json::from_str(&text).unwrap_or_else(|e| panic!("{p:?}: {e}")))
        })
        .collect()
}
