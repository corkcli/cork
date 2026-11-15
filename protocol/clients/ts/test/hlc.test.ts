import { test } from "node:test";
import assert from "node:assert/strict";
import { Clock, compareHlc, type Hlc } from "../src/index.js";

test("now is monotonic when the wall clock stalls or goes back", () => {
  const c = new Clock();
  const xs = [c.now(1000), c.now(1000), c.now(900), c.now(1001)];
  for (let i = 1; i < xs.length; i++) assert.equal(compareHlc(xs[i - 1], xs[i]), -1);
  assert.deepEqual(xs[3], { w: 1001, c: 0 });
});

test("observe moves past remote; far-future remote is ignored", () => {
  const c = new Clock();
  c.now(1000);
  assert.deepEqual(c.observe({ w: 1500, c: 7 }, 1200), { w: 1500, c: 8 });
  const d = new Clock(1000);
  assert.equal(d.observe({ w: 1_000_000, c: 0 }, 1000).w, 1000);
});

test("thousands of mixed events stay strictly increasing", () => {
  const c = new Clock();
  let prev: Hlc = { w: 0, c: 0 };
  for (let i = 0; i < 5000; i++) {
    const phys = 10_000 + (i % 7) * 3 - (i % 5);
    const x = i % 3 === 0 ? c.observe({ w: phys + (i % 11), c: i % 4 }, phys) : c.now(phys);
    assert.equal(compareHlc(prev, x), -1);
    prev = x;
  }
});
