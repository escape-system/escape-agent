//! Project worktree routes backed by the real `git` executable.
//!
//! Every git invocation is an argv array (never a shell) with a deadline and an
//! output cap. `POST /api/worktree` creates a detached worktree under a base
//! directory, `DELETE` removes one (or every linked one when the caller omits
//! `directory`, and `POST /api/worktree/refresh` prunes stale administrative
//! entries and reconciles the saved inventory with
//! `git worktree list --porcelain`.
//!
//! # Inventory scope
//!
//! The inventory is the project's *linked* worktrees. The repository's main
//! working tree is reported by `git worktree list` too, but it is not
//! removable with `git worktree remove` and is not part of the inventory, so
//! refresh excludes it. The main path comes from
//! `git rev-parse --show-toplevel` for the project directory (the same call
//! already required to locate the repository); paths are canonicalised, with
//! the Windows verbatim prefix stripped and case folded, before comparison.
//! Any saved row that resolves to the main working tree is deleted, so a
//! previously polluted inventory heals on the next refresh.
//!
//! # Path policy
//!
//! A worktree target must live inside one of the roots the server already
//! owns: the project's canonical directory, the configured `worktree.directory`
//! (`$OPENCODE_DATA_DIR/worktree/<project>` when unset), or the global data
//! `worktree` root. Parent components are rejected before touching the
//! filesystem, and the nearest existing ancestor is canonicalised, so a symlink
//! cannot move the target outside those roots. Names are a single path
//! component: separators, `.` and `..` are refused.
//!
//! Responses are the upstream bare wire objects the generated client parses:
//! `GET` answers `Worktree.Directory[]` (`[{directory}]`) and `POST` answers
//! `Worktree.Info` (`{directory}`); the 204 routes send no body.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use oc_store::{NewWorktree, Store};
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::AppState;
use crate::data::{no_content, query_error, require_store, schema_error};
use crate::error::ErrorResponse;

/// Deadline for one git invocation.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Captured stdout limit; worktree listings are small.
const MAX_STDOUT: usize = 1024 * 1024;
/// Captured stderr limit kept for diagnostics.
const MAX_STDERR: usize = 8 * 1024;
/// Highest `-N` suffix tried when a requested worktree name is taken.
const MAX_NAME_SUFFIX: u32 = 10;

/// Worktree routes, merged into the protected router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/worktree", get(list).post(create).delete(remove))
        .route("/api/worktree/refresh", post(refresh))
}

/// `GET /api/worktree?projectID=`
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    #[serde(rename = "projectID")]
    project_id: Option<String>,
}

/// `POST /api/worktree`
#[derive(Debug, Deserialize)]
pub struct CreateBody {
    #[serde(rename = "projectID")]
    project_id: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// `DELETE /api/worktree`
#[derive(Debug, Deserialize)]
pub struct RemoveBody {
    #[serde(rename = "projectID")]
    project_id: String,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    force: bool,
}

/// `POST /api/worktree/refresh`
#[derive(Debug, Deserialize)]
pub struct RefreshBody {
    #[serde(rename = "projectID")]
    project_id: String,
}

/// One `worktree <path>` record from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListedWorktree {
    directory: PathBuf,
    branch: Option<String>,
}

/// `GET /api/worktree` — saved inventory for one project.
pub async fn list(
    State(state): State<AppState>,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let Some(project_id) = params
        .project_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
    else {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "SchemaError",
            "Missing required query parameter: projectID",
        )));
    };
    let store = require_store(&state)?;
    require_project(store, project_id).await?;
    let rows = store.list_worktrees(Some(project_id)).await?;
    // The store's wire object is exactly `{directory}`; keep it that way even
    // if bookkeeping columns appear in a row.
    let items: Vec<Value> = rows
        .iter()
        .filter_map(|row| row.get("directory").and_then(Value::as_str))
        .map(|directory| json!({ "directory": directory }))
        .collect();
    Ok(Json(Value::Array(items)))
}

/// `POST /api/worktree` — create a detached git worktree and record it.
#[allow(clippy::too_many_lines)] // resolve, check policy, create, then record is one flow
pub async fn create(
    State(state): State<AppState>,
    body: Result<Json<CreateBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let project_id = require_project_id(&body.project_id)?;
    let store = require_store(&state)?;
    let project = require_project(store, project_id).await?;
    let project_directory = project_directory(&project).await?;

    let base = base_directory(
        &state,
        project_id,
        &project_directory,
        body.directory.as_deref(),
    )
    .await?;
    let source =
        source_directory(store, project_id, &project_directory, body.from.as_deref()).await?;
    let repository = repository_root(&source).await?;
    tokio::fs::create_dir_all(&base).await.map_err(|error| {
        tracing::error!(error = %error, directory = %base.display(), "failed to create worktree base");
        ErrorResponse::from(worktree_error(
            format!("Cannot create worktree base {}: {error}", base.display()),
            None,
        ))
    })?;
    let base = tokio::fs::canonicalize(&base).await.map_err(|error| {
        ErrorResponse::from(worktree_error(
            format!("Cannot resolve worktree base {}: {error}", base.display()),
            None,
        ))
    })?;

    let name = worktree_name(body.name.as_deref(), body.branch.as_deref())?;
    let target = available_target(&base, &name).await?;
    let git_target = git_path(&target);
    let reference = body
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .unwrap_or("HEAD");
    let arguments = [
        OsString::from("worktree"),
        OsString::from("add"),
        OsString::from("--detach"),
        OsString::from("--"),
        git_target.as_os_str().to_os_string(),
        OsString::from(reference),
    ];
    let output = run_git(&repository, &arguments).await?;
    if !output.ok() {
        return Err(ErrorResponse::from(git_worktree_error(
            "create", &output, false,
        )));
    }

    let created = tokio::fs::canonicalize(&git_target).await.map_err(|error| {
        tracing::error!(error = %error, directory = %git_target.display(), "created worktree is unavailable");
        ErrorResponse::from(worktree_error(
            format!(
                "git created {} but it cannot be opened: {error}",
                git_target.display()
            ),
            None,
        ))
    })?;
    let directory = display_path(&created);
    let info = match store
        .create_worktree(NewWorktree {
            id: None,
            project_id: project_id.to_owned(),
            directory: directory.clone(),
            canonical: Some(directory),
            branch: body
                .branch
                .as_deref()
                .map(str::trim)
                .filter(|branch| !branch.is_empty())
                .map(str::to_owned),
            name: Some(name),
            time_created: None,
            time_updated: None,
        })
        .await
    {
        Ok(info) => info,
        Err(error) => {
            // The checkout exists but cannot be recorded; drop it instead of
            // leaving an untracked directory behind.
            let _ = remove_checkout(&repository, &git_target, true).await;
            return Err(ErrorResponse::from(error));
        }
    };
    Ok(Json(info))
}

/// `DELETE /api/worktree` — remove one saved worktree, or all of a project's.
pub async fn remove(
    State(state): State<AppState>,
    body: Result<Json<RemoveBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let project_id = require_project_id(&body.project_id)?;
    let store = require_store(&state)?;
    let project = require_project(store, project_id).await?;
    let project_directory = project_directory(&project).await?;

    let requested = body
        .directory
        .as_deref()
        .map(str::trim)
        .filter(|directory| !directory.is_empty());
    if let Some(requested) = requested {
        let resolved = resolve_path(Path::new(requested))
            .await
            .map_err(ErrorResponse::from)?;
        let registered = registered_directory(store, project_id, &resolved).await?;
        let Some(registered) = registered else {
            return Err(ErrorResponse::from(worktree_not_found(requested)));
        };
        let path = PathBuf::from(&registered);
        remove_registered(
            store,
            project_id,
            &project_directory,
            &registered,
            &path,
            body.force,
        )
        .await?;
    } else {
        let rows = store.list_worktrees(Some(project_id)).await?;
        for row in &rows {
            let Some(registered) = row.get("directory").and_then(Value::as_str) else {
                continue;
            };
            let path = PathBuf::from(registered);
            // The project's own checkout is the main worktree; git refuses to
            // remove it, so a collection delete leaves it in place.
            if same_path(&path, &project_directory).await {
                continue;
            }
            remove_registered(
                store,
                project_id,
                &project_directory,
                registered,
                &path,
                body.force,
            )
            .await?;
        }
    }
    Ok(no_content())
}

/// `POST /api/worktree/refresh` — prune and reconcile against git's inventory.
#[allow(clippy::too_many_lines)] // prune, list, then add and self-heal is one flow
pub async fn refresh(
    State(state): State<AppState>,
    body: Result<Json<RefreshBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let project_id = require_project_id(&body.project_id)?;
    let store = require_store(&state)?;
    let project = require_project(store, project_id).await?;
    let project_directory = project_directory(&project).await?;
    let repository = repository_root(&project_directory).await?;

    let prune_arguments = [
        OsString::from("worktree"),
        OsString::from("prune"),
        OsString::from("--expire"),
        OsString::from("now"),
    ];
    let output = run_git(&repository, &prune_arguments).await?;
    if !output.ok() {
        return Err(ErrorResponse::from(git_worktree_error(
            "prune", &output, false,
        )));
    }

    let arguments = [
        OsString::from("worktree"),
        OsString::from("list"),
        OsString::from("--porcelain"),
    ];
    let output = run_git(&repository, &arguments).await?;
    if !output.ok() {
        return Err(ErrorResponse::from(git_worktree_error(
            "list", &output, false,
        )));
    }
    let listed = parse_porcelain(&output.text());

    // The main working tree never belongs in the inventory. `--show-toplevel`
    // is the *project's* tree, which is a linked worktree when the project
    // directory lives under `.worktrees/<name>`; use the first porcelain
    // record instead (git documents the main worktree as listed first).
    let main = match listed.first() {
        Some(entry) => path_key(&canonical_or(&entry.directory).await).await,
        None => path_key(&repository).await,
    };
    // The project itself is never inventory either: a project may be rooted at
    // a linked worktree, and listing its own checkout would make it look like
    // an extra tree that DELETE refuses to remove.
    let project_key = path_key(&project_directory).await;

    let mut discovered: Vec<(String, Option<String>)> = Vec::new();
    for entry in listed {
        let path = canonical_or(&entry.directory).await;
        let key = path_key(&path).await;
        if key == main || key == project_key {
            continue;
        }
        if tokio::fs::symlink_metadata(&path).await.is_err() {
            continue;
        }
        let directory = display_path(&path);
        if discovered
            .iter()
            .any(|(existing, _)| existing == &directory)
        {
            continue;
        }
        discovered.push((directory, entry.branch));
    }

    let saved = store.list_worktrees(Some(project_id)).await?;
    for (directory, branch) in &discovered {
        let candidate = Path::new(directory);
        let mut known = false;
        for row in &saved {
            if path_matches(row, candidate).await {
                known = true;
                break;
            }
        }
        if known {
            continue;
        }
        store
            .create_worktree(NewWorktree {
                id: None,
                project_id: project_id.to_owned(),
                directory: directory.clone(),
                canonical: Some(directory.clone()),
                branch: branch.clone(),
                name: None,
                time_created: None,
                time_updated: None,
            })
            .await?;
    }

    for row in &saved {
        let Some(directory) = row.get("directory").and_then(Value::as_str) else {
            continue;
        };
        // Explicit self-heal: a row saved by an older build may name the main
        // working tree. It can never be discovered now, but delete it by name
        // so a polluted inventory converges in one refresh.
        if path_key(Path::new(directory)).await == main {
            store
                .delete_worktree_by_directory(project_id, directory)
                .await?;
            continue;
        }
        let mut still_listed = false;
        for (candidate, _) in &discovered {
            if path_matches(row, Path::new(candidate)).await {
                still_listed = true;
                break;
            }
        }
        if !still_listed {
            store
                .delete_worktree_by_directory(project_id, directory)
                .await?;
        }
    }
    Ok(no_content())
}

// ---------------------------------------------------------------- git runner

#[derive(Debug)]
struct GitOutput {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
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

/// Run git in `directory` with a deadline and bounded output.
async fn run_git(directory: &Path, arguments: &[OsString]) -> Result<GitOutput, ApiError> {
    let directory = git_path(directory);
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&directory)
        .args(arguments)
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
    let mut out_take = (&mut stdout).take(MAX_STDOUT as u64);
    let mut err_take = (&mut stderr).take(MAX_STDERR as u64);
    let read = async {
        tokio::join!(
            out_take.read_to_end(&mut out),
            err_take.read_to_end(&mut err)
        )
    };
    let (out_result, err_result) = tokio::time::timeout(GIT_TIMEOUT, read)
        .await
        .map_err(|_| ApiError::service_unavailable("git command timed out after 30 s"))?;
    let read_error = |error: std::io::Error| {
        tracing::error!(error = %error, "failed to read git output");
        ApiError::internal("Internal error")
    };
    out_result.map_err(read_error)?;
    err_result.map_err(read_error)?;

    let status = child.wait().await.map_err(|error| {
        tracing::error!(error = %error, "failed to wait for git");
        ApiError::internal("Internal error")
    })?;
    Ok(GitOutput {
        code: status.code(),
        stdout: out,
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

fn git_worktree_error(operation: &str, output: &GitOutput, force_hint: bool) -> ApiError {
    let diagnostic = output.message();
    let message = if diagnostic.is_empty() {
        format!("git worktree {operation} failed")
    } else {
        format!("git worktree {operation} failed: {diagnostic}")
    };
    let force_required = force_hint && needs_force(&diagnostic);
    worktree_error(message, force_required.then_some(true))
}

fn needs_force(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("contains modified or untracked files") || lower.contains("is dirty")
}

/// The repository root containing `directory`, or a `WorktreeError`.
async fn repository_root(directory: &Path) -> Result<PathBuf, ApiError> {
    let arguments = [
        OsString::from("rev-parse"),
        OsString::from("--show-toplevel"),
    ];
    let output = run_git(directory, &arguments).await?;
    if !output.ok() {
        let diagnostic = output.message();
        let message = if diagnostic.is_empty() {
            format!("{} is not a git repository", directory.display())
        } else {
            format!(
                "{} is not a git repository: {diagnostic}",
                directory.display()
            )
        };
        return Err(worktree_error(message, None));
    }
    let root = output.text();
    let root = root.trim();
    if root.is_empty() {
        return Err(worktree_error(
            format!("{} is not a git repository", directory.display()),
            None,
        ));
    }
    Ok(PathBuf::from(root))
}

async fn remove_checkout(repository: &Path, target: &Path, force: bool) -> Result<(), ApiError> {
    let mut arguments = vec![OsString::from("worktree"), OsString::from("remove")];
    if force {
        arguments.push(OsString::from("--force"));
    }
    arguments.push(OsString::from("--"));
    arguments.push(target.as_os_str().to_os_string());
    let output = run_git(repository, &arguments).await?;
    if output.ok() {
        Ok(())
    } else {
        Err(git_worktree_error("remove", &output, true))
    }
}

/// `git worktree prune --expire now`, ignoring failures during cleanup.
async fn prune(repository: &Path) {
    let arguments = [
        OsString::from("worktree"),
        OsString::from("prune"),
        OsString::from("--expire"),
        OsString::from("now"),
    ];
    let _ = run_git(repository, &arguments).await;
}

// -------------------------------------------------------------------- store

async fn require_project(store: &Store, project_id: &str) -> Result<Value, ErrorResponse> {
    store
        .get_project(project_id)
        .await?
        .ok_or_else(|| ErrorResponse::from(project_not_found(project_id)))
}

fn project_not_found(project_id: &str) -> ApiError {
    ApiError::not_found(
        "ProjectNotFoundError",
        format!("Project not found: {project_id}"),
    )
    .with("projectID", json!(project_id))
}

fn worktree_not_found(directory: &str) -> ApiError {
    ApiError::not_found(
        "WorktreeNotFoundError",
        format!("Worktree not found: {directory}"),
    )
    .with("directory", json!(directory))
}

fn require_project_id(raw: &str) -> Result<&str, ErrorResponse> {
    let project_id = raw.trim();
    if project_id.is_empty() {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "SchemaError",
            "Missing required field: projectID",
        )));
    }
    Ok(project_id)
}

async fn project_directory(project: &Value) -> Result<PathBuf, ErrorResponse> {
    let raw = project
        .get("canonical")
        .and_then(Value::as_str)
        .or_else(|| project.get("directory").and_then(Value::as_str))
        .filter(|directory| !directory.is_empty());
    let Some(raw) = raw else {
        return Err(ErrorResponse::from(worktree_error(
            "Project has no directory",
            None,
        )));
    };
    let path = PathBuf::from(raw);
    Ok(canonical_or(&path).await)
}

/// A registered directory that resolves to `candidate`, in stored form.
async fn registered_directory(
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

async fn path_matches(row: &Value, candidate: &Path) -> bool {
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
async fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    path_key(left).await == path_key(right).await
}

/// Comparable identity of `path`: canonical when it exists, Windows verbatim
/// prefix stripped and case folded on Windows.
async fn path_key(path: &Path) -> PathBuf {
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

// ------------------------------------------------------------------- policy

/// Canonicalise `path` (or its nearest existing ancestor plus the missing tail).
async fn resolve_path(path: &Path) -> Result<PathBuf, ApiError> {
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

async fn canonical_or(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Strip the Windows verbatim prefix so stored paths look like the client's.
fn display_path(path: &Path) -> String {
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
fn git_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(display_path(path))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

fn data_directory() -> PathBuf {
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

fn configured_worktree_directory(state: &AppState) -> Option<PathBuf> {
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
async fn allowed_roots(state: &AppState, project_directory: &Path) -> Vec<PathBuf> {
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

fn project_slug(project_id: &str) -> String {
    project_id
        .chars()
        .take(6)
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

/// Resolve the base directory a new worktree is created inside.
async fn base_directory(
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
async fn source_directory(
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

fn worktree_name(requested: Option<&str>, branch: Option<&str>) -> Result<String, ErrorResponse> {
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
async fn available_target(base: &Path, name: &str) -> Result<PathBuf, ApiError> {
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

// ---------------------------------------------------------- remove plumbing

async fn remove_registered(
    store: &Store,
    project_id: &str,
    project_directory: &Path,
    registered: &str,
    path: &Path,
    force: bool,
) -> Result<(), ErrorResponse> {
    let exists = tokio::fs::symlink_metadata(path).await.is_ok();
    let repository = repository_root(project_directory)
        .await
        .unwrap_or_else(|_| project_directory.to_path_buf());
    if exists {
        remove_checkout(&repository, path, force)
            .await
            .map_err(ErrorResponse::from)?;
    } else {
        // The checkout is already gone; drop git's stale administrative entry.
        prune(&repository).await;
    }

    let removed = store
        .delete_worktree_by_directory(project_id, registered)
        .await?;
    if !removed {
        return Err(ErrorResponse::from(worktree_not_found(registered)));
    }
    Ok(())
}

// --------------------------------------------------------------- porcelain

fn parse_porcelain(text: &str) -> Vec<ListedWorktree> {
    let mut items = Vec::new();
    let mut current: Option<ListedWorktree> = None;
    for line in text.lines() {
        if line.is_empty() {
            if let Some(item) = current.take() {
                items.push(item);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(item) = current.take() {
                items.push(item);
            }
            current = Some(ListedWorktree {
                directory: PathBuf::from(unquote_git_path(rest)),
                branch: None,
            });
        } else if let Some(rest) = line.strip_prefix("branch ")
            && let Some(item) = current.as_mut()
        {
            item.branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_owned());
        }
    }
    if let Some(item) = current.take() {
        items.push(item);
    }
    items
}

/// Decode git's C-style quoting of a path.
///
/// Octal escapes name bytes, not code points, so the result is assembled as
/// bytes and decoded as UTF-8 at the end.
fn unquote_git_path(raw: &str) -> String {
    if raw.len() < 2 || !raw.starts_with('"') || !raw.ends_with('"') {
        return raw.to_owned();
    }
    let inner = &raw[1..raw.len() - 1];
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let mut characters = inner.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            push_utf8(&mut out, character);
            continue;
        }
        match characters.next() {
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('r') => out.push(b'\r'),
            Some('b') => out.push(0x08),
            Some('f') => out.push(0x0c),
            Some('v') => out.push(0x0b),
            Some('a') => out.push(0x07),
            Some('"') => out.push(b'"'),
            Some(first @ '0'..='7') => {
                let mut value = first as u32 - '0' as u32;
                let mut digits = 1;
                while digits < 3 {
                    let Some(next @ '0'..='7') = characters.peek().copied() else {
                        break;
                    };
                    value = value * 8 + (next as u32 - '0' as u32);
                    characters.next();
                    digits += 1;
                }
                out.push(u8::try_from(value).unwrap_or(b'?'));
            }
            Some('\\') | None => out.push(b'\\'),
            Some(other) => push_utf8(&mut out, other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn push_utf8(out: &mut Vec<u8>, character: char) {
    let mut buffer = [0_u8; 4];
    out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
}

// ------------------------------------------------------------------- errors

fn worktree_error(message: impl Into<String>, force_required: Option<bool>) -> ApiError {
    let message = message.into();
    let mut data = json!({ "message": message });
    if let Some(force) = force_required {
        data["forceRequired"] = json!(force);
    }
    ApiError::new(400, "WorktreeError", message)
        .with("name", json!("WorktreeError"))
        .with("data", data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parsing_handles_blocks_and_quoting() {
        let text = concat!(
            "worktree /repo\n",
            "HEAD 0123456789abcdef\n",
            "branch refs/heads/main\n",
            "\n",
            "worktree /repo/linked tree\n",
            "HEAD abcdef0123456789\n",
            "detached\n",
            "\n",
            "worktree \"/repo/space \\\"quote\\\"\"\n",
            "HEAD fedcba9876543210\n",
            "branch refs/heads/topic/x\n",
            "\n",
        );
        let parsed = parse_porcelain(text);
        assert_eq!(
            parsed,
            vec![
                ListedWorktree {
                    directory: PathBuf::from("/repo"),
                    branch: Some("main".to_owned()),
                },
                ListedWorktree {
                    directory: PathBuf::from("/repo/linked tree"),
                    branch: None,
                },
                ListedWorktree {
                    directory: PathBuf::from("/repo/space \"quote\""),
                    branch: Some("topic/x".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn porcelain_without_trailing_blank_line_is_kept() {
        let parsed = parse_porcelain("worktree /repo\nHEAD abc\nbranch refs/heads/main\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].directory, PathBuf::from("/repo"));
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
    }

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

    #[test]
    fn octal_escapes_decode() {
        assert_eq!(unquote_git_path("\"caf\\303\\251\""), "café");
    }

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
