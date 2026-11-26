//! Credential CRUD handlers and `Credential.Value` strict validation.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use oc_store::{CredentialPatch, NewCredential, StoreError};
use oc_types::{ApiError, DataEnvelope};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::{no_content, require_store, schema_error};
use crate::error::ErrorResponse;

use super::bodies::{CreateCredentialBody, UpdateCredentialBody};
use super::errors::credential_not_found;

/// `Credential.Key` — only the documented members are accepted.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyCredential {
    #[serde(rename = "type")]
    _kind: String,
    key: String,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
}

/// `Credential.OAuth`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuthCredential {
    #[serde(rename = "type")]
    _kind: String,
    #[serde(rename = "methodID")]
    method_id: String,
    refresh: String,
    access: String,
    expires: u64,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
}

/// `Credential.External`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalCredential {
    #[serde(rename = "type")]
    _kind: String,
    #[serde(rename = "methodID")]
    method_id: String,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
}

/// Decode one `Credential.Value` strictly.
///
/// The tagged union is dispatched by hand so unknown members are rejected
/// (serde ignores `deny_unknown_fields` on internally tagged enums) and every
/// optional member is omitted rather than stored as `null`.
fn parse_credential_value(value: Value) -> Result<Value, ApiError> {
    let Some(tag) = value.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return Err(invalid_value_message(
            "credential value must be an object with a string type",
        ));
    };
    match tag.as_str() {
        "key" => {
            let parsed: KeyCredential =
                serde_json::from_value(value).map_err(|error| invalid_value(&error))?;
            Ok(with_metadata(
                json!({ "type": "key", "key": parsed.key }),
                parsed.metadata,
            ))
        }
        "oauth" => {
            let parsed: OAuthCredential =
                serde_json::from_value(value).map_err(|error| invalid_value(&error))?;
            Ok(with_metadata(
                json!({
                    "type": "oauth",
                    "methodID": parsed.method_id,
                    "refresh": parsed.refresh,
                    "access": parsed.access,
                    "expires": parsed.expires,
                }),
                parsed.metadata,
            ))
        }
        "external" => {
            let parsed: ExternalCredential =
                serde_json::from_value(value).map_err(|error| invalid_value(&error))?;
            Ok(with_metadata(
                json!({ "type": "external", "methodID": parsed.method_id }),
                parsed.metadata,
            ))
        }
        other => Err(invalid_value_message(&format!(
            "credential type must be oauth, key or external: {other}"
        ))),
    }
}

/// A rejected `Credential.Value` body member.
fn invalid_value(error: &serde_json::Error) -> ApiError {
    invalid_value_message(&format!("invalid credential value: {error}"))
}

fn invalid_value_message(message: &str) -> ApiError {
    ApiError::new(400, "SchemaError", message)
}

/// Attach the optional metadata object; an absent one stays absent.
fn with_metadata(mut value: Value, metadata: Option<Map<String, Value>>) -> Value {
    if let (Some(metadata), Some(object)) = (metadata, value.as_object_mut()) {
        object.insert("metadata".to_owned(), Value::Object(metadata));
    }
    value
}

/// `GET /api/credential`
pub async fn credential_list(
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Vec<Value>>>, ErrorResponse> {
    let store = require_store(&state)?;
    Ok(Json(DataEnvelope::new(store.list_credentials().await?)))
}

/// `GET /api/credential/{id}` — not part of the `OpenAPI` document; the entry
/// shape is the documented `Credential.Entry`.
pub async fn credential_get(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let store = require_store(&state)?;
    let entry = store
        .get_credential(&id)
        .await?
        .ok_or_else(|| credential_not_found(&id))?;
    Ok(Json(DataEnvelope::new(entry)))
}

/// `POST /api/credential`
pub async fn credential_create(
    State(state): State<AppState>,
    body: Result<Json<CreateCredentialBody>, JsonRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let value = parse_credential_value(body.value)?;
    let store = require_store(&state)?;
    if let Some(id) = body.id.as_deref()
        && store.get_credential(id).await?.is_some()
    {
        return Err(ErrorResponse::from(
            ApiError::conflict(format!("Credential already exists: {id}"))
                .with("resource", json!(id)),
        ));
    }
    let integration_id = body.integration_id;
    let entry = store
        .create_credential(NewCredential {
            id: body.id,
            integration_id: integration_id.clone(),
            label: body.label,
            value,
            active: body.activate,
        })
        .await?;
    state
        .quota_cache()
        .write()
        .await
        .invalidate(&integration_id);
    Ok(Json(DataEnvelope::new(entry)))
}

/// `PATCH /api/credential/{id}` — an unknown credential is the upstream no-op.
pub async fn credential_update(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<UpdateCredentialBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let updated = store
        .update_credential(
            &id,
            CredentialPatch {
                label: Some(body.label),
                ..CredentialPatch::default()
            },
        )
        .await?;
    if let Some(pid) = updated
        .as_ref()
        .and_then(|row| row.get("integrationID"))
        .and_then(Value::as_str)
    {
        state.quota_cache().write().await.invalidate(pid);
    }
    Ok(no_content())
}

/// `DELETE /api/credential/{id}` — an unknown credential is the upstream no-op.
pub async fn credential_remove(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    let existing = store.get_credential(&id).await?;
    store.delete_credential(&id).await?;
    if let Some(pid) = existing
        .as_ref()
        .and_then(|row| row.get("integrationID"))
        .and_then(Value::as_str)
    {
        state.quota_cache().write().await.invalidate(pid);
    }
    if let Some(svc) = state.antigravity() {
        svc.remove_by_credential_id(&id);
    }
    Ok(no_content())
}

/// `POST /api/credential/{id}/activate` — an unknown credential is the upstream
/// no-op, so `404` is reserved for the documented errors only.
pub async fn credential_activate(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    match store.activate_credential(&id).await {
        Ok(entry) => {
            if let Some(pid) = entry.get("integrationID").and_then(Value::as_str) {
                state.quota_cache().write().await.invalidate(pid);
            }
            if let Some(svc) = state.antigravity() {
                svc.select_by_credential_id(&id);
            }
            Ok(no_content())
        }
        Err(StoreError::NotFound(_)) => Ok(no_content()),
        Err(error) => Err(ErrorResponse::from(error)),
    }
}
