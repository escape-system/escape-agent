//! Session CRUD: list, create, active, get, patch, delete and the tree walk.

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use oc_store::{NewSession, Order, SessionPatch, SessionQuery};
use oc_types::{
    ApiError, EventPayload, SessionArchived, SessionDeleted, SessionId, SessionMetadataUpdated,
    SessionPermissions, SessionRenamed, SessionUnarchived, now_millis,
};
use serde_json::{Map, Value, json};

use super::LocationQuery;
use super::bodies::{CreateBody, ListParams, PatchBody};
use super::events::created_event_data;
use crate::AppState;
use crate::data::events::publish_durable;
use crate::data::location;
use crate::data::{
    cursor_value, no_content, query_error, require_store, schema_error, session_not_found,
};
use crate::error::ErrorResponse;

/// `GET /api/session`
pub async fn list(
    State(state): State<AppState>,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;

    let order = match params.order.as_deref() {
        Some("asc") => Order::Asc,
        None | Some("desc") => Order::Desc,
        Some(other) => {
            return Err(ApiError::new(
                400,
                "BadRequestError",
                format!("Invalid order {other:?}; expected asc or desc"),
            )
            .into());
        }
    };
    let parent_id = match params.parent_id.as_deref() {
        None => None,
        Some("null") => Some(None),
        Some(id) => Some(Some(id.to_owned())),
    };

    let page = store
        .list_sessions(SessionQuery {
            limit: params.limit.unwrap_or(0),
            order,
            search: params.search,
            parent_id,
            archived: params.archived,
            directory: params.directory,
            project: params.project,
            cursor: params.cursor,
        })
        .await?;

    Ok(Json(json!({
        "data": page.items,
        "cursor": cursor_value(page.cursor),
    })))
}

/// `POST /api/session`
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
    body: Result<Json<CreateBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let requested = body
        .location
        .as_ref()
        .map(|location| location.directory.as_str());
    let resolved = location::resolve(&state, &headers, requested, params.directory.as_deref());
    if !resolved.path().is_dir() {
        return Err(ApiError::new(
            404,
            "LocationNotFoundError",
            format!("Directory not found: {}", resolved.directory),
        )
        .into());
    }

    let store = require_store(&state)?;
    let id = body.id.unwrap_or_else(|| SessionId::new().into_inner());
    let time = now_millis();
    let session = store
        .create_session(NewSession {
            id: id.clone(),
            parent_id: body.parent_id.filter(|parent| !parent.is_empty()),
            project_id: resolved.project.id.as_str().to_owned(),
            directory: resolved.directory.clone(),
            title: body.title,
            agent: body.agent,
            model: body.model.map(|model| json!(model)),
            metadata: body.metadata.map(Value::Object),
            permissions: body.permissions.map(|rules| json!(rules)),
            time_created: time,
        })
        .await?;

    let data = created_event_data(&session, &state.info().version);
    publish_durable(
        &state,
        &id,
        oc_types::SessionCreated::TYPE,
        data,
        oc_types::SessionCreated::VERSION,
    )
    .await?;

    Ok(Json(json!({ "data": session })).into_response())
}

/// `GET /api/session/active`
pub async fn active(State(state): State<AppState>) -> Json<Value> {
    let data = state.runtime().map_or_else(Map::new, |runtime| {
        runtime
            .active()
            .into_iter()
            .map(|session| (session, json!({ "type": "running" })))
            .collect()
    });
    Json(json!({ "data": data }))
}

/// `GET /api/session/:sid`
pub async fn get(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    let session = store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    Ok(Json(json!({ "data": session })))
}

/// `PATCH /api/session/:sid`
pub async fn patch(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<PatchBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let existing = store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;

    let mut patch = SessionPatch::default();
    let mut events: Vec<(String, Value, u32)> = Vec::new();

    if let Some(title) = body.title {
        patch.title = Some(title.clone());
        events.push((
            SessionRenamed::TYPE.to_owned(),
            json!({ "sessionID": &session_id, "title": title }),
            SessionRenamed::VERSION,
        ));
    }
    if let Some(metadata) = body.metadata {
        if !metadata.is_object() {
            return Err(ApiError::new(400, "BadRequestError", "metadata must be an object").into());
        }
        patch.metadata = Some(metadata.clone());
        events.push((
            SessionMetadataUpdated::TYPE.to_owned(),
            json!({ "sessionID": &session_id, "metadata": metadata }),
            SessionMetadataUpdated::VERSION,
        ));
    }
    if let Some(permissions) = body.permissions {
        patch.permissions = Some(json!(&permissions));
        events.push((
            SessionPermissions::TYPE.to_owned(),
            json!({ "sessionID": &session_id, "permissions": permissions }),
            SessionPermissions::VERSION,
        ));
    }
    if let Some(archived) = body.archived {
        let was_archived = existing
            .pointer("/time/archived")
            .is_some_and(|value| !value.is_null());
        patch.archived = Some(archived);
        if archived != was_archived {
            let (kind, version) = if archived {
                (SessionArchived::TYPE, SessionArchived::VERSION)
            } else {
                (SessionUnarchived::TYPE, SessionUnarchived::VERSION)
            };
            events.push((
                kind.to_owned(),
                json!({ "sessionID": &session_id }),
                version,
            ));
        }
    }

    store.patch_session(&session_id, patch).await?;
    for (kind, data, version) in events {
        publish_durable(&state, &session_id, &kind, data, version).await?;
    }
    Ok(no_content())
}

/// `DELETE /api/session/:sid`
pub async fn delete(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;

    // DELETE must cancel an in-flight provider call before the rows disappear;
    // otherwise the actor keeps the stream (and its connection/tokens) alive
    // until the provider answers and only then discovers the session is gone.
    // Interrupt the whole subtree: deleting a parent also deletes its children.
    if let Some(runtime) = state.runtime().cloned() {
        let ids = session_tree_ids(store, &session_id).await?;
        for id in &ids {
            if let Err(error) = runtime.interrupt(id).await {
                tracing::warn!(session = %id, error = %error, "delete could not interrupt session");
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let active = runtime.active();
            if !ids
                .iter()
                .any(|id| active.iter().any(|current| current == id))
            {
                break;
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    session = %session_id,
                    "delete timed out waiting for interrupted sessions to settle"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    publish_durable(
        &state,
        &session_id,
        SessionDeleted::TYPE,
        json!({ "sessionID": &session_id }),
        SessionDeleted::VERSION,
    )
    .await?;
    store.delete_session(&session_id).await?;
    Ok(no_content())
}

/// Root id followed by every descendant, parents before children.
async fn session_tree_ids(
    store: &oc_store::Store,
    root: &str,
) -> Result<Vec<String>, oc_store::StoreError> {
    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::from([root.to_owned()]);
    while let Some(id) = queue.pop_front() {
        for child in store.children(&id).await? {
            queue.push_back(child.id);
        }
        out.push(id);
    }
    Ok(out)
}
