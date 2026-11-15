//! Repo icons people upload from the dashboard's settings. Without one, the
//! dashboard draws Cork's default repo icon, a glyph of its own (nothing
//! is guessed from the repo's name or fetched from another site); an upload
//! replaces it for the whole workspace.
//!
//! The image lives in object storage under `private/repo-icons/`, a folder
//! of the private bucket nothing serves publicly: the dashboard reads it
//! back through `GET /api/workspaces/:id/repo-icons`, as data URLs, behind
//! the same sign-in as everything else. The kv bucket `repo_icon` maps
//! `{workspace}:{repo}` to the object's key, which is named by content, so
//! a replaced icon is a new key and the old object is deleted.
//!
//! Only raster images: an SVG can carry script, and these are shown to the
//! whole team. The dashboard shrinks an upload to 128px before sending it,
//! so `MAX_BYTES` is a ceiling, not a working size.
//!
//! A workspace's own icon (its mark in the switcher, in place of its
//! initial) is kept the same way: the kv bucket `workspace_icon` maps the
//! workspace id to its object, under `private/workspace-icons/`, read back
//! through `GET /api/workspaces/:id/icon`. The workspace list carries its
//! version (`icon_v`), so the dashboard asks for an icon only when there is
//! one and only again when it changes.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::store::{now, Store};

pub const KV_BUCKET: &str = "repo_icon";
/// The private folder, inside the bucket's own prefix.
pub const FOLDER: &str = "private/repo-icons/";
pub const MAX_BYTES: usize = 256 * 1024;
pub const WORKSPACE_KV_BUCKET: &str = "workspace_icon";
pub const WORKSPACE_FOLDER: &str = "private/workspace-icons/";
const TYPES: [(&str, &str); 4] = [("image/png", "png"), ("image/jpeg", "jpg"), ("image/webp", "webp"), ("image/gif", "gif")];

/// Where the bytes go: R2 in production, a map in tests.
pub trait Objects {
    fn put(&self, key: &str, body: &[u8]) -> Result<(), String>;
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    fn delete(&self, key: &str) -> Result<(), String>;
}

impl Objects for crate::s3::Bucket {
    fn put(&self, key: &str, body: &[u8]) -> Result<(), String> {
        crate::s3::Bucket::put(self, key, body)
    }
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        crate::s3::Bucket::get(self, key)
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        crate::s3::Bucket::delete(self, key)
    }
}

/// Objects are immutable (a key names its content), so what was read once
/// is kept: a sidebar's icons cost one bucket read each per server life.
static CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn cached(key: &str) -> Option<String> {
    CACHE.lock().ok()?.as_ref()?.get(key).cloned()
}

fn remember(key: &str, data_url: &str) {
    if let Ok(mut cache) = CACHE.lock() {
        let map = cache.get_or_insert_with(HashMap::new);
        // a small server-wide cap: icons are a few KB, a sidebar a handful
        if map.len() > 2000 {
            map.clear();
        }
        map.insert(key.to_string(), data_url.to_string());
    }
}

fn kv_key(workspace: &str, repo: &str) -> String {
    format!("{workspace}:{repo}")
}

/// `data:image/png;base64,…` -> (type, bytes), refusing anything that is not
/// a small raster image, by its declared type AND its first bytes.
pub fn parse_image(data_url: &str) -> Result<(&'static str, Vec<u8>), String> {
    let rest = data_url.trim().strip_prefix("data:").ok_or("send the image as a data: URL")?;
    let (head, payload) = rest.split_once(',').ok_or("not a data: URL")?;
    let declared = head.strip_suffix(";base64").ok_or("the image must be base64")?.to_ascii_lowercase();
    let (mime, _) = TYPES.iter().find(|(m, _)| *m == declared).ok_or("use a PNG, JPEG, WebP or GIF")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(payload.trim()).map_err(|_| "the image is not valid base64")?;
    if bytes.is_empty() {
        return Err("the image is empty".into());
    }
    if bytes.len() > MAX_BYTES {
        return Err(format!("the image is over {} KB", MAX_BYTES / 1024));
    }
    let sniffed = match bytes.as_slice() {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        _ => return Err("that file is not an image".into()),
    };
    if sniffed != *mime {
        return Err("the file's contents don't match its type".into());
    }
    Ok((mime, bytes))
}

/// The object's key: the workspace's folder, then a hash of the repo and
/// the bytes, so a new icon never overwrites an old one in place.
pub fn object_key(workspace: &str, repo: &str, bytes: &[u8], mime: &str) -> String {
    content_key(FOLDER, workspace, repo, bytes, mime)
}

fn content_key(folder: &str, workspace: &str, salt: &str, bytes: &[u8], mime: &str) -> String {
    let ext = TYPES.iter().find(|(m, _)| *m == mime).map(|(_, e)| *e).unwrap_or("bin");
    let mut hash = Sha256::new();
    hash.update(salt.as_bytes());
    hash.update([0]);
    hash.update(bytes);
    let digest: String = hash.finalize().iter().take(12).map(|b| format!("{b:02x}")).collect();
    let ws: String = workspace.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    format!("{folder}{ws}/{digest}.{ext}")
}

/// A stored icon as a data URL, from the cache or the bucket; None when
/// its object cannot be read.
fn data_url_of(record: &Value, objects: Option<&dyn Objects>) -> Option<String> {
    let (object, mime) = (record.get("key").and_then(Value::as_str)?, record.get("type").and_then(Value::as_str)?);
    if let Some(url) = cached(object) {
        return Some(url);
    }
    match objects?.get(object) {
        Ok(Some(bytes)) => {
            let url = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes));
            remember(object, &url);
            Some(url)
        }
        _ => None,
    }
}

/// Store `data_url` as `repo`'s icon in `workspace`, replacing any before.
pub fn set(store: &Store, objects: &dyn Objects, workspace: &str, repo: &str, by: &str, data_url: &str) -> Value {
    let (mime, bytes) = match parse_image(data_url) {
        Ok(parsed) => parsed,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    let key = object_key(workspace, repo, &bytes, mime);
    if let Err(error) = objects.put(&key, &bytes) {
        return json!({"ok": false, "error": format!("could not store the icon: {error}")});
    }
    let old = store.kv_get(KV_BUCKET, &kv_key(workspace, repo));
    let ts = now();
    let _ = store.kv_put(KV_BUCKET, &kv_key(workspace, repo), &json!({"key": key, "type": mime, "by": by, "ts": ts}), ts);
    if let Some(previous) = old.as_ref().and_then(|r| r.get("key")).and_then(Value::as_str) {
        if previous != key {
            let _ = objects.delete(previous);
        }
    }
    let url = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes));
    remember(&key, &url);
    json!({"ok": true, "repo": repo, "icon": url})
}

/// Drop `repo`'s uploaded icon; the dashboard falls back to its default icon.
pub fn clear(store: &Store, objects: &dyn Objects, workspace: &str, repo: &str) -> Value {
    if let Some(record) = store.kv_get(KV_BUCKET, &kv_key(workspace, repo)) {
        if let Some(key) = record.get("key").and_then(Value::as_str) {
            let _ = objects.delete(key);
        }
        let _ = store.kv_delete(KV_BUCKET, &kv_key(workspace, repo));
    }
    json!({"ok": true, "repo": repo})
}

/// Every uploaded icon in `workspace`, repo -> data URL. One that cannot be
/// read is left out (the dashboard shows its default icon for it).
pub fn all(store: &Store, objects: Option<&dyn Objects>, workspace: &str) -> Value {
    let mut icons = Map::new();
    for (key, record) in store.kv_list(KV_BUCKET, &format!("{workspace}:")) {
        let Some(repo) = key.strip_prefix(&format!("{workspace}:")) else { continue };
        let Some(url) = data_url_of(&record, objects) else { continue };
        icons.insert(repo.to_string(), Value::String(url));
    }
    json!({"ok": true, "icons": icons})
}

// ------------------------------------------------------- the workspace's own

/// The version of `workspace`'s icon (its object's name, which changes with
/// its content), for the workspace list; None without one.
pub fn workspace_version(store: &Store, workspace: &str) -> Option<String> {
    let record = store.kv_get(WORKSPACE_KV_BUCKET, workspace)?;
    let key = record.get("key").and_then(Value::as_str)?;
    Some(key.rsplit('/').next().unwrap_or(key).split('.').next().unwrap_or(key).to_string())
}

/// Store `data_url` as `workspace`'s icon, replacing any before.
pub fn set_workspace(store: &Store, objects: &dyn Objects, workspace: &str, by: &str, data_url: &str) -> Value {
    let (mime, bytes) = match parse_image(data_url) {
        Ok(parsed) => parsed,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    let key = content_key(WORKSPACE_FOLDER, workspace, workspace, &bytes, mime);
    if let Err(error) = objects.put(&key, &bytes) {
        return json!({"ok": false, "error": format!("could not store the icon: {error}")});
    }
    let old = store.kv_get(WORKSPACE_KV_BUCKET, workspace);
    let ts = now();
    let _ = store.kv_put(WORKSPACE_KV_BUCKET, workspace, &json!({"key": key, "type": mime, "by": by, "ts": ts}), ts);
    if let Some(previous) = old.as_ref().and_then(|r| r.get("key")).and_then(Value::as_str) {
        if previous != key {
            let _ = objects.delete(previous);
        }
    }
    let url = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes));
    remember(&key, &url);
    json!({"ok": true, "icon": url, "icon_v": workspace_version(store, workspace)})
}

/// Drop `workspace`'s icon; the dashboard shows its initial again.
pub fn clear_workspace(store: &Store, objects: &dyn Objects, workspace: &str) -> Value {
    if let Some(record) = store.kv_get(WORKSPACE_KV_BUCKET, workspace) {
        if let Some(key) = record.get("key").and_then(Value::as_str) {
            let _ = objects.delete(key);
        }
        let _ = store.kv_delete(WORKSPACE_KV_BUCKET, workspace);
    }
    json!({"ok": true, "icon": null})
}

/// `workspace`'s icon as a data URL (null without one, or when its object
/// cannot be read), with its version.
pub fn workspace_icon(store: &Store, objects: Option<&dyn Objects>, workspace: &str) -> Value {
    let url = store.kv_get(WORKSPACE_KV_BUCKET, workspace).and_then(|record| data_url_of(&record, objects));
    json!({"ok": true, "icon": url, "icon_v": workspace_version(store, workspace)})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

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

    fn png(extra: u8) -> String {
        let bytes = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, extra];
        format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn only_small_raster_images_whose_bytes_match_their_type_are_taken() {
        assert!(parse_image(&png(1)).is_ok());
        assert!(parse_image("data:image/svg+xml;base64,PHN2Zz4=").is_err());
        assert!(parse_image("data:image/png;base64,aGVsbG8=").is_err(), "bytes that are not a PNG");
        assert!(parse_image("https://example.com/a.png").is_err());
        let big = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(vec![0x89; MAX_BYTES + 1]));
        assert!(parse_image(&big).is_err());
    }

    #[test]
    fn an_icon_lives_in_the_private_folder_and_a_new_one_replaces_the_old() {
        let (db, mem) = (store(), Mem::default());
        let first = set(&db, &mem, "ws1", "github.com/acme/api", "kim@x.dev", &png(1));
        assert_eq!(first["ok"], true);
        let keys: Vec<String> = mem.0.borrow().keys().cloned().collect();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].starts_with("private/repo-icons/ws1/"), "{keys:?}");

        set(&db, &mem, "ws1", "github.com/acme/api", "kim@x.dev", &png(2));
        assert_eq!(mem.0.borrow().len(), 1, "the old object is deleted");

        let listed = all(&db, Some(&mem), "ws1");
        assert!(listed["icons"]["github.com/acme/api"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert!(all(&db, Some(&mem), "ws2")["icons"].as_object().unwrap().is_empty(), "another workspace sees none");

        clear(&db, &mem, "ws1", "github.com/acme/api");
        assert!(mem.0.borrow().is_empty());
        assert!(all(&db, Some(&mem), "ws1")["icons"].as_object().unwrap().is_empty());
    }

    /// A workspace's own icon: in its private folder, apart from its repos'
    /// icons, with a version that changes with the picture; a new one
    /// replaces the old object; reset gives the initial back.
    #[test]
    fn a_workspace_icon_is_kept_apart_from_repo_icons_and_versioned_by_content() {
        let (db, mem) = (store(), Mem::default());
        assert_eq!(workspace_icon(&db, Some(&mem), "ws1")["icon"], Value::Null);
        assert_eq!(workspace_version(&db, "ws1"), None);
        assert_eq!(set_workspace(&db, &mem, "ws1", "kim@x.dev", "data:image/svg+xml;base64,PHN2Zz4=")["ok"], false, "no SVG");

        let first = set_workspace(&db, &mem, "ws1", "kim@x.dev", &png(1));
        assert_eq!(first["ok"], true, "{first}");
        let v1 = workspace_version(&db, "ws1").unwrap();
        assert_eq!(first["icon_v"], json!(v1));
        let keys: Vec<String> = mem.0.borrow().keys().cloned().collect();
        assert!(keys.len() == 1 && keys[0].starts_with("private/workspace-icons/ws1/"), "{keys:?}");
        set(&db, &mem, "ws1", "github.com/acme/api", "kim@x.dev", &png(1));
        assert!(all(&db, Some(&mem), "ws1")["icons"].as_object().unwrap().len() == 1, "the repo's icon, not the workspace's");

        set_workspace(&db, &mem, "ws1", "kim@x.dev", &png(2));
        let v2 = workspace_version(&db, "ws1").unwrap();
        assert_ne!(v1, v2, "a new picture is a new version");
        assert_eq!(mem.0.borrow().keys().filter(|k| k.starts_with(WORKSPACE_FOLDER)).count(), 1, "the old object is deleted");
        let got = workspace_icon(&db, Some(&mem), "ws1");
        assert!(got["icon"].as_str().unwrap().starts_with("data:image/png;base64,"), "{got}");
        assert_eq!(workspace_icon(&db, Some(&mem), "ws2")["icon"], Value::Null, "another workspace has none");

        clear_workspace(&db, &mem, "ws1");
        assert_eq!(workspace_icon(&db, Some(&mem), "ws1")["icon"], Value::Null);
        assert_eq!(mem.0.borrow().keys().filter(|k| k.starts_with(WORKSPACE_FOLDER)).count(), 0);
    }
}
