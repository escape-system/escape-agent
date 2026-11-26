//! Integration read handlers and wire view projection helpers.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use oc_types::LocationEnvelope;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::location;
use crate::data::query_error;
use crate::data::session::LocationQuery;
use crate::error::ErrorResponse;

use super::errors::integration_not_found;
use super::registry::{IntegrationRow, snapshot};

/// `GET /api/integration`
pub async fn integration_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let (rows, credentials) = snapshot(&state).await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|row| integration_info(row, &credentials))
        .collect();
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Array(data),
    )))
}

/// `GET /api/integration/{id}`
pub async fn integration_get(
    Path(id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let (rows, credentials) = snapshot(&state).await?;
    let row = find_row(&rows, &id)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        integration_info(row, &credentials),
    )))
}

/// Build the wire `Integration.Info` for one row.
fn integration_info(row: &IntegrationRow, credentials: &[Value]) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), json!(row.id));
    object.insert("name".to_owned(), json!(row.name));
    if let Some(metadata) = &row.metadata {
        object.insert("metadata".to_owned(), metadata.clone());
    }
    object.insert("methods".to_owned(), Value::Array(row.methods.clone()));
    object.insert(
        "connections".to_owned(),
        Value::Array(connections(row, credentials)),
    );
    Value::Object(object)
}

/// Stored credentials of the integration (newest first) plus set env names.
fn connections(row: &IntegrationRow, credentials: &[Value]) -> Vec<Value> {
    let mut items: Vec<Value> = credentials
        .iter()
        .filter(|credential| {
            credential.get("integrationID").and_then(Value::as_str) == Some(row.id.as_str())
        })
        .filter_map(credential_connection)
        .collect();
    items.reverse();
    for method in &row.methods {
        if method.get("type").and_then(Value::as_str) != Some("env") {
            continue;
        }
        let Some(names) = method.get("names").and_then(Value::as_array) else {
            continue;
        };
        for name in names.iter().filter_map(Value::as_str) {
            if std::env::var(name).is_ok_and(|value| !value.is_empty()) {
                items.push(json!({ "type": "env", "name": name }));
            }
        }
    }
    items
}

/// One `Connection.CredentialInfo`, or `None` for a row that is not a valid
/// `Credential.Value` (the connection is then invisible rather than invented).
fn credential_connection(credential: &Value) -> Option<Value> {
    let method = credential.get("value")?.get("type")?.as_str()?;
    if !matches!(method, "key" | "oauth" | "external") {
        return None;
    }
    let mut entry = json!({
        "type": "credential",
        "id": credential.get("id")?.as_str()?,
        "label": credential.get("label")?.as_str()?,
        "method": method,
    });
    if credential.get("active").and_then(Value::as_bool) == Some(true)
        && let Some(obj) = entry.as_object_mut()
    {
        obj.insert("active".to_owned(), Value::Bool(true));
    }
    Some(entry)
}

/// One row by id, or the documented `IntegrationNotFoundError`.
pub(super) fn find_row<'a>(
    rows: &'a [IntegrationRow],
    id: &str,
) -> Result<&'a IntegrationRow, ErrorResponse> {
    rows.iter()
        .find(|row| row.id == id)
        .ok_or_else(|| ErrorResponse::from(integration_not_found(id)))
}

/// Whether the row registers a method of `kind` (optionally with `method_id`).
pub(super) fn has_method(row: &IntegrationRow, kind: &str, method_id: Option<&str>) -> bool {
    row.methods.iter().any(|method| {
        method.get("type").and_then(Value::as_str) == Some(kind)
            && method_id
                .is_none_or(|wanted| method.get("id").and_then(Value::as_str) == Some(wanted))
    })
}

/// An explicit label wins; otherwise the upstream `<name>`, `<name> 2`, ... rule.
pub(super) fn credential_label(
    name: &str,
    requested: Option<String>,
    credentials: &[Value],
    integration_id: &str,
) -> String {
    if let Some(label) = requested {
        return label;
    }
    let taken: BTreeSet<&str> = credentials
        .iter()
        .filter(|credential| {
            credential.get("integrationID").and_then(Value::as_str) == Some(integration_id)
        })
        .filter_map(|credential| credential.get("label").and_then(Value::as_str))
        .collect();
    for index in 0..=taken.len() {
        let candidate = if index == 0 {
            name.to_owned()
        } else {
            format!("{name} {}", index + 1)
        };
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    name.to_owned()
}
