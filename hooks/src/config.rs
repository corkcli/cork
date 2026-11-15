//! Locating the repo and resolving the credential — shared by every hook.
//!
//! The machine's install names the server, the workspace and the repo (by its
//! git origin; machine.rs). A repo still on the retired per-repo setup carries
//! `.cork/config.json` (`.collide/config.json` from before the rename), which
//! is read first so it keeps reporting where it did until it migrates; its
//! non-setup keys (`verify`, `auto_pull`, `land`, `protocol`) stay the repo's
//! own choices either way.
//!
//! Every setting is read through [`get`]: `CORK_X`, else the `COLLIDE_X` a
//! machine set up before the rename still exports.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

pub type Env = BTreeMap<String, String>;

pub fn env_map() -> Env {
    std::env::vars().collect()
}

/// A setting from the environment, `""` when unset. Cork's own settings go
/// by `CORK_X`; a machine set up before the rename exports `COLLIDE_X`
/// (Railway, shell profiles, launchd plists, MCP entries), so either name
/// asks for both, the new one first.
pub fn get<'a>(env: &'a Env, key: &str) -> &'a str {
    let raw = |k: &str| env.get(k).map(String::as_str).filter(|v| !v.is_empty());
    match key.strip_prefix("CORK_").or_else(|| key.strip_prefix("COLLIDE_")) {
        Some(rest) => raw(&format!("CORK_{rest}")).or_else(|| raw(&format!("COLLIDE_{rest}"))).unwrap_or(""),
        None => raw(key).unwrap_or(""),
    }
}

/// [`get`] for code that reads the process environment directly.
pub fn var(key: &str) -> String {
    get(&env_map(), key).to_string()
}

/// Whether a command line sets this setting under either name
/// (`CORK_SHARED_TREE=0 git commit`, or the `COLLIDE_` one an agent learned
/// before the rename).
pub fn sets(command: &str, assignment: &str) -> bool {
    let rest = assignment.strip_prefix("CORK_").or_else(|| assignment.strip_prefix("COLLIDE_")).unwrap_or(assignment);
    command.contains(&format!("CORK_{rest}")) || command.contains(&format!("COLLIDE_{rest}"))
}

/// The machine's data folder under `home`: `.cork`, except that a machine
/// set up before the rename keeps its `.collide` for as long as no `.cork`
/// exists beside it. On macOS and Linux the first session of v95 moves it
/// (machine::move_to_cork) and leaves `.collide` as a link to `.cork`, which
/// then wins. The local server picks the same folder (cork-rs
/// `local::data_home`), and must: its id is a hash of the path.
pub fn data_dir_in(home: &Path) -> PathBuf {
    let cork = home.join(".cork");
    let collide = home.join(".collide");
    if !cork.exists() && collide.exists() { collide } else { cork }
}

/// `~/.cork` (or the `~/.collide` of an install from before the rename).
pub fn data_dir(env: &Env) -> PathBuf {
    data_dir_in(Path::new(&crate::check::home(env)))
}

/// Whether this machine still runs from its `~/.collide` folder.
pub fn legacy_layout(env: &Env) -> bool {
    data_dir(env).file_name().is_some_and(|n| n == ".collide")
}

/// A repo's own Cork config: `.cork/config.json`, else the
/// `.collide/config.json` it was given before the rename. The new name when
/// neither exists.
pub fn repo_config_path(root: &Path) -> PathBuf {
    let cork = root.join(".cork").join("config.json");
    let collide = root.join(".collide").join("config.json");
    if !cork.is_file() && collide.is_file() { collide } else { cork }
}

/// Whether a repo carries a config of its own, under either name.
pub fn has_repo_config(dir: &Path) -> bool {
    dir.join(".cork").join("config.json").is_file() || dir.join(".collide").join("config.json").is_file()
}

/// The repo's own config, `{}` when it has none.
pub fn repo_config(root: &Path) -> Value {
    load_json(&repo_config_path(root))
}

pub fn load_json(path: &Path) -> Value {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(value) if value.is_object() => value,
            _ => Value::Object(Default::default()),
        },
        Err(_) => Value::Object(Default::default()),
    }
}

fn str_field(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

/// `os.path.normpath`: collapse `.` and `..` lexically, never touching disk.
pub fn normpath(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

pub fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normpath(path)
    } else {
        normpath(&std::env::current_dir().unwrap_or_default().join(path))
    }
}

/// The nearest ancestor holding `.cork/config.json` (or `.collide/config.json`),
/// searched upward from each candidate in turn — what makes reporting survive
/// cwd drift.
///
/// Candidates are tried in order, and the NEAREST root above a candidate
/// wins: a repo set up for Cork, or (with Cork installed for the
/// machine) any git repo. An agent whose session started in repo A
/// (`CLAUDE_PROJECT_DIR`) and now works in repo B is rooted in B: looking
/// for a set-up repo above every candidate first rooted it in A, and the
/// session in B was briefed and told about A's work.
pub fn find_repo_root(candidates: &[Option<PathBuf>]) -> Option<PathBuf> {
    let machine = crate::machine::installed(&env_map());
    let mut seen: Vec<PathBuf> = Vec::new();
    for start in candidates.iter().flatten() {
        let mut dir = absolute(start);
        loop {
            if seen.contains(&dir) {
                break;
            }
            seen.push(dir.clone());
            if has_repo_config(&dir) {
                return Some(dir);
            }
            // no repo set up for Cork: with Cork installed for the
            // machine, every git repo is covered, rooted where its .git is
            // (a repo with nothing in it yet is a project folder, not a repo)
            if machine && dir.join(".git").exists() && !is_empty_repo(&dir) {
                return Some(dir);
            }
            match dir.parent() {
                Some(parent) if parent != dir => dir = parent.to_path_buf(),
                _ => break,
            }
        }
    }
    None
}

/// A git repo with nothing in it yet: no commit and no remote. Codex creates
/// one for each project folder it opens, and a chat started there works in
/// repos elsewhere; counted as a repo it was briefed and reported as a
/// machine-only repo of its own (`local/<folder>`), seen on no dashboard.
/// Read from the files, not by running git: this runs on every hook call.
fn is_empty_repo(dir: &Path) -> bool {
    let git = dir.join(".git");
    // a worktree or submodule (`.git` is a file pointing elsewhere) is real
    if !git.is_dir() {
        return false;
    }
    let has_refs = std::fs::read_dir(git.join("refs").join("heads")).map(|mut d| d.next().is_some()).unwrap_or(false)
        || git.join("packed-refs").is_file();
    let has_remote = std::fs::read_to_string(git.join("config")).map(|c| c.contains("[remote ")).unwrap_or(false);
    !has_refs && !has_remote
}

/// Where this machine's hook credential lives: in the data folder, or in
/// the other of `~/.cork` and `~/.collide` when only that one holds it (a
/// credential saved before the rename is found from either). HOME is
/// honoured first and explicitly: on Windows `expanduser` consults
/// USERPROFILE and ignores HOME, but the hooks run under Git Bash, whose HOME
/// is the one the install path means — and an un-overridable HOME once let a
/// test rewrite a developer's real credentials file.
pub fn credentials_path(env: &Env) -> PathBuf {
    let home = if !get(env, "CORK_HOME").is_empty() {
        get(env, "CORK_HOME").to_string()
    } else if !get(env, "HOME").is_empty() {
        get(env, "HOME").to_string()
    } else {
        get(env, "USERPROFILE").to_string()
    };
    let home = PathBuf::from(home);
    let chosen = data_dir_in(&home).join("credentials.json");
    if chosen.is_file() {
        return chosen;
    }
    for other in [".cork", ".collide"] {
        let path = home.join(other).join("credentials.json");
        if path.is_file() {
            return path;
        }
    }
    chosen
}

/// Cork's hosted server, by the one address it goes by.
pub const CLOUD_SERVER: &str = "https://mcp.corkcli.com";

/// A saved server address as it is connected to now. The hosted server also
/// answers on its names from before the rename (`mcp.`/`api.collidemcp.com`)
/// and on its Railway host; a machine or repo set up under one of those
/// connects to `mcp.corkcli.com`, the same service. Any other server
/// (staging, a local one, a self-hosted one) is kept as written.
pub fn canonical_server(server: &str) -> String {
    let trimmed = server.trim().trim_end_matches('/');
    let host = trimmed.split("://").nth(1).unwrap_or("").split(['/', ':']).next().unwrap_or("").to_ascii_lowercase();
    match host.as_str() {
        "mcp.collidemcp.com" | "api.collidemcp.com" | "api.corkcli.com" | "collide-server-production.up.railway.app" => CLOUD_SERVER.to_string(),
        _ => trimmed.to_string(),
    }
}

/// The same Cork server answers on more than one hostname (the classic
/// `api.*` and the `mcp.*` root, under corkcli.com and under collidemcp.com,
/// its domain before the rename); a config that moved between them must not
/// strand a token stored under the old key.
pub fn host_aliases(server: &str) -> Vec<String> {
    let mut domains = vec![server.to_string()];
    for (old, new) in [(".collidemcp.com", ".corkcli.com"), (".corkcli.com", ".collidemcp.com")] {
        if server.contains(old) {
            domains.push(server.replacen(old, new, 1));
        }
    }
    let mut out: Vec<String> = Vec::new();
    for host in domains {
        let mut variants = vec![host.clone()];
        for (old, new) in [("://api.", "://mcp."), ("://mcp.", "://api.")] {
            if host.contains(old) {
                variants.push(host.replacen(old, new, 1));
            }
        }
        for v in variants {
            if !out.contains(&v) {
                out.push(v);
            }
        }
    }
    out
}

/// Most specific first: exact server+workspace, the same workspace under a
/// sibling or any hostname (tokens are workspace-bound, not host-bound), the
/// bare server key and its sibling, then the legacy plain token.
pub fn resolve_token(creds: &Value, server: &str, workspace: &str) -> String {
    let Some(map) = creds.as_object() else { return String::new() };
    let aliases = host_aliases(server);
    if !workspace.is_empty() {
        for alias in &aliases {
            let key = format!("{alias}#{workspace}");
            if let Some(value) = map.get(&key) {
                let text = as_text(value);
                if !text.is_empty() {
                    return text;
                }
            }
        }
        let suffix = format!("#{workspace}");
        for (key, value) in map {
            if key.ends_with(&suffix) {
                let text = as_text(value);
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }
    for alias in &aliases {
        if let Some(value) = map.get(alias) {
            let text = as_text(value);
            if !text.is_empty() {
                return text;
            }
        }
    }
    map.get("token").map(as_text).unwrap_or_default()
}

fn as_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(false) => String::new(),
        other => other.to_string(),
    }
}

pub struct Config {
    pub server: String,
    pub token: String,
    pub repo_id: String,
    /// The repo's own check — `python -m tests.check`, `cargo check`, `npm
    /// test` — from `.cork/config.json` `verify`. Empty means none.
    pub verify: String,
    /// Parse on this machine and send the structure, never the file
    /// (`"local_parse": true` in `.cork/config.json`, which setup writes
    /// for new repos; `CORK_LOCAL_PARSE=1`/`0` overrides either way).
    pub local_parse: bool,
    /// Served by this machine's local server (the free version): the config
    /// came from the machine's install, not from the repo.
    pub machine_local: bool,
}

impl Config {
    /// What a report or a read sends about one file: the structure the
    /// server keeps (symbols, signatures, docstrings, line hashes, the names
    /// and the lines they are on), never the file. Since hooks v75 this is
    /// the only mode, whatever `local_parse` says: a file's text does not
    /// leave the machine. A file with no parser sends its line hashes only.
    pub fn source_fields(&self, path: &str, content: &str) -> serde_json::Map<String, Value> {
        cork_core::local::fields(path, content)
    }

    pub fn usable(&self) -> bool {
        !self.server.is_empty() && !self.token.is_empty() && !self.repo_id.is_empty()
    }
}

pub fn config(root: Option<&Path>, env: &Env) -> Config {
    // its paths, made relative before anything is sent (privacy.rs)
    if let Some(root) = root {
        crate::privacy::note_root(root);
    }
    let repo_cfg = root.map(repo_config).unwrap_or(Value::Object(Default::default()));

    let forced = get(env, "CORK_SERVER_URL").trim_end_matches('/').to_string();
    let mut server = forced.clone();
    if server.is_empty() {
        server = canonical_server(&str_field(&repo_cfg, "server_url"));
    }
    // no Cork config in the repo: the machine's install covers it. An
    // explicit CORK_SERVER_URL only changes which server, not which repo:
    // it used to skip the machine's config entirely, leaving no repo id, and
    // the hooks then reported nothing at all, without a word
    if str_field(&repo_cfg, "server_url").is_empty() && get(env, "CORK_REPO_ID").is_empty() {
        if let Some(mut machine) = root.and_then(|r| crate::machine::repo_config(r, env)) {
            if !forced.is_empty() && forced != machine.server {
                let creds = load_json(&credentials_path(env));
                machine.token = resolve_token(&creds, &forced, "");
                machine.server = forced;
                machine.machine_local = false;
            }
            // the repo's own check stays its choice under the machine's install
            machine.verify = str_field(&repo_cfg, "verify");
            return machine;
        }
    }
    let mut repo_id = get(env, "CORK_REPO_ID").to_string();
    if repo_id.is_empty() {
        repo_id = str_field(&repo_cfg, "repo_id");
    }
    let mut token = get(env, "CORK_TOKEN").to_string();
    if token.is_empty() && !server.is_empty() {
        let creds = load_json(&credentials_path(env));
        token = resolve_token(&creds, &server, &str_field(&repo_cfg, "workspace"));
    }
    let verify = str_field(&repo_cfg, "verify");
    let local_parse = match get(env, "CORK_LOCAL_PARSE").trim() {
        "1" => true,
        "0" => false,
        _ => repo_cfg.get("local_parse").and_then(Value::as_bool).unwrap_or(false),
    };
    Config { server, token, repo_id, verify, local_parse, machine_local: false }
}

/// Repo-relative, forward-slashed; `None` when the path sits outside the repo
/// (not ours to report or gate).
pub fn rel_path(file_path: &Path, root: &Path) -> Option<String> {
    let file = absolute(file_path);
    let root = absolute(root);
    let rel = file.strip_prefix(&root).ok()?;
    let text = rel.to_string_lossy().replace('\\', "/");
    if text.starts_with("..") {
        return None;
    }
    Some(if text.is_empty() { ".".to_string() } else { text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hosted_server_is_reached_by_its_corkcli_address() {
        for old in ["https://mcp.collidemcp.com", "https://api.collidemcp.com/", "https://api.corkcli.com", "https://collide-server-production.up.railway.app"] {
            assert_eq!(canonical_server(old), "https://mcp.corkcli.com", "{old}");
        }
        assert_eq!(canonical_server("https://mcp.corkcli.com/"), "https://mcp.corkcli.com");
        assert_eq!(canonical_server("https://cork-staging.example.com"), "https://cork-staging.example.com");
        assert_eq!(canonical_server("http://127.0.0.1:7600"), "http://127.0.0.1:7600");
        // the token saved under the old address is still found from the new one
        let creds = json!({"https://mcp.collidemcp.com#ws1": "cat_saved"});
        assert_eq!(resolve_token(&creds, &canonical_server("https://mcp.collidemcp.com"), "ws1"), "cat_saved");
    }

    #[test]
    fn an_empty_project_repo_is_not_a_repo_but_one_commit_or_a_remote_is() {
        let dir = std::env::temp_dir().join(format!("collide-empty-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git").join("refs").join("heads")).unwrap();
        std::fs::write(dir.join(".git").join("config"), "[core]\n\tbare = false\n").unwrap();
        assert!(is_empty_repo(&dir));
        std::fs::write(dir.join(".git").join("config"), "[remote \"origin\"]\n\turl = x\n").unwrap();
        assert!(!is_empty_repo(&dir));
        std::fs::write(dir.join(".git").join("config"), "[core]\n").unwrap();
        std::fs::write(dir.join(".git").join("refs").join("heads").join("main"), "abc\n").unwrap();
        assert!(!is_empty_repo(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
    use serde_json::json;

    /// The token lookup survives a server hostname change: a config that moved
    /// from api.* to mcp.* still finds what a teammate stored under the old key.
    #[test]
    fn a_token_under_the_old_host_still_resolves() {
        let creds = json!({"https://api.corkcli.com#ws1": "cat_old_key", "https://api.corkcli.com": "cat_old_plain"});
        assert_eq!(resolve_token(&creds, "https://mcp.corkcli.com", "ws1"), "cat_old_key");
    }

    #[test]
    fn the_exact_key_wins_then_the_workspace_on_any_host_then_the_plain_token() {
        let creds = json!({
            "https://mcp.corkcli.com#ws1": "cat_exact",
            "https://api.corkcli.com#ws1": "cat_alias",
            "https://other.example#ws1": "cat_other",
            "https://mcp.corkcli.com": "cat_server_wide",
            "token": "cat_default",
        });
        assert_eq!(resolve_token(&creds, "https://mcp.corkcli.com", "ws1"), "cat_exact");
        // only a foreign host holds the workspace's token: still fine (workspace-bound)
        let creds = json!({"https://other.example#ws1": "cat_other", "token": "cat_legacy"});
        assert_eq!(resolve_token(&creds, "https://mcp.corkcli.com", "ws1"), "cat_other");
        // nothing workspace-keyed at all: the legacy plain token
        assert_eq!(resolve_token(&json!({"token": "cat_legacy"}), "https://mcp.corkcli.com", "ws1"), "cat_legacy");
    }

    #[test]
    fn a_token_saved_under_the_domain_from_before_the_rename_still_resolves() {
        let creds = json!({"https://mcp.collidemcp.com": "cat_old"});
        assert_eq!(resolve_token(&creds, "https://mcp.corkcli.com", ""), "cat_old");
        assert_eq!(resolve_token(&json!({"https://api.corkcli.com": "cat_new"}), "https://mcp.collidemcp.com", ""), "cat_new");
        assert_eq!(host_aliases("https://mcp.corkcli.com")[0], "https://mcp.corkcli.com", "the exact host first");
    }

    fn env_of(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_setting_reads_cork_first_then_the_collide_name_it_had_before() {
        assert_eq!(get(&env_of(&[("COLLIDE_TOKEN", "old")]), "CORK_TOKEN"), "old");
        assert_eq!(get(&env_of(&[("CORK_TOKEN", "new"), ("COLLIDE_TOKEN", "old")]), "CORK_TOKEN"), "new");
        // asked by either name, the answer is the same
        assert_eq!(get(&env_of(&[("CORK_TOKEN", "new"), ("COLLIDE_TOKEN", "old")]), "COLLIDE_TOKEN"), "new");
        assert_eq!(get(&env_of(&[("CORK_TOKEN", ""), ("COLLIDE_TOKEN", "old")]), "CORK_TOKEN"), "old", "empty is unset");
        assert_eq!(get(&env_of(&[("HOME", "/h")]), "HOME"), "/h");
        assert_eq!(get(&env_of(&[]), "CORK_TOKEN"), "");
        assert!(sets("COLLIDE_SHARED_TREE=0 git commit", "CORK_SHARED_TREE=0"));
        assert!(sets("CORK_SHARED_TREE=0 git commit", "CORK_SHARED_TREE=0"));
        assert!(!sets("git commit", "CORK_SHARED_TREE=0"));
    }

    #[test]
    fn the_data_folder_is_cork_unless_only_the_old_collide_one_exists() {
        let base = std::env::temp_dir().join(format!("cork-data-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let env = env_of(&[("HOME", &base.to_string_lossy())]);
        assert_eq!(data_dir(&env), base.join(".cork"), "a fresh home");
        assert!(!legacy_layout(&env));
        std::fs::create_dir_all(base.join(".collide")).unwrap();
        assert_eq!(data_dir(&env), base.join(".collide"), "kept, never moved");
        assert!(legacy_layout(&env));
        // the credential saved there is found
        std::fs::write(base.join(".collide").join("credentials.json"), "{}").unwrap();
        assert_eq!(credentials_path(&env), base.join(".collide").join("credentials.json"));
        // CORK_HOME, or the COLLIDE_HOME it had before, moves the whole home
        let elsewhere = base.join("elsewhere");
        assert_eq!(data_dir(&env_of(&[("HOME", "/nowhere"), ("COLLIDE_HOME", &elsewhere.to_string_lossy())])), elsewhere.join(".cork"));
        // both exist: .cork; a credential only in .collide is still found
        std::fs::create_dir_all(base.join(".cork")).unwrap();
        assert_eq!(data_dir(&env), base.join(".cork"));
        assert_eq!(credentials_path(&env), base.join(".collide").join("credentials.json"));
        std::fs::write(base.join(".cork").join("credentials.json"), "{}").unwrap();
        assert_eq!(credentials_path(&env), base.join(".cork").join("credentials.json"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_repo_config_is_read_from_cork_then_collide() {
        let root = std::env::temp_dir().join(format!("cork-repo-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".collide")).unwrap();
        assert!(!has_repo_config(&root));
        assert_eq!(repo_config_path(&root), root.join(".cork").join("config.json"));
        std::fs::write(root.join(".collide").join("config.json"), r#"{"verify": "old"}"#).unwrap();
        assert!(has_repo_config(&root));
        assert_eq!(repo_config(&root)["verify"], json!("old"));
        std::fs::create_dir_all(root.join(".cork")).unwrap();
        std::fs::write(root.join(".cork").join("config.json"), r#"{"verify": "new"}"#).unwrap();
        assert_eq!(repo_config(&root)["verify"], json!("new"), "the new name wins");
        assert_eq!(find_repo_root(&[Some(root.join("sub"))]).as_deref(), Some(root.as_path()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_credential_is_still_no_credential() {
        let creds = json!({"https://elsewhere.example#ws9": "cat_unrelated"});
        assert_eq!(resolve_token(&creds, "https://mcp.corkcli.com", "ws1"), "");
    }
}
