//! The native binary keeps itself current.
//!
//! The binary is per machine and gitignored, so no `git pull` ever updates
//! it. It used to wait for the committed Python script to outrank it
//! (`supersede`) and then for Python to fetch its replacement — which made
//! Python a requirement just to stay current. Now the binary asks the server
//! itself: CI publishes every hook version as a GitHub release, the server
//! mirrors those builds at `/artifacts/hooks`, and a binary older than the
//! server's version fetches its own platform's build, in a detached process
//! so no session waits.
//!
//! Nothing is trusted on the way in: the index must carry CI's Ed25519
//! signature over "collide-hook <target> v<N> sha256:<digest>", checked
//! against the public key compiled into THIS binary — so the server, or
//! anyone who takes it over, cannot hand out a build CI did not sign. The
//! download must then match that digest, and the new file must run and
//! report that version, before it replaces anything. The swap is a rename (on Windows,
//! where a running exe can't be overwritten, the old one is renamed aside
//! first). Any doubt leaves the working binary exactly as it was, and the
//! next attempt waits a few hours.
//!
//! Only the managed install updates itself — the copy under `.cork/bin/`
//! (or the `.collide/bin/` of an install from before the rename, and a
//! repo's old per-repo hook there) — never a copy someone put on PATH by
//! hand. The free program is published and signed as `collide` whatever it
//! is called on the machine (`cork`, or an old install's `collide`). The
//! file replaced is the real one behind any link (`collide` is a link to
//! `cork` on macOS and Linux), and a twin that is a copy (Windows) is
//! brought along.
//!
//! The version replaced is kept beside it (`cork.prev`). `cork rollback`
//! puts it back when a release misbehaves, and updates then wait for a
//! version newer than the one rolled back from.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::{self, Env};
use crate::report::HOOK_VERSION;

/// How long after an attempt the next one may run. A machine at work asks
/// at its session starts and prompts, so a release reaches it within the
/// hour (v97 waited six); a server without a build for this platform is
/// asked again an hour later, not on every prompt.
const RETRY_S: u64 = 3600;
const INDEX_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// CI's release-signing key, public half. The private half exists only as a
/// GitHub Actions secret.
pub const SIGNING_KEY: &str = "17092e6d5632783f1d9b08fc134b0343c7e67b4a4b42feb52df6ca295f325ef8";

/// What CI signs for one build — artifacts.rs::signed_statement's twin.
pub fn statement(target: &str, version: u32, sha256: &str) -> String {
    statement_for("collide-hook", target, version, sha256)
}

/// The same for any of the programs a release carries (`collide-hook`, the
/// free version's `collide`).
pub fn statement_for(program: &str, target: &str, version: u32, sha256: &str) -> String {
    format!("{program} {target} v{version} sha256:{sha256}")
}

/// Whether `sig_b64` is CI's signature over this build's statement.
pub fn signed_by_ci(target: &str, version: u32, sha256: &str, sig_b64: &str) -> bool {
    signed_for("collide-hook", target, version, sha256, sig_b64)
}

/// Whether `sig_b64` is CI's signature over `program`'s statement.
pub fn signed_for(program: &str, target: &str, version: u32, sha256: &str, sig_b64: &str) -> bool {
    use base64::Engine;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let Ok(key_bytes) = (0..SIGNING_KEY.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&SIGNING_KEY[i..i + 2], 16))
        .collect::<Result<Vec<u8>, _>>() else { return false };
    let Ok(key_array) = <[u8; 32]>::try_from(key_bytes.as_slice()) else { return false };
    let Ok(key) = VerifyingKey::from_bytes(&key_array) else { return false };
    let Ok(sig_bytes) = base64::engine::general_purpose::STANDARD.decode(sig_b64.trim()) else { return false };
    let Ok(sig_array) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else { return false };
    key.verify(statement_for(program, target, version, sha256).as_bytes(), &Signature::from_bytes(&sig_array)).is_ok()
}

/// This build's release name, as CI publishes it.
pub fn target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

/// The managed install is the one under `.cork/bin/` or `.collide/bin/`.
fn managed(exe: &Path) -> bool {
    let parent = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    parent.file_name().is_some_and(|name| name == "bin")
        && parent.parent().and_then(Path::file_name).is_some_and(|name| name == ".cork" || name == ".collide")
}

fn stamp_path(env: &Env) -> PathBuf {
    config::data_dir(env).join("native").join("selfupdate.json")
}

/// After `cork rollback`: the version rolled back from, which no update
/// installs again (`{"version": N}`).
fn hold_path(env: &Env) -> PathBuf {
    config::data_dir(env).join("native").join("hold.json")
}

/// Where the version a swap replaced is kept: `cork.prev` beside `cork`.
fn prev_path(exe: &Path) -> PathBuf {
    let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("cork");
    let ext = if cfg!(windows) { ".exe" } else { "" };
    exe.with_file_name(format!("{stem}.prev{ext}"))
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Why this program may not update itself now, or None when it may: updates
/// not turned off (`CORK_NATIVE_INSTALL=0`, which the test suites set; or
/// the old `COLLIDE_NATIVE_INSTALL=0`), a platform with a build, the managed
/// binary, and, unless a person asked for it (`cork update`), no attempt in
/// the last hour.
fn held_off(exe: &Path, env: &Env, asked: bool) -> Option<String> {
    if config::get(env, "CORK_NATIVE_INSTALL").trim() == "0" {
        return Some("updates are turned off on this machine (CORK_NATIVE_INSTALL=0)".into());
    }
    if target().is_none() {
        return Some("there is no build of cork for this platform".into());
    }
    if !managed(exe) {
        return Some(format!("{} is not the cork Cork installed (in ~/.cork/bin), so it does not update itself", exe.display()));
    }
    let last = config::load_json(&stamp_path(env)).get("ts").and_then(Value::as_u64).unwrap_or(0);
    (!asked && now_s().saturating_sub(last) < RETRY_S).then(|| "an update was tried within the hour".into())
}

/// Whether an attempt may start now, by itself.
fn due(exe: &Path, env: &Env) -> bool {
    held_off(exe, env, false).is_none()
}

/// Start an update in the background when one may be due; returns at once.
/// Called at session start (`check-credentials`) and whenever this binary
/// hands a hook to a newer committed script — the moment it knows it is
/// behind.
pub fn spawn_if_due(server: &str, env: &Env) {
    let Ok(exe) = std::env::current_exe() else { return };
    if server.is_empty() || !due(&exe, env) {
        return;
    }
    // stamped before spawning: two hooks firing together start one update
    crate::report::save_json(&stamp_path(env), &json!({"ts": now_s(), "from": HOOK_VERSION}));
    let mut command = std::process::Command::new(&exe);
    command
        .arg("self-update")
        .arg(server)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// The machine just signed in to another server (staging, or back to
/// Cork's): what that server offers is news, so the wait set by the last
/// server's answer no longer applies. Clear it and check the new one now,
/// instead of up to six hours later.
pub fn server_changed(server: &str, env: &Env) {
    let _ = std::fs::remove_file(stamp_path(env));
    spawn_if_due(server, env);
}

/// The server said this machine's hooks are behind: update now, not at the
/// next session start hours later. At most one attempt every ten minutes, so
/// a burst of edits starts one update. Returns whether this binary updates
/// itself (the machine install): when it does, nobody needs to be told.
pub fn update_now(server: &str, env: &Env) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    if held_off(&exe, env, true).is_some() {
        return false;
    }
    let last = config::load_json(&stamp_path(env)).get("ts").and_then(Value::as_u64).unwrap_or(0);
    if !server.is_empty() && now_s().saturating_sub(last) >= 600 {
        crate::report::save_json(&stamp_path(env), &json!({"ts": now_s(), "from": HOOK_VERSION}));
        let mut command = std::process::Command::new(&exe);
        command
            .arg("self-update")
            .arg(server)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let _ = command.spawn();
    }
    true
}

/// The version a binary reports (`collide-hook X (hook artifacts vN)`).
fn reported_version(exe: &Path) -> u32 {
    let Ok(output) = std::process::Command::new(exe).arg("--version").output() else { return 0 };
    let text = String::from_utf8_lossy(&output.stdout);
    text.split("hook artifacts v")
        .nth(1)
        .map(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>())
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

/// `self-update <server>`: fetch, verify, swap. Always exits 0 — this runs
/// detached and nothing is listening for its failure.
pub fn run(server: &str, env: &Env) -> i32 {
    let _ = update(server.trim_end_matches('/'), env, false);
    0
}

/// `cork update`: check for a new version now and put it in place, past
/// the background updater's waits (the hourly one, the ten minutes after a
/// server's notice, and the hold after `cork rollback`). One line says what
/// happened.
pub fn update_command(env: &Env) -> i32 {
    let exe = match std::env::current_exe().and_then(std::fs::canonicalize) {
        Ok(exe) => exe,
        Err(problem) => {
            println!("cork update: could not find this program ({problem}).");
            return 1;
        }
    };
    if let Some(why) = held_off(&exe, env, true) {
        println!("cork update: {why}.");
        return 1;
    }
    let server = crate::machine::update_server(env);
    // the background updater waits its hour from here, as from any attempt
    crate::report::save_json(&stamp_path(env), &json!({"ts": now_s(), "from": HOOK_VERSION}));
    match update(&server, env, true) {
        Ok(Outcome::Updated(to)) => {
            // asked for by hand: the hold a rollback left is over
            let _ = std::fs::remove_file(hold_path(env));
            println!("Updated cork v{HOOK_VERSION} → v{to}. Restart open agent sessions to use it.");
            0
        }
        Ok(_) => {
            println!("cork is up to date (v{HOOK_VERSION})");
            0
        }
        Err(why) => {
            println!("cork update: {why}.");
            1
        }
    }
}

/// What an update did.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Nothing newer than this program (or the server is behind it).
    Current,
    /// A newer release, but the one `cork rollback` left: not taken.
    Held(u32),
    /// The new version is in place.
    Updated(u32),
}

/// What to do with the version the server offers: nothing newer, the
/// release rolled back from (taken only when a person asks), or take it.
fn decide(offered: u32, running: u32, held: u32, asked: bool) -> Outcome {
    if offered <= running {
        Outcome::Current
    } else if offered <= held && !asked {
        Outcome::Held(offered)
    } else {
        Outcome::Updated(offered)
    }
}

fn update(server: &str, env: &Env, asked: bool) -> Result<Outcome, String> {
    // the file itself, not the `collide` link an old hook entry ran it by:
    // a rename onto the link would make it a second copy
    let exe = std::env::current_exe().and_then(std::fs::canonicalize).map_err(|e| e.to_string())?;
    let target = target().ok_or("no build for this platform")?;
    // a previous Windows swap left the old binary aside; it is not running now
    let _ = std::fs::remove_file(exe.with_extension("old"));

    let agent = ureq::AgentBuilder::new().timeout(INDEX_TIMEOUT).build();
    crate::http::audit_raw("GET", &format!("{server}/artifacts/hooks"), "");
    let index: Value = serde_json::from_str(
        &agent
            .get(&format!("{server}/artifacts/hooks"))
            .set("User-Agent", &format!("collide-hook/{HOOK_VERSION}"))
            .call()
            .map_err(|e| format!("could not reach {server} ({e})"))?
            .into_string()
            .map_err(|e| format!("could not read {server}'s answer ({e})"))?,
    )
    .map_err(|_| format!("{server} did not answer with Cork's release list"))?;
    let offered = index.get("version").and_then(Value::as_u64).unwrap_or(0) as u32;
    // current, or the server behind this binary; rolled back from: only a
    // later release is taken, unless a person asks for it
    let held = config::load_json(&hold_path(env)).get("version").and_then(Value::as_u64).unwrap_or(0) as u32;
    match decide(offered, HOOK_VERSION, held, asked) {
        Outcome::Updated(_) => {}
        not_now => return Ok(not_now),
    }
    // the free version's one program updates from its own builds, published
    // and signed as `collide` under either of its names on this machine
    let free = exe.file_stem().and_then(|s| s.to_str()).is_some_and(crate::machine::is_program_name);
    let program = if free { "collide" } else { "collide-hook" };
    let list = if program == "collide" { "free_builds" } else { "builds" };
    let build = index
        .get(list)
        .and_then(Value::as_array)
        .and_then(|builds| builds.iter().find(|b| b.get("target").and_then(Value::as_str) == Some(target)))
        .ok_or_else(|| format!("the server has no build of v{offered} for this platform yet"))?;
    let want = build.get("sha256").and_then(Value::as_str).unwrap_or("").to_lowercase();
    if want.len() != 64 {
        return Err("the server named no checksum for the build".into());
    }
    // the server says what the digest is; only CI's key says it is ours
    let sig = build.get("sig").and_then(Value::as_str).unwrap_or("");
    if !signed_for(program, target, offered, &want, sig) {
        return Err("the build is not signed by Cork's release key".into());
    }

    let download = build.get("url").and_then(Value::as_str).map(str::to_string)
        .unwrap_or_else(|| format!("{server}/artifacts/hooks/{target}"));
    crate::http::audit_raw("GET", &download, "");
    let response = ureq::AgentBuilder::new()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .get(&download)
        .set("User-Agent", &format!("collide-hook/{HOOK_VERSION}"))
        .call()
        .map_err(|e| format!("the download failed ({e})"))?;
    let mut bytes = Vec::new();
    response.into_reader().take(MAX_BYTES).read_to_end(&mut bytes).map_err(|e| format!("the download failed ({e})"))?;
    let got = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>();
    if got != want {
        return Err("the download did not match its checksum, so nothing changed".into());
    }

    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("collide-hook");
    let part = exe.with_file_name(format!("{name}.part"));
    std::fs::write(&part, &bytes).map_err(|e| format!("could not write {} ({e})", part.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755));
    }
    if reported_version(&part) != offered {
        let _ = std::fs::remove_file(&part);
        return Err(format!("the download did not run as v{offered}, so nothing changed"));
    }
    keep_previous(&exe);
    swap(&part, &exe).map_err(|e| format!("could not put v{offered} in place ({e})"))?;
    if free {
        bring_twin_along(&exe);
    }
    Ok(Outcome::Updated(offered))
}

/// The version about to be replaced, kept as `cork.prev`: a second name for
/// the same file (the swap's rename then leaves it whole), else a copy.
fn keep_previous(exe: &Path) {
    let prev = prev_path(exe);
    let _ = std::fs::remove_file(&prev);
    if std::fs::hard_link(exe, &prev).is_err() {
        let _ = std::fs::copy(exe, &prev);
    }
}

/// After the free program is swapped: the other of `cork` and `collide` in
/// the same folder becomes the same build, so `cork message` and the old
/// install's hook entries (`~/.collide/bin/collide`) never run two versions.
/// A twin that is a link to it already is; an old install that has no
/// `cork` yet gets one. Copied with the install's own staged rename (a
/// running twin on Windows is renamed aside).
fn bring_twin_along(exe: &Path) {
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let legacy = exe.parent().and_then(Path::parent).and_then(Path::file_name).is_some_and(|n| n == ".collide");
    for twin in [crate::machine::PROGRAM, crate::machine::OLD_PROGRAM] {
        if twin == stem {
            continue;
        }
        let path = exe.with_file_name(format!("{twin}{ext}"));
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        if path.exists() || (legacy && twin == crate::machine::PROGRAM) {
            let _ = crate::machine::copy_executable(exe, &path);
        }
    }
}

/// `cork rollback`: the version the last update replaced goes back in
/// place, and the one it replaced is kept as `cork.prev`, so running it
/// again switches back. A program from before rollback existed has no such
/// command, so the kept one can do it too: `cork.prev rollback` swaps the
/// same two files. Going back, updates wait for a release newer than the
/// version left (and, for an older program that does not read that, the
/// next attempt is a week away); going forward again, they resume.
pub fn rollback(env: &Env) -> i32 {
    let running = match std::env::current_exe().and_then(std::fs::canonicalize) {
        Ok(path) => path,
        Err(problem) => {
            println!("Cork rollback: could not find this program ({problem}).");
            return 1;
        }
    };
    let ext = if cfg!(windows) { ".exe" } else { "" };
    // `cork.prev` (Windows: `cork.prev.exe`), whose file stem is `cork`
    let name = running.file_name().and_then(|n| n.to_str()).unwrap_or("cork");
    let (exe, prev) = match name.strip_suffix(ext).unwrap_or(name).strip_suffix(".prev") {
        Some(main) => (running.with_file_name(format!("{main}{ext}")), running.clone()),
        None => (running.clone(), prev_path(&running)),
    };
    let (now, was) = (reported_version(&exe), reported_version(&prev));
    if !prev.is_file() || was == 0 {
        println!("Cork rollback: no earlier version is kept beside {} (one is kept from the next update on).", exe.display());
        return 1;
    }
    let staged = exe.with_file_name(format!("{}.rolling", exe.file_name().and_then(|n| n.to_str()).unwrap_or("cork")));
    let _ = std::fs::remove_file(&staged);
    if let Err(problem) = std::fs::rename(&exe, &staged).and_then(|_| std::fs::rename(&prev, &exe)) {
        let _ = std::fs::rename(&staged, &exe);
        println!("Cork rollback: nothing changed ({problem}).");
        return 1;
    }
    let _ = std::fs::rename(&staged, &prev);
    if exe.file_stem().and_then(|s| s.to_str()).is_some_and(crate::machine::is_program_name) {
        bring_twin_along(&exe);
    }
    if was < now {
        crate::report::save_json(&hold_path(env), &json!({"version": now}));
        crate::report::save_json(&stamp_path(env), &json!({"ts": now_s() + 7 * 24 * 3600, "from": now}));
        println!("Cork: back on v{was}. v{now} is kept beside it; updates wait for a release newer than v{now}.");
    } else {
        let _ = std::fs::remove_file(hold_path(env));
        let _ = std::fs::remove_file(stamp_path(env));
        println!("Cork: on v{was} again, and updates are on.");
    }
    // the program now in place may predate this command; the kept one has it then
    let again = if was >= ROLLBACK_SINCE { "cork rollback".to_string() } else { format!("{} rollback", prev.display()) };
    println!("To switch back: {}. Restart open agent sessions to use it.", crate::machine::Paint::new(env).command(&again));
    0
}

/// The first version with `rollback`.
const ROLLBACK_SINCE: u32 = 95;

/// Put `part` in place of `exe`. A running binary on Windows can only be
/// renamed aside, not replaced.
fn swap(part: &Path, exe: &Path) -> Result<(), String> {
    if cfg!(windows) {
        let old = exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).map_err(|e| e.to_string())?;
        if let Err(problem) = std::fs::rename(part, exe) {
            let _ = std::fs::rename(&old, exe); // put the working one back
            return Err(problem.to_string());
        }
    } else {
        std::fs::rename(part, exe).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_in_to_another_server_lifts_the_wait() {
        let home = std::env::temp_dir().join(format!("collide-selfupdate-{}", std::process::id()));
        let env: Env = [("HOME".to_string(), home.to_string_lossy().to_string()),
                        ("CORK_NATIVE_INSTALL".to_string(), "0".to_string())].into_iter().collect();
        let stamp = stamp_path(&env);
        std::fs::create_dir_all(stamp.parent().unwrap()).unwrap();
        std::fs::write(&stamp, r#"{"ts": 9999999999, "from": 69}"#).unwrap();
        server_changed("https://staging.example", &env);
        assert!(!stamp.exists(), "the last server's wait is gone");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn only_the_managed_install_updates_itself() {
        assert!(managed(Path::new("/work/repo/.collide/bin/collide-hook")));
        assert!(managed(Path::new("C:/work/repo/.collide/bin/collide-hook.exe")));
        assert!(!managed(Path::new("/usr/local/bin/collide-hook")));
        assert!(!managed(Path::new("/work/repo/bin/collide-hook")));
    }

    #[test]
    fn only_a_signature_from_the_release_key_is_accepted() {
        let sha = "ab".repeat(32);
        // no signature, garbage, and a well-formed signature by another key
        assert!(!signed_by_ci("aarch64-apple-darwin", 34, &sha, ""));
        assert!(!signed_by_ci("aarch64-apple-darwin", 34, &sha, "not base64 !!"));
        use base64::Engine;
        use ed25519_dalek::{Signer, SigningKey};
        let stranger = SigningKey::from_bytes(&[7u8; 32]);
        let forged = stranger.sign(statement("aarch64-apple-darwin", 34, &sha).as_bytes());
        let forged = base64::engine::general_purpose::STANDARD.encode(forged.to_bytes());
        assert!(!signed_by_ci("aarch64-apple-darwin", 34, &sha, &forged));
        // a signature the release key made (with openssl, exactly as CI
        // signs): accepted for its own statement, refused for any other
        let real = "gzGFIfJdBaI+qk09bctIgv8zlvicRnvtlzX0LoVkpmL8ft4ykBFcjXNk0sfVefuMokx9+mdtikPBfpasjQz+DQ==";
        assert!(signed_by_ci("aarch64-apple-darwin", 34, &sha, real));
        assert!(!signed_by_ci("x86_64-apple-darwin", 34, &sha, real));
        assert!(!signed_by_ci("aarch64-apple-darwin", 35, &sha, real));
        assert!(!signed_by_ci("aarch64-apple-darwin", 34, &"cd".repeat(32), real));
    }

    #[test]
    fn the_replaced_version_is_kept_and_a_linked_twin_is_left_a_link() {
        let dir = std::env::temp_dir().join(format!("cork-prev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(if cfg!(windows) { "cork.exe" } else { "cork" });
        std::fs::write(&exe, "v94").unwrap();
        keep_previous(&exe);
        std::fs::write(exe.with_extension("part"), "v95").unwrap();
        swap(&exe.with_extension("part"), &exe).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v95");
        assert_eq!(std::fs::read_to_string(prev_path(&exe)).unwrap(), "v94", "the version replaced is kept whole");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("cork", dir.join("collide")).unwrap();
            bring_twin_along(&exe);
            assert_eq!(std::fs::read_link(dir.join("collide")).unwrap(), Path::new("cork"), "not made a copy");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cork_update_takes_what_the_waits_and_the_hold_would_not() {
        // nothing newer, asked or not
        assert_eq!(decide(100, 100, 0, true), Outcome::Current);
        assert_eq!(decide(99, 100, 0, true), Outcome::Current, "a server behind this program");
        // newer: taken
        assert_eq!(decide(101, 100, 0, false), Outcome::Updated(101));
        // the release a rollback left: held by itself, taken when asked
        assert_eq!(decide(101, 100, 101, false), Outcome::Held(101));
        assert_eq!(decide(101, 100, 101, true), Outcome::Updated(101));
        assert_eq!(decide(102, 100, 101, false), Outcome::Updated(102), "a later release ends the hold");

        let home = std::env::temp_dir().join(format!("cork-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let env: Env = [("HOME".to_string(), home.to_string_lossy().to_string())].into_iter().collect();
        let exe = Path::new("/u/.cork/bin/cork");
        // an attempt a minute ago holds the background updater off, not a person
        crate::report::save_json(&stamp_path(&env), &json!({"ts": now_s() - 60}));
        assert!(held_off(exe, &env, false).is_some());
        assert_eq!(held_off(exe, &env, true), None);
        // turned off, or a copy that is not the managed install: never, and said why
        let mut off = env.clone();
        off.insert("CORK_NATIVE_INSTALL".into(), "0".into());
        assert!(held_off(exe, &off, true).unwrap().contains("CORK_NATIVE_INSTALL=0"));
        assert!(held_off(Path::new("/usr/local/bin/cork"), &env, true).unwrap().contains("/usr/local/bin/cork"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn this_build_knows_its_release_name() {
        // every platform CI builds maps to its release; the test host is one
        assert!(target().is_some());
    }

    #[test]
    fn turned_off_or_unmanaged_never_runs() {
        let mut env = Env::new();
        env.insert("CORK_NATIVE_INSTALL".into(), "0".into());
        assert!(!due(Path::new("/r/.collide/bin/collide-hook"), &env));
        let home = std::env::temp_dir().join("collide-selfupdate-test");
        let _ = std::fs::remove_dir_all(&home);
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().into_owned());
        assert!(!due(Path::new("/usr/local/bin/collide-hook"), &env));
        assert!(due(Path::new("/r/.collide/bin/collide-hook"), &env));
        // a recent attempt holds the next one off
        crate::report::save_json(&stamp_path(&env), &json!({"ts": now_s()}));
        assert!(!due(Path::new("/r/.collide/bin/collide-hook"), &env));
    }
}
