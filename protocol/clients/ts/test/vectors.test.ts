// Validates the encoding against the conformance vectors in schema/vectors:
// send frames round-trip; expected recv frames name only known fields.
import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { SERVER_FIELDS, decodeClientFrame, encodeClientFrame } from "../src/index.js";

const here = dirname(fileURLToPath(import.meta.url));
// dist/test → clients/ts → clients → protocol
const dir = join(here, "..", "..", "..", "..", "schema", "vectors");
const files = existsSync(dir) ? readdirSync(dir).filter((f) => f.endsWith(".json")).sort() : [];

interface Step {
  send?: unknown;
  expect?: Array<{ conn: string; recv: Record<string, unknown> }>;
}

if (files.length === 0) {
  test("conformance vectors", { skip: `no vectors in ${dir} yet` }, () => {});
}

for (const file of files) {
  test(`vector ${file}`, () => {
    const v = JSON.parse(readFileSync(join(dir, file), "utf8")) as { name: string; steps: Step[] };
    assert.ok(typeof v.name === "string" && Array.isArray(v.steps), "vector shape");
    for (const [i, step] of v.steps.entries()) {
      if (step.send !== undefined) {
        const f = decodeClientFrame(step.send as object);
        assert.deepEqual(JSON.parse(encodeClientFrame(f)), step.send, `${file} step ${i}: send round trip`);
      }
      for (const e of step.expect ?? []) {
        const t = e.recv.t;
        assert.ok(typeof t === "string" && t in SERVER_FIELDS, `${file} step ${i}: unknown server tag ${String(t)}`);
        const spec = SERVER_FIELDS[t as keyof typeof SERVER_FIELDS];
        const known = new Set(["t", ...spec.required, ...spec.optional]);
        for (const k of Object.keys(e.recv)) {
          assert.ok(known.has(k), `${file} step ${i}: \`${k}\` is not a field of ${t}`);
        }
      }
    }
  });
}
