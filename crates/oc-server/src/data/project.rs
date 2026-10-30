//! Project routes.
//!
//! `GET /api/project` is a bare `Project.Info[]` and `PATCH /api/project/:id`
//! returns the bare updated `Project.Info`, matching
//! `packages/client/src/promise/generated/types.ts`.
//!
//! The store keeps a minimal row (`id`, `directory`, `canonical`, `name`,
//! `icon`, `time_created`, `time_updated`, `archived`). This module widens it
//! into the upstream shape the generated client validates:
//!
//! - `time.active` is the last update, the closest recorded activity time.
//! - `time.archived` is the last update when the project is archived; the exact
//!   archive time is not stored.
//! - `sandboxes` is empty: no sandboxes exist in this server.
//! - `vcs` is detected from the `.git`/`.hg` markers in the canonical directory.

use std::path::Path;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path as AxumPath, Query, State};
use oc_store::ProjectPatch;
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::{query_error, require_store, schema_error};
use crate::error::ErrorResponse;

/// `GET /api/project`
#[derive(Debug, Deserialize)]
pub struct ListParams {
    /// Literal `true`/`false`, like every other wire boolean.
    archived: Option<String>,
}

/// Body of `PATCH /api/project/:id`.
///
/// `canonical` and `commands` are accepted because the generated client sends
/// them, but only when they match what is already stored: the store has no
/// column for either, and pretending to move a project would be a lie.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UpdatePayload {
    canonical: Option<String>,
    name: Option<String>,
    icon: Option<Value>,
    commands: Option<Value>,
    archived: Option<bool>,
}

/// `GET /api/project`
pub async fn list(
    State(state): State<AppState>,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let archived = match params.archived.as_deref() {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(other) => {
            return Err(ErrorResponse::from(ApiError::new(
                400,
                "SchemaError",
                format!("Invalid archived value {other:?}; expected true or false"),
            )));
        }
    };
    let store = require_store(&state)?;
    let mut rows = store.list_projects(archived).await?;
    if archived.is_none() {
        // Upstream hides archived projects unless `archived=true`.
        rows.retain(|row| row.get("archived").and_then(Value::as_bool) != Some(true));
    }
    let projects = widen_all(&rows).await;
    Ok(Json(Value::Array(projects)))
}

/// `PATCH /api/project/:id`
pub async fn update(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
    body: Result<Json<UpdatePayload>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let Some(current) = store.get_project(&id).await? else {
        return Err(ErrorResponse::from(project_not_found(&id)));
    };

    if let Some(canonical) = payload.canonical.as_deref()
        && current.get("canonical").and_then(Value::as_str) != Some(canonical)
    {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "BadRequestError",
            "Moving a project is not supported",
        )));
    }
    if payload
        .commands
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|commands| !commands.is_empty())
    {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "BadRequestError",
            "Workspace commands are not supported",
        )));
    }

    let patch = ProjectPatch {
        name: payload.name,
        icon: payload.icon.as_ref().and_then(icon_to_storage),
        archived: payload.archived,
    };
    let Some(updated) = store.update_project(&id, patch).await? else {
        return Err(ErrorResponse::from(project_not_found(&id)));
    };
    Ok(Json(widen(&updated).await))
}

fn project_not_found(id: &str) -> ApiError {
    ApiError::not_found("ProjectNotFoundError", format!("Project not found: {id}"))
        .with("projectID", json!(id))
}

/// Store value for the `icon` column: the URL when the caller sent the
/// upstream `{url?,override?,color?}` object, else the raw string.
fn icon_to_storage(icon: &Value) -> Option<String> {
    match icon {
        Value::Null => None,
        Value::String(url) => Some(url.clone()),
        Value::Object(_) => serde_json::to_string(icon).ok(),
        other => Some(other.to_string()),
    }
}

/// Storage value back to the upstream icon object.
fn icon_from_storage(stored: &str) -> Value {
    if stored.is_empty() {
        return Value::Null;
    }
    match serde_json::from_str::<Value>(stored) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({"url": stored}),
    }
}

async fn widen_all(rows: &[Value]) -> Vec<Value> {
    let mut projects = Vec::with_capacity(rows.len());
    for row in rows {
        projects.push(widen(row).await);
    }
    projects
}

async fn widen(row: &Value) -> Value {
    let canonical = row
        .get("canonical")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let time_updated = row
        .pointer("/time/updated")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let archived = row.get("archived").and_then(Value::as_bool) == Some(true);

    let mut map = Map::new();
    if let Some(id) = row.get("id") {
        map.insert("id".to_owned(), id.clone());
    }
    if let Some(directory) = row.get("directory") {
        map.insert("directory".to_owned(), directory.clone());
    }
    map.insert("canonical".to_owned(), json!(canonical));
    if let Some(vcs) = detect_vcs(canonical).await {
        map.insert("vcs".to_owned(), json!(vcs));
    }
    if let Some(name) = row.get("name").cloned() {
        map.insert("name".to_owned(), name);
    }
    if let Some(icon) = row.get("icon").and_then(Value::as_str) {
        let icon = icon_from_storage(icon);
        if !icon.is_null() {
            map.insert("icon".to_owned(), icon);
        }
    }
    let mut time = Map::new();
    time.insert(
        "created".to_owned(),
        row.pointer("/time/created").cloned().unwrap_or(json!(0)),
    );
    time.insert("updated".to_owned(), json!(time_updated));
    time.insert("active".to_owned(), json!(time_updated));
    if archived {
        time.insert("archived".to_owned(), json!(time_updated));
    }
    map.insert("time".to_owned(), Value::Object(time));
    map.insert("sandboxes".to_owned(), json!([]));
    Value::Object(map)
}

/// The VCS provider marker upstream resolves once per project root.
async fn detect_vcs(canonical: &str) -> Option<&'static str> {
    let directory = Path::new(canonical);
    if tokio::fs::try_exists(directory.join(".git")).await.ok()? {
        return Some("git");
    }
    if tokio::fs::try_exists(directory.join(".hg")).await.ok()? {
        return Some("hg");
    }
    None
}
