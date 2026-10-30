//! Review-base resolution from the branch reflog and the default branch.

use std::path::Path;

use oc_types::ApiError;
use serde_json::{Value, json};

use crate::error::ErrorResponse;

use super::git::{current_branch, default_branch, git_text, run_git};

pub(super) async fn base_value(directory: &Path) -> Result<Value, ErrorResponse> {
    let Some(current) = current_branch(directory).await? else {
        return Err(ApiError::service_unavailable("Choose a review base").into());
    };

    let reflog = format!("refs/heads/{current}");
    let history = git_text(
        directory,
        &[
            "reflog",
            "show",
            "--max-count=256",
            "--format=%H%x00%gs",
            &reflog,
        ],
        64 * 1024,
    )
    .await?
    .unwrap_or_default();
    let history = history
        .lines()
        .filter_map(|line| {
            let (commit, message) = line.split_once('\0')?;
            Some((commit.trim().to_owned(), message.to_owned()))
        })
        .collect::<Vec<_>>();
    let renamed = history
        .iter()
        .any(|(_, message)| message.starts_with("Branch: renamed "));
    if !renamed
        && let Some((commit, message)) = history
            .iter()
            .find(|(_, message)| message.starts_with("branch: Created from "))
        && let Some(origin) = message.strip_prefix("branch: Created from ")
        && let Some(candidate) = named_ref(directory, origin).await?
        && candidate.name != current
        && ancestor(directory, commit, "HEAD").await?
        && ancestor(directory, commit, &candidate.reference).await?
    {
        return Ok(json!({
            "name": candidate.name,
            "ref": candidate.reference,
            "source": "reflog",
        }));
    }

    let Some(default) = default_branch(directory).await? else {
        return Err(ApiError::service_unavailable("Choose a review base").into());
    };
    if current != default.name {
        return Err(ApiError::service_unavailable("Choose a review base").into());
    }
    let Some(candidate) = named_ref(directory, &default.reference).await? else {
        return Err(ApiError::service_unavailable("The default review base is unavailable").into());
    };
    Ok(json!({
        "name": candidate.name,
        "ref": candidate.reference,
        "source": "default",
    }))
}

#[derive(Debug)]
struct NamedRef {
    name: String,
    reference: String,
}

/// A branch creation hint must identify a branch, not HEAD or a revision
/// expression; upstream applies the same filter.
async fn named_ref(directory: &Path, input: &str) -> Result<Option<NamedRef>, ApiError> {
    if input == "HEAD"
        || input.ends_with("/HEAD")
        || input
            .chars()
            .any(|character| "~^:@{}\u{20}".contains(character))
    {
        return Ok(None);
    }
    let Some(text) = git_text(
        directory,
        &[
            "rev-parse",
            "--symbolic-full-name",
            "--verify",
            "--end-of-options",
            input,
        ],
        4096,
    )
    .await?
    else {
        return Ok(None);
    };
    let reference = text.trim().to_owned();
    if !(reference.starts_with("refs/heads/") || reference.starts_with("refs/remotes/"))
        || reference.ends_with("/HEAD")
    {
        return Ok(None);
    }
    let name = if let Some(name) = reference.strip_prefix("refs/heads/") {
        name.to_owned()
    } else if let Some(rest) = reference.strip_prefix("refs/remotes/") {
        rest.split_once('/')
            .map_or_else(String::new, |(_, branch)| branch.to_owned())
    } else {
        String::new()
    };
    if name.is_empty() {
        return Ok(None);
    }
    if git_text(directory, &["rev-parse", "--verify", &reference], 4096)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    Ok(Some(NamedRef { name, reference }))
}

async fn ancestor(directory: &Path, commit: &str, reference: &str) -> Result<bool, ApiError> {
    if !commit
        .chars()
        .all(|character| character.is_ascii_hexdigit())
    {
        return Ok(false);
    }
    let output = run_git(
        directory,
        &["merge-base", "--is-ancestor", commit, reference],
        4096,
    )
    .await?;
    Ok(output.ok())
}
