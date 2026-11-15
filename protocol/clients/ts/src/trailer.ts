// The `Cork-Ops:` commit trailer, mirroring core/src/trailer.rs exactly.
// Commits from before the rename carry `Collide-Ops:`, read the same.
export const TRAILER_KEY = "Cork-Ops";
export const OLD_TRAILER_KEY = "Collide-Ops";

/** `Cork-Ops: a,b,c`, wrapped onto several lines past ~900 bytes. */
export function formatTrailer(opIds: readonly string[]): string {
  const lines: string[] = [];
  let cur = "";
  for (const id of opIds) {
    if (cur && cur.length + id.length > 900) {
      lines.push(`${TRAILER_KEY}: ${cur}`);
      cur = "";
    }
    if (cur) cur += ",";
    cur += id;
  }
  if (cur) lines.push(`${TRAILER_KEY}: ${cur}`);
  return lines.join("\n");
}

function lastParagraph(msg: string): string {
  const i = msg.lastIndexOf("\n\n");
  return i < 0 ? msg : msg.slice(i + 2);
}

/** Every op id in the message's final paragraph, in order, deduplicated. */
export function parseTrailer(message: string): string[] {
  const block = lastParagraph(message.trimEnd());
  const ids: string[] = [];
  for (const line of block.split("\n")) {
    const at = line.indexOf(":");
    if (at < 0) continue;
    const key = line.slice(0, at).trim().toLowerCase();
    if (key !== TRAILER_KEY.toLowerCase() && key !== OLD_TRAILER_KEY.toLowerCase()) continue;
    for (const raw of line.slice(at + 1).split(",")) {
      const id = raw.trim();
      if (id && !ids.includes(id)) ids.push(id);
    }
  }
  return ids;
}

/** Add the trailer, joining an existing trailer block when there is one. */
export function appendTrailer(message: string, opIds: readonly string[]): string {
  if (opIds.length === 0) return message;
  const msg = message.trimEnd();
  const last = lastParagraph(msg);
  const isTrailers =
    msg.includes("\n\n") &&
    last.length > 0 &&
    last.split("\n").every((l) => {
      const at = l.indexOf(":");
      return at > 0 && !l.slice(0, at).includes(" ");
    });
  return `${msg}${isTrailers ? "\n" : "\n\n"}${formatTrailer(opIds)}\n`;
}
