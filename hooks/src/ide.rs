//! Which editor the agent works in, and which of its workspaces.
//!
//! The dashboard's breadcrumb reads `org / workspace / repo /
//! branch`, where the workspace is the IDE's: a `.code-workspace` file, a
//! JetBrains project, an Xcode workspace, a Visual Studio solution. Read from
//! the environment the editor's terminal hands its children and from a few
//! files beside the checkout: no subprocess, no network, two directory
//! listings at most, and once per process. Twin of report_hook.py's `_ide`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::config::{self, Env};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ide {
    /// "vscode", "cursor", "windsurf", "jetbrains", "zed", "xcode",
    /// "visualstudio", or "" when unknown
    pub ide: String,
    /// the workspace's display name, "" when none
    pub workspace: String,
}

/// The editor and workspace for this checkout, worked out once per process.
pub fn of(root: &Path, env: &Env) -> Ide {
    static SEEN: OnceLock<Mutex<HashMap<PathBuf, Ide>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if let Some(found) = seen.lock().ok().and_then(|m| m.get(root).cloned()) {
        return found;
    }
    let found = detect(root, env, crate::harness::current());
    if let Ok(mut map) = seen.lock() {
        map.insert(root.to_path_buf(), found.clone());
    }
    found
}

/// `of` without the memo, the harness passed in: what the tests drive.
pub fn detect(root: &Path, env: &Env, harness: &str) -> Ide {
    let ide = editor(env, harness);
    if ide.is_empty() {
        // an agent in a plain terminal works in no IDE's workspace, whatever
        // project files the repo has checked in
        return Ide::default();
    }
    let workspace = workspace(root, env, ide);
    Ide { ide: ide.to_string(), workspace: workspace.chars().take(120).collect() }
}

/// The editor, from what its integrated terminal (or extension host) sets.
pub fn editor(env: &Env, harness: &str) -> &'static str {
    let get = |key: &str| config::get(env, key);
    let bundle = get("__CFBundleIdentifier").to_ascii_lowercase();
    // VS Code's forks all say TERM_PROGRAM=vscode; the app's own path in the
    // git askpass helper (or the macOS bundle id) tells them apart
    let app_paths = ["VSCODE_GIT_ASKPASS_NODE", "VSCODE_GIT_ASKPASS_MAIN", "VSCODE_CWD"]
        .iter()
        .map(|k| get(k).to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("\n");
    let vscode_like = get("TERM_PROGRAM") == "vscode"
        || env.keys().any(|k| k.starts_with("VSCODE_"))
        || bundle.starts_with("com.microsoft.vscode");
    // Cursor's own agent runs these hooks from its extension host
    if !get("CURSOR_TRACE_ID").is_empty() || bundle == "com.todesktop.230313mzl4w4u92" || app_paths.contains("cursor.app")
        || (vscode_like && (app_paths.contains("/cursor") || harness == "cursor")) {
        return "cursor";
    }
    if bundle.contains("windsurf") || app_paths.contains("windsurf") {
        return "windsurf";
    }
    if vscode_like {
        return "vscode";
    }
    if get("TERMINAL_EMULATOR").starts_with("JetBrains") || bundle.starts_with("com.jetbrains.")
        || bundle.starts_with("com.google.android.studio") || !get("IDEA_INITIAL_DIRECTORY").is_empty() {
        return "jetbrains";
    }
    if get("TERM_PROGRAM").eq_ignore_ascii_case("zed") || !get("ZED_TERM").is_empty() || bundle == "dev.zed.zed"
        || bundle == "dev.zed.zed-preview" {
        return "zed";
    }
    if bundle == "com.apple.dt.xcode" || !get("XCODE_VERSION_ACTUAL").is_empty()
        || get("XPC_SERVICE_NAME").contains("com.apple.dt.Xcode") {
        return "xcode";
    }
    if !get("VisualStudioVersion").is_empty() || get("VSAPPIDNAME").eq_ignore_ascii_case("devenv.exe") {
        return "visualstudio";
    }
    ""
}

/// The workspace this IDE has open around `root`, by the IDE's own files:
/// the checkout's folder first, then the one above it.
fn workspace(root: &Path, env: &Env, ide: &str) -> String {
    let places: Vec<&Path> = std::iter::once(root).chain(root.parent()).collect();
    let found = match ide {
        "vscode" | "cursor" | "windsurf" => places.iter().find_map(|dir| code_workspace(dir, root)),
        "jetbrains" => places.iter().find_map(|dir| jetbrains_project(dir)),
        "xcode" => places.iter().find_map(|dir| named_with(dir, &[".xcworkspace"]))
            .or_else(|| places.iter().find_map(|dir| named_with(dir, &[".xcodeproj"]))),
        "visualstudio" => places.iter().find_map(|dir| named_with(dir, &[".sln", ".slnx"])),
        "zed" => places.iter().find(|dir| dir.join(".zed").is_dir()).and_then(|dir| folder_name(dir)),
        _ => None,
    };
    found.unwrap_or_else(|| {
        // the IDE's own word for it, when it gives one
        let dir = config::get(env, "IDEA_INITIAL_DIRECTORY");
        if ide == "jetbrains" && !dir.is_empty() { folder_name(Path::new(dir)).unwrap_or_default() } else { String::new() }
    })
}

fn folder_name(dir: &Path) -> Option<String> {
    dir.file_name().and_then(|n| n.to_str()).map(str::to_string).filter(|n| !n.is_empty())
}

/// Entries of `dir` ending in one of `exts`, sorted, so the pick is stable.
fn entries_with(dir: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| exts.iter().any(|ext| n.ends_with(ext))))
        .collect();
    found.sort();
    found
}

fn stem(path: &Path) -> Option<String> {
    path.file_stem().and_then(|n| n.to_str()).map(str::to_string).filter(|n| !n.is_empty())
}

fn named_with(dir: &Path, exts: &[&str]) -> Option<String> {
    entries_with(dir, exts).first().and_then(|p| stem(p))
}

/// A `.code-workspace` in `dir` whose folders hold (or sit inside) `root`.
fn code_workspace(dir: &Path, root: &Path) -> Option<String> {
    let root = config::normpath(&config::absolute(root));
    entries_with(dir, &[".code-workspace"]).into_iter().find_map(|file| {
        let text = std::fs::read_to_string(&file).ok()?;
        let parsed: Value = serde_json::from_str(&strip_jsonc(&text)).ok()?;
        let folders = parsed.get("folders")?.as_array()?;
        let holds = folders.iter().filter_map(|f| f.get("path").and_then(Value::as_str)).any(|path| {
            let path = Path::new(path);
            let folder = config::normpath(&if path.is_absolute() { path.to_path_buf() } else { dir.join(path) });
            root.starts_with(&folder) || folder.starts_with(&root)
        });
        holds.then(|| stem(&file)).flatten()
    })
}

/// A JetBrains project: its `.idea/.name` when renamed, else its folder.
fn jetbrains_project(dir: &Path) -> Option<String> {
    let idea = dir.join(".idea");
    if !idea.is_dir() {
        return None;
    }
    let named = std::fs::read_to_string(idea.join(".name")).ok().map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    named.or_else(|| folder_name(dir))
}

/// JSON with comments and trailing commas (what VS Code writes) as JSON:
/// the comments go first, so a comma before one still reads as trailing.
fn strip_jsonc(text: &str) -> String {
    strip_pass(&strip_pass(text, false), true)
}

fn strip_pass(text: &str, commas: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut in_string) = (0, false);
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match (c, chars.get(i + 1)) {
            ('"', _) => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            ('/', Some('/')) if !commas => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ('/', Some('*')) if !commas => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            (',', _) if commas => {
                // a comma with only whitespace before the closing bracket
                let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                if !matches!(next, Some(']') | Some('}')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("collide-ide-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_editor_comes_from_its_terminal() {
        assert_eq!(editor(&env(&[("TERM_PROGRAM", "vscode")]), "claude"), "vscode");
        assert_eq!(editor(&env(&[("TERM_PROGRAM", "vscode"), ("CURSOR_TRACE_ID", "x")]), "claude"), "cursor");
        assert_eq!(editor(&env(&[("TERM_PROGRAM", "vscode"),
            ("VSCODE_GIT_ASKPASS_NODE", "/Applications/Windsurf.app/Contents/Frameworks/Windsurf Helper")]), "claude"), "windsurf");
        assert_eq!(editor(&env(&[("__CFBundleIdentifier", "com.todesktop.230313mzl4w4u92")]), "claude"), "cursor");
        assert_eq!(editor(&env(&[("TERMINAL_EMULATOR", "JetBrains-JediTerm")]), "claude"), "jetbrains");
        assert_eq!(editor(&env(&[("TERM_PROGRAM", "zed")]), "claude"), "zed");
        assert_eq!(editor(&env(&[("__CFBundleIdentifier", "com.apple.dt.Xcode")]), "claude"), "xcode");
        assert_eq!(editor(&env(&[("VisualStudioVersion", "17.0")]), "claude"), "visualstudio");
        assert_eq!(editor(&env(&[("VSCODE_PID", "1")]), "cursor"), "cursor");
        assert_eq!(editor(&env(&[("TERM_PROGRAM", "iTerm.app")]), "claude"), "");
        assert_eq!(editor(&env(&[("TERM_PROGRAM", "Apple_Terminal")]), "cursor"), "");
    }

    #[test]
    fn a_code_workspace_names_the_workspace_only_when_it_holds_the_checkout() {
        let base = scratch("code");
        let root = base.join("app");
        std::fs::create_dir_all(&root).unwrap();
        // the parent's workspace lists the checkout, with comments and a trailing comma
        std::fs::write(base.join("Platform.code-workspace"),
            "{\n  // the whole platform\n  \"folders\": [\n    {\"path\": \"app\"}, /* api */ {\"path\": \"api\"},\n  ],\n}\n").unwrap();
        std::fs::write(base.join("Other.code-workspace"), r#"{"folders": [{"path": "elsewhere"}]}"#).unwrap();
        let vscode = env(&[("TERM_PROGRAM", "vscode")]);
        assert_eq!(detect(&root, &vscode, "claude"), Ide { ide: "vscode".into(), workspace: "Platform".into() });
        // one in the checkout itself wins over the parent's
        std::fs::write(root.join("Solo.code-workspace"), r#"{"folders": [{"path": "."}]}"#).unwrap();
        assert_eq!(detect(&root, &vscode, "claude").workspace, "Solo");
        // a workspace that does not hold the checkout is not its workspace
        std::fs::remove_file(root.join("Solo.code-workspace")).unwrap();
        std::fs::write(base.join("Platform.code-workspace"), r#"{"folders": [{"path": "api"}]}"#).unwrap();
        assert_eq!(detect(&root, &vscode, "claude"), Ide { ide: "vscode".into(), workspace: String::new() });
        // no editor, no workspace, whatever files are there
        std::fs::write(root.join("Solo.code-workspace"), r#"{"folders": [{"path": "."}]}"#).unwrap();
        assert_eq!(detect(&root, &env(&[]), "claude"), Ide::default());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn jetbrains_xcode_visual_studio_and_zed_name_theirs() {
        let base = scratch("others");
        let root = base.join("shop");
        std::fs::create_dir_all(root.join(".idea")).unwrap();
        let jetbrains = env(&[("TERMINAL_EMULATOR", "JetBrains-JediTerm")]);
        assert_eq!(detect(&root, &jetbrains, "claude").workspace, "shop");
        std::fs::write(root.join(".idea").join(".name"), "Storefront\n").unwrap();
        assert_eq!(detect(&root, &jetbrains, "claude").workspace, "Storefront");

        std::fs::create_dir_all(base.join("Shop.xcworkspace")).unwrap();
        std::fs::create_dir_all(root.join("Shop.xcodeproj")).unwrap();
        let xcode = env(&[("__CFBundleIdentifier", "com.apple.dt.Xcode")]);
        // a workspace anywhere beats a project
        assert_eq!(detect(&root, &xcode, "claude").workspace, "Shop");
        std::fs::remove_dir_all(base.join("Shop.xcworkspace")).unwrap();
        std::fs::rename(root.join("Shop.xcodeproj"), root.join("ShopApp.xcodeproj")).unwrap();
        assert_eq!(detect(&root, &xcode, "claude").workspace, "ShopApp");

        std::fs::write(root.join("Shop.sln"), "").unwrap();
        assert_eq!(detect(&root, &env(&[("VisualStudioVersion", "17.0")]), "claude").workspace, "Shop");

        let zed = env(&[("TERM_PROGRAM", "zed")]);
        assert_eq!(detect(&root, &zed, "claude").workspace, "");
        std::fs::create_dir_all(base.join(".zed")).unwrap();
        assert_eq!(detect(&root, &zed, "claude").workspace, base.file_name().unwrap().to_str().unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn jsonc_keeps_slashes_and_commas_inside_strings() {
        let text = r#"{"a": "http://x, ]", /* c */ "b": [1, 2,], // tail
}"#;
        let parsed: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(parsed["a"], "http://x, ]");
        assert_eq!(parsed["b"], serde_json::json!([1, 2]));
    }
}
