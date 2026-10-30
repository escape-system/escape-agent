//! Worktree target planning: base directory, source and name selection.

use std::path::{Component, Path, PathBuf};

use oc_store::Store;
use oc_types::ApiError;

use crate::AppState;
use crate::error::ErrorResponse;

use super::MAX_NAME_SUFFIX;
use super::paths::{
    allowed_roots, configured_worktree_directory, data_directory, project_slug,
    registered_directory, resolve_path,
};
use super::worktree_error;

/// Resolve the base directory a new worktree is created inside.
pub(super) async fn base_directory(
    state: &AppState,
    project_id: &str,
    project_directory: &Path,
    requested: Option<&str>,
) -> Result<PathBuf, ApiError> {
    let base = match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(requested) => {
            let path = PathBuf::from(requested);
            if !path.is_absolute() {
                return Err(worktree_error(
                    format!("Worktree directory must be absolute: {requested}"),
                    None,
                ));
            }
            path
        }
        None => {
            if let Some(configured) = configured_worktree_directory(state) {
                configured
            } else {
                let slug = project_slug(project_id);
                let slug = if slug.is_empty() { "project" } else { &slug };
                data_directory().join("worktree").join(slug)
            }
        }
    };
    let resolved = resolve_path(&base).await?;
    let roots = allowed_roots(state, project_directory).await;
    if !roots.iter().any(|root| resolved.starts_with(root)) {
        return Err(worktree_error(
            format!(
                "Worktree directory is outside the project worktree root: {}",
                resolved.display()
            ),
            None,
        ));
    }
    Ok(resolved)
}

/// `from` must be the project directory or one of its saved worktrees.
pub(super) async fn source_directory(
    store: &Store,
    project_id: &str,
    project_directory: &Path,
    from: Option<&str>,
) -> Result<PathBuf, ErrorResponse> {
    let Some(from) = from.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(project_directory.to_path_buf());
    };
    let resolved = resolve_path(Path::new(from)).await?;
    if resolved.starts_with(project_directory) {
        return Ok(resolved);
    }
    if registered_directory(store, project_id, &resolved)
        .await?
        .is_some()
    {
        return Ok(resolved);
    }
    Err(ErrorResponse::from(worktree_error(
        format!("Source directory is not part of the project: {from}"),
        None,
    )))
}

/// Validate a single-component worktree name.
fn validate_name(raw: &str) -> Result<String, ErrorResponse> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(ErrorResponse::from(worktree_error(
            "A worktree name is required",
            None,
        )));
    }
    let portable = !name.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
    });
    let mut components = Path::new(name).components();
    if portable && let (Some(Component::Normal(_)), None) = (components.next(), components.next()) {
        return Ok(name.to_owned());
    }
    Err(ErrorResponse::from(worktree_error(
        format!("Invalid worktree name: {raw}"),
        None,
    )))
}

pub(super) fn worktree_name(
    requested: Option<&str>,
    branch: Option<&str>,
) -> Result<String, ErrorResponse> {
    if let Some(requested) = requested {
        return validate_name(requested);
    }
    if let Some(branch) = branch {
        let last = branch.rsplit('/').next().unwrap_or(branch);
        let cleaned: String = last
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        let cleaned = cleaned.trim_matches(['-', '.']);
        if !cleaned.is_empty() {
            return Ok(cleaned.to_owned());
        }
    }
    let millis = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis(),
        Err(_) => 0,
    };
    Ok(format!("worktree-{millis}"))
}

/// First free `base/name[-N]`, refusing after ten taken names.
pub(super) async fn available_target(base: &Path, name: &str) -> Result<PathBuf, ApiError> {
    let first = base.join(name);
    if tokio::fs::symlink_metadata(&first).await.is_err() {
        return Ok(first);
    }
    for suffix in 2..=MAX_NAME_SUFFIX {
        let candidate = base.join(format!("{name}-{suffix}"));
        if tokio::fs::symlink_metadata(&candidate).await.is_err() {
            return Ok(candidate);
        }
    }
    Err(worktree_error(
        format!("Worktree directory already exists: {}", first.display()),
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_reject_separators_and_parent_components() {
        assert!(validate_name("api").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
        assert_eq!(
            worktree_name(None, Some("feature/task-1")).unwrap(),
            "task-1"
        );
        assert_eq!(
            worktree_name(Some(" named "), Some("ignored")).unwrap(),
            "named"
        );
    }
}
