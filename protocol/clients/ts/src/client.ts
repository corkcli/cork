// The client side, sans-IO, mirroring core/src/client.rs.
//
// Two views of every key are kept apart on purpose:
// - base(key): what this agent's content is built on. Advances on this
//   agent's own acks, and on adopt() once the agent has applied someone
//   else's op locally. Ops are based on it.
// - head(key): the newest version the server has told us about. Seeing a
//   change is not having it.
// A stream whose client applies every remote op as it arrives (a shared notes
// document) sets autoAdopt for its prefix, and deltas advance the base too.
import { base64Encode } from "./base64.js";
import { Coalescer } from "./coalescer.js";
import type {
  Base, Batch, Check, ClientFrame, Head, Hello, Intent, Lease, Op, Promote, Release,
  SendMessage, ServerFrame, Subscribe, Welcome,
} from "./frames.js";
import { PROTO_VERSION } from "./frames.js";
import { Clock } from "./hlc.js";

export interface ClientConfig {
  session: string;
  agent_id: string;
  person: string;
  scope: string;
  token?: string;
}

interface Stream {
  prefix: string;
  coalescer?: Coalescer;
  autoAdopt: boolean;
}

export class Client {
  private clock = new Clock();
  private n = 0;
  private bases = new Map<string, Base>();
  private heads = new Map<string, Head>();
  private pending = new Map<string, string>();
  private streams: Stream[] = [];
  welcome?: Welcome;

  constructor(readonly config: ClientConfig) {}

  hello(resume: boolean): Hello {
    const h: Hello = {
      t: "hello",
      proto: PROTO_VERSION,
      session: this.config.session,
      agent_id: this.config.agent_id,
      person: this.config.person,
      scope: this.config.scope,
    };
    if (this.config.token !== undefined) h.token = this.config.token;
    if (resume) h.resume = true;
    return h;
  }

  /** Per-prefix stream settings: coalescing window (0 = off) and auto-adopt. */
  stream(prefix: string, coalesceMs: number, autoAdopt: boolean): void {
    this.streamWith(prefix, coalesceMs > 0 ? new Coalescer(coalesceMs) : undefined, autoAdopt);
  }

  streamWith(prefix: string, coalescer: Coalescer | undefined, autoAdopt: boolean): void {
    this.streams = this.streams.filter((s) => s.prefix !== prefix);
    this.streams.push({ prefix, coalescer, autoAdopt });
    this.streams.sort((a, b) => b.prefix.length - a.prefix.length);
  }

  private streamFor(key: string): Stream | undefined {
    return this.streams.find((s) => key.startsWith(s.prefix));
  }

  /** `<session>.<n>`, characters outside the op-id alphabet replaced by `_`. */
  nextOpId(): string {
    this.n += 1;
    const s = [...this.config.session]
      .map((c) => (/^[A-Za-z0-9._:@-]$/.test(c) ? c : "_"))
      .slice(0, 100)
      .join("");
    return `${s}.${this.n}`;
  }

  private nextReq(): string {
    this.n += 1;
    return `r${this.n}`;
  }

  base(key: string): Base {
    return { ...(this.bases.get(key) ?? { version: 0, hash: "" }) };
  }

  head(key: string): Head | undefined {
    return this.heads.get(key);
  }

  /** The agent has applied everything up to this version locally. */
  adopt(key: string, base: Base): void {
    this.bases.set(key, { version: base.version, hash: base.hash ?? "" });
  }

  inFlight(): number {
    return this.pending.size;
  }

  /** An op on `key`, based on what this agent has. */
  op(key: string, kind: string, pub: Record<string, unknown> = {}, payload?: Uint8Array, nowMs = Date.now()): Op {
    const op: Op = {
      op_id: this.nextOpId(),
      key,
      base: this.base(key),
      kind,
      author: { session: "", agent_id: "", person: "" },
      hlc: this.clock.now(nowMs),
    };
    if (Object.keys(pub).length) op.public = pub;
    if (payload) op.payload = base64Encode(payload);
    return op;
  }

  /** Send an op: straight away, or through its stream's coalescer. */
  submit(op: Op, nowMs = Date.now()): ClientFrame[] {
    const c = this.streamFor(op.key)?.coalescer;
    return this.track(c ? c.push(op, nowMs) : [op]);
  }

  private track(ops: Op[]): ClientFrame[] {
    return ops.map((o) => {
      this.pending.set(o.op_id, o.key);
      return { t: "op", ...o };
    });
  }

  poll(nowMs = Date.now()): ClientFrame[] {
    const ops: Op[] = [];
    for (const s of this.streams) if (s.coalescer) ops.push(...s.coalescer.due(nowMs));
    return this.track(ops);
  }

  flush(): ClientFrame[] {
    const ops: Op[] = [];
    for (const s of this.streams) if (s.coalescer) ops.push(...s.coalescer.flush());
    return this.track(ops);
  }

  nextPoll(): number | undefined {
    const due = this.streams.map((s) => s.coalescer?.nextDue()).filter((x): x is number => x !== undefined);
    return due.length ? Math.min(...due) : undefined;
  }

  batch(ops: Op[]): Batch {
    for (const o of ops) this.pending.set(o.op_id, o.key);
    const id = this.nextOpId();
    this.n += 1;
    return { t: "batch", batch_id: `${id}.b${this.n}`, atomic: true, ops };
  }

  lease(keys: string[], kinds: string[], intent?: Intent): Lease {
    const f: Lease = { t: "lease", req: this.nextReq(), keys, kinds };
    if (intent) f.intent = intent;
    return f;
  }

  release(keys: string[]): Release {
    return { t: "release", req: this.nextReq(), keys };
  }

  subscribe(keys: string[], depth = 0): Subscribe {
    return { t: "subscribe", req: this.nextReq(), keys, depth };
  }

  /** The pre-write gate: would writes of `kind` on these keys be accepted? */
  check(keys: string[], kind?: string): Check {
    return {
      t: "check",
      req: this.nextReq(),
      keys: keys.map((key) => (kind === undefined ? { key, base: this.base(key) } : { key, base: this.base(key), kind })),
    };
  }

  promote(opIds: string[], evidence: unknown): Promote {
    return { t: "promote", req: this.nextReq(), op_ids: opIds, evidence };
  }

  message(text: string, to?: string, anchor?: string): SendMessage {
    const m: SendMessage = { t: "message", text };
    if (to !== undefined) m.to = to;
    if (anchor !== undefined) m.anchor = anchor;
    return m;
  }

  private see(key: string, version: number, hash: string): void {
    const h = this.heads.get(key) ?? { version: 0, hash: "", promoted_version: 0 };
    if (version >= h.version) {
      h.version = version;
      h.hash = hash;
    }
    this.heads.set(key, h);
  }

  /** Feed a frame from the server; updates bases and heads. */
  receive(f: ServerFrame, nowMs = Date.now()): void {
    switch (f.t) {
      case "welcome":
        this.clock.observe(f.hlc, nowMs);
        for (const l of f.resumed_leases ?? []) this.see(l.key, l.head.version, l.head.hash);
        this.welcome = f;
        break;
      case "ack": {
        this.clock.observe(f.hlc, nowMs);
        const named = this.pending.get(f.op_id);
        if (named !== undefined) {
          this.pending.delete(f.op_id);
          if (f.rekeyed_to !== undefined) this.bases.delete(named);
        }
        this.see(f.key, f.version, f.hash);
        this.bases.set(f.key, { version: f.version, hash: f.hash });
        if (f.moved_to) {
          this.see(f.moved_to.key, f.moved_to.version, f.moved_to.hash);
          this.bases.set(f.moved_to.key, { version: f.moved_to.version, hash: f.moved_to.hash });
        }
        break;
      }
      case "reject":
        this.pending.delete(f.op_id);
        this.see(f.key, f.current.version, f.current.hash);
        break;
      case "delta": {
        if (f.op.hlc) this.clock.observe(f.op.hlc, nowMs);
        this.see(f.op.key, f.version, f.hash);
        if (f.moved_to) this.see(f.moved_to.key, f.moved_to.version, f.moved_to.hash);
        const adopt = this.streamFor(f.op.key)?.autoAdopt ?? false;
        if (adopt && f.via === undefined && f.layer === "draft") {
          this.bases.set(f.op.key, { version: f.version, hash: f.hash });
          if (f.moved_to) this.bases.set(f.moved_to.key, { version: f.moved_to.version, hash: f.moved_to.hash });
        }
        break;
      }
      case "lease_granted":
        f.keys.forEach((k, i) => {
          const h = f.heads[i];
          if (h) this.see(k, h.version, h.hash);
        });
        break;
      case "check_result":
        for (const e of f.results) this.see(e.key, e.current.version, e.current.hash);
        break;
      default:
        break;
    }
  }
}
