//! Patch assembly and per-file rows for `/api/vcs/diff`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oc_types::ApiError;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use super::git::{default_branch, git_failure, has_head, merge_base, run_git};
use super::status::{Change, diff_names, numstat, status_changes};
use super::{MAX_DIFF_BYTES, MAX_FILE_PATCH_BYTES, MAX_STAT_BYTES};

fn patch_args<'a>(unified: &'a str, reference: &'a str, target: Option<&'a str>) -> Vec<&'a str> {
    let mut args = vec![
        "-c",
        "core.quotepath=false",
        "diff",
        "--patch",
        "--no-ext-diff",
        "--no-renames",
        unified,
        reference,
    ];
    if let Some(target) = target {
        args.push(target);
    }
    args.extend(["--", "."]);
    args
}

/// One patch for every tracked change, split by file header.
async fn batch_patches(
    directory: &Path,
    reference: &str,
    target: Option<&str>,
    context: u32,
    limit: usize,
) -> Result<HashMap<String, String>, ApiError> {
    let unified = format!("--unified={context}");
    let output = run_git(directory, &patch_args(&unified, reference, target), limit).await?;
    if output.truncated {
        return Ok(HashMap::new());
    }
    if !output.ok() {
        return Err(git_failure("git diff", &output));
    }
    Ok(split_patch(&output.text()))
}

fn split_patch(text: &str) -> HashMap<String, String> {
    let mut chunks: HashMap<String, String> = HashMap::new();
    let mut current: Option<(String, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            let header = line.trim_end_matches(['\r', '\n']).to_owned();
            if let Some((previous, start)) = current.replace((header, offset)) {
                chunks.insert(previous, text[start..offset].to_owned());
            }
        }
        offset += line.len();
    }
    if let Some((header, start)) = current {
        chunks.insert(header, text[start..offset].to_owned());
    }
    chunks
}

fn header_for(file: &str) -> String {
    format!("diff --git a/{file} b/{file}")
}

/// Resolve a root-relative git path without escaping the repository.
fn confined(root: &Path, file: &str) -> Option<PathBuf> {
    let relative = Path::new(file);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(root.join(relative))
}

/// Patch text for one untracked file, produced by `git diff --no-index`.
async fn untracked_patch(
    root: &Path,
    file: &str,
    context: u32,
    limit: usize,
) -> Result<String, ApiError> {
    if file.ends_with('/') {
        // A folder entry has no patch of its own.
        return Ok(String::new());
    }
    if confined(root, file).is_none() {
        return Ok(String::new());
    }
    let unified = format!("--unified={context}");
    let output = run_git(
        root,
        &[
            "-c",
            "core.quotepath=false",
            "diff",
            "--no-index",
            "--patch",
            "--no-ext-diff",
            "--no-renames",
            &unified,
            "--",
            "/dev/null",
            file,
        ],
        limit,
    )
    .await?;
    if output.truncated {
        return Ok(String::new());
    }
    // `git diff --no-index` exits 1 when the files differ.
    if !matches!(output.code, Some(0 | 1)) {
        return Err(git_failure("git diff --no-index", &output));
    }
    Ok(output.text())
}

/// Line statistics for an untracked file. Binary files and files above the
/// size cap report zero, matching `git --numstat`'s `-` entries.
pub(super) async fn untracked_stat(root: &Path, file: &str) -> (u64, u64) {
    if file.ends_with('/') {
        return (0, 0);
    }
    let Some(path) = confined(root, file) else {
        return (0, 0);
    };
    let Ok(metadata) = tokio::fs::metadata(&path).await else {
        return (0, 0);
    };
    if !metadata.is_file() || metadata.len() > MAX_STAT_BYTES {
        return (0, 0);
    }
    let Ok(mut handle) = tokio::fs::File::open(&path).await else {
        return (0, 0);
    };
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut newlines = 0_u64;
    let mut total = 0_u64;
    let mut last = None;
    let mut scanned = 0_usize;
    loop {
        match handle.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                let window = &buffer[..read];
                if scanned < 8000 {
                    let probe = window.len().min(8000 - scanned);
                    if window[..probe].contains(&0) {
                        return (0, 0);
                    }
                    scanned += read;
                }
                // `split` always yields at least one part, so the newline count
                // is one less than the part count.
                let parts = window.split(|byte| *byte == b'\n').count() - 1;
                newlines += u64::try_from(parts).unwrap_or(0);
                total += u64::try_from(read).unwrap_or(0);
                last = window.last().copied();
            }
            Err(_) => return (0, 0),
        }
    }
    if total == 0 {
        return (0, 0);
    }
    let additions = if last == Some(b'\n') {
        newlines
    } else {
        newlines + 1
    };
    (additions, 0)
}

pub(super) async fn diff_rows(
    directory: &Path,
    root: &Path,
    mode: &str,
    base: Option<&str>,
    context: u32,
    untracked: &str,
) -> Result<Vec<Value>, ApiError> {
    let head = has_head(directory).await?;
    let (reference, target) = match mode {
        "working" => (head.then(|| "HEAD".to_owned()), None),
        _ if !head => return Ok(Vec::new()),
        _ => {
            let base = match base {
                Some(base) => Some(base.to_owned()),
                None => default_branch(directory)
                    .await?
                    .map(|branch| branch.reference),
            };
            let Some(base) = base else {
                return Err(ApiError::service_unavailable("No review base available"));
            };
            let Some(merge_base) = merge_base(directory, &base).await? else {
                return Err(ApiError::service_unavailable(format!(
                    "No merge base available for {base}"
                )));
            };
            let target = (mode == "committed").then(|| "HEAD".to_owned());
            (Some(merge_base), target)
        }
    };
    let target = target.as_deref();

    // Tracked changes plus, outside committed mode, the untracked entries.
    let mut items: Vec<Change> = Vec::new();
    let mut stats: HashMap<String, (u64, u64)> = HashMap::new();
    let mut patches: HashMap<String, String> = HashMap::new();
    if let Some(reference) = reference.as_deref() {
        items.extend(diff_names(directory, reference, target).await?);
        stats = numstat(directory, reference, target).await?;
        patches = batch_patches(directory, reference, target, context, MAX_DIFF_BYTES).await?;
    }
    if target.is_none() {
        for change in status_changes(directory, untracked).await? {
            if reference.is_none() || change.status == "added" {
                items.push(change);
            }
        }
    }

    // Deduplicate by path, keeping the first entry like upstream's `merge`.
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| seen.insert(item.file.clone()));
    items.sort_by(|a, b| a.file.cmp(&b.file));

    let mut total = 0_usize;
    let mut capped = false;
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let (additions, deletions) = match stats.get(&item.file) {
            Some(stat) => *stat,
            None if item.status == "added" => untracked_stat(root, &item.file).await,
            None => (0, 0),
        };
        let header = header_for(&item.file);
        let patch = if capped {
            String::new()
        } else if let Some(batched) = patches.get(&header) {
            batched.clone()
        } else if reference.is_none() || item.status == "added" {
            untracked_patch(
                root,
                &item.file,
                context,
                MAX_DIFF_BYTES
                    .saturating_sub(total)
                    .min(MAX_FILE_PATCH_BYTES),
            )
            .await?
        } else {
            String::new()
        };
        total = total.saturating_add(patch.len());
        if total >= MAX_DIFF_BYTES {
            capped = true;
        }
        rows.push(json!({
            "file": item.file,
            "patch": patch,
            "additions": additions,
            "deletions": deletions,
            "status": item.status,
        }));
    }
    Ok(rows)
}
