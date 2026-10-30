//! Configuration source discovery.
//!
//! Sources are collected here and loaded in the deterministic low → high
//! priority order documented in `docs/14-config-permissions.md`:
//! global directory, `OPENCODE_CONFIG`, ancestor files, project `.opencode`
//! directories, inline content, then the managed system directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) const NAMES: [&str; 2] = ["opencode.json", "opencode.jsonc"];

#[derive(Debug, Clone)]
pub(crate) struct Discovery {
    /// `$XDG_CONFIG_HOME/opencode` (fallback `~/.config/opencode`). Always loaded.
    pub global: PathBuf,
    /// Platform managed directory, loaded last when present.
    pub system: PathBuf,
    pub system_present: bool,
    pub explicit: Option<PathBuf>,
    /// Ancestor `opencode.json(c)` files, outermost first.
    pub direct: Vec<PathBuf>,
    /// Ancestor `.opencode` directories, outermost first, existing only.
    pub project: Vec<PathBuf>,
    pub content: Option<String>,
    pub home: PathBuf,
    pub location: PathBuf,
}

/// Environment accessor used for discovery and substitution.
#[derive(Debug, Clone)]
pub(crate) struct Env {
    map: HashMap<String, String>,
}

impl Env {
    pub(crate) fn new(map: &HashMap<String, String>) -> Self {
        Self { map: map.clone() }
    }

    pub(crate) fn get(&self, key: &str) -> Option<String> {
        self.map
            .get(key)
            .filter(|value| !value.is_empty())
            .cloned()
            .or_else(|| std::env::var(key).ok().filter(|value| !value.is_empty()))
    }
}

#[must_use]
pub(crate) fn discover(location: &Path, env: &Env) -> Discovery {
    let location = absolute(location);
    let home = env
        .get("HOME")
        .or_else(|| env.get("USERPROFILE"))
        .map_or_else(PathBuf::new, PathBuf::from);
    let config_home = env
        .get("XDG_CONFIG_HOME")
        .map_or_else(|| home.join(".config"), PathBuf::from);
    let global = config_home.join("opencode");
    let system = system_directory(env);
    let system_present = system.is_dir();

    let explicit = env.get("OPENCODE_CONFIG").map(|value| {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            path
        } else {
            location.join(path)
        }
    });
    let content = env.get("OPENCODE_CONFIG_CONTENT");

    let walk = !is_under(&location, &global) && !is_under(&location, &system);
    let mut direct = Vec::new();
    let mut project = Vec::new();
    if walk {
        let ancestors = ancestors(&location);
        // Outermost first; inside one directory `opencode.jsonc` loads after
        // `opencode.json`, so it wins.
        for directory in ancestors.iter().rev() {
            if same_path(directory, &global) || same_path(directory, &system) {
                continue;
            }
            for name in NAMES {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    direct.push(candidate);
                }
            }
            let candidate = directory.join(".opencode");
            if candidate.is_dir() {
                project.push(candidate);
            }
        }
    }

    Discovery {
        global,
        system,
        system_present,
        explicit,
        direct,
        project,
        content,
        home,
        location,
    }
}

/// Platform managed configuration directory.
#[must_use]
pub(crate) fn system_directory(env: &Env) -> PathBuf {
    if cfg!(target_os = "macos") {
        return PathBuf::from("/Library/Application Support/opencode");
    }
    if cfg!(windows) {
        return env
            .get("ProgramData")
            .map_or_else(|| PathBuf::from("C:\\ProgramData"), PathBuf::from)
            .join("opencode");
    }
    PathBuf::from("/etc/opencode")
}

/// `path` and its ancestors, innermost first.
pub(crate) fn ancestors(path: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut current = Some(path.to_path_buf());
    while let Some(directory) = current {
        result.push(directory.clone());
        current = directory.parent().map(Path::to_path_buf);
    }
    result
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    }
}

/// Stable comparison key: absolute, `/`-separated, verbatim prefix stripped,
/// and lowercased on Windows. Lexical only: resolving symlinks would perform
/// filesystem (and, for UNC paths, network) access during discovery.
#[must_use]
pub(crate) fn path_key(path: &Path) -> String {
    let absolute = absolute_fallback(path);
    let mut text = absolute.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = text.strip_prefix("//?/") {
        text = stripped.to_owned();
    }
    if cfg!(windows) {
        text.make_ascii_lowercase();
    }
    text
}

fn absolute_fallback(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[must_use]
pub(crate) fn same_path(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

#[must_use]
pub(crate) fn is_under(path: &Path, root: &Path) -> bool {
    let path = path_key(path);
    let root = path_key(root);
    if path == root {
        return true;
    }
    let trimmed = root.trim_end_matches('/');
    if trimmed.is_empty() {
        return path.starts_with('/');
    }
    path.starts_with(&format!("{trimmed}/"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::is_under;

    #[test]
    fn handles_drive_letters_and_unc_paths() {
        #[cfg(windows)]
        {
            assert!(is_under(
                Path::new("C:\\work\\project\\src"),
                Path::new("c:/work/project")
            ));
            assert!(!is_under(
                Path::new("D:\\work\\project"),
                Path::new("C:\\work\\project")
            ));
            assert!(is_under(
                Path::new("\\\\server\\share\\dir\\file"),
                Path::new("\\\\server\\share")
            ));
            assert!(!is_under(
                Path::new("\\\\server\\other\\file"),
                Path::new("\\\\server\\share")
            ));
        }
        #[cfg(not(windows))]
        {
            assert!(is_under(Path::new("/a/b/c"), Path::new("/a/b")));
            assert!(!is_under(Path::new("/a/bc"), Path::new("/a/b")));
        }
    }
}
