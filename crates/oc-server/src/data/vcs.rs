//! Location-scoped VCS routes backed by the real `git` executable.
//!
//! Every invocation is an argv array (no shell) run with `git -C <dir>`, a
//! 10 s deadline and an output cap; the process is killed when either limit is
//! hit. Listing commands run inside the requested location and scope their
//! pathspec to it, while per-file patch commands run at the repository root
//! because git reports root-relative paths.
//!
//! A location outside a repository is not an error: `GET /api/vcs` answers with
//! an empty `branch` object and no `provider`, exactly like upstream's
//! no-provider state, and the change routes report no changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use oc_types::{ApiError, LocationEnvelope};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::AppState;
use crate::data::location;
use crate::data::session::LocationQuery;
use crate::data::{no_content, query_error};
use crate::error::ErrorResponse;

/// Deadline for one git invocation.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Working-copy entries reported by `status` and `diff`.
const MAX_ENTRIES: usize = 2000;
/// Total patch bytes reported by `diff`.
const MAX_DIFF_BYTES: usize = 1024 * 1024;
/// Patch bytes requested for a single untracked file.
const MAX_FILE_PATCH_BYTES: usize = 256 * 1024;
/// Files above this size are reported as binary, like `git --numstat`.
const MAX_STAT_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_CONTEXT: u32 = 3;
const MAX_STDERR: usize = 8 * 1024;

#[derive(Debug)]
struct GitOutput {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
    truncated: bool,
}

impl GitOutput {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Best available diagnostic: stderr, then stdout.
    fn message(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_owned();
        }
        self.text().trim().to_owned()
    }
}

fn git_failure(operation: &str, output: &GitOutput) -> ApiError {
    let message = output.message();
    if message.is_empty() {
        ApiError::service_unavailable(format!("{operation} failed"))
    } else {
        ApiError::service_unavailable(format!("{operation} failed: {message}"))
    }
}

/// Run git with a deadline and an output cap.
async fn run_git(directory: &Path, args: &[&str], limit: usize) -> Result<GitOutput, ApiError> {
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
    let Some(mut stdout) = child.stdout.take() else {
        return Err(ApiError::internal("Internal error"));
    };
    let Some(mut stderr) = child.stderr.take() else {
        return Err(ApiError::internal("Internal error"));
    };

    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut stdout_take = (&mut stdout).take(limit as u64 + 1);
    let mut stderr_take = (&mut stderr).take(MAX_STDERR as u64);
    let read = async {
        tokio::join!(
            stdout_take.read_to_end(&mut out),
            stderr_take.read_to_end(&mut err),
        )
    };
    let (out_result, err_result) = tokio::time::timeout(TIMEOUT, read)
        .await
        .map_err(|_| ApiError::service_unavailable("git command timed out after 10 s"))?;
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

async fn git_text(
    directory: &Path,
    args: &[&str],
    limit: usize,
) -> Result<Option<String>, ApiError> {
    let output = run_git(directory, args, limit).await?;
    Ok(output.ok().then(|| output.text()))
}

async fn is_directory(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
}

/// Repository root containing `directory`, or `None` when it is not a work tree.
async fn repository_root(directory: &Path) -> Result<Option<PathBuf>, ApiError> {
    if !is_directory(directory).await {
        return Ok(None);
    }
    let Some(text) = git_text(directory, &["rev-parse", "--show-toplevel"], 4096).await? else {
        return Ok(None);
    };
    let root = text.trim();
    Ok((!root.is_empty()).then(|| PathBuf::from(root)))
}

async fn has_head(directory: &Path) -> Result<bool, ApiError> {
    Ok(
        git_text(directory, &["rev-parse", "--verify", "HEAD"], 4096)
            .await?
            .is_some(),
    )
}

async fn current_branch(directory: &Path) -> Result<Option<String>, ApiError> {
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
struct DefaultBranch {
    name: String,
    reference: String,
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
async fn default_branch(directory: &Path) -> Result<Option<DefaultBranch>, ApiError> {
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

/// One working-copy entry, root-relative like `git status --porcelain`.
#[derive(Debug, Clone)]
struct Change {
    file: String,
    status: &'static str,
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
async fn status_changes(directory: &Path, untracked: &str) -> Result<Vec<Change>, ApiError> {
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
async fn numstat(
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
async fn diff_names(
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
    if !output.ok() {
        return Err(git_failure("git diff", &output));
    }
    if output.truncated {
        return Ok(HashMap::new());
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
    // `git diff --no-index` exits 1 when the files differ.
    if !matches!(output.code, Some(0 | 1)) {
        return Err(git_failure("git diff --no-index", &output));
    }
    Ok(if output.truncated {
        String::new()
    } else {
        output.text()
    })
}

/// Line statistics for an untracked file. Binary files and files above the
/// size cap report zero, matching `git --numstat`'s `-` entries.
async fn untracked_stat(root: &Path, file: &str) -> (u64, u64) {
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

#[derive(Debug, Deserialize)]
pub struct BranchParams {
    #[serde(rename = "location[directory]")]
    directory: Option<String>,
    search: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct DiffParams {
    #[serde(rename = "location[directory]")]
    directory: Option<String>,
    mode: String,
    base: Option<String>,
    context: Option<u32>,
    untracked: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InitParams {
    #[serde(rename = "location[directory]")]
    directory: Option<String>,
    provider: Option<String>,
}

/// `GET /api/vcs`
pub async fn info(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let directory = resolved.path();

    let mut data = Map::new();
    let mut branch = Map::new();
    if repository_root(&directory).await?.is_some() {
        data.insert("provider".to_owned(), json!("git"));
        if let Some(current) = current_branch(&directory).await? {
            branch.insert("current".to_owned(), json!(current));
        }
        if let Some(default) = default_branch(&directory).await? {
            branch.insert("default".to_owned(), json!(default.name));
        }
    }
    data.insert("branch".to_owned(), Value::Object(branch));
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Object(data),
    )))
}

/// `GET /api/vcs/base`
pub async fn base(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let directory = resolved.path();

    let mut data = Value::Null;
    if repository_root(&directory).await?.is_some() && has_head(&directory).await? {
        data = base_value(&directory).await?;
    }
    Ok(Json(LocationEnvelope::new(resolved.reference(), data)))
}

async fn base_value(directory: &Path) -> Result<Value, ErrorResponse> {
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

/// `GET /api/vcs/status`
pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let directory = resolved.path();

    let mut entries = Vec::new();
    if let Some(root) = repository_root(&directory).await? {
        let mut changes = status_changes(&directory, "all").await?;
        changes.sort_by(|a, b| a.file.cmp(&b.file));
        let file_stats = if has_head(&directory).await? {
            numstat(&directory, "HEAD", None).await?
        } else {
            HashMap::new()
        };
        for change in changes {
            let (additions, deletions) = match file_stats.get(&change.file) {
                Some(stat) => *stat,
                None if change.status == "added" => untracked_stat(&root, &change.file).await,
                None => (0, 0),
            };
            entries.push(json!({
                "file": change.file,
                "additions": additions,
                "deletions": deletions,
                "status": change.status,
            }));
        }
    }
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Array(entries),
    )))
}

/// `GET /api/vcs/branch`
pub async fn branches(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<BranchParams>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    if params.limit == Some(0) {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "SchemaError",
            "limit must be positive",
        )));
    }
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let directory = resolved.path();

    let mut branches: Vec<String> = Vec::new();
    if repository_root(&directory).await?.is_some() {
        let text = git_text(
            &directory,
            &[
                "for-each-ref",
                "--sort=refname",
                "--sort=-committerdate",
                "--format=%(refname:lstrip=2)",
                "refs/heads",
                "refs/remotes",
            ],
            64 * 1024,
        )
        .await?
        .unwrap_or_default();
        let search = params
            .search
            .as_deref()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty());
        let limit = params.limit.unwrap_or(50).min(100);
        branches = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.ends_with("/HEAD"))
            .filter(|line| {
                search
                    .as_deref()
                    .is_none_or(|needle| line.to_lowercase().contains(needle))
            })
            .take(limit)
            .map(str::to_owned)
            .collect();
    }
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Array(branches.into_iter().map(Value::String).collect()),
    )))
}

/// `GET /api/vcs/diff`
pub async fn diff(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<DiffParams>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let context = params.context.unwrap_or(DEFAULT_CONTEXT);
    let untracked = match params.untracked.as_deref() {
        None | Some("files") => "all",
        Some("folders") => "normal",
        Some(other) => {
            return Err(ErrorResponse::from(ApiError::new(
                400,
                "SchemaError",
                format!("Invalid untracked value {other:?}; expected files or folders"),
            )));
        }
    };
    let mode = match params.mode.as_str() {
        "working" | "branch" | "committed" => params.mode.clone(),
        other => {
            return Err(ErrorResponse::from(ApiError::new(
                400,
                "SchemaError",
                format!("Invalid mode {other:?}; expected working, branch or committed"),
            )));
        }
    };
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let directory = resolved.path();

    let mut rows = Vec::new();
    if let Some(root) = repository_root(&directory).await? {
        rows = diff_rows(
            &directory,
            &root,
            &mode,
            params.base.as_deref(),
            context,
            untracked,
        )
        .await?;
    }
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Array(rows),
    )))
}

async fn diff_rows(
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

async fn merge_base(directory: &Path, base: &str) -> Result<Option<String>, ApiError> {
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

/// `POST /api/vcs/init`
pub async fn init(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<InitParams>, QueryRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let directory = resolved.path();
    if !is_directory(&directory).await {
        return Err(ErrorResponse::from(
            ApiError::new(
                400,
                "InvalidRequestError",
                "Project directory does not exist",
            )
            .with("field", json!("location")),
        ));
    }
    if repository_root(&directory).await?.is_some() {
        return Err(ErrorResponse::from(ApiError::conflict(
            "Project already has version control",
        )));
    }
    match params.provider.as_deref() {
        None | Some("git") => {}
        Some(provider) => {
            return Err(ErrorResponse::from(
                ApiError::new(
                    400,
                    "InvalidRequestError",
                    format!("Unknown VCS provider: {provider}"),
                )
                .with("field", json!("provider")),
            ));
        }
    }
    let output = run_git(&directory, &["init"], 4096).await?;
    if !output.ok() {
        return Err(ErrorResponse::from(git_failure("git init", &output)));
    }
    Ok(no_content())
}
