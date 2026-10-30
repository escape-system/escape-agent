//! `GET /api/session/{sid}/context`: the model-visible tail of the transcript.

use axum::Json;
use axum::extract::{Path, State};
use serde_json::{Value, json};

use crate::AppState;
use crate::data::require_store;
use crate::error::ErrorResponse;

use super::common::{messages_all, require_session};

/// `GET /api/session/:sid/context`
pub async fn context(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    let messages = messages_all(store, &session_id).await?;

    // The model view starts after the newest completed compaction.
    let start = messages
        .iter()
        .rposition(|message| {
            message.get("type").and_then(Value::as_str) == Some("compaction")
                && message
                    .get("status")
                    .and_then(Value::as_str)
                    .is_none_or(|status| status == "completed")
        })
        .map_or(0, |index| index + 1);

    Ok(Json(json!({ "data": &messages[start..] })))
}
