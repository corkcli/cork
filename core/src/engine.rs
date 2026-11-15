//! The parse hot path.
//!
//! This runs on every `report_edit`, which in a swarm is the most frequent
//! call in the system: one declaration walk per language table entry, typed
//! edges, signatures and content hashes.

use std::collections::{BTreeMap, HashMap, HashSet};

use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

use crate::langs::{spec_for_ext, language_of, ImportStyle, LangSpec, NameStyle, SigStyle};

pub const EDGE_KINDS: [&str; 4] = ["inherits", "calls", "uses_type", "references"];

const IDENT_TYPES: &[&str] = &[
    "identifier", "type_identifier", "constant", "name", "field_identifier",
    "package_identifier", "namespace_identifier", "scoped_type_identifier",
];
const CALL_TYPES: &[&str] = &[
    "call", "call_expression", "method_invocation", "invocation_expression",
    "function_call_expression", "member_call_expression", "scoped_call_expression",
    "nullsafe_member_call_expression", "macro_invocation", "object_creation_expression",
    "new_expression", "generic_function", "composite_literal",
];
const HERITAGE_TYPES: &[&str] = &[
    "class_heritage", "extends_clause", "implements_clause", "superclass",
    "super_interfaces", "base_list", "base_clause", "class_interface_clause",
    "extends_type_clause", "base_class_clause", "trait_bounds", "delegation_specifiers",
];
/// node kind -> (field holding the object, field holding the member)
const MEMBER_TYPES: &[(&str, &str, &str)] = &[
    ("attribute", "object", "attribute"),
    ("member_expression", "object", "property"),
    ("selector_expression", "operand", "field"),
    ("scoped_identifier", "path", "name"),
    ("field_access", "object", "field"),
    ("member_access_expression", "expression", "name"),
    ("qualified_name", "qualifier", "name"),
    ("scoped_call_expression", "scope", "name"),
    ("class_constant_access_expression", "", ""),
    ("member_call_expression", "object", "name"),
    ("scope_resolution", "scope", "name"),
    ("qualified_identifier", "scope", "name"),
    ("field_expression", "argument", "field"),
];
const UNWRAP_CHAIN: &[&str] = &[
    "generic_type", "parenthesized_expression", "non_null_expression", "as_expression",
    "await_expression", "unary_expression", "type_arguments", "generic_name",
    "reference_type", "pointer_type",
];
const BODY_TYPES: &[&str] = &[
    "block", "statement_block", "class_body", "declaration_list", "compound_statement",
    "field_declaration_list", "enum_body", "interface_body", "body_statement",
    "enum_variant_list", "enumerator_list", "enum_declaration_list", "function_body",
    "impl_body", "trait_body", "namespace_body",
];
const CLIKE_NAME_TYPES: &[&str] = &[
    "identifier", "type_identifier", "field_identifier", "qualified_identifier",
    "operator_name", "destructor_name",
];

pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub hash: String,
    pub refs: Vec<String>,
    pub edges: Vec<(String, String, String)>,
    /// 1-based first and last line of the declaration, decorators included:
    /// the range an agent reads instead of the file.
    pub span: (u32, u32),
    /// Parameter names, in order — what is in scope inside the body.
    pub params: Vec<String>,
    /// Every call this declaration makes to a top-level or imported name:
    /// (name as written, line, argument text). The exact answer to "what
    /// does this caller pass", with no model in the loop.
    pub sites: Vec<(String, u32, String)>,
    /// The declaration's own documentation (to 600 chars) — the
    /// docstring in Python, the comment block directly above it elsewhere —
    /// so a convention an agent leaves in the code reaches the next agent.
    pub doc: String,
}

pub struct Import {
    pub module: String,
    pub names: Vec<(String, String)>,
}

pub struct ParseOutput {
    pub status: &'static str,
    pub language: &'static str,
    /// declaration order preserved
    pub symbols: Vec<Symbol>,
    pub imports: Vec<Import>,
    /// The module's own documentation: Python's module docstring, or a
    /// file-top comment block separated from the first node by a blank line.
    pub doc: String,
}

// ------------------------------------------------------------------ helpers

fn member_fields(kind: &str) -> Option<(&'static str, &'static str)> {
    MEMBER_TYPES.iter().find(|(k, _, _)| *k == kind).map(|(_, o, m)| (*o, *m))
}

fn text_of<'a>(node: Node<'a>, src: &'a [u8]) -> String {
    String::from_utf8_lossy(&src[node.byte_range()]).into_owned()
}

fn named_children<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn all_children<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

fn symbol_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Whitespace runs collapsed to single spaces, ends trimmed.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first `limit` characters — characters, not bytes.
fn take_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit { text.to_string() } else { text.chars().take(limit).collect() }
}

fn rstrip_set(text: &str, set: &str) -> String {
    text.trim_end_matches(|c| set.contains(c)).to_string()
}

fn strip_quotes(text: &str) -> String {
    let text = text.trim();
    let chars: Vec<char> = text.chars().collect();
    if chars.len() >= 2 {
        let (first, last) = (chars[0], chars[chars.len() - 1]);
        if first == last && (first == '"' || first == '\'' || first == '`') {
            return chars[1..chars.len() - 1].iter().collect();
        }
        if first == '<' && last == '>' {
            return chars[1..chars.len() - 1].iter().collect();
        }
    }
    text.to_string()
}

fn has_missing(node: Node) -> bool {
    if node.is_missing() {
        return true;
    }
    if !node.has_error() {
        return false;
    }
    all_children(node)
        .into_iter()
        .any(|child| has_missing(child) || child.kind() == "ERROR" || child.has_error())
}

// ------------------------------------------------------- per-language pieces

fn unwrap<'a>(node: Node<'a>, spec: &LangSpec) -> Node<'a> {
    match spec.names {
        NameStyle::Python if node.kind() == "decorated_definition" => {
            node.child_by_field_name("definition").unwrap_or(node)
        }
        NameStyle::TsLike if node.kind() == "export_statement" => {
            node.child_by_field_name("declaration").unwrap_or(node)
        }
        NameStyle::CLike if node.kind() == "template_declaration" => named_children(node)
            .into_iter()
            .find(|c| {
                matches!(
                    c.kind(),
                    "function_definition" | "class_specifier" | "struct_specifier"
                        | "declaration" | "alias_declaration" | "concept_definition"
                )
            })
            .unwrap_or(node),
        _ => node,
    }
}

fn declarator_name(node: Node, src: &[u8]) -> Option<String> {
    let mut current = Some(node);
    for _ in 0..12 {
        let node = current?;
        if CLIKE_NAME_TYPES.contains(&node.kind()) {
            return Some(text_of(node, src));
        }
        current = node.child_by_field_name("declarator").or_else(|| {
            named_children(node).into_iter().find(|c| {
                c.kind().ends_with("declarator") || c.kind() == "identifier" || c.kind() == "qualified_identifier"
            })
        });
    }
    None
}

fn go_receiver_type(node: Node, src: &[u8]) -> String {
    let params = node
        .child_by_field_name("receiver")
        .or_else(|| named_children(node).into_iter().find(|c| c.kind() == "parameter_list"));
    let Some(params) = params else { return String::new() };
    for decl in named_children(params) {
        if let Some(typ) = decl.child_by_field_name("type") {
            let mut stack = vec![typ];
            while let Some(current) = stack.pop() {
                if current.kind() == "type_identifier" {
                    return text_of(current, src);
                }
                stack.extend(named_children(current));
            }
        }
    }
    String::new()
}

fn name_of(node: Node, src: &[u8], spec: &LangSpec) -> Option<String> {
    let inner = unwrap(node, spec);
    match spec.names {
        NameStyle::Python | NameStyle::Field => {
            inner.child_by_field_name("name").map(|n| text_of(n, src))
        }
        NameStyle::TsLike => {
            if matches!(inner.kind(), "lexical_declaration" | "variable_declaration") {
                for child in named_children(inner) {
                    if child.kind() == "variable_declarator" {
                        if let Some(name) = child.child_by_field_name("name") {
                            return Some(text_of(name, src));
                        }
                    }
                }
                return None;
            }
            inner.child_by_field_name("name").map(|n| text_of(n, src))
        }
        NameStyle::Go => {
            let name = inner.child_by_field_name("name")?;
            if inner.kind() == "method_declaration" {
                let recv = go_receiver_type(inner, src);
                if !recv.is_empty() {
                    return Some(format!("{recv}.{}", text_of(name, src)));
                }
            }
            Some(text_of(name, src))
        }
        NameStyle::Rust => {
            if inner.kind() == "impl_item" {
                let typ = inner.child_by_field_name("type")?;
                let base = text_of(typ, src);
                return Some(match inner.child_by_field_name("trait") {
                    Some(trait_node) => format!("impl {} for {base}", text_of(trait_node, src)),
                    None => format!("impl {base}"),
                });
            }
            inner.child_by_field_name("name").map(|n| text_of(n, src))
        }
        NameStyle::CLike => match inner.kind() {
            "struct_specifier" | "union_specifier" | "enum_specifier" | "class_specifier" => {
                inner.child_by_field_name("body")?;
                inner.child_by_field_name("name").map(|n| text_of(n, src))
            }
            "alias_declaration" | "concept_definition" => {
                inner.child_by_field_name("name").map(|n| text_of(n, src))
            }
            "type_definition" => {
                let decl = inner.child_by_field_name("declarator")?;
                declarator_name(decl, src)
            }
            "function_definition" | "declaration" => {
                let mut decl = inner.child_by_field_name("declarator")?;
                if inner.kind() == "declaration" && decl.kind() == "init_declarator" {
                    decl = decl.child_by_field_name("declarator").unwrap_or(decl);
                }
                declarator_name(decl, src)
            }
            _ => None,
        },
    }
}

/// Go refines `type_spec` into struct / interface / type.
fn kind_override(node: Node, spec: &LangSpec) -> Option<&'static str> {
    if spec.names != NameStyle::Go || node.kind() != "type_spec" {
        return None;
    }
    Some(match node.child_by_field_name("type").map(|t| t.kind()) {
        Some("struct_type") => "struct",
        Some("interface_type") => "interface",
        _ => "type",
    })
}

fn signature_of(node: Node, src: &[u8], spec: &LangSpec) -> String {
    match spec.sig {
        SigStyle::Python => {
            let inner = unwrap(node, spec);
            let end = inner
                .child_by_field_name("body")
                .map(|b| b.start_byte())
                .unwrap_or_else(|| inner.end_byte());
            let raw = String::from_utf8_lossy(&src[inner.start_byte()..end]).into_owned();
            rstrip_set(&collapse(&raw), ":").trim().to_string()
        }
        SigStyle::TsLike => {
            let inner = unwrap(node, spec);
            let end = inner
                .child_by_field_name("body")
                .map(|b| b.start_byte())
                .unwrap_or_else(|| inner.end_byte());
            let raw = String::from_utf8_lossy(&src[inner.start_byte()..end]).into_owned();
            let sig = take_chars(&collapse(&raw), 200);
            rstrip_set(&sig, "{=").trim().to_string()
        }
        SigStyle::Generic => {
            let body = node.child_by_field_name("body").or_else(|| {
                named_children(node).into_iter().find(|c| BODY_TYPES.contains(&c.kind()))
            });
            let end = body.map(|b| b.start_byte()).unwrap_or_else(|| node.end_byte());
            let raw = String::from_utf8_lossy(&src[node.start_byte()..end]).into_owned();
            let sig = take_chars(&collapse(&raw), 200);
            rstrip_set(&sig, "{=:;").trim().to_string()
        }
    }
}

// -------------------------------------------------------------- import specs

fn imports_of(root: Node, src: &[u8], spec: &LangSpec) -> Vec<Import> {
    let mut out: Vec<Import> = Vec::new();
    match spec.imports {
        ImportStyle::Python => {
            // anywhere in the file, in source order: an import inside a
            // function (the usual way around an import cycle) binds a name
            // that function calls, and without it the call was dropped
            let mut found: Vec<Node> = Vec::new();
            let mut stack: Vec<Node> = named_children(root).into_iter().rev().collect();
            while let Some(node) = stack.pop() {
                if matches!(node.kind(), "import_statement" | "import_from_statement") {
                    found.push(node);
                    continue;
                }
                stack.extend(named_children(node).into_iter().rev());
            }
            for node in found {
                if node.kind() == "import_statement" {
                    for child in named_children(node) {
                        if child.kind() == "aliased_import" {
                            let module = child
                                .child_by_field_name("name")
                                .map(|n| text_of(n, src))
                                .unwrap_or_default();
                            let local = child
                                .child_by_field_name("alias")
                                .map(|n| text_of(n, src))
                                .unwrap_or_else(|| module.split('.').next().unwrap_or("").to_string());
                            out.push(Import { module, names: vec![(local, "*".into())] });
                        } else if child.kind() == "dotted_name" {
                            let module = text_of(child, src);
                            let local = module.split('.').next().unwrap_or("").to_string();
                            out.push(Import { module, names: vec![(local, "*".into())] });
                        }
                    }
                } else if node.kind() == "import_from_statement" {
                    let module_node = node.child_by_field_name("module_name");
                    let module = module_node.map(|n| text_of(n, src)).unwrap_or_default();
                    let mut names = Vec::new();
                    for child in named_children(node) {
                        if let Some(m) = module_node {
                            if child.id() == m.id() {
                                continue;
                            }
                        }
                        match child.kind() {
                            "dotted_name" => {
                                let original = text_of(child, src);
                                names.push((original.clone(), original));
                            }
                            "aliased_import" => {
                                let original = child
                                    .child_by_field_name("name")
                                    .map(|n| text_of(n, src))
                                    .unwrap_or_default();
                                let local = child
                                    .child_by_field_name("alias")
                                    .map(|n| text_of(n, src))
                                    .unwrap_or_else(|| original.clone());
                                names.push((local, original));
                            }
                            "wildcard_import" => names.push(("*".into(), "*".into())),
                            _ => {}
                        }
                    }
                    out.push(Import { module, names });
                }
            }
        }
        ImportStyle::TsLike => {
            for node in named_children(root) {
                let kind = node.kind();
                if kind == "import_statement" {
                    let module = node
                        .child_by_field_name("source")
                        .map(|n| strip_quotes(&text_of(n, src)))
                        .unwrap_or_default();
                    let clause = named_children(node).into_iter().find(|c| c.kind() == "import_clause");
                    let names = match clause {
                        Some(clause) => clause_names(clause, src),
                        None => vec![("*".to_string(), "*".to_string())],
                    };
                    out.push(Import { module, names });
                } else if kind == "export_statement" && node.child_by_field_name("source").is_some() {
                    let module = strip_quotes(&text_of(node.child_by_field_name("source").unwrap(), src));
                    out.push(Import { module, names: vec![("*".into(), "*".into())] });
                } else if matches!(kind, "lexical_declaration" | "variable_declaration")
                    || (kind == "export_statement"
                        && matches!(
                            unwrap(node, spec).kind(),
                            "lexical_declaration" | "variable_declaration"
                        ))
                {
                    let inner = unwrap(node, spec);
                    for decl in named_children(inner) {
                        if decl.kind() != "variable_declarator" {
                            continue;
                        }
                        let (Some(value), Some(name)) =
                            (decl.child_by_field_name("value"), decl.child_by_field_name("name"))
                        else {
                            continue;
                        };
                        if value.kind() != "call_expression" {
                            continue;
                        }
                        let (Some(func), Some(args)) =
                            (value.child_by_field_name("function"), value.child_by_field_name("arguments"))
                        else {
                            continue;
                        };
                        if text_of(func, src) != "require" || named_children(args).is_empty() {
                            continue;
                        }
                        let module = strip_quotes(&text_of(named_children(args)[0], src));
                        let names = if name.kind() == "identifier" {
                            vec![(text_of(name, src), "*".to_string())]
                        } else {
                            named_children(name)
                                .into_iter()
                                .filter(|c| c.kind() == "shorthand_property_identifier_pattern")
                                .map(|c| (text_of(c, src), text_of(c, src)))
                                .collect()
                        };
                        out.push(Import { module, names });
                    }
                }
            }
        }
        ImportStyle::Go => {
            let mut stack: Vec<Node> = named_children(root)
                .into_iter()
                .filter(|c| c.kind() == "import_declaration")
                .collect();
            while let Some(node) = stack.pop() {
                if node.kind() == "import_spec" {
                    let module = node
                        .child_by_field_name("path")
                        .map(|n| strip_quotes(&text_of(n, src)))
                        .unwrap_or_default();
                    let mut local = node
                        .child_by_field_name("name")
                        .map(|n| text_of(n, src))
                        .unwrap_or_else(|| {
                            module.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
                        });
                    if local == "_" || local == "." {
                        local = "*".into();
                    }
                    out.push(Import { module, names: vec![(local, "*".into())] });
                    continue;
                }
                stack.extend(named_children(node));
            }
        }
        ImportStyle::Rust => {
            for node in named_children(root) {
                if node.kind() != "use_declaration" {
                    continue;
                }
                let Some(arg) = node.child_by_field_name("argument") else { continue };
                let mut triples: Vec<(String, String, String)> = Vec::new();
                flatten_use(arg, src, "", &mut triples);
                let mut by_module: Vec<(String, Vec<(String, String)>)> = Vec::new();
                for (module, local, original) in triples {
                    match by_module.iter_mut().find(|(m, _)| *m == module) {
                        Some((_, names)) => names.push((local, original)),
                        None => by_module.push((module, vec![(local, original)])),
                    }
                }
                for (module, names) in by_module {
                    out.push(Import { module, names });
                }
            }
        }
        ImportStyle::Java => {
            for node in named_children(root) {
                if node.kind() != "import_declaration" {
                    continue;
                }
                let text = collapse(&text_of(node, src));
                let text = text.trim_end_matches(';').to_string();
                let wildcard = text.ends_with(".*");
                let body = text.replacen("import", "", 1).replacen("static", "", 1);
                let parts: Vec<String> = body
                    .trim()
                    .trim_end_matches(|c| c == '*' || c == '.')
                    .split('.')
                    .filter(|p| !p.is_empty())
                    .map(|p| p.to_string())
                    .collect();
                if parts.is_empty() {
                    continue;
                }
                if wildcard {
                    out.push(Import { module: parts.join("."), names: vec![("*".into(), "*".into())] });
                } else {
                    let last = parts[parts.len() - 1].clone();
                    out.push(Import {
                        module: parts[..parts.len() - 1].join("."),
                        names: vec![(last.clone(), last)],
                    });
                }
            }
        }
        ImportStyle::CSharp => {
            let mut stack = named_children(root);
            while let Some(node) = stack.pop() {
                match node.kind() {
                    "using_directive" => {
                        let text = collapse(&text_of(node, src));
                        let text = text.trim_end_matches(';').to_string();
                        let body = text
                            .replacen("using", "", 1)
                            .replacen("static", "", 1)
                            .replacen("global", "", 1)
                            .trim()
                            .to_string();
                        if let Some(eq) = body.find('=') {
                            let (alias, target) = body.split_at(eq);
                            let target = &target[1..];
                            let parts: Vec<&str> = target.trim().split('.').collect();
                            out.push(Import {
                                module: parts[..parts.len() - 1].join("."),
                                names: vec![(
                                    alias.trim().to_string(),
                                    parts[parts.len() - 1].to_string(),
                                )],
                            });
                        } else {
                            out.push(Import { module: body, names: vec![("*".into(), "*".into())] });
                        }
                    }
                    "namespace_declaration" | "file_scoped_namespace_declaration" | "declaration_list" => {
                        stack.extend(named_children(node));
                    }
                    _ => {}
                }
            }
        }
        ImportStyle::Ruby => {
            for node in named_children(root) {
                if node.kind() != "call" {
                    continue;
                }
                let func = node
                    .child_by_field_name("method")
                    .or_else(|| named_children(node).into_iter().find(|c| c.kind() == "identifier"));
                let Some(func) = func else { continue };
                let name = text_of(func, src);
                if name != "require" && name != "require_relative" && name != "load" {
                    continue;
                }
                let args = node
                    .child_by_field_name("arguments")
                    .or_else(|| named_children(node).into_iter().find(|c| c.kind() == "argument_list"));
                let Some(args) = args else { continue };
                let children = named_children(args);
                if children.is_empty() {
                    continue;
                }
                out.push(Import {
                    module: strip_quotes(&text_of(children[0], src)),
                    names: vec![("*".into(), "*".into())],
                });
            }
        }
        ImportStyle::CLike => {
            let mut stack = named_children(root);
            while let Some(node) = stack.pop() {
                match node.kind() {
                    "preproc_include" => {
                        if let Some(path) = node.child_by_field_name("path") {
                            out.push(Import {
                                module: strip_quotes(&text_of(path, src)),
                                names: vec![("*".into(), "*".into())],
                            });
                        }
                    }
                    "namespace_definition" | "declaration_list" | "linkage_specification"
                    | "preproc_ifdef" | "preproc_if" => stack.extend(named_children(node)),
                    _ => {}
                }
            }
        }
        ImportStyle::Php => {
            let mut stack = named_children(root);
            while let Some(node) = stack.pop() {
                match node.kind() {
                    "namespace_use_declaration" => {
                        for clause in named_children(node) {
                            if clause.kind() != "namespace_use_clause" {
                                continue;
                            }
                            let mut text = collapse(&text_of(clause, src));
                            for prefix in ["function ", "const "] {
                                if text.starts_with(prefix) {
                                    text = text[prefix.len()..].to_string();
                                }
                            }
                            let (full, alias) = match text.find(" as ") {
                                Some(idx) => (text[..idx].to_string(), text[idx + 4..].to_string()),
                                None => (text.clone(), String::new()),
                            };
                            let parts: Vec<String> = full
                                .trim_matches('\\')
                                .split('\\')
                                .filter(|p| !p.is_empty())
                                .map(|p| p.to_string())
                                .collect();
                            if parts.is_empty() {
                                continue;
                            }
                            let original = parts[parts.len() - 1].clone();
                            let local = if alias.trim().is_empty() {
                                original.clone()
                            } else {
                                alias.trim().to_string()
                            };
                            out.push(Import {
                                module: parts[..parts.len() - 1].join("\\"),
                                names: vec![(local, original)],
                            });
                        }
                    }
                    "namespace_definition" | "compound_statement" => stack.extend(named_children(node)),
                    _ => {}
                }
            }
        }
    }
    out
}

fn clause_names(clause: Node, src: &[u8]) -> Vec<(String, String)> {
    let mut names = Vec::new();
    for child in named_children(clause) {
        match child.kind() {
            "identifier" => names.push((text_of(child, src), "default".to_string())),
            "namespace_import" => {
                if let Some(ident) = named_children(child).into_iter().find(|c| c.kind() == "identifier") {
                    names.push((text_of(ident, src), "*".to_string()));
                }
            }
            "named_imports" => {
                for spec in named_children(child) {
                    if spec.kind() != "import_specifier" {
                        continue;
                    }
                    let original = spec
                        .child_by_field_name("name")
                        .map(|n| text_of(n, src))
                        .unwrap_or_default();
                    let local = spec
                        .child_by_field_name("alias")
                        .map(|n| text_of(n, src))
                        .unwrap_or_else(|| original.clone());
                    names.push((local, original));
                }
            }
            _ => {}
        }
    }
    names
}

fn flatten_use(node: Node, src: &[u8], prefix: &str, out: &mut Vec<(String, String, String)>) {
    let join = |prefix: &str, module: &str| -> String {
        if !prefix.is_empty() && !module.is_empty() {
            format!("{prefix}::{module}")
        } else if prefix.is_empty() {
            module.to_string()
        } else {
            prefix.to_string()
        }
    };
    match node.kind() {
        "identifier" | "type_identifier" | "self" | "super" | "crate" => {
            let name = text_of(node, src);
            out.push((prefix.to_string(), name.clone(), name));
        }
        "scoped_identifier" => {
            let module = node.child_by_field_name("path").map(|n| text_of(n, src)).unwrap_or_default();
            let full = join(prefix, &module);
            if let Some(name) = node.child_by_field_name("name") {
                let n = text_of(name, src);
                out.push((full, n.clone(), n));
            }
        }
        "use_as_clause" => {
            let mut inner = Vec::new();
            if let Some(path) = node.child_by_field_name("path") {
                flatten_use(path, src, prefix, &mut inner);
            }
            let alias = node.child_by_field_name("alias").map(|n| text_of(n, src));
            for (module, _local, original) in inner {
                out.push((module, alias.clone().unwrap_or_else(|| original.clone()), original));
            }
        }
        "scoped_use_list" | "use_list" => {
            let module = node.child_by_field_name("path").map(|n| text_of(n, src)).unwrap_or_default();
            let full = join(prefix, &module);
            let list = if node.kind() == "scoped_use_list" {
                node.child_by_field_name("list")
            } else {
                Some(node)
            };
            if let Some(list) = list {
                for child in named_children(list) {
                    flatten_use(child, src, &full, out);
                }
            }
        }
        "use_wildcard" => {
            let children = named_children(node);
            let module = children.first().map(|n| text_of(*n, src)).unwrap_or_default();
            out.push((join(prefix, &module), "*".into(), "*".into()));
        }
        _ => {}
    }
}

// ------------------------------------------------------------- edge typing

fn leftmost_name(node: Node, src: &[u8]) -> (String, String) {
    let mut member = String::new();
    let mut current = Some(node);
    for _ in 0..12 {
        let Some(node) = current else { return (String::new(), String::new()) };
        if IDENT_TYPES.contains(&node.kind()) {
            return (text_of(node, src), member);
        }
        if let Some((obj_field, mem_field)) = member_fields(node.kind()) {
            let kids = named_children(node);
            let mem = if mem_field.is_empty() { None } else { node.child_by_field_name(mem_field) }
                .or_else(|| kids.last().copied());
            if let Some(mem) = mem {
                if member.is_empty() {
                    member = text_of(mem, src);
                }
            }
            let obj = if obj_field.is_empty() { None } else { node.child_by_field_name(obj_field) }
                .or_else(|| kids.first().copied());
            current = obj;
            continue;
        }
        if UNWRAP_CHAIN.contains(&node.kind()) {
            current = named_children(node).first().copied();
            continue;
        }
        return (String::new(), String::new());
    }
    (String::new(), String::new())
}

fn callee_of<'a>(node: Node<'a>) -> Option<Node<'a>> {
    for field in ["function", "name", "constructor", "type", "macro"] {
        if let Some(child) = node.child_by_field_name(field) {
            return Some(child);
        }
    }
    named_children(node).first().copied()
}

type CallSites = Vec<(String, u32, String)>;

/// The argument list of a call, as written, without its parentheses and
/// capped so a call with a lambda in it stays a fact rather than a listing.
fn args_text(call: Node, src: &[u8]) -> String {
    let args = call
        .child_by_field_name("arguments")
        .or_else(|| named_children(call).last().copied());
    let Some(args) = args else { return String::new() };
    let raw = text_of(args, src);
    let inner = raw.trim().trim_start_matches('(').trim_end_matches(')').trim();
    let mut out: String = inner.chars().take(120).collect();
    if inner.chars().count() > 120 {
        out.push('…');
    }
    out
}

fn edges_in(
    node: Node,
    src: &[u8],
    spec: &LangSpec,
    own_name: &str,
) -> (BTreeMap<(String, String), &'static str>, CallSites) {
    let mut sites: CallSites = Vec::new();
    let mut found: BTreeMap<(String, String), &'static str> = BTreeMap::new();
    let rank = |kind: &str| EDGE_KINDS.iter().position(|k| *k == kind).unwrap_or(usize::MAX);
    let mut note = |name: String, member: String, kind: &'static str| {
        if name.is_empty() || name == own_name {
            return;
        }
        let key = (name, member);
        match found.get(&key) {
            Some(prior) if rank(prior) <= rank(kind) => {}
            _ => {
                found.insert(key, kind);
            }
        }
    };

    // the heritage child is matched by span, not node identity
    let heritage = spec
        .heritage_field
        .and_then(|field| node.child_by_field_name(field))
        .map(|n| (n.start_byte(), n.end_byte()));

    let mut stack: Vec<(Node, bool)> = vec![(node, false)];
    while let Some((current, inherits_ctx)) = stack.pop() {
        let mut ctx = inherits_ctx;
        if HERITAGE_TYPES.contains(&current.kind())
            || heritage == Some((current.start_byte(), current.end_byte()))
        {
            ctx = true;
        }
        if CALL_TYPES.contains(&current.kind()) {
            let callee = if member_fields(current.kind()).is_some() {
                Some(current)
            } else {
                callee_of(current)
            };
            if let Some(callee) = callee {
                let (root, member) = leftmost_name(callee, src);
                if !root.is_empty() {
                    if !ctx && root != own_name {
                        sites.push((root.clone(), current.start_position().row as u32 + 1, args_text(current, src)));
                    }
                    note(root, member, if ctx { "inherits" } else { "calls" });
                }
            }
        } else if member_fields(current.kind()).is_some() && !ctx {
            let (root, member) = leftmost_name(current, src);
            if !root.is_empty() {
                note(root, member, "references");
            }
        }
        if IDENT_TYPES.contains(&current.kind()) {
            let name = text_of(current, src);
            if ctx {
                note(name, String::new(), "inherits");
            } else if current.kind() == "type_identifier" || current.kind() == "scoped_type_identifier" {
                note(name, String::new(), "uses_type");
            } else {
                note(name, String::new(), "references");
            }
        }
        for child in all_children(current) {
            stack.push((child, ctx));
        }
    }
    (found, sites)
}

fn top_level<'a>(root: Node<'a>, spec: &LangSpec) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut stack: Vec<Node> = named_children(root).into_iter().rev().collect();
    while let Some(node) = stack.pop() {
        if spec.is_container(node.kind()) {
            let body = node.child_by_field_name("body").or_else(|| {
                named_children(node).into_iter().find(|c| {
                    matches!(
                        c.kind(),
                        "declaration_list" | "compound_statement" | "block" | "import_spec_list"
                    )
                })
            });
            let children = match body {
                Some(body) => named_children(body),
                None => named_children(node),
            };
            stack.extend(children.into_iter().rev());
            continue;
        }
        out.push(node);
    }
    out
}

// -------------------------------------------------------------------- entry

pub fn extension_of(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    match base.rfind('.') {
        Some(idx) if idx > 0 => base[idx..].to_lowercase(),
        _ => String::new(),
    }
}

/// Parameter names of a declaration, in order, across grammars: a bare
/// identifier, a `name` or `pattern` field, else the first identifier inside.
fn params_of(node: Node, src: &[u8]) -> Vec<String> {
    let Some(params) = node.child_by_field_name("parameters") else { return Vec::new() };
    let mut out = Vec::new();
    for child in named_children(params) {
        let kind = child.kind();
        if kind.ends_with("identifier") {
            out.push(text_of(child, src));
            continue;
        }
        let named = child
            .child_by_field_name("name")
            .or_else(|| child.child_by_field_name("pattern"))
            .or_else(|| first_identifier(child));
        if let Some(n) = named {
            let t = text_of(n, src);
            if !t.is_empty() && !matches!(kind, "comment" | "type_parameters") {
                out.push(t);
            }
        }
    }
    out
}

fn first_identifier<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind().ends_with("identifier") {
            return Some(child);
        }
        if let Some(found) = first_identifier(child) {
            return Some(found);
        }
    }
    None
}

// ------------------------------------------------------------ documentation

const COMMENT_KINDS: &[&str] = &["comment", "line_comment", "block_comment", "doc_comment"];

/// The last row a node occupies: some grammars end a line comment at
/// column 0 of the next row, which is not a row it is on.
fn end_row(node: Node) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row { end.row - 1 } else { end.row }
}

/// The first paragraph of a declaration's own documentation: the docstring
/// in Python, the comment block sitting directly above the declaration in
/// every other language. Whitespace-collapsed and capped at 400 chars.
fn doc_of(node: Node, src: &[u8], spec: &LangSpec) -> String {
    if spec.name == "python" {
        return unwrap(node, spec)
            .child_by_field_name("body")
            .and_then(|body| docstring_in(body, src))
            .unwrap_or_default();
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut expect_row = node.start_position().row;
    let mut current = node.prev_sibling();
    while let Some(sibling) = current {
        if !COMMENT_KINDS.contains(&sibling.kind()) || end_row(sibling) + 1 != expect_row {
            break;
        }
        // a comment trailing the previous statement is that statement's
        if let Some(before) = sibling.prev_sibling() {
            if end_row(before) == sibling.start_position().row {
                break;
            }
        }
        chunks.insert(0, text_of(sibling, src));
        expect_row = sibling.start_position().row;
        current = sibling.prev_sibling();
    }
    clean_doc(&strip_comment_markers(&chunks))
}

/// The module's documentation: Python's module docstring, or a file-top
/// comment block that a blank line separates from the first node (a header
/// touching the first declaration documents that declaration instead).
fn module_doc_of(root: Node, src: &[u8], spec: &LangSpec) -> String {
    if spec.name == "python" {
        return docstring_in(root, src).unwrap_or_default();
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut last_row: Option<usize> = None;
    let mut next: Option<Node> = None;
    for child in named_children(root) {
        if !COMMENT_KINDS.contains(&child.kind()) {
            next = Some(child);
            break;
        }
        if let Some(row) = last_row {
            if child.start_position().row > row + 1 {
                next = Some(child);
                break;
            }
        }
        chunks.push(text_of(child, src));
        last_row = Some(end_row(child));
    }
    let Some(row) = last_row else { return String::new() };
    if let Some(node) = next {
        if !COMMENT_KINDS.contains(&node.kind()) && node.kind() != "package_clause" && node.start_position().row == row + 1 {
            return String::new();
        }
    }
    clean_doc(&strip_comment_markers(&chunks))
}

/// A Python docstring: the first statement of a body that is a bare string.
fn docstring_in(body: Node, src: &[u8]) -> Option<String> {
    let first = named_children(body).into_iter().find(|c| c.kind() != "comment")?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let string = named_children(first).into_iter().next()?;
    if string.kind() != "string" {
        return None;
    }
    Some(clean_doc(&strip_string_quotes(&text_of(string, src))))
}

fn strip_string_quotes(raw: &str) -> String {
    let body = raw.trim_start_matches(|c| "rRbBuUfF".contains(c));
    for quote in ["\"\"\"", "'''", "\"", "'"] {
        if body.len() >= 2 * quote.len() && body.starts_with(quote) && body.ends_with(quote) {
            return body[quote.len()..body.len() - quote.len()].to_string();
        }
    }
    body.to_string()
}

fn strip_comment_markers(chunks: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for chunk in chunks {
        for line in chunk.lines() {
            let mut t = line.trim();
            for open in ["/**", "/*!", "/*"] {
                if let Some(rest) = t.strip_prefix(open) {
                    t = rest;
                    break;
                }
            }
            if let Some(rest) = t.strip_suffix("*/") {
                t = rest;
            }
            t = t.trim();
            for open in ["///", "//!", "//", "*", "#"] {
                if let Some(rest) = t.strip_prefix(open) {
                    t = rest;
                    break;
                }
            }
            lines.push(t.trim().to_string());
        }
    }
    lines.join("\n")
}

/// The documentation up to its first tag line (`@param`), whitespace
/// collapsed, capped — every paragraph, since agents put the conventions
/// after a summary line.
fn clean_doc(raw: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with('@') {
            break;
        }
        out.push(t);
    }
    take_chars(&collapse(&out.join(" ")), 600)
}

pub fn parse(path: &str, content: &str) -> Option<ParseOutput> {
    let spec = spec_for_ext(&extension_of(path))?;
    let mut parser = Parser::new();
    parser.set_language(&language_of(spec.name)).ok()?;
    let src = content.as_bytes();
    let tree = parser.parse(src, None)?;
    let root = tree.root_node();
    if root.has_error() || has_missing(root) {
        return Some(ParseOutput {
            status: "partial",
            language: spec.name,
            symbols: Vec::new(),
            imports: Vec::new(),
            doc: String::new(),
        });
    }

    let nodes = top_level(root, spec);
    let imports = imports_of(root, src, spec);
    let mut imported: HashSet<String> = HashSet::new();
    for import in &imports {
        for (local, _original) in &import.names {
            if local != "*" {
                imported.insert(local.clone());
            }
        }
    }

    let mut declarations: Vec<(String, Node, String)> = Vec::new();
    for node in nodes {
        if spec.is_import(node.kind()) {
            continue;
        }
        let inner = unwrap(node, spec);
        if spec.kind_of(node.kind()).is_none() && spec.kind_of(inner.kind()).is_none() {
            continue;
        }
        let Some(name) = name_of(node, src, spec) else { continue };
        if name.is_empty() {
            continue;
        }
        let kind = kind_override(inner, spec)
            .or_else(|| spec.kind_of(inner.kind()))
            .or_else(|| spec.kind_of(node.kind()))
            .unwrap_or("definition");
        declarations.push((name, node, kind.to_string()));
    }

    let top_names: HashSet<String> = declarations.iter().map(|(name, _, _)| name.clone()).collect();
    let mut symbols = Vec::with_capacity(declarations.len());
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (name, node, kind) in declarations {
        let bare = name.rsplit("::").next().unwrap_or(&name).rsplit('.').next().unwrap_or(&name).to_string();
        let (touched, raw_sites) = edges_in(node, src, spec, &bare);
        let mut sites: Vec<(String, u32, String)> = raw_sites
            .into_iter()
            .filter(|(target, _, _)| top_names.contains(target) || imported.contains(target))
            .collect();
        sites.sort();
        sites.dedup();
        let mut edges: Vec<(String, String, String)> = touched
            .into_iter()
            .filter(|((target, _member), _kind)| {
                top_names.contains(target) || imported.contains(target)
            })
            .map(|((target, member), kind)| (target, member, kind.to_string()))
            .collect();
        edges.sort();
        let mut refs: Vec<String> = edges.iter().map(|(target, _, _)| target.clone()).collect();
        refs.sort();
        refs.dedup();
        let symbol = Symbol {
            name: name.clone(),
            kind,
            signature: signature_of(node, src, spec),
            hash: symbol_hash(&text_of(node, src)),
            refs,
            edges,
            span: (node.start_position().row as u32 + 1, node.end_position().row as u32 + 1),
            params: params_of(unwrap(node, spec), src),
            sites,
            doc: doc_of(node, src, spec),
        };
        // a duplicate declaration overwrites the earlier one in place
        match seen.get(&name) {
            Some(&idx) => symbols[idx] = symbol,
            None => {
                seen.insert(name, symbols.len());
                symbols.push(symbol);
            }
        }
    }

    let doc = module_doc_of(root, src, spec);
    Some(ParseOutput { status: "clean", language: spec.name, symbols, imports, doc })
}

#[cfg(test)]
mod parse_ported_tests {
    use super::*;

    const CLEAN_PY: &str = "import os\n\ndef greet(name: str) -> str:\n    return f\"hi {name}\"\n\n@staticmethod\ndef decorated_fn(x):\n    return x\n\nclass Widget:\n    def method(self):\n        pass\n";
    const BROKEN_PY: &str = "def greet(name: str) -> str\n    return f\"hi {name\"\n\ndef half_typed(\n";

    fn names(out: &ParseOutput) -> Vec<&str> {
        let mut names: Vec<&str> = out.symbols.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        names
    }

    fn symbol<'a>(out: &'a ParseOutput, name: &str) -> &'a Symbol {
        out.symbols.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no symbol {name}"))
    }

    #[test]
    fn a_clean_parse_extracts_top_level_symbols_only() {
        let out = parse("a.py", CLEAN_PY).expect("python is supported");
        assert_eq!(out.status, "clean");
        assert_eq!(out.language, "python");
        assert_eq!(names(&out), vec!["Widget", "decorated_fn", "greet"]);
        assert_eq!(symbol(&out, "greet").kind, "function");
        assert_eq!(symbol(&out, "Widget").kind, "class");
        assert!(symbol(&out, "greet").signature.contains("def greet(name: str) -> str"));
    }

    #[test]
    fn a_broken_parse_is_partial_with_no_symbols() {
        let out = parse("a.py", BROKEN_PY).expect("python is supported");
        assert_eq!(out.status, "partial");
        assert!(out.symbols.is_empty());
        let ts = parse("broken.ts", "export function f( {\n").expect("typescript is supported");
        assert_eq!(ts.status, "partial");
    }

    #[test]
    fn an_unsupported_extension_is_not_parsed() {
        assert!(parse("notes.txt", "hello").is_none());
        assert_eq!(parse("x.ts", "export const a = 1\n").unwrap().language, "typescript");
    }

    #[test]
    fn a_symbol_hash_survives_a_move_and_an_unrelated_reformat() {
        let a = parse("a.py", "def f():\n    return 1\n\ndef g():\n    return 2\n").unwrap();
        let b = parse("a.py", "def g():\n    return 2\n\n\n\ndef f():\n    return 1\n").unwrap();
        assert_eq!(symbol(&a, "f").hash, symbol(&b, "f").hash);
        assert_eq!(symbol(&a, "g").hash, symbol(&b, "g").hash);
    }

    #[test]
    fn a_symbol_hash_moves_with_its_body_or_decorator() {
        let a = parse("a.py", "def f():\n    return 1\n").unwrap();
        let b = parse("a.py", "def f():\n    return 2\n").unwrap();
        assert_ne!(symbol(&a, "f").hash, symbol(&b, "f").hash);
        let c = parse("a.py", "@cached\ndef f():\n    return 1\n").unwrap();
        assert_ne!(symbol(&c, "f").hash, symbol(&a, "f").hash);
        assert_eq!(symbol(&c, "f").name, "f");
    }

    #[test]
    fn typescript_declarations_carry_their_kinds() {
        let src = "export function fetchUser(id: string): Promise<User> { return get(id) }\nexport class Store { }\ninterface User { id: string }\ntype Alias = string\nexport const helper = (x: number) => x * 2\n";
        let out = parse("a.ts", src).unwrap();
        assert_eq!(out.status, "clean");
        assert_eq!(names(&out), vec!["Alias", "Store", "User", "fetchUser", "helper"]);
        assert_eq!(symbol(&out, "fetchUser").kind, "function");
        assert_eq!(symbol(&out, "User").kind, "interface");
        assert_eq!(symbol(&out, "helper").kind, "variable");
    }

    #[test]
    fn refs_name_same_file_and_imported_callees() {
        let caller = "from billing import compute_tax\n\ndef helper(x):\n    return x * 2\n\ndef invoice_total(items):\n    return helper(sum(items)) + compute_tax(items)\n";
        let out = parse("app.py", caller).unwrap();
        let refs: Vec<&str> = symbol(&out, "invoice_total").refs.iter().map(String::as_str).collect();
        assert_eq!(refs, vec!["compute_tax", "helper"]);
        assert!(symbol(&out, "helper").refs.is_empty());
        let ts = parse("app.ts", "import { fee } from './fees'\nexport function total(x: number) { return fee(x) + 1 }\n").unwrap();
        assert!(symbol(&ts, "total").refs.iter().any(|r| r == "fee"));
    }

    #[test]
    fn every_edge_is_typed_by_how_the_name_is_used() {
        let app = "from .billing import compute_tax, Ledger\n\n\ndef invoice_total(items):\n    return sum(items) + compute_tax(items)\n\n\nclass AuditLedger(Ledger):\n    def post(self, amount):\n        return invoice_total([amount])\n";
        let out = parse("app.py", app).unwrap();
        let has = |name: &str, edge: (&str, &str, &str)| {
            symbol(&out, name).edges.iter().any(|(t, m, k)| (t.as_str(), m.as_str(), k.as_str()) == edge)
        };
        assert!(has("invoice_total", ("compute_tax", "", "calls")));
        assert!(has("AuditLedger", ("Ledger", "", "inherits")));
        assert!(has("AuditLedger", ("invoice_total", "", "calls")));
        // refs stay the flat name list
        assert_eq!(symbol(&out, "invoice_total").refs, vec!["compute_tax".to_string()]);
        // a member access through a namespace keeps the member name
        let ns = parse("m.py", "import os.path as osp\n\ndef f():\n    return osp.join('a')\n").unwrap();
        assert!(symbol(&ns, "f").edges.iter().any(|(t, m, k)| t == "osp" && m == "join" && k == "calls"));
        assert_eq!(ns.imports[0].module, "os.path");
        assert_eq!(ns.imports[0].names, vec![("osp".to_string(), "*".to_string())]);
    }

    #[test]
    fn each_language_names_its_declarations_kind() {
        let cases: &[(&str, &str, &str, &str)] = &[
            ("a.go", "package a\nimport \"fmt\"\nfunc F() { fmt.Println() }\n", "F", "function"),
            ("a.rs", "use crate::b::g;\npub fn f() { g(); }\n", "f", "function"),
            ("A.java", "package p;\nimport com.x.Y;\nclass A { int m() { return Y.g(); } }\n", "A", "class"),
            ("a.js", "import { g } from \"./m\";\nexport function f() { return g(); }\n", "f", "function"),
            ("A.cs", "using X;\nclass A { public int M() { return 1; } }\n", "A", "class"),
            ("a.rb", "require \"json\"\ndef top(x)\n  x\nend\n", "top", "function"),
            ("a.c", "#include \"u.h\"\nint f(int a) { return a; }\n", "f", "function"),
            ("a.cpp", "struct S {};\nint g() { return 1; }\n", "g", "function"),
            ("a.php", "<?php\nfunction f(int $a) { return $a; }\n", "f", "function"),
        ];
        for (path, src, name, kind) in cases {
            let out = parse(path, src).unwrap_or_else(|| panic!("{path} is supported"));
            assert_eq!(out.status, "clean", "{path}");
            assert_eq!(symbol(&out, name).kind, *kind, "{path}");
        }
    }

    #[test]
    fn every_supported_language_parses_a_typical_file_cleanly() {
        let samples: &[(&str, &str, &str)] = &[
            ("a.go", "package a\ntype S struct{ X int }\nfunc F(a int) int { return a }\n", "F"),
            ("a.rs", "pub struct S { x: i32 }\npub fn f(x: i32) -> i32 { x }\n", "f"),
            ("A.java", "package p;\npublic class A { public int m(int a) { return a; } }\n", "A"),
            ("A.cs", "namespace N {\n  public class A { public int M(int a) { return a; } }\n}\n", "A"),
            ("a.rb", "class A\n  def m(a)\n    a\n  end\nend\ndef top(x)\n  x\nend\n", "top"),
            ("a.c", "static int f(int a) { return a; }\nint main(void) { return f(1); }\n", "main"),
            ("a.cpp", "class A { public: int m(int a); };\ntemplate <typename T> T id(T t) { return t; }\n", "A"),
            ("a.php", "<?php\nfunction f(int $a) { return $a; }\nclass A {}\n", "f"),
            ("a.js", "export function f(a) { return a; }\nclass K { m() {} }\n", "f"),
            ("a.tsx", "export function Widget() { return 1; }\n", "Widget"),
        ];
        for (path, src, expected) in samples {
            let out = parse(path, src).unwrap_or_else(|| panic!("{path} is supported"));
            assert_eq!(out.status, "clean", "{path}");
            assert!(out.symbols.iter().any(|s| s.name == *expected), "{path}: {:?}", names(&out));
        }
    }
}
