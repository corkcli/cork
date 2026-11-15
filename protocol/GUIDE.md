# Cork protocol

A real-time protocol for agents that edit the same things at once. Every
agent session holds one WebSocket. Every change is a small **typed op** on
one **versioned key**. When two agents touch the same function, the server
settles it within one round trip using a table of which kinds of change
compose. It never reads the code.

The protocol does not depend on what the keys are. Code is one domain (keys
are symbols, the graph is calls). Notes are another (keys are notes, the
graph is links). Tasks are a third (keys are tasks, the graph is
blockers). You can add your own with a JSON table and a short trait impl.

```
server/protocol/
  SPEC.md                  the normative spec
  schema/frames.schema.json  every frame, as JSON Schema
  schema/vectors/*.json    golden scenarios any implementation must replay
  core/                    cork-protocol: the sans-IO Engine, Client, traits, HLC, log
  domains/code/            cork-protocol-code: code:/file: keys, the code table
  domains/notes/           cork-protocol-notes: note: keys, appends commute, coalescing on
  domains/tasks/           cork-protocol-tasks: task: keys, a work queue where two claims conflict
  server/                  cork-protocol-server: the reference server
  clients/ts/              @corkcli/protocol: TypeScript client (browser + node 22)
  conformance/             replays the vectors, checks the schema, end-to-end socket tests
```

## The ten rules

1. **One socket per session.** Every message is one JSON frame on that
   socket, `{"t": "...", ...}`: ops, gate checks, leases, deltas and
   messages. `hello` carries the protocol version.
2. **The unit is a typed op**: `{op_id, key, base: {version, hash}, kind,
   public, payload?, author, intent_id?, hlc}`. `public` holds the few fields
   the kind declares (`new_name`, `new_signature`). `payload` is opaque bytes
   the server never reads.
3. **Each key is sequenced on its own.** If `base.version` is the current
   version, the op is accepted and the version goes up by one. Ops on
   different keys never wait for each other, and there is no global lock.
4. **Stale writes are rebased without reading code.** For each op since the
   writer's base, the server looks up (mine, theirs) in the domain's table:
   - `commute`: accept, and tell the writer what it was rebased over.
   - `rekey`: their rename moved the key, so the op lands on the new key.
   - `conflict`: reject at once with their ops and their intent.
5. **Leases are tied to the connection. There is no heartbeat.** A lease
   lasts while the socket is open. After a disconnect it is kept for a grace
   period (5 s by default) in case the session resumes, and then it is
   released on its own. Writes against a lease are judged by the same table.
6. **Changes are pushed instantly.** Subscriptions name keys and a graph
   depth. When an op is accepted, the delta goes out at once to everyone
   subscribed to that key, or to any key that depends on it within their
   depth.
7. **Coalescing is the client's choice.** It is off by default. Turn it on
   for fast streams such as notes: same-key ops in a 50–200 ms window
   collapse into one.
8. **The ack comes after the log append.** The op log is hash-chained like a
   ledger and sits behind a `Log` trait, which also has a snapshot hook.
9. **Drafts, then promotion.** Accepted ops are drafts in their author's
   layer, and everyone can see them. After an agent's tested push, `promote`
   marks them as proven. Commits carry `Collide-Ops: <ids>`.
10. **The server never needs code.** It sees key names, versions, hashes,
    kinds and public fields. Encrypting the payload is up to the client.

Atomic `batch` frames are all-or-nothing: every op is checked first, then
all are applied. They cover refactors that span many keys. Files without
symbols (configs, lockfiles, binaries) use `file:<path>` keys under the same
rules.

## Two agents, one function, a few milliseconds

Alice (Claude) and Bob (Codex) both start from version 0 of `parse_config`
and both change its body. The frames below are taken from
`schema/vectors/01-two-agents-one-function.json`, trimmed for reading.

```
t=0   alice → {"t":"hello","proto":1,"session":"alice-1","agent_id":"claude-a","person":"alice@example.com","scope":"acme/api"}
      alice ← {"t":"welcome","session":"alice-1","resumed":false,"grace_ms":5000,"domains":[{"name":"code",…},{"name":"notes",…}]}
      bob   → hello …   bob ← welcome …
      both  → {"t":"subscribe","keys":["code:src/cli.py::parse_config"],"depth":0}

t=10  alice → {"t":"op","op_id":"alice-1.1","key":"code:src/cli.py::parse_config",
               "base":{"version":0,"hash":""},"kind":"modify_body","payload":"ZGlmZi1h",
               "intent_id":"tighten-validation"}
      alice ← {"t":"ack","op_id":"alice-1.1","version":1,"hash":"5b41…","seq":1}
      bob   ← {"t":"delta","op":{"op_id":"alice-1.1","kind":"modify_body","author":{"person":"alice@example.com",…}},
               "version":1,"layer":"draft"}

t=12  bob   → {"t":"op","op_id":"bob-1.1","key":"code:src/cli.py::parse_config",
               "base":{"version":0,"hash":""},"kind":"modify_body","payload":"ZGlmZi1i",
               "intent_id":"retry-on-timeout"}
      bob   ← {"t":"reject","op_id":"bob-1.1","reason":"conflict",
               "current":{"version":1,"hash":"5b41…"},
               "theirs":[{"op_id":"alice-1.1","kind":"modify_body","payload":"ZGlmZi1h","author":{"agent_id":"claude-a",…}}],
               "their_intent":{"id":"tighten-validation"},"holder":{"session":"alice-1",…}}

      (Bob merges Alice's payload locally. The server never saw either body.)

t=19  bob   → {"t":"op","op_id":"bob-1.2", …, "base":{"version":1,"hash":"5b41…"},"payload":"bWVyZ2Vk"}
      bob   ← {"t":"ack","op_id":"bob-1.2","version":2,"seq":2}
      alice ← {"t":"delta","op":{"op_id":"bob-1.2",…},"version":2}

t=20  bob   → {"t":"message","anchor":"code:src/cli.py::parse_config","text":"merged your validation into my retry change; please re-run the CLI tests"}
      alice ← {"t":"message","from":{"session":"bob-1",…},"anchor":"code:src/cli.py::parse_config","text":"…"}
```

Bob learned about the collision, along with Alice's change and her reason,
in the same round trip as his write. He did not poll, take a lock, or read
any files. Over localhost the conformance test does write, conflict and
merge-write in about 0.4 ms. The release-build engine takes about 12 µs per
op on distinct keys.

If Alice had **renamed** the function instead, Bob's body edit would not
have conflicted. `modify_body × rename = rekey`, so it lands on
`code:src/cli.py::load_config`, and the ack says `"rekeyed_to"`
(`schema/vectors/02-rename-rekeys-a-stale-edit.json`).

## Run it

```sh
cd server/protocol
cargo test                                   # unit, engine, vectors, schema, sockets
cargo run -p cork-protocol-server -- --addr 127.0.0.1:7878 --grace-ms 5000
```

The reference server serves the code and notes domains from memory, with no
auth, and accepts any path. You can talk to it from any WebSocket client, for
example `websocat ws://127.0.0.1:7878`, and paste frames.

From Rust (a blocking client; `features = ["ws-client-blocking"]`):

```rust
use cork_protocol::{ws_client::BlockingClient, ClientConfig};
let (mut c, _welcome) = BlockingClient::connect("ws://127.0.0.1:7878",
    ClientConfig { session: "s1".into(), agent_id: "claude".into(), person: "me@x".into(),
                   scope: "acme/api".into(), token: None }, false, std::time::Duration::from_secs(5))?;
let gate = c.check(&["code:src/cli.py::parse_config"], Some("modify_body"), timeout)?;   // would it land?
let op = c.client.op("code:src/cli.py::parse_config", "modify_body", Default::default(), Some(b"..."), c.now_ms());
match c.write(op, timeout)? { Ok(ack) => …, Err(reject) => … }
```

From TypeScript (`clients/ts`, built with `tsc`; run `npm test` there):

```ts
import { Connection } from "@corkcli/protocol";
const c = await Connection.connect("ws://127.0.0.1:7878", { session: "s1", agent_id: "claude", person: "me@x", scope: "acme/api" });
c.client.stream("note:", 100, true);             // coalesce notes, follow their deltas
const res = await c.write(c.client.op("note:standup", "append", {}, new TextEncoder().encode("- shipped\n")));
```

## Embedding the engine

The engine is **sans-IO**. You give it frames, the connection they came
from, and the time, and it returns the frames to send:

```rust
let mut engine = Engine::builder()
    .domain(CodeDomain::with_graph(my_call_graph))   // Arc<dyn Graph>
    .domain(NotesDomain::new())
    .log(my_durable_log)                             // impl Log: append → then ack
    .authorize(|hello| verify(hello.token.as_deref()))   // -> Result<Peer, String>
    .grace_ms(5_000)
    .build();

engine.connected(conn, now);
for out in engine.handle_text(conn, &text, now) { send(out.conn, out.frame.encode()); /* close if fatal error */ }
engine.disconnected(conn, now);
// on a timer: if engine.next_deadline() <= now { engine.tick(now) }
```

For the same inputs it always returns the same frames. That determinism is
what lets the conformance vectors check it exactly. `core/src/ws_server.rs`
is a complete transport in about 120 lines.

## Adding a domain

1. Write `table.json`. List the key `prefixes` and the `kinds`, giving each
   kind its required `public` fields, whether it `moves_key`, and the
   prefixes it applies to. Then fill in the full `relations[mine][theirs]`
   matrix. `Table::from_json` rejects tables with gaps or contradictions.
2. Implement `Domain`. `table()` is required. Override `normalize_key`,
   `rekey_target` (for kinds that move keys), `graph()` (a `Graph`:
   `dependents(scope, key)`) and `observe` (to derive the graph from ops)
   as you need them.
3. Register it with `Engine::builder().domain(...)`. Every client learns
   about it from `welcome.domains`.
4. Add a vector to `schema/vectors/`, and a cell-by-cell test like
   `every_table_cell_behaves_as_declared` in `conformance/tests/engine.rs`.

`domains/notes` is a complete example: about 100 lines plus tests.

## What the server knows

The server stores key names, versions, hashes, kinds, public fields and
opaque payloads. It reads no code and runs no merge. It uses no global
lock. Leases need no heartbeat, and clients do not poll. The log is
tamper-evident. See SPEC.md §18.
