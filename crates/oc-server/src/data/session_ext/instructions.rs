//! Instruction entry list, put and delete under
//! `/api/experimental/session/{sid}/instructions/entries`.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::{no_content, require_store, schema_error};
use crate::error::ErrorResponse;

use super::common::require_session;

/// Largest accepted instruction entry, matching the documented 1 MiB cap.
const INSTRUCTION_MAX_BYTES: usize = 1024 * 1024;

/// `PUT /api/experimental/session/:sid/instructions/entries/:key`
#[derive(Debug, Deserialize)]
pub struct InstructionBody {
    value: Value,
}

fn valid_instruction_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-'))
}

fn invalid_instruction_key(key: &str) -> ApiError {
    ApiError::new(
        400,
        "InvalidRequestError",
        format!("Invalid instruction entry key {key:?}"),
    )
}

/// `GET /api/experimental/session/:sid/instructions/entries`
pub async fn instruction_list(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    let entries = store.instruction_entries(&session_id).await?;
    Ok(Json(json!({ "data": entries })))
}

/// `PUT /api/experimental/session/:sid/instructions/entries/:key`
pub async fn instruction_put(
    Path((session_id, key)): Path<(String, String)>,
    State(state): State<AppState>,
    body: Result<Json<InstructionBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if !valid_instruction_key(&key) {
        return Err(invalid_instruction_key(&key).into());
    }
    let encoded = serde_json::to_string(&body.value)
        .map_err(|_| ErrorResponse::from(ApiError::internal("Internal error")))?;
    let actual = encoded.len();
    if actual > INSTRUCTION_MAX_BYTES {
        return Err(ApiError::new(
            413,
            "InstructionEntryValueTooLargeError",
            format!("instruction entry is {actual} bytes; the maximum is {INSTRUCTION_MAX_BYTES}"),
        )
        .with("actualBytes", json!(actual))
        .with("maxBytes", json!(INSTRUCTION_MAX_BYTES))
        .into());
    }
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    store
        .set_instruction_entry(&session_id, &key, Some(encoded.as_str()))
        .await?;
    Ok(no_content())
}

/// `DELETE /api/experimental/session/:sid/instructions/entries/:key`
pub async fn instruction_delete(
    Path((session_id, key)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    if !valid_instruction_key(&key) {
        return Err(invalid_instruction_key(&key).into());
    }
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    store.set_instruction_entry(&session_id, &key, None).await?;
    Ok(no_content())
}
