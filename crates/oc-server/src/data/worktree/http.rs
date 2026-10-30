//! `/api/worktree` handlers and the saved-inventory lookups they need.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::response::Response;
use oc_store::{NewWorktree, Store};
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::{no_content, query_error, require_store, schema_error};
use crate::error::ErrorResponse;

use super::git::{
    git_worktree_error, parse_porcelain, prune, remove_checkout, repository_root, run_git,
};
use super::paths::{
    canonical_or, display_path, git_path, path_key, path_matches, registered_directory,
    resolve_path, same_path,
};
use super::plan::{available_target, base_directory, source_directory, worktree_name};
use super::worktree_error;

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
