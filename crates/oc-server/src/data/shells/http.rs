//! `/api/shell` HTTP handlers: create, list, inspect, page output and delete.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use oc_types::{ApiError, EventLocation, LocationEnvelope};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::location;
use crate::data::session::LocationQuery;
use crate::data::{no_content, query_error, require_store, schema_error, session_not_found};
use crate::error::ErrorResponse;

use super::OUTPUT_LIMIT;
use super::manager::{CreateShell, manager};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CreateBody {
    command: String,
    cwd: Option<String>,
    /// Milliseconds; zero or absent means no timeout.
    timeout: Option<u64>,
    metadata: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OutputQuery {
    #[serde(rename = "location[directory]")]
    directory: Option<String>,
    cursor: Option<String>,
    limit: Option<String>,
}

/// `GET /api/shell`
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let shells = manager().list(Some(resolved.directory.as_str()));
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        json!(shells),
    )))
}

/// `POST /api/shell`
pub(crate) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
    body: Result<Json<CreateBody>, JsonRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let metadata = metadata_object(payload.metadata)?;
    let cwd = payload
        .cwd
        .filter(|value| !value.is_empty())
        .map_or_else(|| resolved.path(), PathBuf::from);
    let info = manager()
        .create(CreateShell {
            command: payload.command,
            cwd,
            shell: configured_shell(&state),
            env: BTreeMap::new(),
            timeout: payload.timeout.map(Duration::from_millis),
            metadata,
            bus: Some(state.bus().clone()),
            location: Some(resolved.event()),
            location_directory: Some(resolved.directory.clone()),
        })
        .await
        .map_err(ErrorResponse::from)?;
    Ok(Json(LocationEnvelope::new(resolved.reference(), info)))
}

/// `GET /api/shell/:id`
pub async fn get_one(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    if !manager().owned_by(&id, &resolved.directory) {
        return Err(ErrorResponse::from(shell_not_found(&id)));
    }
    let info = manager()
        .get(&id)
        .ok_or_else(|| ErrorResponse::from(shell_not_found(&id)))?;
    Ok(Json(LocationEnvelope::new(resolved.reference(), info)))
}

/// `GET /api/shell/:id/output`
pub(crate) async fn output(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<OutputQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let cursor = parse_number(params.cursor.as_deref(), "cursor")?;
    let limit = match params.limit.as_deref() {
        Some(raw) => usize::try_from(parse_number(Some(raw), "limit")?).map_err(|_| {
            ErrorResponse::from(ApiError::new(
                400,
                "InvalidRequestError",
                "limit is out of range",
            ))
        })?,
        None => OUTPUT_LIMIT,
    };
    let page = manager()
        .output(&id, cursor, limit)
        .map_err(ErrorResponse::from)?;
    Ok(Json(LocationEnvelope::new(resolved.reference(), page)))
}

/// `DELETE /api/shell/:id`
pub async fn remove(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    if !manager().owned_by(&id, &resolved.directory) {
        return Err(ErrorResponse::from(shell_not_found(&id)));
    }
    manager().remove(&id).await.map_err(ErrorResponse::from)?;
    Ok(no_content())
}

#[derive(Debug, Deserialize)]
pub(crate) struct SessionShellBody {
    #[serde(default)]
    id: Option<String>,
    command: String,
}

/// `POST /api/session/:sid/shell`
pub(crate) async fn session_shell(
    State(state): State<AppState>,
    AxumPath(sid): AxumPath<String>,
    body: Result<Json<SessionShellBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let session = store
        .get_session(&sid)
        .await?
        .ok_or_else(|| session_not_found(&sid))?;
    let cwd = session
        .get("location")
        .and_then(|location| location.get("directory"))
        .and_then(Value::as_str)
        .map_or_else(|| state.directory().to_path_buf(), PathBuf::from);
    let mut metadata = Map::new();
    metadata.insert("sessionID".to_owned(), json!(sid));
    metadata.insert("background".to_owned(), json!(true));
    if let Some(id) = payload.id {
        metadata.insert("messageID".to_owned(), json!(id));
    }
    let location = EventLocation {
        directory: cwd.to_string_lossy().into_owned(),
    };
    let location_directory = Some(location.directory.clone());
    let mut env = BTreeMap::new();
    if let Some(variables) = session
        .get("metadata")
        .and_then(|metadata| metadata.get("environment"))
        .and_then(Value::as_object)
    {
        for (key, value) in variables {
            if let Some(value) = value.as_str() {
                env.insert(key.clone(), value.to_owned());
            }
        }
    }
    env.insert("OPENCODE_SESSION_ID".to_owned(), sid);
    manager()
        .create(CreateShell {
            command: payload.command,
            cwd,
            shell: configured_shell(&state),
            env,
            timeout: None,
            metadata: Value::Object(metadata),
            bus: Some(state.bus().clone()),
            location: Some(location),
            location_directory,
        })
        .await
        .map_err(ErrorResponse::from)?;
    Ok(no_content())
}

fn configured_shell(state: &AppState) -> Option<String> {
    state.config().map(|config| config.shell())
}

fn metadata_object(metadata: Option<Value>) -> Result<Value, ErrorResponse> {
    match metadata {
        None | Some(Value::Null) => Ok(json!({})),
        Some(value @ Value::Object(_)) => Ok(value),
        Some(_) => Err(ErrorResponse::from(ApiError::new(
            400,
            "InvalidRequestError",
            "metadata must be an object",
        ))),
    }
}

fn parse_number(raw: Option<&str>, field: &str) -> Result<u64, ErrorResponse> {
    match raw {
        None => Ok(0),
        Some(raw) => raw.parse::<u64>().map_err(|_| {
            ErrorResponse::from(ApiError::new(
                400,
                "InvalidRequestError",
                format!("{field} must be a non-negative integer"),
            ))
        }),
    }
}

pub(super) fn shell_not_found(id: &str) -> ApiError {
    ApiError::not_found(
        "ShellNotFoundError",
        format!("shell command not found: {id}"),
    )
    .with("id", json!(id))
}
