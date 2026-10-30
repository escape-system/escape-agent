//! Form lifecycle handlers under `/api/session/{sid}/form`.

use std::collections::HashSet;

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use oc_store::{NewForm, StoreError};
use oc_types::{ApiError, EventEnvelope, EventLocation};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::{no_content, query_error, require_store, schema_error};
use crate::error::ErrorResponse;

use super::common::require_session;

fn form_not_found(id: &str) -> ApiError {
    ApiError::not_found("FormNotFoundError", format!("Form not found: {id}")).with("id", json!(id))
}

fn form_settled(id: &str) -> ApiError {
    ApiError::new(
        409,
        "FormAlreadySettledError",
        format!("Form {id} has already been answered or cancelled"),
    )
    .with("id", json!(id))
}

fn form_invalid(id: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(400, "FormInvalidAnswerError", message).with("id", json!(id))
}

fn map_form_store_error(error: StoreError, id: &str) -> ErrorResponse {
    match error {
        StoreError::NotFound(_) => form_not_found(id).into(),
        StoreError::Conflict(_) => form_settled(id).into(),
        other => other.into(),
    }
}

/// Publishes an ephemeral event tagged with the session's location.
///
/// Ephemeral events carry no durable marker: they are live notifications, not
/// entries in the session log.
fn publish_ephemeral(state: &AppState, session: &Value, kind: &str, data: Value) {
    let directory = session
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .map_or_else(
            || state.directory().to_string_lossy().into_owned(),
            str::to_owned,
        );
    let mut event = EventEnvelope::now(kind, data);
    event.location = Some(EventLocation { directory });
    state.publish(event);
}

/// `POST /api/session/:sid/form`
#[derive(Debug, Deserialize)]
pub struct FormCreateBody {
    #[serde(default)]
    id: Option<String>,
    title: String,
    #[serde(default)]
    metadata: Option<Value>,
    fields: Value,
}

/// `POST /api/session/:sid/form/:form_id/reply`
#[derive(Debug, Deserialize)]
pub struct FormReplyBody {
    answer: Value,
}

/// `DELETE /api/session/:sid/form/:form_id?message=`
#[derive(Debug, Default, Deserialize)]
pub struct FormCancelParams {
    #[serde(default)]
    message: Option<String>,
}

const FIELD_KINDS: [&str; 6] = [
    "string",
    "number",
    "integer",
    "boolean",
    "multiselect",
    "external",
];

fn invalid_form(message: impl Into<String>) -> ApiError {
    ApiError::new(400, "BadRequestError", message)
}

fn validate_form_fields(fields: &Value) -> Result<(), ApiError> {
    let items = fields
        .as_array()
        .ok_or_else(|| invalid_form("fields must be an array"))?;
    if items.is_empty() {
        return Err(invalid_form("fields must not be empty"));
    }
    let mut keys = HashSet::new();
    for field in items {
        let object = field
            .as_object()
            .ok_or_else(|| invalid_form("each field must be an object"))?;
        let key = object
            .get("key")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| invalid_form("each field needs a non-empty key"))?;
        if !keys.insert(key.to_owned()) {
            return Err(invalid_form(format!("duplicate field key: {key}")));
        }
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_form(format!("field {key} needs a type")))?;
        if !FIELD_KINDS.contains(&kind) {
            return Err(invalid_form(format!(
                "field {key} has unknown type {kind:?}"
            )));
        }
        if kind == "external" && object.get("url").and_then(Value::as_str).is_none() {
            return Err(invalid_form(format!("external field {key} needs a url")));
        }
    }
    Ok(())
}

fn validate_answer(detail: &Value, answer: &Value) -> Result<(), String> {
    let answer = answer
        .as_object()
        .ok_or_else(|| "answer must be an object".to_owned())?;
    let fields = detail
        .get("fields")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut known = HashSet::new();
    for field in &fields {
        let Some(object) = field.as_object() else {
            continue;
        };
        let Some(key) = object.get("key").and_then(Value::as_str) else {
            continue;
        };
        known.insert(key.to_owned());
        let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
        let required = object
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let hidden = object
            .get("hidden")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Some(value) = answer.get(key) else {
            if required && !hidden {
                return Err(format!("missing required field {key}"));
            }
            continue;
        };
        let valid = match kind {
            "string" => value.is_string(),
            "number" => value.is_number() || is_special_number(value),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "boolean" => value.is_boolean(),
            "multiselect" => value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
            _ => true,
        };
        if !valid {
            return Err(format!("field {key} does not accept this value"));
        }
    }
    for key in answer.keys() {
        if !known.contains(key) {
            return Err(format!("unknown field {key}"));
        }
    }
    Ok(())
}

fn is_special_number(value: &Value) -> bool {
    matches!(value.as_str(), Some("Infinity" | "-Infinity" | "NaN"))
}

/// `GET /api/session/:sid/form`
pub async fn form_list(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    let forms = store.list_forms(Some(session_id.as_str())).await?;
    Ok(Json(json!({ "data": forms })))
}

/// `POST /api/session/:sid/form`
pub async fn form_create(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<FormCreateBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.title.trim().is_empty() {
        return Err(ApiError::new(400, "BadRequestError", "title must not be empty").into());
    }
    if let Some(metadata) = &body.metadata
        && !metadata.is_object()
    {
        return Err(ApiError::new(400, "BadRequestError", "metadata must be an object").into());
    }
    validate_form_fields(&body.fields)?;
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let form = store
        .create_form(NewForm {
            id: body.id.filter(|id| !id.is_empty()),
            session_id: session_id.clone(),
            title: body.title,
            metadata: body.metadata,
            fields: body.fields,
        })
        .await?;
    publish_ephemeral(&state, &session, "form.created", json!({ "form": &form }));
    Ok(Json(json!({ "data": form })))
}

/// `GET /api/session/:sid/form/:form_id`
pub async fn form_get(
    Path((session_id, form_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    let form = store
        .get_form(&session_id, &form_id)
        .await
        .map_err(|error| map_form_store_error(error, &form_id))?;
    Ok(Json(json!({ "data": form })))
}

/// `POST /api/session/:sid/form/:form_id/reply`
pub async fn form_reply(
    Path((session_id, form_id)): Path<(String, String)>,
    State(state): State<AppState>,
    body: Result<Json<FormReplyBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let detail = store
        .get_form(&session_id, &form_id)
        .await
        .map_err(|error| map_form_store_error(error, &form_id))?;
    let status = detail
        .pointer("/state/status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status != "pending" {
        return Err(form_settled(&form_id).into());
    }
    validate_answer(&detail, &body.answer).map_err(|message| form_invalid(&form_id, message))?;
    let answer = body.answer;
    store
        .reply_form(&session_id, &form_id, &json!({ "answer": &answer }))
        .await
        .map_err(|error| map_form_store_error(error, &form_id))?;
    publish_ephemeral(
        &state,
        &session,
        "form.replied",
        json!({ "id": form_id, "sessionID": session_id, "answer": answer }),
    );
    Ok(no_content())
}

/// `DELETE /api/session/:sid/form/:form_id`
pub async fn form_cancel(
    Path((session_id, form_id)): Path<(String, String)>,
    State(state): State<AppState>,
    params: Result<Query<FormCancelParams>, QueryRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    store
        .cancel_form_with_message(&session_id, &form_id, params.message.as_deref())
        .await
        .map_err(|error| map_form_store_error(error, &form_id))?;
    publish_ephemeral(
        &state,
        &session,
        "form.cancelled",
        json!({ "id": form_id, "sessionID": session_id }),
    );
    Ok(no_content())
}
