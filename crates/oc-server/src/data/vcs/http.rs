//! HTTP handlers for the location-scoped VCS routes.

use std::collections::HashMap;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use oc_types::{ApiError, LocationEnvelope};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::location;
use crate::data::session::LocationQuery;
use crate::data::{no_content, query_error};
use crate::error::ErrorResponse;

use super::DEFAULT_CONTEXT;
use super::diff::{diff_rows, untracked_stat};
use super::git::{
    current_branch, default_branch, git_failure, git_text, has_head, is_directory, repository_root,
    run_git,
};
use super::refs::base_value;
use super::status::{numstat, status_changes};

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
