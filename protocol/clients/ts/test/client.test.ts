import { test } from "node:test";
import assert from "node:assert/strict";
import { Client, base64Decode, base64Encode, utf8, type Delta, type Op } from "../src/index.js";

const cfg = { session: "s 1", agent_id: "a", person: "p", scope: "repo" };

test("tracks bases from acks, not deltas", () => {
  const cl = new Client(cfg);
  const o = cl.op("code:a.py::f", "modify_body", {}, utf8("x"), 1);
  assert.equal(o.op_id, "s_1.1");
  assert.equal(o.payload, "eA==");
  const frames = cl.submit(o, 1);
  assert.equal(frames.length, 1);
  assert.equal(frames[0].t, "op");
  assert.equal(cl.inFlight(), 1);
  cl.receive({ t: "ack", op_id: o.op_id, key: o.key, version: 1, hash: "h1", hlc: { w: 2, c: 0 }, seq: 1 }, 2);
  assert.equal(cl.inFlight(), 0);
  assert.deepEqual(cl.base("code:a.py::f"), { version: 1, hash: "h1" });
  const d: Delta = { t: "delta", op: { ...o, op_id: "other.1" }, version: 2, hash: "h2", seq: 2, layer: "draft" };
  cl.receive(d, 3);
  assert.equal(cl.base("code:a.py::f").version, 1, "seeing is not having");
  assert.equal(cl.head("code:a.py::f")?.version, 2);
  cl.adopt("code:a.py::f", { version: 2, hash: "h2" });
  assert.equal(cl.base("code:a.py::f").version, 2);
});

test("auto-adopt streams follow deltas and coalesce", () => {
  const cl = new Client({ ...cfg, session: "n" });
  cl.stream("note:", 100, true);
  assert.equal(cl.submit(cl.op("note:x", "append", {}, utf8("a"), 0), 0).length, 0);
  assert.equal(cl.submit(cl.op("note:x", "append", {}, utf8("b"), 10), 10).length, 0);
  assert.equal(cl.nextPoll(), 100);
  assert.equal(cl.poll(100).length, 1);
  const op: Op = { op_id: "o.1", key: "note:x", kind: "append" };
  cl.receive({ t: "delta", op, version: 7, hash: "h7", seq: 1, layer: "draft" }, 0);
  assert.equal(cl.base("note:x").version, 7);
});

test("rekeyed ack moves the base; moved_to seeds the new key", () => {
  const cl = new Client(cfg);
  const o = cl.op("code:a.py::f", "rename", { new_name: "g" });
  cl.submit(o);
  cl.receive({
    t: "ack", op_id: o.op_id, key: "code:a.py::f", version: 3, hash: "h3", hlc: { w: 1, c: 0 }, seq: 4,
    moved_to: { key: "code:a.py::g", version: 1, hash: "g1" },
  });
  assert.deepEqual(cl.base("code:a.py::g"), { version: 1, hash: "g1" });
});

test("frame builders use req ids and bases", () => {
  const cl = new Client(cfg);
  const h = cl.hello(true);
  assert.deepEqual(h, { t: "hello", proto: 1, session: "s 1", agent_id: "a", person: "p", scope: "repo", resume: true });
  const c = cl.check(["code:a.py::f"], "modify_body");
  assert.equal(c.req, "r1");
  assert.deepEqual(c.keys[0], { key: "code:a.py::f", base: { version: 0, hash: "" }, kind: "modify_body" });
  const b = cl.batch([cl.op("k:1", "x"), cl.op("k:2", "x")]);
  assert.equal(b.atomic, true);
  assert.equal(b.ops.length, 2);
});

test("base64 round trips", () => {
  for (let n = 0; n < 40; n++) {
    const bytes = new Uint8Array(n).map((_, i) => (i * 37) & 255);
    assert.deepEqual(base64Decode(base64Encode(bytes)), bytes);
  }
  assert.equal(base64Encode(utf8("hello")), "aGVsbG8=");
});
