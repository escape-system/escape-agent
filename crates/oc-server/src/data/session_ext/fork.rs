//! `POST /api/session/{sid}/fork`: copies a session's history prefix into a new
//! child session and publishes the durable fork boundary.

use std::collections::HashMap;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use oc_store::{NewMessage, NewSession};
use oc_types::{MessageId, SessionId, now_millis};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::events::publish_durable;
use crate::data::{require_store, schema_error};
use crate::error::ErrorResponse;

use super::common::{
    id_of, message_body, message_not_found, messages_all, non_null, remap_keep_id, require_session,
    time_of,
};

/// `POST /api/session/:sid/fork`
#[derive(Debug, Default, Deserialize)]
pub struct ForkBody {
    #[serde(default)]
    before: Option<String>,
}

/// `POST /api/session/:sid/fork`
#[allow(clippy::too_many_lines)] // one linear copy-then-publish handler; splitting hides commit order
pub async fn fork(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<ForkBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let parent = require_session(store, &session_id).await?;
    let messages = messages_all(store, &session_id).await?;

    // `before` excludes the boundary message; without it the whole history is
    // copied (upstream's `through` boundary).
    let requested = body.before.as_deref().filter(|id| !id.is_empty());
    let cut = match requested {
        Some(before) => messages
            .iter()
            .position(|message| id_of(message) == before)
            .ok_or_else(|| message_not_found(&session_id, before))?,
        None => messages.len(),
    };
    // A fork with no history has no boundary to report; upstream refuses that
    // fork entirely, so no event is published rather than inventing a message.
    let boundary = match requested {
        Some(before) => Some(json!({ "type": "before", "messageID": before })),
        None => messages
            .last()
            .map(|message| json!({ "type": "through", "messageID": id_of(message) })),
    };

    let id = SessionId::new().into_inner();
    let directory = parent
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let project_id = parent
        .get("projectID")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let session = store
        .create_session(NewSession {
            id,
            parent_id: None,
            project_id,
            directory,
            title: parent
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned),
            agent: parent
                .get("agent")
                .and_then(Value::as_str)
                .map(str::to_owned),
            model: non_null(parent.get("model")),
            metadata: non_null(parent.get("metadata")),
            permissions: None,
            time_created: now_millis(),
        })
        .await?;

    // Identity is assigned up front so compaction `keepID` references can be
    // remapped before any row is written.
    let new_session_id = id_of(&session);
    let mut identities: HashMap<String, String> = HashMap::new();
    for message in &messages[..cut] {
        identities.insert(id_of(message), MessageId::new().into_inner());
    }

    for message in &messages[..cut] {
        let old_id = id_of(message);
        let kind = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if kind.is_empty() {
            continue;
        }
        let mut data = message_body(message);
        if kind == "compaction" {
            remap_keep_id(&mut data, &identities);
        }
        let new_id = identities
            .get(&old_id)
            .cloned()
            .unwrap_or_else(|| MessageId::new().into_inner());
        store
            .append_message(NewMessage {
                id: new_id,
                session_id: new_session_id.clone(),
                kind,
                data,
                time_created: time_of(message).unwrap_or_else(now_millis),
            })
            .await?;
    }

    // The fork (session row plus copied history) is committed; the durable
    // event records the parent boundary for every follower of the child log.
    if let Some(boundary) = boundary {
        publish_durable(
            &state,
            &new_session_id,
            "session.forked",
            json!({
                "sessionID": new_session_id,
                "parentID": session_id,
                "boundary": boundary,
            }),
            2,
        )
        .await?;
    }

    Ok(Json(json!({ "data": session })))
}
