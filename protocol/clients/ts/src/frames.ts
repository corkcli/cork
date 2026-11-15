// Every frame on the wire, mirroring core/src/frames.rs field for field.
// Optional fields (`?`) are the ones the server omits when empty or that
// default when absent; required fields are always present.

export const PROTO_VERSION = 1;

export interface Hlc { w: number; c: number }
/** `content`: hash of the content the writer has, used when `hash` is empty;
 * `{version: 0, hash: C}` names the base by content too (SPEC §5.1). */
export interface Base { version: number; hash?: string; content?: string }
export interface Head { version: number; hash: string; promoted_version?: number }
export interface MovedTo { key: string; version: number; hash: string }
export interface Peer { session?: string; agent_id?: string; person?: string }
export interface Intent { id?: string; text?: string }

export interface Op {
  op_id: string;
  key: string;
  base?: Base;
  kind: string;
  public?: Record<string, unknown>;
  /** Opaque bytes, standard base64. */
  payload?: string;
  /** Filled by the server. */
  author?: Peer;
  intent_id?: string;
  hlc?: Hlc;
  /** Hash of the key's content after this op (lets later writers base by content). */
  content_hash?: string;
  leased?: boolean;
}

// ---------------------------------------------------------------- client → server

export interface Hello { t: "hello"; proto: number; session: string; agent_id: string; person: string; scope: string; token?: string; resume?: boolean }
export interface OpFrame extends Op { t: "op" }
export interface Batch { t: "batch"; batch_id: string; atomic?: boolean; ops: Op[] }
export interface Lease { t: "lease"; req?: string; keys: string[]; kinds?: string[]; intent?: Intent }
export interface Release { t: "release"; req?: string; keys: string[] }
export interface Subscribe { t: "subscribe"; req?: string; keys: string[]; depth?: number }
export interface Unsubscribe { t: "unsubscribe"; req?: string; keys: string[] }
export interface CheckKey { key: string; base?: Base; kind?: string }
export interface Check { t: "check"; req?: string; keys: CheckKey[] }
export interface Promote { t: "promote"; req?: string; op_ids: string[]; evidence?: unknown }
export interface SendMessage { t: "message"; req?: string; to?: string; anchor?: string; text: string }

export type ClientFrame = Hello | OpFrame | Batch | Lease | Release | Subscribe | Unsubscribe | Check | Promote | SendMessage;

// ---------------------------------------------------------------- server → client

export type Layer = "draft" | "promoted";
export type RejectReason = "conflict" | "leased" | "invalid" | "stale_lease" | "batch_aborted" | "unknown_base";
export type ReleaseReason = "released" | "expired" | "superseded";
export type CheckStatus = "ok" | "stale" | "leased" | "unknown_base";
export type Would = "accept" | "rebase" | "rekey" | "conflict";

export interface DomainInfo { name: string; prefixes: string[]; kinds: string[]; coalesce_ms: number }
export interface LeaseInfo { key: string; kinds: string[]; intent?: Intent; head: Head }
export interface TheirOp {
  op_id: string; key: string; version: number; kind: string;
  public?: Record<string, unknown>; payload?: string; author: Peer; intent_id?: string; hlc: Hlc; layer: Layer;
}

export interface Welcome {
  t: "welcome"; proto: number; session: string; resumed: boolean; hlc: Hlc; grace_ms: number;
  resumed_leases?: LeaseInfo[]; resumed_subscriptions?: string[]; domains: DomainInfo[];
}
export interface Ack {
  t: "ack"; op_id: string; key: string; version: number; hash: string; hlc: Hlc; seq: number;
  batch_id?: string; rebased_over?: TheirOp[]; rekeyed_to?: string; moved_to?: MovedTo; same_as?: string;
}
export interface Reject {
  t: "reject"; op_id: string; key: string; reason: RejectReason; current: Head;
  theirs?: TheirOp[]; their_intent?: Intent; holder?: Peer; batch_id?: string; message?: string;
}
export interface Delta { t: "delta"; op: Op; version: number; hash: string; seq: number; layer: Layer; via?: string[]; moved_to?: MovedTo }
export interface LeaseGranted { t: "lease_granted"; req?: string; keys: string[]; kinds: string[]; intent?: Intent; heads: Head[] }
export interface LeaseConflict { key: string; holder: Peer; kinds: string[]; intent?: Intent }
export interface LeaseDenied { t: "lease_denied"; req?: string; keys: string[]; holder: Peer; intent?: Intent; conflicts: LeaseConflict[] }
export interface LeaseReleased { t: "lease_released"; req?: string; keys: string[]; reason: ReleaseReason; holder: Peer }
export interface CheckEntry {
  key: string; status: CheckStatus; current: Head; would?: Would; rekeyed_to?: string;
  theirs?: TheirOp[]; holder?: Peer; intent?: Intent;
}
export interface CheckResult { t: "check_result"; req?: string; results: CheckEntry[] }
export interface PromotedKey { key: string; promoted_version: number }
export interface Promoted { t: "promoted"; req?: string; op_ids: string[]; keys: PromotedKey[]; unknown?: string[]; evidence?: unknown }
export interface Message { t: "message"; from: Peer; to?: string; anchor?: string; text: string; hlc: Hlc }
export interface ErrorFrame { t: "error"; code: string; message: string; fatal?: boolean; req?: string }

export type ServerFrame =
  | Welcome | Ack | Reject | Delta | LeaseGranted | LeaseDenied | LeaseReleased
  | CheckResult | Promoted | Message | ErrorFrame;

// ---------------------------------------------------------------- field tables

export interface FieldSpec { required: readonly string[]; optional: readonly string[] }

/** Client frames: required = no serde default in frames.rs. */
export const CLIENT_FIELDS: Readonly<Record<ClientFrame["t"], FieldSpec>> = {
  hello: { required: ["proto", "session", "agent_id", "person", "scope"], optional: ["token", "resume"] },
  op: { required: ["op_id", "key", "kind"], optional: ["base", "public", "payload", "author", "intent_id", "hlc", "content_hash", "leased"] },
  batch: { required: ["batch_id", "ops"], optional: ["atomic"] },
  lease: { required: ["keys"], optional: ["req", "kinds", "intent"] },
  release: { required: ["keys"], optional: ["req"] },
  subscribe: { required: ["keys"], optional: ["req", "depth"] },
  unsubscribe: { required: ["keys"], optional: ["req"] },
  check: { required: ["keys"], optional: ["req"] },
  promote: { required: ["op_ids"], optional: ["req", "evidence"] },
  message: { required: ["text"], optional: ["req", "to", "anchor"] },
};

export const SERVER_FIELDS: Readonly<Record<ServerFrame["t"], FieldSpec>> = {
  welcome: { required: ["proto", "session", "resumed", "hlc", "grace_ms", "domains"], optional: ["resumed_leases", "resumed_subscriptions"] },
  ack: { required: ["op_id", "key", "version", "hash", "hlc", "seq"], optional: ["batch_id", "rebased_over", "rekeyed_to", "moved_to", "same_as"] },
  reject: { required: ["op_id", "key", "reason", "current"], optional: ["theirs", "their_intent", "holder", "batch_id", "message"] },
  delta: { required: ["op", "version", "hash", "seq", "layer"], optional: ["via", "moved_to"] },
  lease_granted: { required: ["keys", "kinds", "heads"], optional: ["req", "intent"] },
  lease_denied: { required: ["keys", "holder", "conflicts"], optional: ["req", "intent"] },
  lease_released: { required: ["keys", "reason", "holder"], optional: ["req"] },
  check_result: { required: ["results"], optional: ["req"] },
  promoted: { required: ["op_ids", "keys"], optional: ["req", "unknown", "evidence"] },
  message: { required: ["from", "text", "hlc"], optional: ["to", "anchor"] },
  error: { required: ["code", "message"], optional: ["fatal", "req"] },
};

export const CLIENT_TAGS = Object.keys(CLIENT_FIELDS) as ClientFrame["t"][];
export const SERVER_TAGS = Object.keys(SERVER_FIELDS) as ServerFrame["t"][];

export class ProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProtocolError";
  }
}

function check(obj: unknown, table: Readonly<Record<string, FieldSpec>>, side: string): void {
  if (typeof obj !== "object" || obj === null || Array.isArray(obj)) throw new ProtocolError(`${side} frame is not an object`);
  const t = (obj as { t?: unknown }).t;
  if (typeof t !== "string" || !(t in table)) throw new ProtocolError(`unknown ${side} frame tag ${JSON.stringify(t)}`);
  for (const f of table[t].required) {
    if ((obj as Record<string, unknown>)[f] === undefined) throw new ProtocolError(`${t} frame is missing \`${f}\``);
  }
}

function parse(text: string | object): unknown {
  if (typeof text !== "string") return text;
  try {
    return JSON.parse(text);
  } catch (e) {
    throw new ProtocolError(`not JSON: ${(e as Error).message}`);
  }
}

/** Parse and validate a server frame (text or an already parsed object). */
export function decodeServerFrame(text: string | object): ServerFrame {
  const v = parse(text);
  check(v, SERVER_FIELDS, "server");
  return v as ServerFrame;
}

export function decodeClientFrame(text: string | object): ClientFrame {
  const v = parse(text);
  check(v, CLIENT_FIELDS, "client");
  return v as ClientFrame;
}

export function encodeClientFrame(f: ClientFrame): string {
  return JSON.stringify(f);
}
