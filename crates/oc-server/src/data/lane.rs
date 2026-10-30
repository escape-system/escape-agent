//! Experimental lane routes.
//!
//! `oc-lanes` is attached with [`crate::AppState::with_lanes`]; when it is not
//! attached the routes answer the documented `503 ServiceUnavailableError`
//! instead of returning an empty queue that would look healthy.

use axum::Json;
use axum::Router;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use oc_lanes::LaneError;
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::{no_content, query_error, schema_error};
use crate::error::ErrorResponse;

/// Lane routes, merged into the protected router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/experimental/lane", get(status))
        .route("/api/experimental/lane/{id}/order", post(order))
        .route("/api/experimental/lane/{id}/cancel", post(cancel))
}

/// `GET /api/experimental/lane?lane=`
#[derive(Debug, Default, Deserialize)]
pub struct StatusParams {
    /// Restrict the snapshot to one lane.
    #[serde(default)]
    lane: Option<String>,
}

/// `POST /api/experimental/lane/:id/order`
#[derive(Debug, Deserialize)]
pub struct OrderBody {
    /// New priority; higher values dispatch first.
    priority: i32,
}

/// `GET /api/experimental/lane`
pub async fn status(
    State(state): State<AppState>,
    params: Result<Query<StatusParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let lanes = state
        .lanes()
        .ok_or_else(|| ApiError::service_unavailable("lane service not attached"))?;
    let rows = lanes.status(params.lane.as_deref()).await;
    let value = serde_json::to_value(rows)
        .map_err(|_| ErrorResponse::from(ApiError::internal("Internal error")))?;
    Ok(Json(json!({ "data": value })))
}

/// `POST /api/experimental/lane/:id/order`
pub async fn order(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<OrderBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let lanes = state
        .lanes()
        .ok_or_else(|| ApiError::service_unavailable("lane service not attached"))?;
    lanes
        .reorder(&id, body.priority)
        .await
        .map_err(|error| ErrorResponse::from(lane_error(error)))?;
    Ok(no_content())
}

/// `POST /api/experimental/lane/:id/cancel`
pub async fn cancel(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let lanes = state
        .lanes()
        .ok_or_else(|| ApiError::service_unavailable("lane service not attached"))?;
    lanes
        .cancel(&id)
        .await
        .map_err(|error| ErrorResponse::from(lane_error(error)))?;
    Ok(no_content())
}

fn lane_error(error: LaneError) -> ApiError {
    match error {
        LaneError::UnknownTicket(id) => ApiError::not_found(
            "LaneTicketNotFoundError",
            format!("Lane ticket not found: {id}"),
        )
        .with("ticketID", json!(id)),
        LaneError::NotQueued { id } => ApiError::conflict(format!(
            "Lane ticket is not queued and cannot be reordered: {id}"
        ))
        .with("ticketID", json!(id)),
        LaneError::Shutdown => ApiError::service_unavailable("lane service is shut down"),
        LaneError::EmptyLane => ApiError::bad_request("lane name must not be empty"),
        LaneError::QueueFull { lane, max } => ApiError::new(
            429,
            "LaneQueueFullError",
            format!("lane `{lane}` queue is full (max {max})"),
        )
        .with("lane", json!(lane)),
        LaneError::Timeout { id, timeout_ms } => ApiError::new(
            504,
            "LaneTimeoutError",
            format!("ticket `{id}` did not finish within {timeout_ms} ms"),
        )
        .with("ticketID", json!(id)),
    }
}
