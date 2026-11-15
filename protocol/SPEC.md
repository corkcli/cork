# Cork protocol, version 1

Status: draft, implemented by `core/` (Rust, the reference) and `clients/ts/`.
The key words MUST, SHOULD and MAY are used as in RFC 2119.

1. [Model](#1-model)
2. [Transport and framing](#2-transport-and-framing)
3. [Frames](#3-frames)
4. [Ops and their lifecycle](#4-ops-and-their-lifecycle)
5. [Resolving an op: accept, rebase, rekey, reject](#5-resolving-an-op-accept-rebase-rekey-reject)
6. [Batches](#6-batches)
7. [Leases and grace](#7-leases-and-grace)
8. [Subscriptions and deltas](#8-subscriptions-and-deltas)
9. [The gate: `check`](#9-the-gate-check)
10. [Drafts and promotion](#10-drafts-and-promotion)
11. [Messages](#11-messages)
12. [The log](#12-the-log)
13. [Clocks](#13-clocks)
14. [Coalescing](#14-coalescing)
15. [Domains](#15-domains)
16. [The code domain](#16-the-code-domain)
17. [The notes domain](#17-the-notes-domain)
18. [Security model](#18-security-model)
19. [Errors](#19-errors)
20. [Versioning](#20-versioning)
21. [Conformance](#21-conformance)
22. [Future: `git-remote-cork`](#22-future-git-remote-cork)
23. [Integration: Cork's server](#23-integration-corks-server)

---

## 1. Model

A **scope** (a repo, a workspace's notes) holds **keys**. Every key has its
own **version** (`u64`, 0 = never written) and **hash** (a chain over the ops
applied to it, §4.3). Keys never wait on each other: there is no global lock
and no global version.

A **session** is one agent's identity on one connection. Sessions write
**ops**: typed changes to one key (`modify_body`, `rename`, `append`...). The
key's **domain** declares the kinds and a **compatibility table** that says,
for every pair of kinds, whether a later op written on a stale base
**commutes** with an earlier one, **rekeys** (the earlier one moved the key,
the later one follows), or **conflicts**.

The server never needs content. It sees key names, versions, hashes, kinds,
kind-declared **public fields** (`new_name`, `new_signature`, `new_path`,
`target`) and an opaque **payload** it stores and forwards. Merging content is
the client's job; the server's job is to say, in one round trip, whether the
client must merge, and with what.

## 2. Transport and framing

- One WebSocket per agent session carries everything: ops, gate checks,
  leases, subscriptions, deltas and messages.
- Each WebSocket message is one UTF-8 JSON **text** frame: an object with a
  string field `t` naming the frame type. Binary messages MUST be ignored.
- Receivers MUST ignore fields they do not know (forward compatibility).
  Senders SHOULD omit optional fields that are empty.
- The first client frame on a connection MUST be `hello`. Anything else
  before it gets `error{code: "hello_required"}`.
- Frames on one connection are processed in order. There is no ordering
  across connections.
- A server sends `error{fatal: true}` and then closes the connection.

The JSON Schema for every frame is `schema/frames.schema.json`.

### Common types

| Type | Shape | Notes |
|---|---|---|
| `Hlc` | `{w: u64, c: u32}` | wall-clock ms + logical counter, ordered by `(w, c)` (§13) |
| `Base` | `{version: u64, hash: string, content?: string}` | what a write builds on; `hash: ""` skips the hash check; `content` names the base by content hash instead (§5.1) |
| `Head` | `{version, hash, promoted_version}` | a key's current state |
| `Peer` | `{session, agent_id, person}` | who; always filled by the server |
| `Intent` | `{id?, text?}` | why: an intent id and/or a sentence |
| `MovedTo` | `{key, version, hash}` | where a rename/move sent a key's subject, and its head there |
| `TheirOp` | `{op_id, key, version, kind, public?, payload?, author, intent_id?, hlc, layer}` | someone else's accepted op, shown to a writer it affects |
| `Layer` | `"draft" \| "promoted"` | §10 |
| op id | `[A-Za-z0-9._:@-]{1,128}` | client-chosen, globally unique; the TS/Rust clients use `<session>.<n>` |

## 3. Frames

`req` (optional string) on a client frame is echoed on its answer, so a
client can match answers to requests. An `error` for an op carries the op's
`op_id` in `req`.

### Client → server

| `t` | Fields | Answer |
|---|---|---|
| `hello` | `proto: 1`, `session`, `agent_id`, `person`, `scope`, `token?`, `resume?` | `welcome` |
| `op` | the op's fields inline (§4.1) | `ack` or `reject` |
| `batch` | `batch_id`, `atomic` (default `true`), `ops: Op[]` | one `ack` or `reject` per op |
| `lease` | `req?`, `keys`, `kinds` (empty = every kind of the domain), `intent?` | `lease_granted` or `lease_denied` |
| `release` | `req?`, `keys` | `lease_released{reason: "released"}` |
| `subscribe` | `req?`, `keys` (exact, or a prefix ending in `*`), `depth` (default 0) | none (errors only) |
| `unsubscribe` | `req?`, `keys` | none |
| `check` | `req?`, `keys: [{key, base?, kind?}]` | `check_result` |
| `promote` | `req?`, `op_ids`, `evidence` (any JSON) | `promoted` |
| `message` | `req?`, `to?`, `anchor?`, `text` | none, or `error{code: "undelivered"}` |

### Server → client

| `t` | Fields |
|---|---|
| `welcome` | `proto`, `session`, `resumed`, `hlc`, `grace_ms`, `resumed_leases: [{key, kinds, intent?, head}]`, `resumed_subscriptions: string[]`, `domains: [{name, prefixes, kinds, coalesce_ms}]` |
| `ack` | `op_id`, `key` (where it landed), `version`, `hash`, `hlc`, `seq`, `batch_id?`, `rebased_over?: TheirOp[]`, `rekeyed_to?`, `moved_to?: MovedTo`, `same_as?` (§5.2) |
| `reject` | `op_id`, `key`, `reason`, `current: Head`, `theirs?: TheirOp[]`, `their_intent?: Intent`, `holder?: Peer`, `batch_id?`, `message?` |
| `delta` | `op: Op` (key = where it landed, author filled, hlc = server stamp), `version`, `hash`, `seq`, `layer`, `via?: string[]`, `moved_to?: MovedTo` |
| `lease_granted` | `req?`, `keys`, `kinds`, `intent?`, `heads: Head[]` (in `keys` order) |
| `lease_denied` | `req?`, `keys`, `holder: Peer` (first), `intent?`, `conflicts: [{key, holder, kinds, intent?}]` |
| `lease_released` | `req?`, `keys`, `reason: released \| expired \| superseded`, `holder: Peer` |
| `check_result` | `req?`, `results: [{key, status: ok \| stale \| leased \| unknown_base, current: Head, would?: accept \| rebase \| rekey \| conflict, rekeyed_to?, theirs?, holder?, intent?}]` |
| `promoted` | `req?`, `op_ids` (promoted), `keys: [{key, promoted_version}]`, `unknown?: string[]`, `evidence` |
| `message` | `from: Peer`, `to?`, `anchor?`, `text`, `hlc` |
| `error` | `code`, `message`, `fatal?`, `req?` |

`reject.reason` is one of:

| reason | meaning | the client should |
|---|---|---|
| `conflict` | an op since the base conflicts per the table | merge `theirs` locally, adopt `current`, write again |
| `leased` | another session holds a conflicting lease | wait for `lease_released`, or message the `holder` |
| `invalid` | malformed: unknown key/kind, missing public field, base ahead of the key, base hash mismatch, oversize payload | fix the op; do not retry as is |
| `stale_lease` | the op said `leased: true` but the session holds no lease on the key | re-lease, re-check |
| `unknown_base` | the base was named by content (§5.1) and no version the server keeps has it | look at `theirs` (others' drafts), bring the copy up to date or write from a `{version, hash}` base |
| `batch_aborted` | another op of the same atomic batch was rejected | fix that op, resend the batch |

`welcome` adds `grace_ms`, `resumed_subscriptions` and `domains` to the
fields named in the design brief; `ack` adds `hash` and `moved_to`; `reject`
adds `holder` and the `batch_aborted` reason; `lease_denied` adds
`conflicts`. Each is needed for a client to continue without another round
trip.

## 4. Ops and their lifecycle

### 4.1 The op

```json
{
  "t": "op",
  "op_id": "alice-1.17",
  "key": "code:src/cli.py::parse_config",
  "base": {"version": 4, "hash": "9c1e…"},
  "kind": "rename",
  "public": {"new_name": "load_config"},
  "payload": "<base64, opaque>",
  "intent_id": "i-42",
  "hlc": {"w": 1760000000123, "c": 0},
  "content_hash": "sha256:…",
  "leased": false
}
```

- `author` MAY be sent; the server overwrites it from the session.
- `content_hash` (optional) is the hash of the key's content after the op,
  as the writer computes it (code: sha256 of the symbol's normalized
  source). The server never interprets it; it lets later writers name their
  base by content (§5.1). Writers that know content SHOULD always send it.
- `public` MUST contain every field the kind declares (§15) and SHOULD
  contain nothing secret: the server reads it.
- `payload` is base64. The server MUST NOT interpret it. Its size limit is
  a server setting (reference: 1 MiB).

### 4.2 Lifecycle

```
client: op ──► server: validate ─► resolve (§5) ─► lease check (§7) ─► log append (§12) ─► apply ─► ack ─► deltas (§8)
                          │              │                  │                  │
                          └─ invalid     └─ conflict        └─ leased          └─ error{code:"log"}, nothing applied
```

1. **Validate**: op id, a domain owns the key, the key normalizes, the kind
   exists and applies to the key, required public fields, payload size, and
   for a key-moving kind a target different from the key.
2. **Idempotency**: an `op_id` that was already accepted returns the stored
   `ack`, unchanged, and nothing else happens.
   **The same result**: an op that would make the key what it already is
   is acked as it stands (§5.2), and nothing else happens.
3. **Resolve** against the key's history (§5).
4. **Leases**: the key the op lands on (and a rename's target) must not be
   leased by another session for kinds incompatible with the op's kind (§7).
5. **Append** to the log. Only after `append` returns MAY the server ack.
6. **Apply**: the key's version becomes `version + 1`, its hash is chained
   (§4.3), the op joins the key's history in the author's draft layer.
7. **Ack** to the writer, then **deltas** to everyone else concerned.

### 4.3 Key hashes

```
key_hash(v+1) = sha256_hex( key_hash(v) \n op_id \n kind \n sha256_hex(payload or "") \n from_key_or_empty )
key_hash(0)   = ""
```

`from_key` is set only on the target of a rename/move (the key the subject
came from). A key's hash proves which ops it has seen without carrying
content; clients learn it from acks and deltas and send it back as
`base.hash`.

## 5. Resolving an op: accept, rebase, rekey, reject

Every key keeps its last `history_per_key` ops (reference: 64). A writer's
base older than that window is rejected `conflict` with what remains.

```
resolve(scope, session, op):
    key   ← op.key
    from  ← op.base.version
    check_hash ← op.base.hash ≠ ""
    rebased ← []
    repeat at most 32 times:
        st ← state(scope, key)                       # version 0 if never written
        if from > st.version:            reject invalid ("base ahead")
        if check_hash and hash_at(st, from) known and ≠ op.base.hash:
                                         reject invalid ("base hash")
        check_hash ← false                           # only the named key's hash is checked
        if history does not reach back to from: reject conflict (theirs = what is left)

        follow ← none;  conflicts ← [];  walked_move ← false
        for e in st.history where e.version > from, oldest first:
            if e.moved_to:                walked_move ← true
            if e.arrived_from and e.author ≠ session:
                conflicts += e; continue             # a subject moved onto this key: look first
            if e.author = session:                   # own pipelined ops never conflict
                if e.moved_to and conflicts = []: follow ← e.moved_to; break
                continue
            match table[op.kind][e.kind]:
                commute                       → rebased += e
                rekey and e.moved_to, no conflicts yet → follow ← e.moved_to; break
                otherwise                     → conflicts += e
        if conflicts ≠ []: reject conflict (theirs = conflicts, key = this key)

        # the writer saw the move already but still names the old key
        if follow = none and not walked_move and st.moved_to:
            e ← the op that moved it
            rel ← rekey if e.author = session else table[op.kind][e.kind]
            rekey    → follow ← st.moved_to
            commute  → stay (the op revives the old key)
            conflict → reject conflict (theirs = [e])

        if follow = none: return accept(key, rebased)
        key ← follow.key;  from ← follow.version     # continue on the new key
    reject invalid ("rekey chain longer than 32 hops")
```

Outcomes:

- **accept**: `rebased = []`; the ack carries nothing extra.
- **rebase**: the ack's `rebased_over` lists the commuting ops (with their
  payloads); the client applies them locally.
- **rekey**: the op lands on the key the rename/move sent the subject to;
  the ack's `key` and `rekeyed_to` name it. Chains (rename, then move) are
  followed hop by hop, and a conflict found on a later hop is reported with
  that hop's `key`.
- **reject conflict**: `theirs` lists every conflicting op since the base
  (with payloads), `holder` is the first one's author, `their_intent` is its
  `intent_id` plus the text of a lease its author holds, if any.

On a rename/move op itself, the domain computes the target from the public
fields (§15). When the op lands, the source key records `moved_to` and the
target key is bumped (`version + 1`, hash chained with `from_key`) with an
`arrived_from` history entry. Leases and subscriptions on the source follow
it to the target.

An op whose kind moves the key and that is itself rekeyed computes its
target from the key it lands on.

### 5.1 Bases by content

A writer that knows content but not versions (a hook looking at a file on
disk) names its base by the content's hash `C`, in either of two equivalent
forms:

- `base: {version: 0, hash: "", content: C}`, or
- `base: {version: 0, hash: C}` (a non-empty hash at version 0 can never be
  a chain hash, so this is unambiguous).

Before §5 runs, the server turns it into a version:

```
version_of_hash(st, C):
    st.version = 0                                 → 0     # any copy of a new key is a copy of nothing
    C = st.hash                                    → st.version
    newest history entry e with e.hash = C or e.content_hash = C → e.version
    C = st.content0                                → 0
    otherwise                                      → unknown
start = max(version_of_hash(st, C), base.version)      # then §5 from there, no chain-hash check
```

- `content_hash` on ops feeds the lookup; `st.content0` (the content at
  version 0) is recorded from the first op accepted on a key whose base was
  named by content.
- The lookup reaches as far back as the key's history window
  (`history_per_key`) plus version 0.
- **Unknown** content on a key with history means the writer's copy holds
  something the server never saw (a human edit, a local merge). An op is
  rejected `unknown_base`; `check` answers `status: "unknown_base"` (no
  `would`). Both list in `theirs` the other sessions' unpromoted ops on the
  key: what that copy may be missing.
- After a conflict the writer merges locally; the merged content is new to
  the server, so the retry MUST use the reject's `current` (`version` +
  chain `hash`). Acks give `{version, hash}` for the next op.

### 5.2 The same result

Two writers that make a key the same content are not in conflict: the
second has nothing to add. Before §5 runs, an op whose `content_hash` is
non-empty and equals the `content_hash` of the key's head entry (and that
does not move its key, on a key whose subject has not moved away) is acked
at the head: `version`, `hash` and `seq` are the head's, `same_as` names
the op that made that content, and the op id is not kept (a resend is
decided again; a `promote` lists it in `unknown`). Nothing is logged,
applied or sent to anyone else. Insert of an identical symbol, a body edit
to the same body, a file replaced by the same file: all `ack`.

For a landed op (§10.1) any version in the key's history window counts:
the head descends from that content, so the commit is already in it.
Returning to an older version's content is otherwise a real change, judged
by §5.

## 6. Batches

```json
{"t": "batch", "batch_id": "alice-1.b3", "atomic": true, "ops": [ …ops without "t"… ]}
```

**Atomic** (`atomic: true`, the default) is two-phase:

1. **Check**: every op is validated and resolved (§4.2 steps 1–4) against the
   current state. No op may touch a key (its landing key or a rename's
   target) or op id another op of the batch touches; that is `invalid`.
2. If any op failed: the failing ops get their own `reject`, every other op
   gets `reject{reason: "batch_aborted"}`, all with `batch_id`. Nothing is
   applied or logged.
3. **Apply**: all ops are appended as **one** log entry (`LogRecord::Batch`)
   and applied; one `ack` per op, all with the same `seq` and `batch_id`;
   then deltas. An op the key already holds (§5.2) is acked first, with
   `same_as` and the batch's id, and is not part of the entry.

Resending a batch whose ops were all accepted returns the original acks.

**Non-atomic** (`atomic: false`): each op is decided and logged on its own,
in order, as if sent alone (with `batch_id` on answers).

## 7. Leases and grace

A lease says "I am about to do these kinds of change to these keys":

```json
{"t": "lease", "req": "L1", "keys": ["code:src/cli.py::parse_config"], "kinds": ["modify_body"],
 "intent": {"id": "i-42", "text": "tighten validation"}}
```

- **Granting**: a lease is granted when, for every key, every lease another
  session holds on it is **compatible**: for each pair of kinds `(a, b)`,
  neither `table[a][b]` nor `table[b][a]` is `conflict`. A lease frame is
  all-or-nothing. A session re-leasing a key replaces its own kinds.
  Compatible leases coexist (a `rename` lease next to a `modify_body` lease).
- **Denied**: `lease_denied` names the first holder and lists every
  conflict. The denied session becomes a **waiter** on those keys and gets
  `lease_released` when they are released.
- **Writes against leases**: an op of kind `k` on a key leased by another
  session for kinds `L` is rejected `leased` when any `l ∈ L` is incompatible
  with `k`. The holder's own writes are never blocked by its lease.
- **No heartbeat.** A lease lives while its session's connection lives.
  When the connection drops, the session is **detached**: its leases and
  subscriptions stay for `grace_ms` (reference default 5000, a server
  setting). A `hello{resume: true}` with the same `session` (same scope and
  person) inside the grace reattaches it; `welcome.resumed_leases` lists what
  it still holds, with heads. After the grace, the server releases them by
  itself (`lease_released{reason: "expired"}` to waiters, subscribers of the
  keys and co-holders) and forgets the session.
- A `hello` without `resume` for a known session starts over: its leases
  are released with `reason: "superseded"`.
- A second connection resuming a session supersedes the first:
  `error{code: "superseded", fatal: true}` to the old one.
- An op with `leased: true` from a session holding no lease on the key is
  rejected `stale_lease` (the lease expired, was released, or the session
  started over).
- Lease holders receive deltas for their leased keys (as if subscribed at
  depth 0).

Sans-IO: the transport calls `disconnected(conn, now)` and calls `tick(now)`
at or after `next_deadline()`.

## 8. Subscriptions and deltas

```json
{"t": "subscribe", "keys": ["code:src/api.py::route", "code:src/db.py::*"], "depth": 2}
```

- A key ending in `*` is a prefix subscription.
- `depth` is capped by the server (reference: 3). Depth `d` means: tell me
  about my keys, and about any key my keys depend on within `d` hops of the
  domain's graph.
- When an op is accepted on key `K` (and, for a rename/move, its target),
  the server walks `dependents` breadth-first from `K` up to the cap. A
  session gets one delta per op if it:
  - subscribes to `K` (or a matching prefix) at any depth: `via` absent;
  - holds a lease on `K`: `via` absent;
  - subscribes to a key `S` reached at distance `n ≤ its depth`: `via` is
    the path `[S, …, K]` (shortest).
- The writer never gets a delta for its own op (it gets the ack).
- Deltas are sent in the same handler that acks, so they leave the server
  before any later frame is processed: **instant push**, no polling.
- Sessions in other scopes never see anything.
- Rename/move: subscriptions on the source are extended to the target.

## 9. The gate: `check`

`check` asks "would these writes be accepted?" without writing:

```json
{"t": "check", "req": "q1", "keys": [{"key": "code:src/cli.py::parse_config", "base": {"version": 0, "hash": ""}, "kind": "modify_body"}]}
```

Per key: `status` is `ok` (base current, no conflicting lease), `stale`
(others wrote since the base, or a rekey/conflict is predicted),
`unknown_base` (the base names content the server never saw on a key with
history, §5.1; `theirs` = others' unpromoted ops) or `leased` (another session holds an incompatible
lease; `holder`, `intent`; this takes precedence). With a
`kind`, `would` is the outcome §5 predicts (`accept`, `rebase`, `rekey`,
`conflict`), `theirs` the ops involved, `rekeyed_to` the landing key. Without
a `base`, the base is the current head. Without a `kind`, any lease held by
another session counts as `leased`, and a `stale` key is a reader's
question: `theirs` lists the other sessions' ops since the base (following a
rename or move to the key that now holds the subject, named in
`rekeyed_to`). A writer about to call a function checks it this way, and
learns of a new signature, a rename or a delete before its call goes in.
`check` changes nothing and logs nothing; a write after it can still be
rejected (time passes).

## 10. Drafts and promotion

- Every accepted op is a **draft** in its author's layer: applied, visible
  to everyone (deltas carry `layer: "draft"`), not yet proven.
- `promote{op_ids, evidence}` marks ops **promoted**. It is what an agent's
  tested push sends: `evidence` is free-form JSON (`{"tests": "passed",
  "commit": "<sha>"}`). An op may be promoted by any session of the same
  **person** in the same scope (the hook that pushes is not the session that
  wrote). Unknown op ids and other people's ops are listed in `unknown`.
- Per key the server tracks `version` (draft head) and `promoted_version`
  (highest promoted). Both appear in every `Head`.
- Promotion is logged (`LogRecord::Promote`). Subscribers get each promoted
  op again as a delta with `layer: "promoted"`; the promoter gets `promoted`.

### 10.1 Landed history

An op whose `op_id` starts with `git.` is history that already landed on
the shared branch: a commit's ops as a client converts them
(`git.<sha>.<n>`, `is_landed`). It is never rejected `conflict`,
`unknown_base` or `leased`: it goes over the drafts it meets (the key's
version moves on as for any accepted op), it is promoted as it is applied
(its history entry is in the promoted layer and `promoted_version` is its
version, in replay too), and its deltas carry `layer: "promoted"`. A writer
whose draft it superseded meets it the normal way: a `conflict` naming it
in `theirs`, or a `check` that says stale. Validation still applies, and
§5.2 acks a commit whose content the key already went through.

### Commit trailer

A commit that carries ops names them in its final trailer block:

```
Fix config parsing on Windows

Co-Authored-By: …
Cork-Ops: alice-1.17,alice-1.18,alice-2.3
```

- Key `Cork-Ops` (case-insensitive on read; `Collide-Ops`, the key before the rename, is read the same), value a comma-separated op id
  list, spaces around commas allowed. Long lists MAY repeat the line;
  writers wrap past ~900 bytes. Only the final paragraph is read.
- `trailer::format_trailer`, `parse_trailer`, `append_trailer` (Rust) and
  `formatTrailer`, `parseTrailer`, `appendTrailer` (TS) implement it.
- A checkout's git history therefore says which ops its content contains: a
  client can `adopt` those versions as its bases (§4, `Client::adopt`).

## 11. Messages

`message{to?, anchor?, text}`:

- `anchor` (a key): delivered to sessions that subscribe to it, lease it, or
  have unpromoted ops in its history: whoever is on that code.
- `to`: a session id, agent id or person; with `anchor` too, the
  intersection.
- neither: every session in the scope.

Only attached sessions receive; the sender never receives its own message.
No recipient → `error{code: "undelivered"}`. Messages are not logged.

## 12. The log

The op log is a hash chain, like a ledger:

```
LogEntry { seq: u64 (1, 2, …), prev: hash of entry seq-1 (64 zeros for seq 1), hash, hlc, record }
hash = sha256_hex( prev \n seq \n "<hlc.w>.<hlc.c>" \n canonical_json(record) )
record = {"r": "accept", …Accepted} | {"r": "batch", batch_id, accepted: […]} | {"r": "promote", scope, op_ids, keys, evidence, by}
```

`canonical_json` sorts object keys at every depth and has no whitespace, so
the hash does not depend on a serializer's key order. `verify_chain(entries,
prev, first_seq)` detects any edit (`BadHash`), reordering (`BrokenLink`) or
gap (`Gap`).

- The engine computes `seq` and hashes; a `Log` implementation only makes
  entries durable (`append`). **An op is acked only after `append` returns
  Ok.** On Err, nothing is applied and the writer gets `error{code: "log",
  req: op_id}`; resending is safe.
- `snapshot_every() -> Option<n>`: the engine hands `snapshot(&Snapshot)`
  its state every `n` entries, after applying them. A snapshot is
  `{seq, hash, hlc, keys, ops}`; restoring is `EngineBuilder::snapshot(s)`
  then `Engine::replay(entries after s.seq)` (which verifies the chain).
- Leases, subscriptions and messages are connection state and are not
  logged.

## 13. Clocks

Every node keeps a hybrid logical clock `{w, c}`:

```
now(phys):       w' = max(last.w, phys); c' = (w' = last.w) ? last.c + 1 : 0
observe(r, phys):
    if r.w > phys + max_drift (60 s): now(phys)          # never adopt a far-future stamp
    w' = max(last.w, r.w, phys)
    c' = both equal  → max(last.c, r.c) + 1
         last.w only → last.c + 1
         r.w only    → r.c + 1
         else        → 0
```

The server observes the client's `op.hlc`, then stamps each accepted op;
the stamp replaces `op.hlc` in the log and in deltas and is the ack's `hlc`.
Server stamps are strictly increasing, so `(hlc)` orders every accepted op
across keys, and `seq` orders log entries.

## 14. Coalescing

Coalescing is a **client-side, per-stream** setting, **off by default**. A
stream is a key prefix. For fast streams (notes, typing), a client MAY
buffer ops for 50–200 ms; ops on the same key with the same kind collapse
into one (the first op's base, the last op's id; the payload merged by the
stream's merge function: last-wins by default, concatenation for notes
`append`). An op of another kind on a buffered key flushes the buffered op
first, so per-key order is kept. `welcome.domains[].coalesce_ms` is the
domain's suggestion (code: 0, notes: 100); the client decides.

## 15. Domains

A domain supplies:

| Piece | Where |
|---|---|
| key prefixes, kinds (with required `public` fields, `moves_key`, applicable prefixes), the compatibility table | `table.json`, loaded with `Table::from_json`, which rejects incomplete tables, unknown kinds, `rekey` toward a kind that does not move keys, and prefixes the domain does not own |
| key normalization | `Domain::normalize_key` |
| where a key-moving op sends its subject | `Domain::rekey_target` |
| the dependency graph | `Domain::graph() -> Option<&dyn Graph>`; `Graph::dependents(scope, key)` |
| graph upkeep from ops (optional) | `Domain::observe(scope, key, op)` |

`table.json`:

```json
{
  "domain": "notes", "version": 1, "prefixes": ["note:"], "coalesce_ms": 100,
  "kinds": {"append": {"doc": "…", "public": [], "moves_key": false, "keys": []}, …},
  "relations": {"<mine>": {"<theirs>": "commute | rekey | conflict", …}, …},
  "notes": {"<mine>/<theirs>": "why"}
}
```

`relations[mine][theirs]`: *mine* is the op arriving on a stale base,
*theirs* an op accepted since that base (or a lease's declared kind). Every
cell MUST be present.

**Replayable kinds.** A `commute` cell is a promise that the two ops compose
when applied in either order. For content changes that means the later op is
replayed as a typed operation on current content (rename symbol X, append
these bytes), not as a textual diff against its base.

## 16. The code domain

Keys:

- `code:<path>::<symbol>`: a symbol; `<symbol>` is dot-qualified
  (`Config.load`). Paths are repo-relative with `/`; `./`, `//` and `\` are
  normalized; `..` and absolute paths are invalid.
- `file:<path>`: a whole file, the fallback for anything without symbols
  (configs, lockfiles, binaries). Same rules; one key per file. A path is
  keyed one way or the other by the client; the server does not relate
  `file:p` to `code:p::*` (a client MAY subscribe to `code:p::*`).

Kinds:

| kind | keys | public | moves key |
|---|---|---|---|
| `modify_body` | `code:` | | |
| `signature_change` | `code:` | `new_signature` | |
| `rename` | `code:` | `new_name` (replaces the last segment; with a dot, the whole symbol) | → `code:<path>::<new symbol>` |
| `insert` | `code:` | | |
| `delete` | both | | |
| `move` | both | `new_path` | → `code:<new_path>::<symbol>` / `file:<new_path>` |
| `file_replace` | `file:` | | |

Table (`relations[mine][theirs]`):

| mine \ theirs | `modify_body` | `signature_change` | `rename` | `insert` | `delete` | `move` | `file_replace` |
|---|---|---|---|---|---|---|---|
| `modify_body` | conflict | conflict | **rekey** | conflict | conflict | **rekey** | conflict |
| `signature_change` | conflict | conflict | **rekey** | conflict | conflict | **rekey** | conflict |
| `rename` | commute | commute | conflict | conflict | conflict | **rekey** | conflict |
| `insert` | conflict | conflict | commute | conflict | commute | commute | conflict |
| `delete` | conflict | conflict | conflict | conflict | commute | conflict | conflict |
| `move` | commute | commute | **rekey** | conflict | conflict | conflict | conflict |
| `file_replace` | conflict | conflict | conflict | conflict | conflict | **rekey** | conflict |

Why, cell by cell where it is not obvious:

- `modify_body × modify_body`: two bodies; the server cannot merge text it
  never sees.
- `modify_body × signature_change`, `signature_change × modify_body`: the
  body is written for one signature, the signature changed under it.
- `modify_body × rename`, `× move`: their op moved the symbol; the edit
  follows it to the new key without anyone reading code.
- `rename × modify_body`, `move × modify_body` (and `× signature_change`): a
  name or a location and a body are independent; the rename/move is replayed
  on the current body.
- `rename × rename`, `move × move`: two destinations for one symbol.
- `insert × rename`, `× move`, `× delete`: their op freed the name; the new
  symbol takes it (the old key lives again).
- `insert × insert`: two symbols created under one name.
- `delete × *`: deleting something someone just changed needs a human (or
  agent) decision, except `delete × delete`, which is idempotent.
- Writing onto a key a subject was **moved into** since the base always
  conflicts, whatever the kinds (§5, `arrived_from`).
- Callers are other keys: a `signature_change` never blocks a caller's edit;
  it reaches the callers' subscribers as a delta (§8).

**Content hashes.** Cork's hooks name a base by what their copy holds
(`base: {version: 0, hash: "", content}`, §5.1) and send each op's
`content_hash`, both computed one way (`cork_core::local::symbol_hash`):
SHA-256 hex of the text with `\r\n` replaced by `\n` and trailing
whitespace stripped from every line. For a `code:` key the text is the
symbol's source lines, decorators included (the parser's span); for a
`file:` key, the whole file. A delete's `content_hash` is `""`. Symbol names
with whitespace (`impl Foo`) are keyed with `_` for each run of it
(`impl_Foo`).

The graph is calls: `dependents(callee) = callers`. Cork's server feeds
it from its parse index (`CodeDomain::with_graph`); the reference server
starts empty (`CodeDomain::graph_handle().add_edge`).

## 17. The notes domain

Keys `note:<slug>` (lowercased, spaces to `-`, `[a-z0-9-_/.]`). Kinds:
`edit_text`, `retitle` (`new_slug`, moves the key), `link` / `unlink`
(`target`, a note key), `delete`, `append`. The graph is links:
`link` from A to B makes A a dependent of B (the domain maintains it in
`observe`). Coalescing suggestion: 100 ms; `coalesce_merge` concatenates
appends.

| mine \ theirs | `edit_text` | `retitle` | `link` | `unlink` | `delete` | `append` |
|---|---|---|---|---|---|---|
| `edit_text` | conflict | **rekey** | commute | commute | conflict | conflict |
| `retitle` | commute | conflict | commute | commute | conflict | commute |
| `link` | commute | **rekey** | commute | commute | conflict | commute |
| `unlink` | commute | **rekey** | commute | commute | conflict | commute |
| `delete` | conflict | conflict | conflict | conflict | commute | conflict |
| `append` | conflict | **rekey** | commute | commute | conflict | commute |

`append × append` commutes: both land, in sequence order. Links are a set
ordered by sequence: for the same target the later op wins.

## 18. Security model

- **The server never sees code.** It sees key names (paths and symbol
  names), versions, hashes, kinds and public fields. The public fields a
  kind declares are deliberately small (a new name, a new signature, a new
  path) and are the only content-derived data it reads.
- **Payloads are opaque.** Encrypting them is the client's business (for
  example with a key shared by a team and never given to the server); the
  server stores, hashes and forwards ciphertext. Hashes cover the payload
  bytes as sent.
- **Identity** comes from `hello` and the server's authorizer
  (`EngineBuilder::authorize(|hello| -> Result<Peer, String>)`), which checks
  `token` and returns the person and agent the session acts as. The
  `author` of every op, the `from` of every message and promotion rights
  come from that identity, never from client-supplied fields. Without an
  authorizer (the reference server) the hello is trusted.
- **Scopes isolate.** No frame, delta, lease or message crosses scopes.
- **The log is tamper-evident** (§12); a mirror can verify it without
  trusting the server that wrote it.
- **Limits**: payload size, batch size, key length, rekey hops, history
  depth and subscription depth are bounded (Config).

## 19. Errors

`error{code, message, fatal?, req?}`:

| code | fatal | when |
|---|---|---|
| `hello_required` | no | a frame before `hello` |
| `proto` | yes | `hello.proto` is not this server's version |
| `bad_hello` | yes | missing session or scope |
| `unauthorized` | yes | the authorizer refused the hello |
| `already_hello` | no | a second `hello` on one connection |
| `scope_mismatch` | yes | resuming a session of another scope or person |
| `superseded` | yes | another connection resumed this session |
| `bad_frame` | no | not JSON, or not a known frame |
| `invalid` | no | a bad `lease`, `subscribe`, `check`, `message` or `batch` frame |
| `undelivered` | no | a `message` with no live recipient |
| `log` | no | the log refused the append; nothing was applied (`req` = op id) |

## 20. Versioning

`hello.proto` and `welcome.proto` carry the protocol version (now `1`). A
server MUST refuse a version it does not speak with a fatal `proto` error.
Within a version, new optional fields and new frame types MAY be added;
receivers ignore what they do not know. Removing or changing the meaning of
a field, a frame or a reason is a new version. Domain tables carry their own
`version`; changing a cell is a new table version.

## 21. Conformance

`schema/vectors/*.json` are golden scenarios. Each is
`{name, description, config?, steps}`; a step does one thing at time `at`
(ms): `connect`, `send` (a complete client frame), `disconnect`, `tick`, or
`graph` (`[[scope, dependent, dependency], …]`), and lists **exactly** the
frames the server must emit, in order, as `{conn, recv}`. `recv` matches as a
subset; `"$any"` matches anything, `"$bind:x"` remembers a value, `"$ref:x"`
must equal it (and is substituted in later `send` frames). The Rust runner
(`conformance/`) replays them against the Engine and validates every frame
sent and received against `frames.schema.json`; the TS tests round-trip
every `send` frame and check every `recv` against the TS frame tables.

## 22. Future: `git-remote-cork`

Reserved. A git remote helper (`git-remote-cork`, invoked for
`cork://<server>/<scope>` remotes) that exports promoted ops as commits:

- `git fetch cork://…` turns each scope's promoted ops into commits on
  `refs/cork/<person>/<session>`, with `Cork-Ops:` trailers, payloads
  decrypted client-side; drafts are available under `refs/cork/drafts/…`.
- `git push cork://…` reads the trailers of the pushed commits and sends
  `promote` with `{"commit": <sha>}` as evidence.
- The helper needs nothing the protocol does not already carry: ops,
  promotion, the trailer and the verifiable log.

## 23. Integration: Cork's server

Cork's server (`server/cork-rs`, `protocol_ws.rs`) runs the Engine in
both of its builds (cloud and the free local server).

**For clients (the hooks):**

- **URL**: `GET <server>/protocol/v1/ws` (WebSocket upgrade).
- **hello**: `token` = the bearer credential the hooks send to `/report`
  (an `Authorization: Bearer …` header on the upgrade also works); `scope` =
  the repo id as the hooks send it. The server resolves the canonical
  workspace scope (aliases and renames included) and ignores the hello's
  `agent_id`/`person`: `agent_id` is `<user>#<session>` (Cork's agent
  id), `person` the account's email.
- **Bases**: §5.1 (`base.content` + `content_hash` on every op).
- **Viewers** (read-only seats) may connect, subscribe and check; `op`,
  `batch`, `lease`, `promote` and `message` get `error{code: "read_only"}`.
- **Dashboard**: a dashboard sign-in token in `hello.token`, with
  `?workspace=` on the URL (or `workspace` in the hello), connects read-only.

**What the server does with it:**

- The op log is `app_kv` rows (`protocol_log`, one per entry, keyed by the
  zero-padded seq), written insert-if-absent, so they ride the database's
  changeset shipping; a snapshot (`protocol_snapshot`) every 1000 entries
  prunes the entries it covers. At boot, and before a frame when another
  machine appended, the Engine is rebuilt or caught up from snapshot +
  replay. A frame that appended is answered once the log has it.
- `dependents(code:<path>::<sym>)` = its callers in Cork's code graph.
- A `reject` for `conflict` or `leased` writes a ledger row `op_conflict`
  and a `collision` on the dashboard's feed (Inbox), unless every op in
  `theirs` is landed history (§10.1): a write on a copy older than what
  landed is a stale write, on the ledger only; accepted ops (an ack with
  `same_as` too) touch presence only; a promotion writes `op_promoted`.
  `op_conflict` rows a landed op got before §10.1 are not catches, and a
  sweep at boot took their collisions off the feed (`landed_sweep`).
- Messages reach each recipient once. A client `message` is filed in the
  repo's thread; a recipient who heard it live on a socket does not also
  get it in Cork's inbox, one who did not (no session connected) does,
  so `undelivered` means "not live", not "lost". A dashboard message to one
  agent session arrives as a `message` frame when that session is
  connected, else in its inbox.
- `person` is the credential's account (email), the same for every session
  it opens: a hook's fallback (`<session>.d<pid>`) and ingest
  (`<session>.ingest`) sessions may promote its ops.
- `GET /api/workspaces/:id/protocol?repo=` (dashboard auth): sessions,
  leases, recent ops and conflicts, key draft/promoted counts. `/health`
  carries `protocol` (connections, ops accepted, rejects, log seq).
