//! Images people attach to a message for agents. The dashboard uploads the
//! raw bytes, gets a link back and puts it in the message text; the agent
//! fetches the link like any other URL, with no credential, so the link
//! itself is the secret: 128 random bits.
//!
//! The bytes live in object storage under `uploads/{workspace}/{id}.{ext}`
//! and this server serves them at `GET /u/{id}.{ext}`. The kv bucket
//! `uploads` maps the id to who sent it, its type, size and when it goes:
//! a free workspace's image a day after it was sent, a paid one's never.
//! An hourly sweep deletes what has expired.
//!
//! Only raster images, checked by the declared type AND the first bytes, as
//! `repoicons` does: an SVG can carry script, and the link is public.
//!
//! **Channel files** (`save_file`, channels.rs) take more: images, videos
//! (MP4, WebM, QuickTime) and other files (PDF, zip, text, CSV, code...),
//! up to `file_cap` by plan. Images and videos are recognised by their
//! first bytes, and a file that claims to be one must be one. Markup a
//! browser would run (HTML, SVG, XML with either) is refused whatever it is
//! called, and so are executables. Only images and videos are served
//! inline; every other file is served as `application/octet-stream` with
//! `Content-Disposition: attachment`, so nothing uploaded ever renders as a
//! page. A channel file lives as long as the workspace's history window
//! (`retention: "history"`, read against the plan at the time it is
//! served, so a plan change moves it), and one uploaded and never posted
//! goes after a day.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::repoicons::Objects;
use crate::store::{now, Store};

pub const KV_BUCKET: &str = "uploads";
pub const FOLDER: &str = "uploads/";
pub const MAX_BYTES: usize = 10 * 1024 * 1024;
/// How long a free workspace's image is kept.
pub const FREE_TTL_S: f64 = 86_400.0;
const SWEEP_EVERY_S: u64 = 3600;
const TYPES: [(&str, &str); 4] = [("image/png", "png"), ("image/jpeg", "jpg"), ("image/gif", "gif"), ("image/webp", "webp")];

/// A refusal: the HTTP status and the error the dashboard shows.
pub type Refusal = (u16, String);

/// 32 hex characters from the OS's CSPRNG: the link is the only key to the
/// image, so `compat::new_id` (unique, not unpredictable) will not do.
pub fn new_id() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("OS randomness source (/dev/urandom) unavailable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// `Content-Type` -> (type, extension), refusing anything but the four
/// raster types; parameters (`; charset=…`) and case are ignored.
pub fn declared_type(content_type: &str) -> Result<(&'static str, &'static str), Refusal> {
    let mime = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    TYPES
        .iter()
        .find(|(m, _)| *m == mime)
        .copied()
        .ok_or((415, "use a PNG, JPEG, GIF or WebP image".into()))
}

/// The bytes, refused when empty, too big, or not what `mime` says they are.
pub fn check_bytes(mime: &str, bytes: &[u8]) -> Result<(), Refusal> {
    if bytes.is_empty() {
        return Err((400, "the image is empty".into()));
    }
    if bytes.len() > MAX_BYTES {
        return Err((413, format!("the image is over {} MB", MAX_BYTES / (1024 * 1024))));
    }
    let sniffed = match bytes {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        _ => return Err((400, "that file is not an image".into())),
    };
    if sniffed != mime {
        return Err((400, "the file's contents don't match its type".into()));
    }
    Ok(())
}

/// When an image sent at `created` goes: a day on the free plan, never on a
/// paid one.
pub fn expires_at(plan: &str, created: f64) -> Option<i64> {
    if plan == "free" { Some((created + FREE_TTL_S) as i64) } else { None }
}

pub fn object_key(workspace: &str, id: &str, ext: &str) -> String {
    let ws: String = workspace.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    format!("{FOLDER}{ws}/{id}.{ext}")
}

/// Store an upload for `workspace` and answer with its public link under
/// `public_url`. `(status, body)`, as the route sends them.
#[allow(clippy::too_many_arguments)]
pub fn save(
    store: &Store, objects: &dyn Objects, workspace: &str, by: &str, content_type: &str, bytes: &[u8], plan: &str,
    public_url: &str,
) -> (u16, Value) {
    let (mime, ext) = match declared_type(content_type).and_then(|(mime, ext)| check_bytes(mime, bytes).map(|_| (mime, ext))) {
        Ok(found) => found,
        Err((status, error)) => return (status, json!({"ok": false, "error": error})),
    };
    let id = new_id();
    let key = object_key(workspace, &id, ext);
    if let Err(error) = objects.put(&key, bytes) {
        tracing::warn!("uploads: {error}");
        return (502, json!({"ok": false, "error": "could not store the image"}));
    }
    let created = now();
    let expires = expires_at(plan, created);
    let record = json!({
        "workspace": workspace, "by": by, "key": key, "type": mime, "ext": ext,
        "size": bytes.len(), "created": created, "expires_at": expires,
    });
    if store.kv_put(KV_BUCKET, &id, &record, created).is_err() {
        let _ = objects.delete(&key);
        return (500, json!({"ok": false, "error": "could not store the image"}));
    }
    let url = format!("{}/u/{id}.{ext}", public_url.trim_end_matches('/'));
    (200, json!({"ok": true, "url": url, "expires_at": expires}))
}

/// The record behind `/u/{file}`, when `file` is a live upload's
/// `{id}.{ext}`; None for anything unknown, malformed or expired.
pub fn lookup(store: &Store, file: &str, at: f64) -> Option<Value> {
    let (id, ext) = file.split_once('.')?;
    if !is_id(id) {
        return None;
    }
    let record = store.kv_get(KV_BUCKET, id)?;
    if record.get("ext").and_then(Value::as_str) != Some(ext) {
        return None;
    }
    if expired(store, &record, at, &mut Vec::new()) {
        return None;
    }
    Some(record)
}

/// 32 lowercase hex characters: an upload's id.
pub fn is_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether an upload is past keeping at `at`: its own `expires_at`, or for
/// a channel file the workspace's history window as its plan is now
/// (`windows` remembers each workspace's, for a sweep), or a day when it
/// was never posted.
fn expired(store: &Store, record: &Value, at: f64, windows: &mut Vec<(String, Option<f64>)>) -> bool {
    if record.get("expires_at").and_then(Value::as_f64).is_some_and(|expires| expires <= at) {
        return true;
    }
    if record.get("retention").and_then(Value::as_str) != Some("history") {
        return false;
    }
    let created = record.get("created").and_then(Value::as_f64).unwrap_or(0.0);
    let posted = record.get("attached").and_then(Value::as_str).is_some_and(|m| !m.is_empty());
    if !posted && at - created > UNPOSTED_TTL_S {
        return true;
    }
    let workspace = record.get("workspace").and_then(Value::as_str).unwrap_or("").to_string();
    let window = match windows.iter().find(|(ws, _)| *ws == workspace) {
        Some((_, window)) => *window,
        None => {
            let window = crate::access::plan_for(store, &workspace).history_window_s();
            windows.push((workspace, window));
            window
        }
    };
    window.is_some_and(|window| created + window <= at)
}

/// Delete every expired upload, object then record; how many went. An
/// object that cannot be deleted keeps its record, for the next sweep.
pub fn sweep(store: &Store, objects: &dyn Objects, at: f64) -> usize {
    let mut gone = 0;
    let mut windows = Vec::new();
    for (id, record) in store.kv_list(KV_BUCKET, "") {
        if !expired(store, &record, at, &mut windows) {
            continue;
        }
        if forget(store, objects, &id).is_ok() {
            gone += 1;
        }
    }
    gone
}

/// Delete one upload, object then record. The record stays when the
/// object could not be deleted, for the next sweep.
pub fn forget(store: &Store, objects: &dyn Objects, id: &str) -> Result<(), String> {
    let Some(record) = store.kv_get(KV_BUCKET, id) else { return Ok(()) };
    if let Some(key) = record.get("key").and_then(Value::as_str) {
        if let Err(error) = objects.delete(key) {
            tracing::warn!("uploads: {error}");
            return Err(error);
        }
    }
    let _ = store.kv_delete(KV_BUCKET, id);
    Ok(())
}

// ------------------------------------------------------- channel files

/// A channel file the composer uploaded but nobody posted goes after this.
pub const UNPOSTED_TTL_S: f64 = 86_400.0;
const MB: usize = 1024 * 1024;

/// The largest channel file a workspace on `plan` may share: 25 MB on Free,
/// 100 MB on a paid plan. Bodies are held in memory on their way to the
/// bucket, which is what bounds the paid size.
pub fn file_cap(plan: &str) -> usize {
    if plan == "free" { 25 * MB } else { 100 * MB }
}

/// Extensions never taken: markup a browser runs, and programs.
const REFUSED: [&str; 24] = [
    "html", "htm", "xhtml", "xht", "shtml", "mht", "mhtml", "svg", "svgz", "xsl", "xslt", "hta", "exe", "msi", "scr",
    "bat", "cmd", "com", "pif", "vbs", "vbe", "wsf", "jse", "cpl",
];

/// What a channel file is, once its bytes are read: `kind` image, video or
/// file; `mime` what it is served as (images and videos their own type,
/// every other file `application/octet-stream`); `ext` the link's
/// extension; `shown` the type the dashboard names it by.
#[derive(Debug, Clone, PartialEq)]
pub struct Classified {
    pub kind: &'static str,
    pub mime: String,
    pub ext: String,
    pub shown: String,
}

/// An image or video recognised by its first bytes: (mime, extension).
fn sniff_media(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    let image = match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some(("image/png", "png")),
        [0xFF, 0xD8, 0xFF, ..] => Some(("image/jpeg", "jpg")),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(("image/webp", "webp")),
        [b'G', b'I', b'F', b'8', ..] => Some(("image/gif", "gif")),
        _ => None,
    };
    if image.is_some() {
        return image;
    }
    // ISO media: a box whose type is `ftyp`; QuickTime's brand is "qt  "
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some(if &bytes[8..12] == b"qt  " { ("video/quicktime", "mov") } else { ("video/mp4", "mp4") });
    }
    // an older QuickTime file opens on a movie or data box instead
    if bytes.len() >= 8 && matches!(&bytes[4..8], b"moov" | b"mdat" | b"wide" | b"free" | b"pnot") {
        return Some(("video/quicktime", "mov"));
    }
    // Matroska's EBML header, WebM by its doc type
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) && bytes[..bytes.len().min(64)].windows(4).any(|w| w == b"webm") {
        return Some(("video/webm", "webm"));
    }
    None
}

/// Markup a browser would run if it ever rendered the file: HTML, SVG, or
/// XML that carries either, past any byte-order mark and leading space.
pub fn is_markup(bytes: &[u8]) -> bool {
    let start = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let head: Vec<u8> = start.iter().take(1024).map(u8::to_ascii_lowercase).collect();
    let trimmed = head.iter().position(|b| !b.is_ascii_whitespace()).map(|i| &head[i..]).unwrap_or(&[]);
    let opens = ["<!doctype html", "<html", "<svg", "<script", "<head", "<body", "<iframe"];
    if opens.iter().any(|tag| trimmed.starts_with(tag.as_bytes())) {
        return true;
    }
    let has = |needle: &[u8]| head.windows(needle.len()).any(|w| w == needle);
    trimmed.starts_with(b"<?xml") && (has(b"<svg") || has(b"<html") || has(b"<script"))
}

/// A file's name as it is shown and downloaded: its last path segment,
/// without control characters or quotes, at most 120 characters.
pub fn clean_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = last.chars().filter(|c| !c.is_control() && !matches!(c, '"' | '<' | '>')).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    let cleaned: String = cleaned.chars().take(120).collect();
    if cleaned.is_empty() { "file".into() } else { cleaned }
}

fn name_ext(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Read a channel file: what it is, or why it is refused. `declared` is the
/// upload's `Content-Type` (often empty or generic for a file).
pub fn classify(name: &str, declared: &str, bytes: &[u8], cap: usize) -> Result<Classified, Refusal> {
    if bytes.is_empty() {
        return Err((400, "the file is empty".into()));
    }
    if bytes.len() > cap {
        return Err((413, format!("the file is over {} MB", cap / MB)));
    }
    let declared = declared.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let ext = name_ext(name);
    if REFUSED.contains(&ext.as_str()) || declared.contains("html") || declared.contains("svg") || declared.contains("javascript") {
        return Err((415, "that kind of file can't be shared: no web pages, SVGs or programs".into()));
    }
    if is_markup(bytes) {
        return Err((415, "that file is a web page or an SVG inside: it can't be shared".into()));
    }
    let media_ext = ["png", "jpg", "jpeg", "gif", "webp", "mp4", "m4v", "mov", "webm"];
    match sniff_media(bytes) {
        Some((mime, sniffed_ext)) => {
            // a declared image or video must be the one it is
            let family = mime.split('/').next().unwrap_or("");
            if (declared.starts_with("image/") || declared.starts_with("video/")) && !declared.starts_with(family) {
                return Err((400, "the file's contents don't match its type".into()));
            }
            if declared.starts_with("image/") && declared != mime && !(declared == "image/jpg" && mime == "image/jpeg") {
                return Err((400, "the file's contents don't match its type".into()));
            }
            let kind = if family == "image" { "image" } else { "video" };
            Ok(Classified { kind, mime: mime.into(), ext: sniffed_ext.into(), shown: mime.into() })
        }
        None => {
            if declared.starts_with("image/") || declared.starts_with("video/") || media_ext.contains(&ext.as_str()) {
                return Err((400, "the file's contents don't match its type".into()));
            }
            let safe: String = ext.chars().filter(char::is_ascii_alphanumeric).take(10).collect();
            let shown = match safe.as_str() {
                "pdf" => "application/pdf",
                "zip" => "application/zip",
                "gz" | "tgz" => "application/gzip",
                "txt" | "log" | "md" => "text/plain",
                "csv" => "text/csv",
                "json" => "application/json",
                _ if !declared.is_empty() && !declared.starts_with("text/html") => declared.as_str(),
                _ => "application/octet-stream",
            };
            Ok(Classified {
                kind: "file",
                mime: "application/octet-stream".into(),
                ext: if safe.is_empty() { "bin".into() } else { safe },
                shown: shown.chars().take(100).collect(),
            })
        }
    }
}

/// An image's width and height from its header (PNG, GIF, JPEG, WebP), so
/// a message keeps its shape before the picture loads.
pub fn image_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let be16 = |b: &[u8], i: usize| -> Option<u32> { Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
    let le16 = |b: &[u8], i: usize| -> Option<u32> { Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
    let be32 = |b: &[u8], i: usize| -> Option<u32> { Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?)) };
    let le24 = |b: &[u8], i: usize| -> Option<u32> { Some(u32::from_le_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, 0])) };
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some((be32(bytes, 16)?, be32(bytes, 20)?)),
        [b'G', b'I', b'F', b'8', ..] => Some((le16(bytes, 6)?, le16(bytes, 8)?)),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => match bytes.get(12..16)? {
            b"VP8X" => Some((le24(bytes, 24)? + 1, le24(bytes, 27)? + 1)),
            b"VP8 " => Some((le16(bytes, 26)? & 0x3FFF, le16(bytes, 28)? & 0x3FFF)),
            b"VP8L" => {
                let b = bytes.get(21..25)?;
                let bits = u32::from_le_bytes(b.try_into().ok()?);
                Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
            }
            _ => None,
        },
        [0xFF, 0xD8, ..] => {
            // walk the segments to the first frame header (SOF0..SOF15, not DHT/JPG/DAC)
            let mut i = 2;
            while i + 9 < bytes.len() {
                if bytes[i] != 0xFF {
                    i += 1;
                    continue;
                }
                let marker = bytes[i + 1];
                if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                    return Some((be16(bytes, i + 7)?, be16(bytes, i + 5)?));
                }
                if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                    i += 2;
                    continue;
                }
                i += 2 + be16(bytes, i + 2)? as usize;
            }
            None
        }
        _ => None,
    }
}

/// Who uploaded a channel file and where it may be posted.
pub struct FileFor<'a> {
    pub workspace: &'a str,
    pub channel: &'a str,
    /// the uploader's label (email or uid) and account
    pub by: &'a str,
    pub by_uid: &'a str,
}

/// Store a channel file and answer `{ok, id, url, name, kind, mime, size,
/// width?, height?}`: the id is what a message names to carry it.
pub fn save_file(
    store: &Store, objects: &dyn Objects, owner: &FileFor, name: &str, declared: &str, bytes: &[u8], cap: usize,
    public_url: &str,
) -> (u16, Value) {
    let found = match classify(name, declared, bytes, cap) {
        Ok(found) => found,
        Err((status, error)) => return (status, json!({"ok": false, "error": error})),
    };
    let id = new_id();
    let key = object_key(owner.workspace, &id, &found.ext);
    if let Err(error) = objects.put(&key, bytes) {
        tracing::warn!("uploads: {error}");
        return (502, json!({"ok": false, "error": "could not store the file"}));
    }
    let created = now();
    let url = format!("{}/u/{id}.{}", public_url.trim_end_matches('/'), found.ext);
    let name = clean_name(name);
    let mut record = json!({
        "workspace": owner.workspace, "channel": owner.channel, "by": owner.by, "by_uid": owner.by_uid,
        "key": key, "type": found.mime, "shown": found.shown, "ext": found.ext, "kind": found.kind, "name": name,
        "size": bytes.len(), "created": created, "expires_at": null, "retention": "history", "url": url, "attached": "",
    });
    if found.kind == "image" {
        if let Some((width, height)) = image_size(bytes) {
            record["width"] = json!(width);
            record["height"] = json!(height);
        }
    }
    if store.kv_put(KV_BUCKET, &id, &record, created).is_err() {
        let _ = objects.delete(&key);
        return (500, json!({"ok": false, "error": "could not store the file"}));
    }
    let mut answer = attachment_of(&id, &record);
    answer["ok"] = json!(true);
    (200, answer)
}

/// An upload as a message carries it.
pub fn attachment_of(id: &str, record: &Value) -> Value {
    let field = |k: &str| record.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "id": id, "url": field("url"), "name": field("name"), "kind": field("kind"), "mime": field("shown"),
        "size": field("size"), "width": field("width"), "height": field("height"),
    })
}

/// The headers an upload is served with: (content type, disposition).
/// Images and videos show inline; anything else downloads, as bytes,
/// under its own name.
pub fn served_as(record: &Value) -> (String, String) {
    let field = |k: &str| record.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match field("kind").as_str() {
        "" | "image" | "video" => (field("type"), "inline".into()),
        _ => {
            let name = clean_name(&field("name"));
            let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '_' }).collect();
            let encoded: String = name
                .bytes()
                .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
                .collect();
            ("application/octet-stream".into(), format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"))
        }
    }
}

/// The hourly sweep, on its own thread (the bucket client blocks). Nothing
/// to sweep without object storage, so no thread either.
pub fn start_sweeper(store: Arc<Store>) {
    let Some(bucket) = crate::s3::Bucket::from_env() else { return };
    std::thread::Builder::new()
        .name("collide-uploads".into())
        .spawn(move || loop {
            let gone = sweep(&store, &bucket, now());
            if gone > 0 {
                tracing::info!("uploads: {gone} expired image(s) deleted");
            }
            std::thread::sleep(std::time::Duration::from_secs(SWEEP_EVERY_S));
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Mem(RefCell<HashMap<String, Vec<u8>>>);

    impl Objects for Mem {
        fn put(&self, key: &str, body: &[u8]) -> Result<(), String> {
            self.0.borrow_mut().insert(key.into(), body.to_vec());
            Ok(())
        }
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(self.0.borrow().get(key).cloned())
        }
        fn delete(&self, key: &str) -> Result<(), String> {
            self.0.borrow_mut().remove(key);
            Ok(())
        }
    }

    const PNG: [u8; 9] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0];

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn only_raster_images_whose_bytes_match_their_type_are_taken() {
        assert_eq!(declared_type("image/PNG; charset=binary").unwrap(), ("image/png", "png"));
        assert_eq!(declared_type("image/svg+xml").unwrap_err().0, 415);
        assert_eq!(declared_type("").unwrap_err().0, 415);
        assert!(check_bytes("image/png", &PNG).is_ok());
        assert!(check_bytes("image/jpeg", &[0xFF, 0xD8, 0xFF, 0xE0]).is_ok());
        assert!(check_bytes("image/gif", b"GIF89a").is_ok());
        assert!(check_bytes("image/webp", b"RIFF\0\0\0\0WEBPVP8 ").is_ok());
        assert_eq!(check_bytes("image/jpeg", &PNG).unwrap_err().0, 400, "a PNG sent as a JPEG");
        assert_eq!(check_bytes("image/png", b"<svg onload=alert(1)>").unwrap_err().0, 400);
        assert_eq!(check_bytes("image/png", b"").unwrap_err().0, 400);
    }

    #[test]
    fn ten_megabytes_is_the_ceiling() {
        let mut at_limit = vec![0u8; MAX_BYTES];
        at_limit[..PNG.len()].copy_from_slice(&PNG);
        assert!(check_bytes("image/png", &at_limit).is_ok());
        at_limit.push(0);
        assert_eq!(check_bytes("image/png", &at_limit).unwrap_err().0, 413);
    }

    #[test]
    fn a_free_workspace_keeps_an_image_a_day_and_a_paid_one_keeps_it() {
        assert_eq!(expires_at("free", 1000.5), Some(87_400));
        assert_eq!(expires_at("team", 1000.0), None);
        assert_eq!(expires_at("business", 1000.0), None);
    }

    #[test]
    fn an_upload_is_served_by_its_link_until_it_expires_then_swept() {
        let (db, mem) = (store(), Mem::default());
        let (status, free) = save(&db, &mem, "ws1", "kim@x.dev", "image/png", &PNG, "free", "https://mcp.example.com/");
        assert_eq!(status, 200, "{free}");
        let url = free["url"].as_str().unwrap();
        let file = url.strip_prefix("https://mcp.example.com/u/").expect(url);
        assert!(file.ends_with(".png") && file.len() == 36, "{file}");
        let record = lookup(&db, file, now()).unwrap();
        assert!(record["key"].as_str().unwrap().starts_with("uploads/ws1/"));
        assert_eq!(record["type"], "image/png");
        assert!(lookup(&db, &file.replace(".png", ".gif"), now()).is_none(), "the extension is part of the link");
        assert!(lookup(&db, "../../etc.png", now()).is_none());

        let (_, paid) = save(&db, &mem, "ws2", "kim@x.dev", "image/png", &PNG, "team", "https://mcp.example.com");
        assert_eq!(paid["expires_at"], Value::Null);
        assert_eq!(mem.0.borrow().len(), 2);

        let later = now() + FREE_TTL_S + 1.0;
        assert!(lookup(&db, file, later).is_none(), "expired is unknown");
        assert_eq!(sweep(&db, &mem, later), 1);
        assert_eq!(mem.0.borrow().len(), 1, "the paid upload stays");
        assert!(db.kv_get(KV_BUCKET, &file[..32]).is_none());
    }

    // ------------------------------------------------- channel files

    const JPEG: [u8; 4] = [0xFF, 0xD8, 0xFF, 0xE0];
    const MP4: &[u8] = b"\0\0\0\x18ftypisom\0\0\x02\0";
    const MOV: &[u8] = b"\0\0\0\x14ftypqt  \0\0\0\0";
    const WEBM: &[u8] = b"\x1A\x45\xDF\xA3\x9F\x42\x86\x81\x01\x42\x82\x84webm";
    const CAP: usize = 25 * 1024 * 1024;

    fn kind(name: &str, declared: &str, bytes: &[u8]) -> Result<(&'static str, String), u16> {
        classify(name, declared, bytes, CAP).map(|c| (c.kind, c.mime)).map_err(|(status, _)| status)
    }

    #[test]
    fn images_and_videos_are_known_by_their_bytes() {
        assert_eq!(kind("a.png", "image/png", &PNG), Ok(("image", "image/png".into())));
        assert_eq!(kind("photo.png", "", &JPEG), Ok(("image", "image/jpeg".into())), "a misnamed photo is still the photo it is");
        assert_eq!(kind("a.png", "image/png", &JPEG), Err(400), "a declared type must be the one it is");
        assert_eq!(kind("clip.mp4", "video/mp4", MP4), Ok(("video", "video/mp4".into())));
        assert_eq!(kind("clip.mov", "video/quicktime", MOV), Ok(("video", "video/quicktime".into())));
        assert_eq!(kind("clip.webm", "video/webm", WEBM), Ok(("video", "video/webm".into())));
        assert_eq!(kind("clip.mp4", "video/mp4", b"just some words"), Err(400), "a video that is not one");
        assert_eq!(kind("pic.gif", "", b"GIF? no"), Err(400), "an image by name that is not one");
        assert_eq!(kind("a.mp4", "image/png", MP4), Err(400), "a video sent as an image");
        let pdf = classify("plan.pdf", "application/pdf", b"%PDF-1.7 ...", CAP).unwrap();
        assert_eq!((pdf.kind, pdf.mime.as_str(), pdf.ext.as_str(), pdf.shown.as_str()), ("file", "application/octet-stream", "pdf", "application/pdf"));
        let zip = classify("logs.zip", "application/x-zip-compressed", b"PK\x03\x04...", CAP).unwrap();
        assert_eq!((zip.kind, zip.ext.as_str()), ("file", "zip"));
        assert_eq!(classify("README", "", b"plain words", CAP).unwrap().ext, "bin");
    }

    #[test]
    fn markup_and_programs_are_refused_whatever_they_are_called() {
        let svg = b"<svg xmlns='http://www.w3.org/2000/svg' onload='alert(1)'/>";
        assert_eq!(kind("cat.png", "image/png", svg), Err(415), "an SVG dressed as a PNG");
        assert_eq!(kind("cat.svg", "image/svg+xml", svg), Err(415));
        assert_eq!(kind("cat.txt", "text/plain", svg), Err(415), "an SVG dressed as text");
        assert_eq!(kind("notes.txt", "text/plain", b"\xEF\xBB\xBF\n  <!DOCTYPE html><script>steal()</script>"), Err(415), "HTML dressed as text");
        assert_eq!(kind("data.csv", "text/csv", b"<html><body onload=x()>"), Err(415));
        assert_eq!(kind("feed.xml", "", b"<?xml version='1.0'?>\n<svg><script/></svg>"), Err(415), "SVG inside XML");
        assert_eq!(kind("page.html", "", b"hello"), Err(415), "a web page by name");
        assert_eq!(kind("page.txt", "text/html", b"hello"), Err(415), "a web page by type");
        assert_eq!(kind("setup.exe", "", b"MZ\x90\0"), Err(415));
        assert!(kind("notes.md", "text/markdown", b"# Notes\n\nan <svg> tag mentioned in words").is_ok(), "markup named in words is fine");
        assert_eq!(kind("empty.txt", "", b""), Err(400));
    }

    #[test]
    fn a_file_is_capped_by_plan() {
        assert_eq!(file_cap("free"), 25 * 1024 * 1024);
        assert_eq!(file_cap("units"), 100 * 1024 * 1024);
        let big = vec![b'a'; file_cap("free") + 1];
        assert_eq!(classify("big.txt", "", &big, file_cap("free")).unwrap_err().0, 413);
        assert!(classify("big.txt", "", &big, file_cap("units")).is_ok());
    }

    #[test]
    fn only_images_and_videos_show_inline_and_files_download_by_name() {
        assert_eq!(served_as(&json!({"type": "image/png"})), ("image/png".into(), "inline".into()), "an older upload");
        assert_eq!(served_as(&json!({"kind": "video", "type": "video/mp4"})), ("video/mp4".into(), "inline".into()));
        let (mime, disposition) = served_as(&json!({"kind": "file", "type": "application/octet-stream", "name": "Q3 \"plan\" ü.pdf"}));
        assert_eq!(mime, "application/octet-stream");
        assert_eq!(disposition, "attachment; filename=\"Q3 plan _.pdf\"; filename*=UTF-8''Q3%20plan%20%C3%BC.pdf");
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name(".env"), "env");
    }

    #[test]
    fn an_images_size_is_read_from_its_header() {
        let mut png = PNG.to_vec();
        png.resize(24, 0);
        png[16..20].copy_from_slice(&640u32.to_be_bytes());
        png[20..24].copy_from_slice(&480u32.to_be_bytes());
        assert_eq!(image_size(&png), Some((640, 480)));
        assert_eq!(image_size(b"GIF89a\x20\x03\x58\x02"), Some((800, 600)));
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0xE0, 0x02, 0x80, 0x03];
        assert_eq!(image_size(&jpeg), Some((640, 480)));
        assert_eq!(image_size(b"nothing"), None);
    }

    #[test]
    fn a_channel_file_lasts_the_history_window_and_an_unposted_one_a_day() {
        let (db, mem) = (store(), Mem::default());
        let owner = FileFor { workspace: "ws1", channel: "c1", by: "kim@x.dev", by_uid: "u-kim" };
        let (status, saved) = save_file(&db, &mem, &owner, "shot.png", "image/png", &PNG, CAP, "https://mcp.example.com");
        assert_eq!(status, 200, "{saved}");
        let id = saved["id"].as_str().unwrap().to_string();
        assert_eq!(saved["kind"], json!("image"));
        assert_eq!(saved["url"], json!(format!("https://mcp.example.com/u/{id}.png")));
        let file = format!("{id}.png");
        assert!(lookup(&db, &file, now() + 3600.0).is_some());
        assert!(lookup(&db, &file, now() + UNPOSTED_TTL_S + 60.0).is_none(), "never posted: a day");
        let mut record = db.kv_get(KV_BUCKET, &id).unwrap();
        record["attached"] = json!("m1");
        db.kv_put(KV_BUCKET, &id, &record, now()).unwrap();
        // posted: the free plan's 30 days of history
        assert!(lookup(&db, &file, now() + 29.0 * 86_400.0).is_some());
        let later = now() + 31.0 * 86_400.0;
        assert!(lookup(&db, &file, later).is_none());
        assert_eq!(sweep(&db, &mem, later), 1);
        assert!(mem.0.borrow().is_empty() && db.kv_get(KV_BUCKET, &id).is_none());
        let (status, refused) = save_file(&db, &mem, &owner, "x.svg", "image/svg+xml", b"<svg/>", CAP, "");
        assert_eq!((status, refused["ok"].clone()), (415, json!(false)));
        assert!(mem.0.borrow().is_empty(), "nothing stored for a refusal");
    }

    #[test]
    fn a_refused_upload_stores_nothing() {
        let (db, mem) = (store(), Mem::default());
        let (status, body) = save(&db, &mem, "ws1", "kim@x.dev", "image/gif", &PNG, "free", "");
        assert_eq!((status, body["ok"].clone()), (400, json!(false)));
        assert!(mem.0.borrow().is_empty() && db.kv_list(KV_BUCKET, "").is_empty());
    }
}
