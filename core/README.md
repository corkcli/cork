# cork-core

Cork's code parser and graph, in Rust. The server (`cork-rs`) and the
hooks (`cork-hooks`) both link it, so a file is read the same way on the
agent's machine and on the server.

## What is in it

**The parse engine** (`engine.rs`, `langs.rs`). `report_edit` fires on every
Edit and Write an agent makes, and parsing is the only CPU-bound work on that
path. One tree-sitter walk per language table entry yields the declarations
with their signatures, content hashes, spans, parameters, call sites and
docstrings, the typed edges between them (`inherits` / `calls` / `uses_type`
/ `references`), the imports, and the module's own documentation. Twelve
grammars: Python, TypeScript, JavaScript, Go, Rust, Java, C#, Ruby, C, C++
and PHP.

**Graph resolution** (`graph.rs`). Turns one file's name-level edges into
cross-file edges by resolving its imports against the files the repo is known
to have, per language.

**Louvain community detection** (`community.rs`). The most expensive thing
Cork computes: `repo_map` clusters the symbol graph into subsystems, and
`repo_map` rides in every briefing. The partition is cached by graph
fingerprint, but the fingerprint moves on every reported edit, so the cache
is coldest exactly when the most agents are working. Node order is shuffled
with a seeded PRNG, so a given seed always yields the same partition.

**The local view** (`local.rs`). What the hook sends instead of the file:
the structure above plus a hash per line and where names occur. Source never
leaves the machine.

**Typed edits** (`edit.rs`). `rename`, `add_param` and `remove_param` applied
to source through the syntax tree, on the agent's machine. Python and the
TypeScript family; other grammars are refused, not guessed.

## Build

```sh
cd server/cork-core
cargo build --release
cargo test --release
```

## Adding a language

One `LangSpec` entry in `src/langs.rs` and its grammar in `Cargo.toml`. The
engine itself never changes.
