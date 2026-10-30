//! Path canonicalisation and confinement roots for worktree targets.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use oc_store::Store;
use oc_types::ApiError;
use serde_json::Value;

use crate::AppState;
use crate::error::ErrorResponse;

use super::worktree_error;

/// A registered directory that resolves to `candidate`, in stored form.
pub(super) async fn registered_directory(
    store: &Store,
    project_id: &str,
    candidate: &Path,
) -> Result<Option<String>, ErrorResponse> {
    let rows = store.list_worktrees(Some(project_id)).await?;
    for row in rows {
        let Some(directory) = row.get("directory").and_then(Value::as_str) else {
            continue;
        };
        if path_matches(&row, candidate).await {
            return Ok(Some(directory.to_owned()));
        }
    }
    Ok(None)
}

pub(super) async fn path_matches(row: &Value, candidate: &Path) -> bool {
    let Some(directory) = row.get("directory").and_then(Value::as_str) else {
        return false;
    };
    same_path(Path::new(directory), candidate).await
}

/// Whether two path spellings name the same filesystem location.
///
/// Raw strings are never compared: both sides are canonicalised and reduced to
/// a [`path_key`], which folds the differences Windows introduces (the `\\?\`
/// verbatim prefix and letter case).
pub(super) async fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    path_key(left).await == path_key(right).await
}

/// Comparable identity of `path`: canonical when it exists, Windows verbatim
/// prefix stripped and case folded on Windows.
pub(super) async fn path_key(path: &Path) -> PathBuf {
    let canonical = canonical_or(path).await;
    #[cfg(windows)]
    {
        PathBuf::from(display_path(&canonical).to_lowercase())
    }
    #[cfg(not(windows))]
    {
        canonical
    }
}

/// Canonicalise `path` (or its nearest existing ancestor plus the missing tail).
pub(super) async fn resolve_path(path: &Path) -> Result<PathBuf, ApiError> {
    if path.as_os_str().is_empty() {
        return Err(worktree_error("A worktree directory is required", None));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(worktree_error(
            format!(
                "Worktree directory may not contain '..': {}",
                path.display()
            ),
            None,
        ));
    }
    if tokio::fs::symlink_metadata(path).await.is_ok() {
        return tokio::fs::canonicalize(path).await.map_err(|error| {
            worktree_error(format!("Cannot resolve {}: {error}", path.display()), None)
        });
    }

    let mut ancestor = path.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        if tokio::fs::symlink_metadata(&ancestor).await.is_ok() {
            break;
        }
        let Some(name) = ancestor.file_name().map(std::ffi::OsStr::to_os_string) else {
            return Err(worktree_error(
                format!("Cannot resolve {}", path.display()),
                None,
            ));
        };
        tail.push(name);
        let Some(parent) = ancestor.parent().map(Path::to_path_buf) else {
            return Err(worktree_error(
                format!("Cannot resolve {}", path.display()),
                None,
            ));
        };
        ancestor = parent;
    }
    let mut resolved = tokio::fs::canonicalize(&ancestor).await.map_err(|error| {
        worktree_error(
            format!("Cannot resolve {}: {error}", ancestor.display()),
            None,
        )
    })?;
    for part in tail.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

pub(super) async fn canonical_or(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Strip the Windows verbatim prefix so stored paths look like the client's.
pub(super) fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        let stripped = match text.strip_prefix(r"\\?\") {
            Some(stripped) => stripped,
            None => &*text,
        };
        stripped.to_owned()
    }
    #[cfg(not(windows))]
    {
        text.into_owned()
    }
}

/// The path handed to git: unchanged on Unix, prefix-stripped on Windows.
pub(super) fn git_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(display_path(path))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

pub(super) fn data_directory() -> PathBuf {
    if let Some(directory) = std::env::var_os("OPENCODE_DATA_DIR") {
        return PathBuf::from(directory);
    }
    if let Some(directory) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(directory).join("opencode");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/opencode");
    }
    std::env::temp_dir().join("opencode")
}

fn expand_home(raw: &str) -> PathBuf {
    let Some(home) = std::env::var_os("HOME") else {
        return PathBuf::from(raw);
    };
    if raw == "~" {
        return PathBuf::from(home);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(raw)
}

pub(super) fn configured_worktree_directory(state: &AppState) -> Option<PathBuf> {
    let configured = state
        .config()?
        .merged()
        .get("worktree")?
        .get("directory")?
        .as_str()?
        .trim();
    if configured.is_empty() {
        return None;
    }
    let path = expand_home(configured);
    Some(if path.is_absolute() {
        path
    } else {
        state.directory().join(path)
    })
}

/// Roots a worktree target may live under.
pub(super) async fn allowed_roots(state: &AppState, project_directory: &Path) -> Vec<PathBuf> {
    let mut roots = vec![project_directory.to_path_buf()];
    if let Some(configured) = configured_worktree_directory(state) {
        let resolved = resolve_path(&configured)
            .await
            .unwrap_or_else(|_| configured.clone());
        roots.push(resolved);
    }
    let global = data_directory().join("worktree");
    let resolved = resolve_path(&global)
        .await
        .unwrap_or_else(|_| global.clone());
    roots.push(resolved);
    roots
}

pub(super) fn project_slug(project_id: &str) -> String {
    project_id
        .chars()
        .take(6)
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn path_keys_compare_canonical_forms() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        assert!(same_path(path, path).await);
        assert!(!same_path(path, &path.join("missing")).await);
        #[cfg(windows)]
        {
            // Windows paths are case-insensitive and `canonicalize` hands back
            // a verbatim (`\\?\`) form; both spellings must compare equal.
            let upper = PathBuf::from(path.to_string_lossy().to_uppercase());
            assert!(
                same_path(path, &upper).await,
                "{} vs {}",
                path.display(),
                upper.display()
            );
            let verbatim = PathBuf::from(format!(r"\\?\{}", path.display()));
            assert!(
                same_path(path, &verbatim).await,
                "{} vs {}",
                path.display(),
                verbatim.display()
            );
        }
    }
}
