//! `POST /api/experimental/session/import`.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::HeaderMap;
use oc_store::{NewMessage, NewSession};
use oc_types::{ApiError, MessageId, now_millis};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::location;
use crate::data::{require_store, schema_error, session_not_found};
use crate::error::ErrorResponse;

use super::common::{message_body, non_null, remap_keep_id, time_of};

#[derive(Debug, Deserialize)]
pub struct ImportBody {
    info: Value,
    messages: Vec<Value>,
    #[serde(default)]
    location: Option<ImportLocation>,
}

#[derive(Debug, Deserialize)]
pub struct ImportLocation {
    directory: String,
}

#[allow(clippy::too_many_lines)] // one linear create-then-copy handler; splitting hides rollback
pub async fn import(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<ImportBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let info = body
        .info
        .as_object()
        .cloned()
        .ok_or_else(|| ApiError::new(400, "InvalidRequestError", "info must be an object"))?;
    let session_id = info
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ApiError::new(400, "InvalidRequestError", "info.id is required"))?
        .to_owned();
    let parent_id = info
        .get("parentID")
        .and_then(Value::as_str)
        .filter(|parent| !parent.is_empty())
        .map(str::to_owned);

    let store = require_store(&state)?;
    if store.get_session(&session_id).await?.is_some() {
        return Err(ApiError::conflict(format!("Session {session_id} already exists")).into());
    }
    if let Some(parent) = &parent_id
        && store.get_session(parent).await?.is_none()
    {
        return Err(session_not_found(parent).into());
    }
    for message in &body.messages {
        let kind = message.get("type").and_then(Value::as_str);
        if kind.is_none_or(str::is_empty) {
            return Err(
                ApiError::new(400, "InvalidRequestError", "each message needs a type").into(),
            );
        }
    }

    let requested = body
        .location
        .as_ref()
        .map(|location| location.directory.as_str())
        .or_else(|| {
            info.get("location")
                .and_then(|location| location.get("directory"))
                .and_then(Value::as_str)
        });
    let resolved = location::resolve(&state, &headers, requested, None);
    if !resolved.path().is_dir() {
        return Err(ApiError::not_found(
            "LocationNotFoundError",
            format!("Directory not found: {}", resolved.directory),
        )
        .with("location", json!({ "directory": resolved.directory }))
        .into());
    }

    let time_created = info
        .get("time")
        .and_then(|time| time.get("created"))
        .and_then(Value::as_i64)
        .unwrap_or_else(now_millis);
    let session = store
        .create_session(NewSession {
            id: session_id.clone(),
            parent_id,
            project_id: resolved.project.id.as_str().to_owned(),
            directory: resolved.directory.clone(),
            title: info.get("title").and_then(Value::as_str).map(str::to_owned),
            agent: info.get("agent").and_then(Value::as_str).map(str::to_owned),
            model: non_null(info.get("model")),
            metadata: non_null(info.get("metadata")),
            permissions: non_null(info.get("permissions")),
            time_created,
        })
        .await?;

    // `session_message.id` is a database-wide primary key, while
    // `append_message` only checks duplicates inside the target session, so a
    // transcript imported next to its source would collide. Assign every copied
    // message a fresh identity and remap compaction `keepID` references, as fork
    // does.
    let mut identities: HashMap<String, String> = HashMap::new();
    for message in &body.messages {
        if let Some(old) = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            identities.insert(old.to_owned(), MessageId::new().into_inner());
        }
    }

    for message in body.messages {
        let kind = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let old_id = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let id = old_id
            .as_ref()
            .and_then(|old| identities.get(old))
            .cloned()
            .unwrap_or_else(|| MessageId::new().into_inner());
        let mut data = message_body(&message);
        if kind == "compaction" {
            remap_keep_id(&mut data, &identities);
        }
        let appended = store
            .append_message(NewMessage {
                id,
                session_id: session_id.clone(),
                kind,
                data,
                time_created: time_of(&message).unwrap_or(time_created),
            })
            .await;
        if let Err(error) = appended {
            // A half-imported session would be worse than no import at all.
            let _ = store.delete_session(&session_id).await;
            return Err(error.into());
        }
    }

    Ok(Json(json!({ "data": session })))
}
