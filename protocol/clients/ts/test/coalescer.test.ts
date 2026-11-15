import { test } from "node:test";
import assert from "node:assert/strict";
import { Coalescer, type Op } from "../src/index.js";

const op = (key: string, kind: string, id: string, extra: Partial<Op> = {}): Op => ({ op_id: id, key, kind, ...extra });

test("collapses same key, same kind; keeps the first base", () => {
  const c = new Coalescer(100);
  assert.deepEqual(c.push(op("note:x", "edit_text", "1", { base: { version: 4, hash: "h4" } }), 0), []);
  assert.deepEqual(c.push(op("note:x", "edit_text", "2", { payload: "bGF0ZXI=" }), 50), []);
  assert.deepEqual(c.push(op("note:y", "edit_text", "3"), 60), []);
  assert.equal(c.due(99).length, 0);
  const due = c.due(100);
  assert.equal(due.length, 1);
  assert.equal(due[0].op_id, "2");
  assert.equal(due[0].base?.version, 4);
  assert.equal(due[0].payload, "bGF0ZXI=");
  assert.equal(c.due(160).length, 1);
  assert.equal(c.length, 0);
});

test("flushes on kind change to keep per-key order", () => {
  const c = new Coalescer(100);
  c.push(op("note:x", "append", "1"), 0);
  const out = c.push(op("note:x", "retitle", "2"), 10);
  assert.equal(out.length, 1);
  assert.equal(out[0].op_id, "1");
  assert.equal(c.flush()[0].op_id, "2");
});

test("custom merge concatenates", () => {
  const c = new Coalescer(10, (e, l) => {
    e.payload = (e.payload ?? "") + (l.payload ?? "");
    e.op_id = l.op_id;
  });
  c.push(op("k", "append", "1", { payload: "a" }), 0);
  c.push(op("k", "append", "2", { payload: "b" }), 1);
  assert.equal(c.nextDue(), 10);
  assert.equal(c.flush()[0].payload, "ab");
});

test("mergeAppends concatenates appends like the notes domain", async () => {
  const { Coalescer, mergeAppends } = await import("../src/coalescer.js");
  const { base64Encode, base64Decode, utf8 } = await import("../src/base64.js");
  const c = new Coalescer(100, mergeAppends);
  const mk = (id: string, text: string) => ({ op_id: id, key: "note:log", kind: "append", base: { version: 3, hash: "h" }, payload: base64Encode(utf8(text)) });
  c.push(mk("1", "hello "), 0);
  c.push(mk("2", "world"), 10);
  const out = c.due(100);
  assert.equal(out.length, 1);
  assert.equal(out[0].op_id, "2");
  assert.equal(out[0].base?.version, 3);
  assert.equal(new TextDecoder().decode(base64Decode(out[0].payload!)), "hello world");
});
