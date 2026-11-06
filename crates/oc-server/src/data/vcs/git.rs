//! Git plumbing for VCS routes: the runner, repository discovery and branches.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use oc_types::ApiError;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::data::git::GitOutput;

use super::{MAX_STDERR, TIMEOUT};

pub(super) fn git_failure(operation: &str, output: &GitOutput) -> ApiError {
    let message = output.message();
    if message.is_empty() {
        ApiError::service_unavailable(format!("{operation} failed"))
    } else {
        ApiError::service_unavailable(format!("{operation} failed: {message}"))
    }
}

/// Run git with a deadline and an output cap.
pub(super) async fn run_git(
    directory: &Path,
    args: &[&str],
    limit: usize,
) -> Result<GitOutput, ApiError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ApiError::service_unavailable("git executable not available")
        } else {
            tracing::error!(error = %error, "failed to spawn git");
            ApiError::internal("Internal error")
        }
    })?;
    let Some(stdout) = child.stdout.take() else {
        return Err(ApiError::internal("Internal error"));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(ApiError::internal("Internal error"));
    };

    let mut out = Vec::new();
    let mut err = Vec::new();
    let read_stdout = async {
        let mut stdout_take = stdout.take(limit as u64 + 1);
        let res = stdout_take.read_to_end(&mut out).await;
        drop(stdout_take);
        res
    };
    let read_stderr = async {
        let mut stderr_take = stderr.take(MAX_STDERR as u64);
        let res = stderr_take.read_to_end(&mut err).await;
        drop(stderr_take);
        res
    };
    let (out_result, err_result) =
        tokio::time::timeout(TIMEOUT, async { tokio::join!(read_stdout, read_stderr) })
            .await
            .map_err(|_| ApiError::service_unavailable("git command timed out after 30 s"))?;
    let read_error = |error: std::io::Error| {
        tracing::error!(error = %error, "failed to read git output");
        ApiError::internal("Internal error")
    };
    out_result.map_err(read_error)?;
    err_result.map_err(read_error)?;

    let truncated = out.len() > limit;
    if truncated {
        out.truncate(limit);
        let _ = child.kill().await;
    }
    let status = child.wait().await.map_err(|error| {
        tracing::error!(error = %error, "failed to wait for git");
        ApiError::internal("Internal error")
    })?;

    Ok(GitOutput {
        code: status.code(),
        stdout: out,
        stderr: String::from_utf8_lossy(&err).into_owned(),
        truncated,
    })
}

pub(super) async fn git_text(
    directory: &Path,
    args: &[&str],
    limit: usize,
) -> Result<Option<String>, ApiError> {
    let output = run_git(directory, args, limit).await?;
    Ok(output.ok().then(|| output.text()))
}

pub(super) async fn is_directory(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
}

/// Repository root containing `directory`, or `None` when it is not a work tree.
pub(super) async fn repository_root(directory: &Path) -> Result<Option<PathBuf>, ApiError> {
    if !is_directory(directory).await {
        return Ok(None);
    }
    let Some(text) = git_text(directory, &["rev-parse", "--show-toplevel"], 4096).await? else {
        return Ok(None);
    };
    let root = text.trim();
    Ok((!root.is_empty()).then(|| PathBuf::from(root)))
}

pub(super) async fn has_head(directory: &Path) -> Result<bool, ApiError> {
    Ok(
        git_text(directory, &["rev-parse", "--verify", "HEAD"], 4096)
            .await?
            .is_some(),
    )
}

pub(super) async fn current_branch(directory: &Path) -> Result<Option<String>, ApiError> {
    let Some(text) = git_text(
        directory,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        4096,
    )
    .await?
    else {
        return Ok(None);
    };
    let branch = text.trim();
    Ok((!branch.is_empty()).then(|| branch.to_owned()))
}

#[derive(Debug, Clone)]
pub(super) struct DefaultBranch {
    pub(super) name: String,
    pub(super) reference: String,
}

async fn primary_remote(directory: &Path) -> Result<Option<String>, ApiError> {
    let Some(text) = git_text(directory, &["remote"], 4096).await? else {
        return Ok(None);
    };
    let mut remotes = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if remotes.contains(&"origin") {
        return Ok(Some("origin".to_owned()));
    }
    Ok(remotes.drain(..).next().map(str::to_owned))
}

async fn local_branches(directory: &Path) -> Result<Vec<String>, ApiError> {
    let Some(text) = git_text(
        directory,
        &["for-each-ref", "--format=%(refname:lstrip=2)", "refs/heads"],
        64 * 1024,
    )
    .await?
    else {
        return Ok(Vec::new());
    };
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Upstream's default-branch resolution: the primary remote's `HEAD`, then the
/// configured `init.defaultBranch`, then `main`, then `master`.
pub(super) async fn default_branch(directory: &Path) -> Result<Option<DefaultBranch>, ApiError> {
    if let Some(remote) = primary_remote(directory).await? {
        let remote_head = format!("refs/remotes/{remote}/HEAD");
        if let Some(text) = git_text(directory, &["symbolic-ref", &remote_head], 4096).await? {
            let reference = text.trim().to_owned();
            if let Some(rest) = reference.strip_prefix("refs/remotes/")
                && let Some(branch) = rest.strip_prefix(&format!("{remote}/"))
                && !branch.is_empty()
            {
                return Ok(Some(DefaultBranch {
                    name: branch.to_owned(),
                    reference,
                }));
            }
        }
    }

    let locals = local_branches(directory).await?;
    if let Some(text) =
        git_text(directory, &["config", "--get", "init.defaultBranch"], 4096).await?
    {
        let name = text.trim();
        if !name.is_empty() && locals.iter().any(|local| local == name) {
            return Ok(Some(DefaultBranch {
                name: name.to_owned(),
                reference: format!("refs/heads/{name}"),
            }));
        }
    }
    for candidate in ["main", "master"] {
        if locals.iter().any(|local| local == candidate) {
            return Ok(Some(DefaultBranch {
                name: candidate.to_owned(),
                reference: format!("refs/heads/{candidate}"),
            }));
        }
    }
    Ok(None)
}

pub(super) async fn merge_base(directory: &Path, base: &str) -> Result<Option<String>, ApiError> {
    let Some(text) = git_text(
        directory,
        &["merge-base", "--end-of-options", base, "HEAD"],
        4096,
    )
    .await?
    else {
        return Ok(None);
    };
    let commit = text.trim();
    Ok((!commit.is_empty()).then(|| commit.to_owned()))
}
