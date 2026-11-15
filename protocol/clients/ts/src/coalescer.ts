// Buffers ops for a window and collapses same-key, same-kind ops into one,
// mirroring Coalescer in core/src/client.rs. An op of another kind on a
// buffered key flushes the buffered one first, so per-key order is kept.
import type { Op } from "./frames.js";
import { base64Decode, base64Encode } from "./base64.js";

/** Collapse `later` into `earlier` (mutating it). */
export type MergeFn = (earlier: Op, later: Op) => void;

/** Keep the earlier op's base; take everything else from the later one. */
export const mergeLastWins: MergeFn = (earlier, later) => {
  const base = earlier.base;
  for (const k of Object.keys(earlier)) delete (earlier as unknown as Record<string, unknown>)[k];
  Object.assign(earlier, later);
  if (base === undefined) delete earlier.base;
  else earlier.base = base;
};

/**
 * The notes stream's merge (cork_protocol_notes::coalesce_merge): appends
 * concatenate their payloads; anything else, the later op wins.
 */
export const mergeAppends: MergeFn = (earlier, later) => {
  if (earlier.kind === "append" && later.kind === "append") {
    const a = earlier.payload ? base64Decode(earlier.payload) : new Uint8Array();
    const b = later.payload ? base64Decode(later.payload) : new Uint8Array();
    const joined = new Uint8Array(a.length + b.length);
    joined.set(a);
    joined.set(b, a.length);
    mergeLastWins(earlier, later);
    earlier.payload = base64Encode(joined);
  } else {
    mergeLastWins(earlier, later);
  }
};

export class Coalescer {
  private buf: Array<{ due: number; op: Op }> = [];
  constructor(readonly windowMs: number, private readonly merge: MergeFn = mergeLastWins) {}

  /** Buffer `op`; returns ops that must go out now. */
  push(op: Op, nowMs: number): Op[] {
    const i = this.buf.findIndex((e) => e.op.key === op.key);
    if (i >= 0) {
      if (this.buf[i].op.kind === op.kind) {
        this.merge(this.buf[i].op, op);
        return [];
      }
      const [earlier] = this.buf.splice(i, 1);
      this.buf.push({ due: nowMs + this.windowMs, op });
      return [earlier.op];
    }
    this.buf.push({ due: nowMs + this.windowMs, op });
    return [];
  }

  /** Ops whose window has passed, in the order they were buffered. */
  due(nowMs: number): Op[] {
    const out = this.buf.filter((e) => e.due <= nowMs).map((e) => e.op);
    this.buf = this.buf.filter((e) => e.due > nowMs);
    return out;
  }

  flush(): Op[] {
    const out = this.buf.map((e) => e.op);
    this.buf = [];
    return out;
  }

  nextDue(): number | undefined {
    return this.buf.length ? Math.min(...this.buf.map((e) => e.due)) : undefined;
  }

  get length(): number {
    return this.buf.length;
  }
}
