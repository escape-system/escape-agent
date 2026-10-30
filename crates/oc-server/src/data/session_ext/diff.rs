//! `GET /api/session/{sid}/diff`: staged revert diffs recomputed against the
//! current tree.

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::{query_error, require_store};
use crate::error::ErrorResponse;

use super::common::{message_not_found, require_session};
use super::revert::{boundary_seq, revert_diffs};

/// `GET /api/session/:sid/diff`
#[derive(Debug, Default, Deserialize)]
pub struct DiffParams {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // accepted; diffs are recomputed with a single full-file hunk
    context: Option<String>,
}

/// `GET /api/session/:sid/diff`
pub async fn diff(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    params: Result<Query<DiffParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    for message_id in [params.from.as_deref(), params.to.as_deref()]
        .into_iter()
        .flatten()
    {
        if store.get_message(&session_id, message_id).await?.is_none() {
            return Err(message_not_found(&session_id, message_id).into());
        }
    }

    // A staged revert recomputes against the tree, so a later edit is visible
    // instead of being masked by the cached list stored on staging.
    let mut files = Vec::new();
    if let Some(message_id) = session.pointer("/revert/messageID").and_then(Value::as_str)
        && let Some(seq) = boundary_seq(store, &session_id, message_id).await?
    {
        files = revert_diffs(store, &state, &session, &session_id, seq).await?;
    }
    Ok(Json(json!({ "data": files })))
}
