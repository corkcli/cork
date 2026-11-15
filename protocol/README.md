# Cork protocol

**Coordination for agents that work on the same things at the same time.**

When several AI agents (or agents and people) share a codebase, a doc set or
a task board, they overwrite each other, double-book work and learn about it
too late. The Cork protocol fixes that at the wire. Each change is a small
typed op on one versioned key. The server decides in one round trip, from a
table, whether two changes compose, and it never reads the content.

- **No locks, no polling, no heartbeats.** One WebSocket per session. Leases
  last while the socket is open.
- **Collisions are settled at once**: `commute` (both land), `rekey` (their
  rename moved the thing; yours follows it) or `conflict` (you're rejected
  with their ops and their stated intent, so you can merge).
- **Content-blind.** The server sees key names, versions, hashes and op
  kinds. Payloads are opaque bytes you may encrypt end to end.
- **Domain-agnostic.** What a key *is* is a plug-in: a JSON table plus a
  short trait impl. Three ship here: code, notes and tasks.
- **Verifiable.** A sans-IO, deterministic engine; golden vectors any
  implementation must replay; a hash-chained op log.

It is the protocol behind [Cork](https://corkcli.com), released under
the MIT license so anyone can build on it.

## Two agents, one task

```
alice → {"t":"op","op_id":"a.1","key":"task:eng-42","base":{"version":0},"kind":"claim"}
alice ← {"t":"ack","op_id":"a.1","version":1}

bob   → {"t":"op","op_id":"b.1","key":"task:eng-42","base":{"version":0},"kind":"claim"}
bob   ← {"t":"reject","reason":"conflict","theirs":[{"op_id":"a.1","kind":"claim"}],"holder":{"person":"alice@example.com"}}

bob   → {"t":"op","op_id":"b.2","key":"task:eng-42","base":{"version":0},"kind":"comment","payload":"…"}
bob   ← {"t":"ack","op_id":"b.2","version":2,"rebased_over":[{"op_id":"a.1","kind":"claim"}]}
```

Bob learns that Alice holds the task in the same round trip as his claim.
His comment, sent from the same stale version, still lands, because
`comment × claim = commute` in the tasks table. The same rules work for
code, where `modify_body × modify_body = conflict` and
`modify_body × rename = rekey`. See `schema/vectors/` for complete,
replayable scenarios.

## Quick start

```sh
cargo test                                                   # engine, domains, vectors, schema, sockets
cargo run -p cork-protocol-server -- --addr 127.0.0.1:7878
websocat ws://127.0.0.1:7878                                 # paste frames by hand
```

TypeScript (browser or Node 22):

```sh
cd clients/ts && npm install && npm test
```

```ts
import { Connection } from "@corkcli/protocol";
const c = await Connection.connect("ws://127.0.0.1:7878",
  { session: "s1", agent_id: "claude", person: "me@example.com", scope: "acme/board" });
const res = await c.write(c.client.op("task:eng-42", "claim", {}));
```

The reference server keeps everything in memory and has no auth. For
production, embed the engine behind your own transport, authorizer and
durable log. See [GUIDE.md](GUIDE.md).

## What's here

```
SPEC.md                  the normative spec (frames, resolution, leases, log, security)
GUIDE.md                 the engine: embedding, the Rust and TS clients, adding a domain
schema/                  JSON Schema for every frame; golden vectors any implementation replays
core/                    cork-protocol: the sans-IO Engine, Client, traits, HLC, log
domains/code/            code: and file: keys; symbols, renames, moves; the graph is calls
domains/notes/           note: keys; appends commute, coalescing on; the graph is links
domains/tasks/           task: keys; claims, statuses, blockers, merges; the graph is blockers
server/                  the reference WebSocket server
clients/ts/              @corkcli/protocol, the TypeScript client
conformance/             replays the vectors, checks the schema, end-to-end socket tests
```

## Bring your own domain

The protocol carries anything you can name with a key and describe with a
handful of change kinds: docs and specs (`doc:<id>#<section>`), database
schemas (`table:<name>`), infrastructure (`tf:<resource>`), design files,
shared agent memory (`fact:<topic>`).

1. Write `table.json`. List your key prefixes and kinds, then fill in the
   complete `relations[mine][theirs]` matrix with `commute`, `rekey` or
   `conflict`. A table with a gap or a contradiction is rejected at load.
2. Implement `Domain`. Only `table()` is required. Override `normalize_key`,
   `rekey_target` (for kinds that move a key), `graph()` and `observe` as you
   need them.
3. Register it with `Engine::builder().domain(...)`. Clients discover it
   from `welcome.domains`.
4. Add a vector to `schema/vectors/` and the domain to the cell-by-cell test
   in `conformance/tests/engine.rs`. That test drives every cell of your
   table through the engine.

`domains/tasks` is a complete example in about 100 lines plus tests.

## Implementing the protocol in another language

The engine is deterministic: the same frames at the same times always produce
the same output. An implementation conforms when it replays every file in
`schema/vectors/` exactly and every frame validates against
`schema/frames.schema.json`. SPEC.md §21 describes the vector format.

## Status

Protocol version 1, draft. Frames may gain optional fields; anything that
changes meaning becomes version 2 (SPEC.md §20). Issues and domain proposals
are welcome.

## License

MIT
