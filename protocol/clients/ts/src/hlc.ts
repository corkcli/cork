// Hybrid logical clock, mirroring core/src/hlc.rs: ordering is (w, c).
import type { Hlc } from "./frames.js";

export function compareHlc(a: Hlc, b: Hlc): number {
  return a.w !== b.w ? (a.w < b.w ? -1 : 1) : a.c === b.c ? 0 : a.c < b.c ? -1 : 1;
}

export class Clock {
  private last: Hlc = { w: 0, c: 0 };
  constructor(private readonly maxDriftMs = 60_000) {}

  lastStamp(): Hlc {
    return { ...this.last };
  }

  now(physMs: number): Hlc {
    this.last = physMs > this.last.w ? { w: physMs, c: 0 } : { w: this.last.w, c: this.last.c + 1 };
    return { ...this.last };
  }

  observe(remote: Hlc, physMs: number): Hlc {
    if (remote.w > physMs + this.maxDriftMs) return this.now(physMs);
    const last = this.last;
    const w = Math.max(last.w, remote.w, physMs);
    let c: number;
    if (w === last.w && w === remote.w) c = Math.max(last.c, remote.c) + 1;
    else if (w === last.w) c = last.c + 1;
    else if (w === remote.w) c = remote.c + 1;
    else c = 0;
    this.last = { w, c };
    return { ...this.last };
  }
}
