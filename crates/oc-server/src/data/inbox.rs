//! Session inbox routes: list, change delivery, cancel.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::{delivery_from_wire, no_content, require_store, schema_error, session_not_found};
use crate::error::ErrorResponse;

/// `PATCH /api/session/:sid/inbox/:id`
#[derive(Debug, Deserialize)]
pub struct DeliveryBody {
    delivery: String,
}

/// `GET /api/session/:sid/inbox`
pub async fn list(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    let items = store.list_inbox(&session_id).await?;
    Ok(Json(json!({ "data": items })))
}

/// `PATCH /api/session/:sid/inbox/:id`
pub async fn update(
    Path((session_id, id)): Path<(String, String)>,
    State(state): State<AppState>,
    body: Result<Json<DeliveryBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let delivery = delivery_from_wire(Some(&body.delivery))?;
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;

    store
        .set_delivery(&session_id, &id, delivery)
        .await
        .map_err(|error| match error {
            oc_store::StoreError::NotFound(_) => ErrorResponse::from(ApiError::not_found(
                "NotFoundError",
                format!("Inbox item not found: {id}"),
            )),
            other => ErrorResponse::from(other),
        })?;
    Ok(no_content())
}

/// `DELETE /api/session/:sid/inbox/:id`
pub async fn delete(
    Path((session_id, id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    // A delivered item is already gone; cancellation is a no-op, not an error.
    store.cancel_inbox(&session_id, &id).await?;
    Ok(no_content())
}
