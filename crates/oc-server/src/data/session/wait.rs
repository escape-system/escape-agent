//! `/experimental/session/{sid}/wait` idle wait.

use std::time::Duration;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::response::Response;

use super::bodies::WaitParams;
use crate::AppState;
use crate::data::{
    no_content, query_error, require_store, runtime_error, runtime_unavailable, session_not_found,
};
use crate::error::ErrorResponse;

/// `POST /api/experimental/session/:sid/wait`
pub async fn wait(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    params: Result<Query<WaitParams>, QueryRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    let Some(runtime) = state.runtime() else {
        return Err(runtime_unavailable().into());
    };
    let raw_timeout = params.timeout.unwrap_or(30.0);
    let timeout_secs = if raw_timeout.is_finite() {
        raw_timeout.clamp(0.0, 600.0)
    } else {
        30.0
    };
    let timeout = Duration::from_secs_f64(timeout_secs);
    runtime
        .wait_idle(&session_id, timeout)
        .await
        .map_err(runtime_error)?;
    Ok(no_content())
}
