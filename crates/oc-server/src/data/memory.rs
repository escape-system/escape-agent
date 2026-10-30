//! Experimental memory routes: list/search, write and forget.
//!
//! Scope resolution is explicit: `scope` defaults to `project`, and a project
//! id comes from `scopeID`, the `sessionID`'s stored project, or the request
//! location. The memory service itself rejects a `project`/`agent`/`session`
//! scope without an id, so no route here can read across projects by omission.
//!
//! All routes answer `503 ServiceUnavailableError` when no
//! [`oc_memory::MemoryService`] is attached.

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use oc_memory::{ListRequest, NewMemoryInput};
use oc_store::MemoryQuery;
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::location;
use crate::data::{
    no_content, query_error, require_memory, require_store, schema_error, session_not_found,
};
use crate::error::ErrorResponse;

/// `GET /api/experimental/memory`
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    /// `global`, `project`, `agent` or `session`; defaults to `project`.
    scope: Option<String>,
    /// Scope id; required for `agent`/`session` and optional for `project`.
    #[serde(rename = "scopeID")]
    scope_id: Option<String>,
    /// Resolve the default project (and `session` scope id) from this session.
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    /// Free-text query; when present the route searches instead of listing.
    query: Option<String>,
    /// Restrict to one kind.
    kind: Option<String>,
    /// Require one tag.
    tag: Option<String>,
    /// Page size.
    limit: Option<usize>,
}

/// `POST /api/experimental/memory`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteBody {
    text: String,
    kind: Option<String>,
    scope: Option<String>,
    #[serde(rename = "scopeID")]
    scope_id: Option<String>,
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    importance: Option<u8>,
    confidence: Option<f32>,
}

/// `GET /api/experimental/memory?scope=&scopeID=&query=&kind=&tag=&limit=`
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let memory = require_memory(&state)?;
    let scope = resolve_scope_pair(
        &state,
        &headers,
        params.scope,
        params.scope_id,
        params.session_id.as_deref(),
    )
    .await?;
    let tags: Vec<String> = params.tag.into_iter().collect();
    let kinds: Vec<String> = params.kind.into_iter().collect();

    if let Some(query) = params.query.filter(|query| !query.trim().is_empty()) {
        let store = require_store(&state)?;
        let hits = store
            .search_memories(MemoryQuery {
                scopes: vec![scope],
                query: Some(query),
                kinds,
                tags,
                limit: params.limit.unwrap_or(0),
                pinned_min_importance: Some(oc_memory::PINNED_IMPORTANCE),
            })
            .await?;
        let data: Vec<Value> = hits
            .into_iter()
            .map(|hit| {
                json!({
                    "id": hit.id,
                    "kind": hit.kind,
                    "text": hit.text,
                    "tags": hit.tags,
                    "importance": hit.importance,
                    "score": hit.score,
                })
            })
            .collect();
        return Ok(Json(json!({ "data": data })));
    }

    let items = memory
        .list(ListRequest {
            scopes: vec![scope],
            kind: kinds.into_iter().next(),
            tags,
            limit: params.limit.unwrap_or(0),
            cursor: None,
        })
        .await?;
    Ok(Json(json!({ "data": items })))
}

/// `POST /api/experimental/memory`
pub async fn write(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<WriteBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let memory = require_memory(&state)?;
    let (scope, scope_id) = resolve_scope_pair(
        &state,
        &headers,
        body.scope,
        body.scope_id,
        body.session_id.as_deref(),
    )
    .await?;
    let item = memory
        .write(NewMemoryInput {
            text: body.text,
            kind: body.kind,
            scope: Some(scope),
            scope_id,
            tags: body.tags,
            importance: body.importance,
            confidence: body.confidence,
            source: Some("user".to_owned()),
            source_session: body.session_id,
            source_message: None,
        })
        .await?;
    Ok(Json(json!({ "data": item })))
}

/// `DELETE /api/experimental/memory/:id`
pub async fn forget(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let memory = require_memory(&state)?;
    if memory.forget(&id).await? {
        Ok(no_content())
    } else {
        Err(ApiError::not_found("NotFoundError", format!("Memory not found: {id}")).into())
    }
}

/// Resolve a single `(scope, scope_id)` pair.
///
/// `agent` and `session` scopes require an explicit id; a `session` scope
/// falls back to `sessionID`. A `project` scope falls back to the session's
/// stored project, then to the request location.
async fn resolve_scope_pair(
    state: &AppState,
    headers: &HeaderMap,
    scope: Option<String>,
    scope_id: Option<String>,
    session_id: Option<&str>,
) -> Result<(String, Option<String>), ErrorResponse> {
    let scope = scope
        .map(|scope| scope.trim().to_owned())
        .filter(|scope| !scope.is_empty())
        .unwrap_or_else(|| "project".to_owned());
    let scope_id = scope_id
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty());
    match scope.as_str() {
        "global" => Ok(("global".to_owned(), None)),
        "project" => {
            let id = match scope_id {
                Some(id) => id,
                None => project_id(state, headers, session_id).await?,
            };
            Ok(("project".to_owned(), Some(id)))
        }
        "agent" => scope_id.map_or_else(
            || Err(missing_scope_id("agent")),
            |id| Ok(("agent".to_owned(), Some(id))),
        ),
        "session" => scope_id
            .or_else(|| session_id.map(str::to_owned))
            .map_or_else(
                || Err(missing_scope_id("session")),
                |id| Ok(("session".to_owned(), Some(id))),
            ),
        other => Err(ApiError::bad_request(format!(
            "unknown memory scope {other:?}; expected global, project, agent or session"
        ))
        .into()),
    }
}

fn missing_scope_id(scope: &str) -> ErrorResponse {
    ApiError::bad_request(format!("memory scope {scope:?} needs a scopeID")).into()
}

/// The project id for a session, or for the request location.
async fn project_id(
    state: &AppState,
    headers: &HeaderMap,
    session_id: Option<&str>,
) -> Result<String, ErrorResponse> {
    if let Some(session_id) = session_id {
        let store = require_store(state)?;
        let session = store
            .get_session(session_id)
            .await?
            .ok_or_else(|| ErrorResponse::from(session_not_found(session_id)))?;
        let project = session
            .get("projectID")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        return Ok(project);
    }
    Ok(location::resolve(state, headers, None, None)
        .project
        .id
        .as_str()
        .to_owned())
}
