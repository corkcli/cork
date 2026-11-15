//! Back to where the person was. An agent started in a repo none of their
//! workspaces has, and the browser opened on the question of where it goes
//! (report.rs). Once they answer it, the app the agent runs in (VS Code,
//! Terminal, iTerm, Cursor…) comes back to the front, so adding a repo is
//! one click and never a hunt for the window they left. A detached watcher
//! asks the server whether the repo still waits for them and, on a Mac,
//! brings that app back with `open -b` once it stops waiting (added, or put
//! off with Not now). It reads only the person's own waiting repos, with
//! this machine's credential; elsewhere nothing is switched.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::{self, Env};

const TIMEOUT: Duration = Duration::from_secs(10);
/// Every two seconds while an answer is likely, then every ten.
const QUICK: Duration = Duration::from_secs(2);
const QUICK_FOR: Duration = Duration::from_secs(120);
const SLOW: Duration = Duration::from_secs(10);
const GIVE_UP: Duration = Duration::from_secs(20 * 60);

/// The app the harness runs in, as LaunchServices knows it.
#[derive(Clone, Debug, PartialEq)]
pub struct HostApp {
    pub bundle: String,
    /// what the page calls it: "Visual Studio Code", "Terminal"
    pub name: String,
}

/// "Visual Studio Code" from `/Applications/Visual Studio Code.app/Contents/MacOS/Code`:
/// the outermost app bundle on the program's path.
fn app_name(program: &str) -> String {
    program.split('/').find_map(|part| part.strip_suffix(".app")).unwrap_or("").to_string()
}

/// lsappinfo's `"CFBundleIdentifier"="com.microsoft.VSCode"`: the id, or ""
/// for a process it knows no app for (`[ NULL ]`).
fn bundle_of(answer: &str) -> String {
    answer
        .split_once("\"=\"")
        .map(|(_, id)| id.trim().trim_end_matches('"').to_string())
        .unwrap_or_default()
}

/// The app this hook runs under: the nearest ancestor process LaunchServices
/// knows as an app (the agent's shell and helpers are not). None off a Mac,
/// or when the chain runs out first (a tmux server, a remote shell).
pub fn host_app() -> Option<HostApp> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let mut pid = std::process::id();
    for _ in 0..16 {
        let (up, program) = crate::listen::ancestor(pid)?;
        if up <= 1 {
            return None;
        }
        let out = Command::new("/usr/bin/lsappinfo").args(["info", "-only", "bundleid", &up.to_string()]).output().ok()?;
        let bundle = bundle_of(&String::from_utf8_lossy(&out.stdout));
        if !bundle.is_empty() {
            let name = app_name(&program);
            return Some(HostApp { name: if name.is_empty() { bundle.clone() } else { name }, bundle });
        }
        pid = up;
    }
    None
}

/// Start the watcher that brings `app` back once `repo_id` stops waiting;
/// false when it could not start or `CORK_RETURN=0`.
pub fn spawn_watch(root: &Path, repo_id: &str, app: &HostApp, env: &Env) -> bool {
    if config::get(env, "CORK_RETURN") == "0" {
        return false;
    }
    let Ok(me) = std::env::current_exe() else { return false };
    let mut command = Command::new(me);
    command
        .arg("add-repo-wait")
        .arg(repo_id)
        .arg(&app.bundle)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().is_ok()
}

/// Whether `repo_id` is among the person's waiting repos in a
/// `/api/pending-repos` answer.
fn is_waiting(answer: &Value, repo_id: &str) -> Option<bool> {
    let key = |id: &str| id.trim().to_ascii_lowercase();
    let repos = answer.get("repos")?.as_array()?;
    Some(repos.iter().any(|r| r.get("repo_id").and_then(Value::as_str).is_some_and(|id| key(id) == key(repo_id))))
}

/// `cork add-repo-wait <repo_id> <bundle id>`, run detached in the repo's
/// folder: once the repo, seen waiting, waits no more, the app comes back.
/// A repo never seen waiting switches nothing, and after twenty minutes the
/// watcher gives up.
pub fn command(args: &[String], env: &Env) -> i32 {
    let (Some(repo_id), Some(bundle)) = (args.first(), args.get(1)) else { return 0 };
    let root = std::env::current_dir().ok();
    let cfg = config::config(root.as_deref(), env);
    if cfg.server.is_empty() || cfg.token.is_empty() || bundle.is_empty() {
        return 0;
    }
    let started = Instant::now();
    let mut seen = false;
    while started.elapsed() < GIVE_UP {
        let answer = crate::http::get(&cfg.server, "/api/pending-repos", &cfg.token, &crate::report::user_agent(), TIMEOUT);
        match answer.ok().and_then(|a| is_waiting(&a, repo_id)) {
            Some(true) => seen = true,
            Some(false) if seen => {
                let _ = Command::new("/usr/bin/open")
                    .args(["-b", bundle])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                return 0;
            }
            _ => {}
        }
        std::thread::sleep(if started.elapsed() < QUICK_FOR { QUICK } else { SLOW });
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_app_is_named_by_its_bundle_on_the_path() {
        assert_eq!(app_name("/Applications/Visual Studio Code.app/Contents/MacOS/Code"), "Visual Studio Code");
        assert_eq!(app_name("/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"), "Terminal");
        // a helper inside an app is named by the outer one
        assert_eq!(app_name("/Applications/Cursor.app/Contents/Frameworks/Cursor Helper.app/Contents/MacOS/Cursor Helper"), "Cursor");
        assert_eq!(app_name("/bin/zsh"), "");
    }

    #[test]
    fn lsappinfo_names_the_bundle_or_nothing() {
        assert_eq!(bundle_of("\"CFBundleIdentifier\"=\"com.microsoft.VSCode\"\n"), "com.microsoft.VSCode");
        assert_eq!(bundle_of("\"CFBundleIdentifier\"=[ NULL ] \n"), "");
        assert_eq!(bundle_of(""), "");
    }

    #[test]
    fn a_repo_waits_until_it_leaves_the_pending_list() {
        let waiting = json!({"repos": [{"repo_id": "github.com/Lithometric/juice"}, {"repo_id": "github.com/acme/x"}]});
        assert_eq!(is_waiting(&waiting, "github.com/lithometric/juice"), Some(true));
        assert_eq!(is_waiting(&json!({"repos": []}), "github.com/lithometric/juice"), Some(false));
        // an answer that is not the list says nothing either way
        assert_eq!(is_waiting(&json!({"error": "unauthorized"}), "github.com/lithometric/juice"), None);
    }
}
