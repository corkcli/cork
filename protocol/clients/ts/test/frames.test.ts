import { test } from "node:test";
import assert from "node:assert/strict";
import { CLIENT_TAGS, ProtocolError, SERVER_TAGS, decodeClientFrame, decodeServerFrame, encodeClientFrame } from "../src/index.js";

test("tag tables match frames.rs", () => {
  assert.deepEqual([...CLIENT_TAGS].sort(), ["batch", "check", "hello", "lease", "message", "op", "promote", "release", "subscribe", "unsubscribe"]);
  assert.deepEqual([...SERVER_TAGS].sort(), [
    "ack", "check_result", "delta", "error", "lease_denied", "lease_granted", "lease_released", "message", "promoted", "reject", "welcome",
  ]);
});

test("decode rejects unknown tags, missing fields and non-JSON", () => {
  assert.throws(() => decodeServerFrame('{"t":"nope"}'), ProtocolError);
  assert.throws(() => decodeServerFrame('{"op_id":"x"}'), ProtocolError);
  assert.throws(() => decodeServerFrame('{"t":"ack","op_id":"x"}'), /missing `key`/);
  assert.throws(() => decodeServerFrame("not json"), ProtocolError);
  assert.throws(() => decodeClientFrame('{"t":"op","op_id":"a","key":"k"}'), /missing `kind`/);
  assert.throws(() => decodeClientFrame('{"t":"welcome"}'), ProtocolError);
});

test("round trips a client frame", () => {
  const text = '{"t":"op","op_id":"a.1","key":"code:a.py::f","base":{"version":0,"hash":""},"kind":"modify_body"}';
  assert.deepEqual(JSON.parse(encodeClientFrame(decodeClientFrame(text))), JSON.parse(text));
  const ack = decodeServerFrame({ t: "ack", op_id: "a.1", key: "k", version: 1, hash: "h", hlc: { w: 1, c: 0 }, seq: 1 });
  assert.equal(ack.t, "ack");
});
