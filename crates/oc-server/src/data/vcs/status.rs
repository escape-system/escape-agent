//! Working-copy status and per-file diff statistics.

use std::collections::HashMap;
use std::path::Path;

use oc_types::ApiError;

use super::MAX_ENTRIES;
use super::git::{git_failure, run_git};

/// One working-copy entry, root-relative like `git status --porcelain`.
#[derive(Debug, Clone)]
pub(super) struct Change {
    pub(super) file: String,
    pub(super) status: &'static str,
}

/// Upstream's `kind()` mapping.
fn kind(code: &[u8]) -> &'static str {
    if code == b"??" {
        return "added";
    }
    let has = |byte: u8| code.contains(&byte);
    if has(b'U') {
        "modified"
    } else if has(b'A') && !has(b'D') {
        "added"
    } else if has(b'D') && !has(b'A') {
        "deleted"
    } else {
        "modified"
    }
}

/// `git status --porcelain=v1 -z` restricted to the location.
pub(super) async fn status_changes(
    directory: &Path,
    untracked: &str,
) -> Result<Vec<Change>, ApiError> {
    let flag = format!("--untracked-files={untracked}");
    let output = run_git(
        directory,
        &[
            "status",
            "--porcelain=v1",
            &flag,
            "--no-renames",
            "-z",
            "--",
            ".",
        ],
        MAX_ENTRIES * 8,
    )
    .await?;
    if !output.ok() {
        return Err(git_failure("git status", &output));
    }
    let mut changes = Vec::new();
    for chunk in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|chunk| chunk.len() > 3)
        .take(MAX_ENTRIES)
    {
        let file = String::from_utf8_lossy(&chunk[3..]).into_owned();
        if !file.is_empty() {
            changes.push(Change {
                file,
                status: kind(&chunk[..2]),
            });
        }
    }
    Ok(changes)
}

/// `git diff --numstat -z` statistics keyed by root-relative path.
pub(super) async fn numstat(
    directory: &Path,
    reference: &str,
    target: Option<&str>,
) -> Result<HashMap<String, (u64, u64)>, ApiError> {
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-renames",
        "--numstat",
        "-z",
        reference,
    ];
    if let Some(target) = target {
        args.push(target);
    }
    args.extend(["--", "."]);
    let output = run_git(directory, &args, MAX_ENTRIES * 16).await?;
    if !output.ok() {
        return Err(git_failure("git diff --numstat", &output));
    }
    let mut stats = HashMap::new();
    for chunk in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
    {
        let mut fields = chunk.splitn(3, |byte| *byte == b'\t');
        let (Some(additions), Some(deletions), Some(file)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let count = |bytes: &[u8]| -> u64 {
            std::str::from_utf8(bytes)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
        };
        stats.insert(
            String::from_utf8_lossy(file).into_owned(),
            (count(additions), count(deletions)),
        );
    }
    Ok(stats)
}

/// `git diff --name-status -z` entries.
pub(super) async fn diff_names(
    directory: &Path,
    reference: &str,
    target: Option<&str>,
) -> Result<Vec<Change>, ApiError> {
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-renames",
        "--name-status",
        "-z",
        reference,
    ];
    if let Some(target) = target {
        args.push(target);
    }
    args.extend(["--", "."]);
    let output = run_git(directory, &args, MAX_ENTRIES * 8).await?;
    if !output.ok() {
        return Err(git_failure("git diff --name-status", &output));
    }
    let fields = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .collect::<Vec<_>>();
    let mut changes = Vec::new();
    for pair in fields.chunks(2).take(MAX_ENTRIES) {
        let [code, file] = pair else { continue };
        let file = String::from_utf8_lossy(file).into_owned();
        if !file.is_empty() {
            changes.push(Change {
                file,
                status: kind(code),
            });
        }
    }
    Ok(changes)
}
