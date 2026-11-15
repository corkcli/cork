# @corkcli/protocol (TypeScript)

The TypeScript client for Cork's real-time agent protocol (`../../SPEC.md`).
No dependencies, no build step beyond `tsc`. It runs in browsers and in node 22+,
which both have a global `WebSocket`.

- `frames.ts`: every frame, field for field with `core/src/frames.rs`, plus
  `decodeServerFrame` / `decodeClientFrame` / `encodeClientFrame`.
- `client.ts`: the sans-IO `Client`. It tracks each key's **base** (what your
  content is built on: your own acks, plus `adopt`) apart from its **head** (the
  newest version you have seen). It builds ops and frames, and runs per-prefix streams.
- `coalescer.ts`: buffers a fast stream and collapses same-key, same-kind ops.
- `ws.ts`: `Connection`. It sends hello, returns promises for acks and replies,
  raises events for deltas and messages, and reconnects with `resume: true`.
- `trailer.ts`: the `Cork-Ops:` commit trailer (`Collide-Ops:` still read).

## Use

```ts
import { Connection, utf8 } from "@corkcli/protocol";

const conn = await Connection.connect("ws://localhost:7400", {
  session: "s-42", agent_id: "claude-1", person: "kevin@example.com", scope: "github.com/acme/app",
}, { onDelta: (d) => console.log("changed:", d.op.key, "via", d.via) });

conn.subscribe(["code:src/cli.py::main"], 1);           // main and what it calls
const op = conn.client.op("code:src/cli.py::parse_config", "modify_body", {}, utf8("<encrypted diff>"));
const res = await conn.write(op);
if (res.t === "reject") console.log(res.reason, res.theirs, res.their_intent);   // merge locally, retry
```

A notes stream with coalescing on (100 ms window; appends collapse into one op)
and auto-adopt (this client applies every remote op as it arrives):

```ts
import { Coalescer, mergeAppends } from "@corkcli/protocol";
conn.client.streamWith("note:", new Coalescer(100, mergeAppends), true);
```

## Test

```sh
tsc -p . && node --test "dist/test/*.test.js"
```

The vectors test replays `../../schema/vectors/*.json`. Every `send` frame must
round-trip, and every expected `recv` frame may name only fields its frame has.
