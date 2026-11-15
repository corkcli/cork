//! Parse on the machine, send the structure: source never leaves.
//!
//! The server keeps, from every file an agent writes or reads, only its
//! structure — symbols with signatures, hashes, spans, parameters, call
//! sites and docstrings, the imports, and the module doc — plus a hash per
//! line (for line-level motion) and where names occur (for the semantic
//! lint). It used to receive the whole file and throw the rest away. The
//! native hook now computes exactly those facts here, with the same parser
//! the server runs, and sends them instead of the file.
//!
//! What still travels is what the server has always kept: names,
//! signatures, docstrings, the argument text of calls. File bodies do not.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};

use crate::engine::{extension_of, Import, ParseOutput, Symbol};

/// A name occurs on at most this many lines before the rest are dropped —
/// the lint reads a handful; a file that says `self` 900 times need not ship
/// all of them.
pub const MAX_LINES_PER_NAME: usize = 25;

/// The per-line hash the server's line motion uses: first 8 hex of SHA-1,
/// one per `\n`-separated line. `diff::line_hashes` on the server, the same.
pub fn line_hashes(content: &str) -> Vec<String> {
    content
        .split('\n')
        .map(|line| {
            let mut hasher = Sha1::new();
            hasher.update(line.as_bytes());
            format!("{:x}", hasher.finalize())[..8].to_string()
        })
        .collect()
}

/// Every identifier in the file with the 1-based lines it appears on
/// (`str::lines` numbering, as the lint counts), capped per name. Dotted
/// chains are not kept: the lint matches a dotted rule by its last segment.
pub fn name_lines(content: &str) -> BTreeMap<String, Vec<usize>> {
    let mut out: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, line) in content.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_ascii_alphabetic() || c == b'_' {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                // a name glued to a preceding digit is not a name (0x1f, 3rd)
                if start > 0 && bytes[start - 1].is_ascii_digit() {
                    continue;
                }
                let name = &line[start..i];
                let lines = out.entry(name.to_string()).or_default();
                if lines.last() != Some(&(index + 1)) && lines.len() < MAX_LINES_PER_NAME {
                    lines.push(index + 1);
                }
            } else {
                i += 1;
            }
        }
    }
    out
}

/// The parse as JSON, field for field.
pub fn structure_json(parsed: &ParseOutput) -> Value {
    let symbols: Vec<Value> = parsed
        .symbols
        .iter()
        .map(|s| {
            json!({
                "name": s.name, "kind": s.kind, "signature": s.signature, "hash": s.hash,
                "refs": s.refs,
                "edges": s.edges.iter().map(|(a, b, c)| json!([a, b, c])).collect::<Vec<_>>(),
                "span": [s.span.0, s.span.1], "params": s.params,
                "sites": s.sites.iter().map(|(a, b, c)| json!([a, b, c])).collect::<Vec<_>>(),
                "doc": s.doc,
            })
        })
        .collect();
    let imports: Vec<Value> = parsed
        .imports
        .iter()
        .map(|i| json!({"module": i.module, "names": i.names.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>()}))
        .collect();
    json!({"status": parsed.status, "language": parsed.language, "symbols": symbols, "imports": imports, "doc": parsed.doc})
}

/// Everything the hook sends in place of the file. A file with no parser
/// (a `.sql` migration) sends `structure: null` — the server treats it as
/// unparsed, exactly as it did the file, and its line motion and claims
/// still work from the hashes.
pub fn fields(path: &str, content: &str) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert(
        "structure".into(),
        crate::engine::parse(path, content).map(|parsed| structure_json(&parsed)).unwrap_or(Value::Null),
    );
    out.insert("line_hashes".into(), json!(line_hashes(content)));
    out.insert("names".into(), json!(name_lines(content)));
    out
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

fn triples(value: Option<&Value>) -> Vec<(String, String, String)> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    let t = t.as_array()?;
                    Some((t.first()?.as_str()?.to_string(), t.get(1)?.as_str()?.to_string(), t.get(2)?.as_str()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The content hash the protocol bases a write on (SPEC.md §16, "Content
/// hashes"): SHA-256 hex of the text with `\r\n` turned into `\n` and the
/// trailing whitespace of every line stripped, so a line-ending or an
/// editor's trailing spaces never read as a change. Every hook and server
/// that names a symbol's content by hash uses this one function.
pub fn symbol_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(normalized(text).as_bytes());
    format!("{:x}", hasher.finalize())
}

fn normalized(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    text.split('\n').map(str::trim_end).collect::<Vec<_>>().join("\n")
}

/// `symbol_hash` with the symbol's own name blanked where it is declared:
/// the same before and after a pure rename, which is how a rename is told
/// from a delete and an insert.
pub fn shape_hash(text: &str, name: &str) -> String {
    let bare = bare_name(name);
    let text = normalized(text);
    let blanked = if bare.is_empty() { text } else { text.replacen(bare, "\u{0}", 1) };
    symbol_hash(&blanked)
}

/// The last segment of a qualified name: `Config.load` → `load`,
/// `impl Foo` → `Foo`.
pub fn bare_name(name: &str) -> &str {
    name.rsplit([' ', '.', ':']).next().unwrap_or(name)
}

/// One declaration as the protocol sees it: where it is and what its
/// content hashes to.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalSymbol {
    pub name: String,
    pub signature: String,
    /// 1-based first and last line, decorators included.
    pub span: (u32, u32),
    /// `symbol_hash` of the lines in `span`.
    pub hash: String,
    /// `shape_hash` of the same lines.
    pub shape: String,
}

/// The declarations of a file, for keying protocol ops. `None` when no
/// parser knows the file (it is keyed as a whole file); `clean: false`
/// when the parse hit a syntax error, so its symbols cannot be trusted.
pub struct LocalParse {
    pub clean: bool,
    pub symbols: Vec<LocalSymbol>,
}

pub fn symbols(path: &str, content: &str) -> Option<LocalParse> {
    let parsed = crate::engine::parse(path, content)?;
    let lines: Vec<&str> = content.split('\n').collect();
    let symbols = parsed
        .symbols
        .iter()
        .map(|s| {
            let from = (s.span.0.max(1) as usize - 1).min(lines.len());
            let to = (s.span.1 as usize).clamp(from, lines.len());
            let text = lines[from..to].join("\n");
            LocalSymbol {
                name: s.name.clone(),
                signature: s.signature.clone(),
                span: s.span,
                hash: symbol_hash(&text),
                shape: shape_hash(&text, &s.name),
            }
        })
        .collect();
    Some(LocalParse { clean: parsed.status == "clean", symbols })
}

/// The server's side: the structure a hook sent, as the parse the server
/// would have made. The language comes from the path (a client cannot name
/// one the server does not know) and the status must be one the parser
/// produces; anything else is refused, and the report is treated as
/// unparsed.
pub fn from_structure(path: &str, structure: &Value) -> Option<ParseOutput> {
    let spec = crate::langs::spec_for_ext(&extension_of(path))?;
    let status: &'static str = match structure.get("status").and_then(Value::as_str)? {
        "clean" => "clean",
        "partial" => "partial",
        _ => return None,
    };
    let symbols: Vec<Symbol> = structure
        .get("symbols")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|s| {
            let span = s.get("span").and_then(Value::as_array)?;
            Some(Symbol {
                name: text(s, "name"),
                kind: text(s, "kind"),
                signature: text(s, "signature"),
                hash: text(s, "hash"),
                refs: strings(s.get("refs")),
                edges: triples(s.get("edges")),
                span: (span.first()?.as_u64()? as u32, span.get(1)?.as_u64()? as u32),
                params: strings(s.get("params")),
                sites: s
                    .get("sites")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| {
                                let t = t.as_array()?;
                                Some((t.first()?.as_str()?.to_string(), t.get(1)?.as_u64()? as u32, t.get(2)?.as_str()?.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                doc: text(s, "doc"),
            })
        })
        .filter(|s| !s.name.is_empty() && !s.hash.is_empty())
        .collect();
    let imports: Vec<Import> = structure
        .get("imports")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|i| Import {
                    module: text(i, "module"),
                    names: i
                        .get("names")
                        .and_then(Value::as_array)
                        .map(|n| {
                            n.iter()
                                .filter_map(|p| {
                                    let p = p.as_array()?;
                                    Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ParseOutput { status, language: spec.name, symbols, imports, doc: text(structure, "doc") })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "\"\"\"Tax rules.\"\"\"\nimport os\n\ndef compute_tax(amount, rate):\n    \"\"\"Apply the rate.\"\"\"\n    return round(amount * rate, 2)\n\n\ndef total(items):\n    return sum(compute_tax(i, 0.2) for i in items)\n";

    #[test]
    fn the_structure_round_trips_to_the_same_parse() {
        let direct = crate::engine::parse("pkg/tax.py", SRC).unwrap();
        let sent = fields("pkg/tax.py", SRC);
        let back = from_structure("pkg/tax.py", &sent["structure"]).unwrap();
        assert_eq!(structure_json(&direct), structure_json(&back));
        assert_eq!(back.language, direct.language);
        // and the file body is nowhere in what is sent
        let wire = serde_json::to_string(&sent).unwrap();
        assert!(!wire.contains("return round(amount * rate, 2)"));
    }

    #[test]
    fn a_client_cannot_name_a_language_or_status_the_parser_does_not_make() {
        let sent = fields("pkg/tax.py", SRC);
        let mut forged = sent["structure"].clone();
        forged["status"] = json!("whatever");
        assert!(from_structure("pkg/tax.py", &forged).is_none());
        assert!(from_structure("notes.xyz", &sent["structure"]).is_none());
    }

    #[test]
    fn the_content_hash_ignores_line_endings_and_trailing_space_only() {
        let h = symbol_hash("def f():\n    return 1\n");
        assert_eq!(h, symbol_hash("def f():  \r\n    return 1\t\r\n"));
        assert_ne!(h, symbol_hash("def f():\n    return 2\n"));
        assert_ne!(h, symbol_hash("def f():\n  return 1\n"), "leading space is code");
        // pinned: another implementation must hash exactly this way
        assert_eq!(symbol_hash("a \r\nb"), "7e18f737311b2dc3b2f269dd78396b0351f14fb66efa879f768cb23181883c78");
    }

    #[test]
    fn a_rename_keeps_the_shape_and_a_body_edit_does_not() {
        let renamed = SRC.replace("def compute_tax(", "def tax_of(");
        let before = symbols("pkg/tax.py", SRC).unwrap();
        let after = symbols("pkg/tax.py", &renamed).unwrap();
        assert!(before.clean && after.clean);
        let (b, a) = (&before.symbols[0], &after.symbols[0]);
        assert_eq!((b.name.as_str(), a.name.as_str()), ("compute_tax", "tax_of"));
        assert_ne!(b.hash, a.hash);
        assert_eq!(b.shape, a.shape);
        let edited = symbols("pkg/tax.py", &SRC.replace("rate, 2)", "rate, 4)")).unwrap();
        assert_ne!(edited.symbols[0].shape, b.shape);
        assert_eq!(edited.symbols[1].hash, before.symbols[1].hash, "the other symbol is untouched");
        assert_eq!(b.span, (4, 6));
        assert!(symbols("notes.txt", "x").is_none());
        assert!(!symbols("a.py", "def (:\n").unwrap().clean);
        assert_eq!(bare_name("Config.load"), "load");
        assert_eq!(bare_name("impl Foo"), "Foo");
    }

    #[test]
    fn names_carry_their_lines() {
        let names = name_lines(SRC);
        assert_eq!(names["compute_tax"], vec![4, 10]);
        assert_eq!(line_hashes("a\nb").len(), 2);
    }
}
