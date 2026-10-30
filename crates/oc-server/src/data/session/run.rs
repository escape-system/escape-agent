//! Runtime control: interrupt and detach to background.

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use serde_json::{Value, json};

use super::bodies::InterruptParams;
use crate::AppState;
use crate::data::{
    no_content, query_error, require_store, runtime_error, runtime_unavailable, session_not_found,
};
use crate::error::ErrorResponse;

/// `POST /api/session/:sid/interrupt`
pub async fn interrupt(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    params: Result<Query<InterruptParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    let Some(runtime) = state.runtime() else {
        return Ok(Json(json!({ "interrupted": false })));
    };
    let interrupted = runtime
        .interrupt(&session_id)
        .await
        .map_err(runtime_error)?;
    Ok(Json(json!({ "interrupted": interrupted })))
}

/// `POST /api/session/:sid/background`
pub async fn background(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    // Execution is actor-based: the handler never runs a turn, so marking the
    // running execution as backgrounded is the detach. The turn's foreground
    // `subagent` wait polls the flag, returns `status: running`, and the child
    // keeps going. With no running execution there is nothing to detach, which
    // is still a 204.
    let runtime = state.runtime().ok_or_else(runtime_unavailable)?;
    runtime
        .request_background(&session_id)
        .map_err(runtime_error)?;
    Ok(no_content())
}
