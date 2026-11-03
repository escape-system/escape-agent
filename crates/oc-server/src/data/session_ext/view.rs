//! `PUT /api/session/{sid}/environment` and `POST /api/session/{sid}/view`.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use oc_store::SessionPatch;
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::events::publish_durable;
use crate::data::{no_content, require_store, schema_error};
use crate::error::ErrorResponse;

use super::common::{float_i64, require_session};

/// `PUT /api/session/:sid/environment`
#[derive(Debug, Deserialize)]
pub struct EnvironmentBody {
    variables: Map<String, Value>,
}

/// `PUT /api/session/:sid/environment`
pub async fn environment(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<EnvironmentBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.variables.values().any(|value| !value.is_string()) {
        return Err(ApiError::new(400, "BadRequestError", "variables must map to strings").into());
    }
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let mut metadata = session
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    metadata.insert("environment".to_owned(), Value::Object(body.variables));
    store
        .patch_session(
            &session_id,
            SessionPatch {
                metadata: Some(Value::Object(metadata)),
                ..SessionPatch::default()
            },
        )
        .await?;
    Ok(no_content())
}

/// `POST /api/session/:sid/view`
#[derive(Debug, Deserialize)]
pub struct ViewBody {
    idle: f64,
}

/// `POST /api/session/:sid/view`
pub async fn view(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<ViewBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if !body.idle.is_finite() || body.idle < 0.0 {
        return Err(ApiError::new(400, "BadRequestError", "idle must be an epoch in ms").into());
    }
    let Some(idle) = float_i64(body.idle) else {
        return Err(ApiError::new(400, "BadRequestError", "idle must be an epoch in ms").into());
    };
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    publish_durable(
        &state,
        &session_id,
        "session.viewed",
        json!({ "sessionID": session_id, "idle": idle }),
        1,
    )
    .await?;
    Ok(no_content())
}
