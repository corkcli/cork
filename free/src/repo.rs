//! Repo identity and scope keying.
//!
//! Every piece of a repo's state lives under `{workspace_id}:{repo_id}`, and
//! that key is never re-derived once written. So the question this module
//! answers — "which canonical id does this incoming name mean?" — decides
//! whether an agent finds its teammates' work or a fresh empty scope.
//!
//! The rules are deliberately conservative. A friendly short name and the
//! full git URL of the same repo share one scope; two genuinely different
//! repos that merely share a basename never do. When it cannot tell, it
//! demands an exact match rather than guessing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::RwLock;

use sha1::{Digest, Sha1};

/// The ephemeral-key form of a path: short, fixed width, and not the path
/// itself (these keys end up in scan prefixes).
///
/// sha1, not sha256 — and that is not an aesthetic choice. Both servers scan
/// `hot:{scope}:{path_key}:` and write `salience:{scope}:{path_key}`, so a
/// different digest here would silently split every path into two key spaces:
/// the gate would find no hot markers already written, the dashboard would
/// show half the scar tissue, and nothing would error. A test pins it.
pub fn path_key(path: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(path.as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// A friendly key so equivalent forms of the same repo collide: the bare name
/// and any host-qualified URL share one. `github.com/acme/app`,
/// `git@github.com:acme/app.git`, a local path (`C:\src\app` too) and `app`
/// all normalize to `app`; a
/// branch-qualified `app#main` keeps its suffix and is a different scope.
///
/// Advisory only — two same-named repos in different orgs are separated by
/// the alias registry, never silently merged.
pub fn normalize_repo_id(repo_id: &str) -> String {
    let mut s = repo_id.trim().to_lowercase();
    if s.is_empty() {
        return s;
    }
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("git@") {
        s = rest.to_string();
    }
    // a Windows path (an agent passing its working directory, C:\Users\me\app)
    // splits like a POSIX one, so it lands on the repo's name, not a phantom scope
    s = s.replace([':', '\\'], "/");
    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let base = s.rsplit('/').next().unwrap_or(s);
    if base.is_empty() { s.to_string() } else { base.to_string() }
}

/// A folder on one machine rather than a repository: a `local/...` id (what
/// the free version names a repo with no remote), an absolute path (an agent
/// passing its working directory), a home-relative one. No one else could be
/// working in it, so it never joins a workspace. Repo names and `owner/name`
/// stay welcome: they resolve onto the repo they name.
pub fn is_machine_folder(repo_id: &str) -> bool {
    let id = repo_id.trim();
    let bytes = id.as_bytes();
    id.starts_with("local/")
        || id.starts_with('/')
        || id.starts_with('~')
        || id.starts_with('\\')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// The access-list key for a repo id: lowercase, scheme and `.git` stripped,
/// no `#branch` — so `github.com/Acme/API#main` and `acme/api` match.
pub fn repo_key(repo_id: &str) -> String {
    let mut s = repo_id.trim().to_lowercase();
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("git@") {
        s = rest.replacen(':', "/", 1);
    }
    let s = s.split('#').next().unwrap_or("").trim_end_matches('/');
    s.strip_suffix(".git").unwrap_or(s).to_string()
}

/// The org/owner segment of a host-qualified id; empty for bare names.
/// Used by workspace routing, which has not moved across yet.
#[allow(dead_code)]
pub fn repo_org(repo_id: &str) -> String {
    let key = repo_key(repo_id);
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() >= 2 { parts[parts.len() - 2].to_string() } else { String::new() }
}

/// A key of the bare `owner/name` form: two segments, and the first is no
/// host (a host has a dot). `acme/api`, not `github.com/api` or `api`.
fn is_owner_name(key: &str) -> bool {
    let mut parts = key.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) => !owner.is_empty() && !name.is_empty() && !owner.contains('.'),
        _ => false,
    }
}

/// `(host, "owner/name")` of a host-qualified key: three or more segments,
/// the first a host (it has a dot). `None` for anything else, a folder on
/// one machine included (`/home/jo/acme/api` has no host).
fn host_and_owner_name(key: &str) -> Option<(&str, &str)> {
    let (host, rest) = key.split_once('/')?;
    if host.is_empty() || !host.contains('.') || rest.matches('/').count() < 1 {
        return None;
    }
    let name_at = rest.rfind('/')?;
    let owner_at = rest[..name_at].rfind('/').map(|i| i + 1).unwrap_or(0);
    let owner_name = &rest[owner_at..];
    is_owner_name(owner_name).then_some((host, owner_name))
}

/// Two spellings of ONE repo: the same access key, or a bare `owner/name`
/// beside `host/owner/name` (an agent passing `initech/TPS` while the hook
/// reports `github.com/initech/tps`). Whether the bare form is unambiguous —
/// one host has that `owner/name`, not two — is the caller's to know; this
/// only compares the pair.
pub fn same_repo(a: &str, b: &str) -> bool {
    let (ka, kb) = (repo_key(a), repo_key(b));
    if ka.is_empty() || kb.is_empty() {
        return false;
    }
    if ka == kb {
        return true;
    }
    let bridged = |bare: &str, full: &str| {
        is_owner_name(bare) && host_and_owner_name(full).is_some_and(|(_, owner_name)| owner_name == bare)
    };
    bridged(&ka, &kb) || bridged(&kb, &ka)
}

/// One repo spelled `owner/name` and `host/owner/name` counts once — the
/// host-qualified key stands for both — unless two hosts have that
/// `owner/name`, when neither can say which it meant.
pub fn dedupe_spellings(keys: std::collections::BTreeSet<String>) -> std::collections::BTreeSet<String> {
    keys.iter()
        .filter(|key| {
            let matches = keys.iter().filter(|other| *other != *key && same_repo(key, other)).count();
            // a bare key (one slash) with exactly one host-qualified twin
            // is that twin
            !(matches == 1 && key.matches('/').count() == 1)
        })
        .cloned()
        .collect()
}

/// Is `repo_id` one of `bound`, in any spelling of it?
pub fn bound_contains(bound: &std::collections::BTreeSet<String>, repo_id: &str) -> bool {
    let key = repo_key(repo_id);
    if bound.contains(&key) {
        return true;
    }
    let twins: Vec<&String> = bound.iter().filter(|other| same_repo(&key, other)).collect();
    twins.len() == 1
}

/// The repo a workspace already has under the same name and host, but
/// another owner: `github.com/globex/web` arriving where `github.com/acme/web`
/// is bound is most likely the same repo after a transfer on GitHub (whose
/// old URL keeps serving the same refs, so git never tells a stale clone).
/// Never a guess between several: only exactly one such repo answers.
/// `known` is the workspace's bound ids and ids with data, less any the
/// people said are a different repo.
pub fn lookalike(known: &[String], incoming: &str) -> Option<String> {
    let key = repo_key(incoming);
    let (host, owner_name) = host_and_owner_name(&key)?;
    let (owner, name) = owner_name.split_once('/')?;
    let mut found: Vec<(String, &String)> = Vec::new();
    for id in known {
        let candidate = repo_key(id);
        if candidate == key || found.iter().any(|(k, _)| *k == candidate) {
            continue;
        }
        let Some((their_host, their_owner_name)) = host_and_owner_name(&candidate) else { continue };
        let Some((their_owner, their_name)) = their_owner_name.split_once('/') else { continue };
        if their_host == host && their_name == name && their_owner != owner {
            found.push((candidate.clone(), id));
        }
    }
    match found.as_slice() {
        [(_, only)] => Some((*only).clone()),
        _ => None,
    }
}

/// How host/org-qualified an id is; a full URL outranks a bare name when both
/// claim the same normalized key.
fn qualification(repo_id: &str) -> usize {
    repo_id.matches('/').count()
}

/// First id seen for a normalized key wins; genuinely ambiguous keys fall
/// back to exact matching.
#[derive(Default)]
pub struct Aliases {
    alias: RwLock<HashMap<String, String>>,
    ambiguous: RwLock<HashSet<String>>,
    renames: RwLock<HashMap<String, String>>,
    /// `{workspace}:{owner/name}` -> every host-qualified id seen with that
    /// owner and name, by access key. One entry: a bare `owner/name` means
    /// it. Two (github.com and gitlab.com both have `acme/api`): ambiguous,
    /// and the bare form stays exact.
    hosts: RwLock<HashMap<String, BTreeMap<String, String>>>,
    /// `{workspace}:{owner/name}` whose bare AND host-qualified spellings
    /// both held data when the server started: two scopes until someone
    /// merges them (`repo-merge`), so no deploy ever hides history.
    split: RwLock<HashSet<String>>,
}

impl Aliases {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a durable rename: a repo renamed on GitHub reports under its
    /// new id while its history lives under the old one.
    pub fn add_rename(&self, workspace_id: &str, from: &str, canonical: &str) {
        if let Ok(mut renames) = self.renames.write() {
            renames.insert(format!("{workspace_id}:{from}"), canonical.to_string());
        }
    }

    /// Undo [`Self::add_rename`]: `from` resolves on its own again.
    pub fn remove_rename(&self, workspace_id: &str, from: &str) {
        if let Ok(mut renames) = self.renames.write() {
            renames.remove(&format!("{workspace_id}:{from}"));
        }
    }

    /// Note a host-qualified id under its `owner/name`, so a bare spelling
    /// knows how many hosts it could mean. Cheap when already noted.
    fn note_host(&self, workspace_id: &str, repo_id: &str) {
        let key = repo_key(repo_id);
        let Some((_, owner_name)) = host_and_owner_name(&key) else { return };
        let slot = format!("{workspace_id}:{owner_name}");
        let known = self.hosts.read().map(|h| h.get(&slot).is_some_and(|ids| ids.contains_key(&key))).unwrap_or(true);
        if !known {
            if let Ok(mut hosts) = self.hosts.write() {
                hosts.entry(slot).or_default().entry(key).or_insert_with(|| repo_id.to_string());
            }
        }
    }

    /// May `incoming` share `canonical`'s scope because one is the bare
    /// `owner/name` of the other? Only when one host has that `owner/name`
    /// and the pair was not already two scopes holding data.
    fn owner_name_bridge(&self, workspace_id: &str, canonical: &str, incoming: &str) -> bool {
        if !same_repo(canonical, incoming) {
            return false;
        }
        let (ka, kb) = (repo_key(canonical), repo_key(incoming));
        let bare = if is_owner_name(&ka) { ka } else { kb };
        let slot = format!("{workspace_id}:{bare}");
        if self.split.read().map(|s| s.contains(&slot)).unwrap_or(true) {
            return false;
        }
        self.hosts.read().map(|h| h.get(&slot).map_or(0, BTreeMap::len) <= 1).unwrap_or(false)
    }

    /// The one host-qualified id a bare `owner/name` means, when exactly one
    /// is known and the pair is not split.
    fn qualified_for(&self, workspace_id: &str, repo_id: &str) -> Option<String> {
        let key = repo_key(repo_id);
        if !is_owner_name(&key) {
            return None;
        }
        let slot = format!("{workspace_id}:{key}");
        if self.split.read().map(|s| s.contains(&slot)).unwrap_or(true) {
            return None;
        }
        let hosts = self.hosts.read().ok()?;
        match hosts.get(&slot) {
            Some(ids) if ids.len() == 1 => ids.values().next().cloned(),
            _ => None,
        }
    }

    /// Seed from scopes that already carry data, so a friendly alias resolves
    /// to the existing canonical scope instead of minting an empty one. A
    /// fuller URL wins over a bare name; a key two equally-qualified repos
    /// both claim is marked ambiguous and demands an exact id thereafter.
    pub fn seed(&self, scopes: &[String]) {
        let mut best: HashMap<String, String> = HashMap::new();
        let mut ambiguous: HashSet<String> = HashSet::new();
        for scope in scopes {
            let Some((workspace_id, repo_id)) = scope.split_once(':') else { continue };
            if repo_id.is_empty() {
                continue;
            }
            let nkey = format!("{workspace_id}:{}", normalize_repo_id(repo_id));
            if ambiguous.contains(&nkey) {
                continue;
            }
            match best.get(&nkey) {
                None => {
                    best.insert(nkey, repo_id.to_string());
                }
                Some(current) if current == repo_id => {}
                Some(current) => {
                    let (new_rank, old_rank) = (qualification(repo_id), qualification(current));
                    if new_rank > old_rank {
                        best.insert(nkey, repo_id.to_string());
                    } else if new_rank == old_rank {
                        // two distinct repos, same basename, equally qualified:
                        // aliasing either way would merge them
                        ambiguous.insert(nkey.clone());
                        best.remove(&nkey);
                    }
                }
            }
        }
        if let Ok(mut alias) = self.alias.write() {
            alias.extend(best);
        }
        if let Ok(mut marked) = self.ambiguous.write() {
            marked.extend(ambiguous);
        }
        // `owner/name` beside `host/owner/name`: every host-qualified id is
        // noted under its owner/name, and a bare spelling that ALSO holds
        // data is a split pair — kept exact until merged, never hidden
        let mut bare: Vec<String> = Vec::new();
        for scope in scopes {
            let Some((workspace_id, repo_id)) = scope.split_once(':') else { continue };
            let key = repo_key(repo_id);
            if is_owner_name(&key) {
                bare.push(format!("{workspace_id}:{key}"));
            } else {
                self.note_host(workspace_id, repo_id);
            }
        }
        let hosts = self.hosts.read().map(|h| h.clone()).unwrap_or_default();
        if let Ok(mut split) = self.split.write() {
            split.extend(bare.into_iter().filter(|slot| hosts.get(slot).is_some_and(|ids| !ids.is_empty())));
        }
    }

    /// Force `{workspace_id}:normalize(repo_id)` to resolve onto `canonical`,
    /// overwriting any existing claim. `resolve`'s first-seen rule is right
    /// for organic discovery, but a repo that just landed in a workspace
    /// (`move_repo`) needs its bare basename bridged to it immediately —
    /// waiting for the next `resolve` call could instead confirm a stale
    /// claim left by whatever used to sit at that key.
    pub fn force_alias(&self, workspace_id: &str, repo_id: &str, canonical: &str) {
        let nkey = format!("{workspace_id}:{}", normalize_repo_id(repo_id));
        if let Ok(mut alias) = self.alias.write() {
            alias.insert(nkey, canonical.to_string());
        }
    }

    /// Map an incoming repo id to the canonical id already holding this
    /// workspace's data.
    pub fn resolve(&self, workspace_id: &str, repo_id: &str) -> String {
        self.resolve_inner(workspace_id, repo_id, 0)
    }

    fn resolve_inner(&self, workspace_id: &str, repo_id: &str, depth: usize) -> String {
        if depth > 4 {
            return repo_id.to_string();
        }
        // an explicit rename comes first: redirect before basename aliasing so
        // the new name lands on the old scope
        let mut repo_id = repo_id.to_string();
        if let Ok(renames) = self.renames.read() {
            if let Some(canonical) = renames.get(&format!("{workspace_id}:{repo_id}")) {
                if canonical != &repo_id {
                    repo_id = canonical.clone();
                }
            }
        }

        // a #branch-suffixed id whose base this workspace already knows is the
        // SAME repo on a branch, not a second repo: fold it onto the base
        // scope (the branch rides every event as metadata instead). A
        // suffixed id with no known base keeps its own scope.
        if let Some((base, _branch)) = repo_id.split_once('#') {
            let base = base.trim();
            if !base.is_empty() {
                let base_key = format!("{workspace_id}:{}", normalize_repo_id(base));
                let known = self.alias.read().map(|a| a.contains_key(&base_key)).unwrap_or(false);
                if known {
                    return self.resolve_inner(workspace_id, base, depth + 1);
                }
            }
        }

        let nkey = format!("{workspace_id}:{}", normalize_repo_id(&repo_id));
        self.note_host(workspace_id, &repo_id);
        if self.ambiguous.read().map(|a| a.contains(&nkey)).unwrap_or(false) {
            // two repos share the basename, but `acme/api` still names one
            // of them when a single host has that owner and name
            if let Some(full) = self.qualified_for(workspace_id, &repo_id) {
                return self.resolve_inner(workspace_id, &full, depth + 1);
            }
            return repo_id;
        }
        let existing = self.alias.read().ok().and_then(|a| a.get(&nkey).cloned());
        if let Some(canonical) = existing {
            // basename aliasing bridges a BARE name and the full URL of one
            // repo — never two host-qualified ids that merely share a basename
            // (github.com/acme/api and github.com/other/api are two repos).
            // `acme/api` and `github.com/acme/api` are one repo, though: the
            // same owner and name, on the one host that has them
            if canonical != repo_id
                && canonical.contains('/')
                && repo_id.contains('/')
                && repo_key(&canonical) != repo_key(&repo_id)
            {
                if self.owner_name_bridge(workspace_id, &canonical, &repo_id) {
                    return canonical;
                }
                if let Some(full) = self.qualified_for(workspace_id, &repo_id) {
                    return self.resolve_inner(workspace_id, &full, depth + 1);
                }
                return repo_id;
            }
            return canonical;
        }
        if let Ok(mut alias) = self.alias.write() {
            alias.insert(nkey, repo_id.clone()); // first-seen claims the key
        }
        repo_id
    }

    pub fn scope_for(&self, workspace_id: &str, repo_id: &str) -> String {
        format!("{workspace_id}:{}", self.resolve(workspace_id, repo_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_on_one_machine_never_joins_a_workspace() {
        for folder in ["local/my-app", r"C:\Users\jo\app", "C:/Users/jo/app", "/home/jo/app", "~/code/app", r"\\server\share\app"] {
            assert!(is_machine_folder(folder), "{folder}");
        }
        for repo in ["github.com/acme/api", "git@gitlab.com:group/app.git", "acme/api", "collide", "r-doc-empty"] {
            assert!(!is_machine_folder(repo), "{repo}");
        }
    }

    #[test]
    fn owner_name_and_its_host_qualified_url_are_one_repo() {
        assert!(same_repo("initech/TPS", "github.com/initech/tps"));
        assert!(same_repo("github.com/initech/tps", "initech/TPS"));
        assert!(same_repo("git@github.com:Acme/API.git", "https://github.com/acme/api#main"));
        // another owner, or a host standing where the owner should be, is not
        assert!(!same_repo("github.com/acme/api", "github.com/other/api"));
        assert!(!same_repo("other/api", "github.com/acme/api"));
        assert!(!same_repo("github.com/api", "gitlab.com/github.com/api"));
        // a folder on one machine never becomes a repo by its last two names
        assert!(!same_repo("acme/api", "/home/jo/acme/api"));
        assert!(!same_repo("acme/api", "api"));
    }

    #[test]
    fn a_bare_owner_name_lands_on_the_one_host_qualified_scope() {
        let aliases = Aliases::new();
        aliases.seed(&["w:github.com/initech/tps".to_string()]);
        assert_eq!(aliases.resolve("w", "initech/TPS"), "github.com/initech/tps");
        assert_eq!(aliases.resolve("w", "github.com/initech/tps"), "github.com/initech/tps");
        // another workspace knows nothing of it
        assert_eq!(aliases.resolve("v", "initech/TPS"), "initech/TPS");
        // a basename two owners share still lets owner/name find its own
        let shared = Aliases::new();
        shared.seed(&["w:github.com/acme/api".to_string(), "w:github.com/other/api".to_string()]);
        assert_eq!(shared.resolve("w", "acme/api"), "github.com/acme/api");
        assert_eq!(shared.resolve("w", "other/api"), "github.com/other/api");
        assert_eq!(shared.resolve("w", "github.com/other/api"), "github.com/other/api");
    }

    #[test]
    fn a_bare_owner_name_on_two_hosts_stays_exact() {
        let aliases = Aliases::new();
        aliases.seed(&["w:github.com/acme/api".to_string(), "w:gitlab.com/acme/api".to_string()]);
        assert_eq!(aliases.resolve("w", "acme/api"), "acme/api");
        // and a second host seen at runtime makes the first's bridge ambiguous too
        let runtime = Aliases::new();
        runtime.seed(&["w:github.com/acme/web".to_string()]);
        assert_eq!(runtime.resolve("w", "gitlab.com/acme/web"), "gitlab.com/acme/web");
        assert_eq!(runtime.resolve("w", "acme/web"), "acme/web");
    }

    #[test]
    fn two_spellings_that_both_hold_data_stay_two_scopes_until_merged() {
        let aliases = Aliases::new();
        aliases.seed(&["w:github.com/initech/tps".to_string(), "w:initech/TPS".to_string()]);
        assert_eq!(aliases.resolve("w", "initech/TPS"), "initech/TPS");
        assert_eq!(aliases.resolve("w", "github.com/initech/tps"), "github.com/initech/tps");
        // the merge is an explicit alias, which outranks the split
        aliases.add_rename("w", "initech/TPS", "github.com/initech/tps");
        assert_eq!(aliases.resolve("w", "initech/TPS"), "github.com/initech/tps");
        aliases.remove_rename("w", "initech/TPS");
        assert_eq!(aliases.resolve("w", "initech/TPS"), "initech/TPS");
    }

    #[test]
    fn a_repo_under_a_new_owner_finds_the_one_the_workspace_has() {
        let known = vec!["github.com/acme/web".to_string(), "github.com/initech/tps".to_string()];
        assert_eq!(lookalike(&known, "github.com/globex/web").as_deref(), Some("github.com/acme/web"));
        assert_eq!(lookalike(&known, "git@github.com:Globex/Web.git").as_deref(), Some("github.com/acme/web"));
        // the repo itself, another name, another host, a bare id: nothing
        assert_eq!(lookalike(&known, "github.com/acme/web"), None);
        assert_eq!(lookalike(&known, "github.com/globex/papaya"), None);
        assert_eq!(lookalike(&known, "gitlab.com/globex/web"), None);
        assert_eq!(lookalike(&known, "globex/web"), None);
        // two candidates: no guess
        let two = vec!["github.com/acme/web".to_string(), "github.com/someone/web".to_string()];
        assert_eq!(lookalike(&two, "github.com/globex/web"), None);
        // one repo spelled twice is one candidate
        let spelled = vec!["github.com/acme/web".to_string(), "github.com/Acme/Web".to_string()];
        assert_eq!(lookalike(&spelled, "github.com/globex/web").as_deref(), Some("github.com/acme/web"));
        // a pair the people called different is left out of `known` by the caller
        assert_eq!(lookalike(&[], "github.com/globex/web"), None);
    }

    #[test]
    fn a_working_directory_lands_on_the_repo_name_on_every_platform() {
        for id in ["github.com/acme/app", "git@github.com:acme/app.git", r"C:\Users\jo\app", r"C:\Users\jo\app\", "/home/jo/app", "App"] {
            assert_eq!(normalize_repo_id(id), "app", "{id}");
        }
    }
}
