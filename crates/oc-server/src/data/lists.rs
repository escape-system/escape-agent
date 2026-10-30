//! Truthful list routes for subsystems that are not implemented yet.
//!
//! The remaining handlers answer with the exact upstream envelope and a
//! truthful value: no web-search provider is registered and the account is
//! signed out of a Console workspace. `GET /api/form` is backed by the durable
//! form table.
//!
//! Routes backed by a real manager live in [`crate::data::mcp`],
//! [`crate::data::lsp`], [`crate::data::plugin`], [`crate::data::shells`],
//! [`crate::data::integration`] and [`crate::data::worktree`].

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use oc_types::LocationEnvelope;
use serde_json::Value;

use crate::AppState;
use crate::data::location;
use crate::data::session::LocationQuery;
use crate::data::{query_error, require_store};
use crate::error::ErrorResponse;

/// Resolve the location and wrap `data` in the location envelope.
fn envelope(
    state: &AppState,
    headers: &HeaderMap,
    directory: Option<&str>,
    data: Value,
) -> Json<LocationEnvelope<Value>> {
    let resolved = location::resolve(state, headers, None, directory);
    Json(LocationEnvelope::new(resolved.reference(), data))
}

fn empty_list() -> Value {
    Value::Array(Vec::new())
}

/// `GET /api/form` — pending forms across all sessions, newest first.
pub async fn forms(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    let forms = store.list_forms(None).await?;
    Ok(envelope(
        &state,
        &headers,
        params.directory.as_deref(),
        Value::Array(forms),
    ))
}

/// `GET /api/experimental/usage/limits` — signed out of the Console workspace.
#[allow(clippy::unused_async)] // axum handlers must be async; there is nothing to await
pub async fn usage_limits(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    Ok(envelope(
        &state,
        &headers,
        params.directory.as_deref(),
        Value::Null,
    ))
}

/// `GET /api/websearch/provider` — no web-search provider is registered.
#[allow(clippy::unused_async)] // axum handlers must be async; there is nothing to await
pub async fn websearch_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    Ok(envelope(
        &state,
        &headers,
        params.directory.as_deref(),
        empty_list(),
    ))
}
