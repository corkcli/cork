import { test } from "node:test";
import assert from "node:assert/strict";
import { appendTrailer, formatTrailer, parseTrailer } from "../src/index.js";

test("round trip", () => {
  const msg = appendTrailer("Fix the parser\n\nLonger body.", ["a.1", "b.2"]);
  assert.equal(msg, "Fix the parser\n\nLonger body.\n\nCork-Ops: a.1,b.2\n");
  assert.deepEqual(parseTrailer(msg), ["a.1", "b.2"]);
});

test("joins an existing trailer block and tolerates spaces", () => {
  const msg = appendTrailer("Subject\n\nCo-Authored-By: X <x@y>", ["a.1"]);
  assert.ok(msg.endsWith("Co-Authored-By: X <x@y>\nCork-Ops: a.1\n"));
  assert.deepEqual(parseTrailer("S\n\nCollide-Ops: a.1 , b.2\ncollide-ops: b.2,c.3\n"), ["a.1", "b.2", "c.3"]);
});

test("only the final paragraph counts", () => {
  assert.deepEqual(parseTrailer("S\n\nCork-Ops: a.1\n\nbody after"), []);
});

test("wraps long lists", () => {
  const ids = Array.from({ length: 200 }, (_, i) => `session-${i}.${i}`);
  const t = formatTrailer(ids);
  assert.ok(t.split("\n").length > 1);
  assert.ok(t.split("\n").every((l) => l.length < 1000));
  assert.deepEqual(parseTrailer(`S\n\n${t}`), ids);
});
