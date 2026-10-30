//! `GET /api/experimental/session/{sid}/export`, including credential redaction.

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::{query_error, require_store};
use crate::error::ErrorResponse;

use super::common::{messages_all, require_session};

/// `GET /api/experimental/session/:sid/export?sanitize=`
#[derive(Debug, Default, Deserialize)]
pub struct ExportParams {
    #[serde(default)]
    sanitize: Option<String>,
}

const SECRET_KEYS: [&str; 13] = [
    "authorization",
    "apikey",
    "api_key",
    "api-key",
    "token",
    "access_token",
    "refresh_token",
    "secret",
    "client_secret",
    "password",
    "credential",
    "credentials",
    "private_key",
];

fn is_secret_key(key: &str) -> bool {
    SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str())
}

/// Drops credential-bearing keys recursively. Export-only; never mutates the
/// stored transcript.
fn sanitize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !is_secret_key(key));
            for item in map.values_mut() {
                sanitize(item);
            }
        }
        Value::Array(items) => {
            for item in items {
                sanitize(item);
            }
        }
        _ => {}
    }
}

/// `GET /api/experimental/session/:sid/export`
pub async fn export(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    params: Result<Query<ExportParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let sanitize_requested = match params.sanitize.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(other) => {
            return Err(ApiError::new(
                400,
                "InvalidRequestError",
                format!("sanitize must be true or false, got {other:?}"),
            )
            .into());
        }
    };
    let store = require_store(&state)?;
    let info = require_session(store, &session_id).await?;
    let mut messages = messages_all(store, &session_id).await?;
    let data = if sanitize_requested {
        let mut info = info;
        sanitize(&mut info);
        for message in &mut messages {
            sanitize(message);
        }
        json!({ "info": info, "messages": messages })
    } else {
        json!({ "info": info, "messages": messages })
    };
    Ok(Json(json!({ "data": data })))
}
