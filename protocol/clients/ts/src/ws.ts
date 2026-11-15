// A WebSocket connection over the global WebSocket (browsers, node 22+):
// hello on open, promise-returning requests matched by op_id / req, events
// for everything else, a timer for coalesced streams, and reconnect with
// `resume: true` (inside the server's grace period leases survive).
import { Client, type ClientConfig } from "./client.js";
import type {
  Ack, CheckResult, ClientFrame, Delta, ErrorFrame, Intent, LeaseDenied, LeaseGranted,
  LeaseReleased, Message, Op, Promoted, Reject, ServerFrame, Welcome,
} from "./frames.js";
import { decodeServerFrame, encodeClientFrame } from "./frames.js";

export interface ConnectOptions {
  resume?: boolean;
  /** Reconnect after an unexpected close (default true). */
  reconnect?: boolean;
  /** Request timeout in ms (default 10 000). */
  timeoutMs?: number;
  onDelta?: (d: Delta) => void;
  onMessage?: (m: Message) => void;
  onLeaseReleased?: (r: LeaseReleased) => void;
  onError?: (e: ErrorFrame) => void;
  /** Every frame, after the client state is updated. */
  onFrame?: (f: ServerFrame) => void;
  onReconnect?: (w: Welcome) => void;
}

interface Waiter {
  match: (f: ServerFrame) => boolean;
  resolve: (f: ServerFrame) => void;
  reject: (e: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

export class CorkError extends Error {
  constructor(readonly frame: ErrorFrame) {
    super(`${frame.code}: ${frame.message}`);
    this.name = "CorkError";
  }
}
/** The name before the rename to Cork. */
export const CollideError = CorkError;

export class Connection {
  readonly client: Client;
  private ws?: WebSocket;
  private waiters: Waiter[] = [];
  private pollTimer?: ReturnType<typeof setTimeout>;
  private closed = false;
  private backoff = 250;

  private constructor(private readonly url: string, config: ClientConfig, private readonly opts: ConnectOptions) {
    this.client = new Client(config);
  }

  static async connect(url: string, config: ClientConfig, opts: ConnectOptions = {}): Promise<Connection> {
    const c = new Connection(url, config, opts);
    await c.open(opts.resume ?? false);
    return c;
  }

  private get timeoutMs(): number {
    return this.opts.timeoutMs ?? 10_000;
  }

  private open(resume: boolean): Promise<Welcome> {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.url);
      this.ws = ws;
      let welcomed = false;
      ws.onopen = () => {
        ws.send(encodeClientFrame(this.client.hello(resume)));
      };
      ws.onmessage = (ev: MessageEvent) => {
        let f: ServerFrame;
        try {
          f = decodeServerFrame(String(ev.data));
        } catch {
          return;
        }
        this.client.receive(f);
        if (!welcomed) {
          if (f.t === "welcome") {
            welcomed = true;
            this.backoff = 250;
            resolve(f);
          } else if (f.t === "error") {
            reject(new CorkError(f));
          }
          return;
        }
        this.dispatch(f);
      };
      ws.onerror = () => {
        if (!welcomed) reject(new Error(`could not connect to ${this.url}`));
      };
      ws.onclose = () => {
        if (!welcomed) return;
        if (!this.closed && (this.opts.reconnect ?? true)) this.scheduleReconnect();
        else this.failAll(new Error("connection closed"));
      };
    });
  }

  private scheduleReconnect(): void {
    const wait = this.backoff;
    this.backoff = Math.min(this.backoff * 2, 10_000);
    setTimeout(() => {
      if (this.closed) return;
      this.open(true).then(
        (w) => this.opts.onReconnect?.(w),
        () => this.scheduleReconnect(),
      );
    }, wait);
  }

  private dispatch(f: ServerFrame): void {
    const i = this.waiters.findIndex((w) => w.match(f));
    if (i >= 0) {
      const [w] = this.waiters.splice(i, 1);
      clearTimeout(w.timer);
      if (f.t === "error") w.reject(new CorkError(f));
      else w.resolve(f);
    } else {
      if (f.t === "delta") this.opts.onDelta?.(f);
      else if (f.t === "message") this.opts.onMessage?.(f);
      else if (f.t === "lease_released") this.opts.onLeaseReleased?.(f);
      else if (f.t === "error") this.opts.onError?.(f);
    }
    this.opts.onFrame?.(f);
  }

  private failAll(e: Error): void {
    for (const w of this.waiters.splice(0)) {
      clearTimeout(w.timer);
      w.reject(e);
    }
  }

  private wait(match: (f: ServerFrame) => boolean): Promise<ServerFrame> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.waiters = this.waiters.filter((w) => w.timer !== timer);
        reject(new Error("timed out waiting for the server"));
      }, this.timeoutMs);
      this.waiters.push({ match, resolve, reject, timer });
    });
  }

  send(f: ClientFrame): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) throw new Error("not connected");
    this.ws.send(encodeClientFrame(f));
  }

  private sendAll(fs: ClientFrame[]): void {
    for (const f of fs) this.send(f);
    this.armPoll();
  }

  private armPoll(): void {
    if (this.pollTimer) return;
    const next = this.client.nextPoll();
    if (next === undefined) return;
    this.pollTimer = setTimeout(() => {
      this.pollTimer = undefined;
      try {
        this.sendAll(this.client.poll());
      } catch {
        // disconnected: the ops stay buffered until reconnect's next poll
      }
    }, Math.max(0, next - Date.now()));
  }

  /** Send an op; resolves with its ack or reject. A coalesced op resolves when its window flushes. */
  write(op: Op): Promise<Ack | Reject> {
    const id = op.op_id;
    const p = this.wait((f) => ((f.t === "ack" || f.t === "reject") && f.op_id === id) || (f.t === "error" && f.req === id));
    this.sendAll(this.client.submit(op));
    return p as Promise<Ack | Reject>;
  }

  private request<T extends ServerFrame>(f: ClientFrame & { req?: string }, tags: T["t"][]): Promise<T> {
    const req = f.req;
    const p = this.wait((x) => (tags.includes(x.t as T["t"]) || x.t === "error") && "req" in x && x.req === req);
    this.send(f);
    return p as Promise<T>;
  }

  check(keys: string[], kind?: string): Promise<CheckResult> {
    return this.request<CheckResult>(this.client.check(keys, kind), ["check_result"]);
  }

  lease(keys: string[], kinds: string[], intent?: Intent): Promise<LeaseGranted | LeaseDenied> {
    return this.request<LeaseGranted | LeaseDenied>(this.client.lease(keys, kinds, intent), ["lease_granted", "lease_denied"]);
  }

  release(keys: string[]): Promise<LeaseReleased> {
    return this.request<LeaseReleased>(this.client.release(keys), ["lease_released"]);
  }

  promote(opIds: string[], evidence: unknown): Promise<Promoted> {
    return this.request<Promoted>(this.client.promote(opIds, evidence), ["promoted"]);
  }

  subscribe(keys: string[], depth = 0): void {
    this.send(this.client.subscribe(keys, depth));
  }

  unsubscribe(keys: string[]): void {
    this.send({ t: "unsubscribe", keys });
  }

  message(text: string, to?: string, anchor?: string): void {
    this.send(this.client.message(text, to, anchor));
  }

  /** Flush coalesced ops and close; leases release after the grace period. */
  close(): void {
    this.closed = true;
    try {
      for (const f of this.client.flush()) this.send(f);
    } catch {
      // already closed
    }
    if (this.pollTimer) clearTimeout(this.pollTimer);
    this.ws?.close();
    this.failAll(new Error("connection closed"));
  }
}
