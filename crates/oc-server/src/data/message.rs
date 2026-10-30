//! Message transcript routes.

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use oc_store::{MessageQuery, Order};
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::{cursor_value, query_error, require_store, session_not_found};
use crate::error::ErrorResponse;

/// `GET /api/session/:sid/message`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageParams {
    limit: Option<usize>,
    order: Option<String>,
    cursor: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// `GET /api/session/:sid/message`
pub async fn list(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    params: Result<Query<MessageParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;

    let order = match params.order.as_deref() {
        Some("asc") => Order::Asc,
        None | Some("desc") => Order::Desc,
        Some(other) => {
            return Err(ApiError::new(
                400,
                "BadRequestError",
                format!("Invalid order {other:?}; expected asc or desc"),
            )
            .into());
        }
    };

    let page = store
        .list_messages(MessageQuery {
            session_id: session_id.clone(),
            limit: params.limit.unwrap_or(0),
            order,
            cursor: params.cursor,
            kind: params.kind,
        })
        .await?;

    Ok(Json(json!({
        "data": page.items,
        "cursor": cursor_value(page.cursor),
    })))
}

/// `GET /api/session/:sid/message/:mid`
pub async fn get(
    Path((session_id, message_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    let message = store.get_message(&session_id, &message_id).await?;
    let Some(message) = message else {
        return Err(ApiError::not_found(
            "MessageNotFoundError",
            format!("Message not found: {message_id}"),
        )
        .with("sessionID", json!(session_id))
        .with("messageID", json!(message_id))
        .into());
    };
    Ok(Json(json!({ "data": message })))
}
