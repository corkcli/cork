# Working on Cork's open-source code

This repository is Cork's free version: the hooks that run in agent
sessions, the code parser, the real-time protocol, the local engine and the
Claude Code plugin. It is exported from Cork's main repository with each
release, as a plain tree. Cork's cloud (accounts, billing, teams across
machines, embeddings) is not here.

## Layout

| Folder | What it is | License |
|---|---|---|
| `hooks/` | `cork-hooks`: what runs in agent sessions (briefings, the write gate, landing, messages, the MCP bridge, install) | MIT |
| `core/` | `cork-core`: the code parser and graph the hooks and the engine share | MIT |
| `protocol/` | The real-time protocol: spec, schema and golden vectors, the sans-IO engine, the code, notes and tasks domains, a reference server, a TypeScript client, the conformance suite | MIT |
| `free/` | `cork-free`: the local engine. It builds the one program, `cork`, with the hooks linked in, and runs as `cork local` on 127.0.0.1 | FSL-1.1-MIT |
| `plugin/` | The Claude Code and Codex plugin | MIT |
| `skills/` | Official Cork skills for agents that use Cork | MIT |

A new file takes the license of the folder it is in.

## Build and test

What CI runs (`.github/workflows/build.yml`, on Linux, macOS and Windows):

```sh
cd hooks && cargo test --release
cd free && cargo build --release && cargo test --release
```

Also:

- `cd core && cargo test` for the parser.
- `cd protocol && cargo test` for the protocol workspace, and
  `cd protocol/clients/ts && npm install && npm test` for its client.
- `cd hooks && cargo test --lib` runs only the hooks' unit tests, in seconds.

The end-to-end tests in `hooks/tests/` need git. They build `../free` and
the protocol's reference server in release mode, so the first run takes
minutes. `shared_checkout.rs` also needs node and npm, and skips itself
without them.

## Rules for changes

- **The cloud is not in this tree.** Each cloud module the engine calls has a
  stand-in under `free/src/free/`, with the same names and free answers
  (for example, `effective_access` is always "write": on one machine there
  are no viewers). The `cloud` feature is empty here. Never make the free
  build depend on it.
- **Tests run without the cloud.** A test, or the part of one, that needs a
  cloud module or asserts cloud-only behavior goes behind
  `#[cfg(feature = "cloud")]`. Keep the rest of the test running in the free
  build.
- **Hooks fail open.** If the engine is down or slow, the agent's tool call
  still goes ahead. `hooks/tests/protocol.rs` checks this.
- **Source code stays on the machine.** The hooks send a file's structure
  (names, signatures, docstrings, who calls what, a hash per line), never its
  text. The free version makes no calls out except to check for a signed
  update of itself. corkcli.com/data lists every request; keep it true.
- **Match the code around you**: its naming, its idioms and its comment
  density. Comments say why, in plain sentences.
- **`plugin/hooks.lock`** pins the released `cork` binaries by SHA-256. It
  changes with a release, not by hand.

## When Cork itself is running here

If you run Cork while you work on this repo (`npx corkcli`), its hooks brief
your agents and coordinate them like in any other repo. The skills in
`skills/` explain what arrives and how to act on it.

## Questions and changes

Open an issue or a pull request at
[github.com/lithometric/cork](https://github.com/lithometric/cork). More
about Cork: [corkcli.com](https://corkcli.com).
