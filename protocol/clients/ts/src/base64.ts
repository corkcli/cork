// Standard base64 with padding, without Buffer (browsers and node 22).
const A = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

export function base64Encode(bytes: Uint8Array): string {
  let s = "";
  for (let i = 0; i < bytes.length; i += 3) {
    const n = (bytes[i] << 16) | ((bytes[i + 1] ?? 0) << 8) | (bytes[i + 2] ?? 0);
    s += A[(n >> 18) & 63] + A[(n >> 12) & 63];
    s += i + 1 < bytes.length ? A[(n >> 6) & 63] : "=";
    s += i + 2 < bytes.length ? A[n & 63] : "=";
  }
  return s;
}

/** Accepts standard and URL-safe alphabets; throws on other characters. */
export function base64Decode(s: string): Uint8Array {
  const clean = s.replace(/=+$/, "");
  const out: number[] = [];
  let acc = 0;
  let bits = 0;
  for (const ch of clean) {
    let v = A.indexOf(ch);
    if (ch === "-") v = 62;
    if (ch === "_") v = 63;
    if (v < 0) throw new Error(`bad base64 character ${JSON.stringify(ch)}`);
    acc = (acc << 6) | v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out.push((acc >> bits) & 255);
      acc &= (1 << bits) - 1;
    }
  }
  return new Uint8Array(out);
}

export function utf8(s: string): Uint8Array {
  return new TextEncoder().encode(s);
}
