//! Session routes: CRUD, selection, move, prompts and the pending-input
//! admission endpoints.
//!
//! `/prompt`, `/synthetic`, `/compact` and `/command` durably admit input
//! (inbox row, user message, enqueued event) and then wake the runtime when it
//! is attached. Execution itself never happens in a handler. `/shell` lives in
//! [`crate::data::shells`].

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use oc_store::{Delivery, NewInbox, NewMessage, NewSession, Order, SessionPatch, SessionQuery};
use oc_types::{
    ApiError, EventPayload, InboxItem, MessageId, ModelRef, PermissionRule, SessionAgentSelected,
    SessionArchived, SessionDeleted, SessionId, SessionInboxEnqueued, SessionMetadataUpdated,
    SessionModelSelected, SessionMoved, SessionPermissions, SessionRenamed, SessionUnarchived,
    now_millis,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::events::publish_durable;
use crate::data::location;
use crate::data::{
    cursor_value, delivery_from_wire, no_content, query_error, require_config, require_store,
    runtime_error, runtime_unavailable, schema_error, session_not_found,
};
use crate::error::ErrorResponse;

/// Deep-object `location[directory]` shared by location-scoped routes.
#[derive(Debug, Default, Deserialize)]
pub struct LocationQuery {
    /// Requested working directory.
    #[serde(rename = "location[directory]")]
    pub directory: Option<String>,
}

/// `GET /api/session`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    limit: Option<usize>,
    order: Option<String>,
    search: Option<String>,
    #[serde(rename = "parentID")]
    parent_id: Option<String>,
    archived: Option<bool>,
    directory: Option<String>,
    project: Option<String>,
    cursor: Option<String>,
}

/// `POST /api/session`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "parentID", default)]
    parent_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<ModelRef>,
    #[serde(default)]
    location: Option<CreateLocation>,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
    #[serde(default)]
    permissions: Option<Vec<PermissionRule>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateLocation {
    directory: String,
}

/// `PATCH /api/session/:sid`
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    permissions: Option<Vec<PermissionRule>>,
    #[serde(default)]
    archived: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct AgentBody {
    agent: String,
}

#[derive(Debug, Deserialize)]
pub struct ModelBody {
    model: ModelRef,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveBody {
    directory: String,
    #[serde(default)]
    #[allow(dead_code)] // accepted for wire compatibility; move is immediate
    delivery: Option<String>,
}

/// `POST /api/session/:sid/interrupt?resume=`
#[derive(Debug, Default, Deserialize)]
pub struct InterruptParams {
    #[serde(default)]
    #[allow(dead_code)] // no live execution to resume
    resume: Option<bool>,
}

/// `POST /api/session/:sid/prompt`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptBody {
    #[serde(default)]
    id: Option<String>,
    text: String,
    #[serde(default)]
    files: Option<Vec<Value>>,
    #[serde(default)]
    agents: Option<Vec<Value>>,
    #[serde(default)]
    skills: Option<Vec<Value>>,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
    #[serde(default)]
    delivery: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // accepted; there is no paused execution to resume
    resume: Option<bool>,
}

/// `POST /api/session/:sid/synthetic`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntheticBody {
    #[serde(default)]
    id: Option<String>,
    text: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
    #[serde(default)]
    delivery: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    resume: Option<bool>,
}

/// `POST /api/session/:sid/compact`
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    delivery: Option<String>,
}

/// `POST /api/session/:sid/command`
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    files: Option<Vec<Value>>,
    #[serde(default)]
    agents: Option<Vec<Value>>,
    #[serde(default)]
    skills: Option<Vec<Value>>,
    #[serde(default)]
    delivery: Option<String>,
}

/// `POST /api/experimental/session/:sid/wait`
#[derive(Debug, Default, Deserialize)]
pub struct WaitParams {
    /// Seconds to wait; defaults to 30 and is clamped to 10 minutes.
    #[serde(default)]
    timeout: Option<f64>,
}

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

/// `POST /api/session/:sid/agent`
pub async fn select_agent(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<AgentBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.agent.trim().is_empty() {
        return Err(ApiError::new(400, "BadRequestError", "agent must not be empty").into());
    }
    let store = require_store(&state)?;
    let existing = store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    let config = require_config(&state)?;
    if config.agent(&body.agent).is_none() {
        return Err(ApiError::not_found(
            "AgentNotFoundError",
            format!("Agent not found: {}", body.agent),
        )
        .with("agentID", json!(&body.agent))
        .into());
    }
    let previous = existing
        .get("agent")
        .and_then(Value::as_str)
        .map(str::to_owned);

    store
        .patch_session(
            &session_id,
            SessionPatch {
                agent: Some(body.agent.clone()),
                ..SessionPatch::default()
            },
        )
        .await?;

    let payload = SessionAgentSelected {
        session_id: SessionId::from_wire(session_id.as_str()),
        agent: body.agent,
        previous,
    };
    publish_durable(
        &state,
        &session_id,
        SessionAgentSelected::TYPE,
        payload.data(),
        SessionAgentSelected::VERSION,
    )
    .await?;
    Ok(no_content())
}

/// `POST /api/session/:sid/model`
pub async fn select_model(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<ModelBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.model.id.is_empty() || body.model.provider_id.is_empty() {
        return Err(
            ApiError::new(400, "BadRequestError", "model requires id and providerID").into(),
        );
    }
    if let Some(catalog) = state.catalog()
        && catalog
            .model(&body.model.provider_id, &body.model.id)
            .is_none()
    {
        return Err(ApiError::not_found(
            "ModelNotFoundError",
            format!(
                "Model not found: {}/{}",
                body.model.provider_id, body.model.id
            ),
        )
        .with("providerID", json!(&body.model.provider_id))
        .with("modelID", json!(&body.model.id))
        .into());
    }
    let store = require_store(&state)?;
    let existing = store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    let previous = existing
        .get("model")
        .and_then(|value| serde_json::from_value::<ModelRef>(value.clone()).ok());

    store
        .patch_session(
            &session_id,
            SessionPatch {
                model: Some(json!(&body.model)),
                ..SessionPatch::default()
            },
        )
        .await?;

    let payload = SessionModelSelected {
        session_id: SessionId::from_wire(session_id.as_str()),
        model: body.model,
        previous,
    };
    publish_durable(
        &state,
        &session_id,
        SessionModelSelected::TYPE,
        payload.data(),
        SessionModelSelected::VERSION,
    )
    .await?;
    Ok(no_content())
}

/// `POST /api/session/:sid/move`
pub async fn move_session(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<MoveBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;

    let target = location::build(state.directory(), &body.directory);
    if !target.path().is_dir() {
        return Err(ApiError::new(
            400,
            "BadRequestError",
            format!("Directory does not exist: {}", body.directory),
        )
        .into());
    }

    store
        .patch_session(
            &session_id,
            SessionPatch {
                directory: Some(target.directory.clone()),
                ..SessionPatch::default()
            },
        )
        .await?;

    let payload = SessionMoved {
        session_id: SessionId::from_wire(session_id.as_str()),
        location: oc_types::SessionLocation::new(target.directory.clone()),
        project_id: target.project.id.clone(),
        subpath: None,
    };
    publish_durable(
        &state,
        &session_id,
        SessionMoved::TYPE,
        payload.data(),
        SessionMoved::VERSION,
    )
    .await?;
    Ok(no_content())
}

/// `POST /api/session/:sid/interrupt`
pub async fn interrupt(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    params: Result<Query<InterruptParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    let Some(runtime) = state.runtime() else {
        return Ok(Json(json!({ "interrupted": false })));
    };
    let interrupted = runtime
        .interrupt(&session_id)
        .await
        .map_err(runtime_error)?;
    Ok(Json(json!({ "interrupted": interrupted })))
}

/// `POST /api/session/:sid/background`
pub async fn background(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    // Execution is actor-based: the handler never runs a turn, so marking the
    // running execution as backgrounded is the detach. The turn's foreground
    // `subagent` wait polls the flag, returns `status: running`, and the child
    // keeps going. With no running execution there is nothing to detach, which
    // is still a 204.
    let runtime = state.runtime().ok_or_else(runtime_unavailable)?;
    runtime
        .request_background(&session_id)
        .map_err(runtime_error)?;
    Ok(no_content())
}

/// `POST /api/session/:sid/prompt`
pub async fn prompt(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<PromptBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.text.trim().is_empty() {
        return Err(ApiError::new(400, "BadRequestError", "text must not be empty").into());
    }
    // Raw `/compress` compatibility (the web app also exposes it through
    // `GET /api/command`): the text never reaches the model and no user message
    // is admitted, exactly like the upstream `/compact` command.
    if body.text.trim().eq_ignore_ascii_case("/compress") {
        let item = admit_compaction(&state, &session_id, body.id, body.delivery).await?;
        return Ok(Json(json!({ "data": item })));
    }
    let mut extra = Map::new();
    if let Some(files) = body.files {
        extra.insert("files".to_owned(), Value::Array(files));
    }
    if let Some(agents) = body.agents {
        extra.insert("agents".to_owned(), Value::Array(agents));
    }
    if let Some(skills) = body.skills {
        extra.insert("skills".to_owned(), Value::Array(skills));
    }
    if let Some(metadata) = body.metadata {
        extra.insert("metadata".to_owned(), Value::Object(metadata));
    }
    let item = admit(
        &state,
        &session_id,
        body.id,
        body.text,
        extra,
        body.delivery,
    )
    .await?;
    Ok(Json(json!({ "data": item })))
}

/// Durably admit one user prompt, then wake the runtime.
///
/// Only `steer` input is materialised as a message here: the running turn reads
/// the transcript from the store at its next step, so the row must already be
/// durable before `notify`. `queue` input is only written to the inbox; the
/// runtime promotes that row when the current turn settles, which is what keeps
/// a queued prompt invisible to the turn that was already running.
///
/// Ordering for both modes is store → event → notify.
async fn admit(
    state: &AppState,
    session_id: &str,
    id: Option<String>,
    text: String,
    extra: Map<String, Value>,
    delivery: Option<String>,
) -> Result<Value, ErrorResponse> {
    let store = require_store(state)?;
    store
        .get_session(session_id)
        .await?
        .ok_or_else(|| session_not_found(session_id))?;

    let delivery = delivery_from_wire(delivery.as_deref())?;
    let id = id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| MessageId::new().into_inner());
    if store.get_message(session_id, &id).await?.is_some() {
        return Err(ApiError::conflict(format!(
            "Prompt message ID conflicts with an existing durable record: {id}"
        ))
        .into());
    }

    let mut payload = Map::new();
    payload.insert("text".to_owned(), Value::String(text));
    payload.extend(extra);
    let payload = Value::Object(payload);

    // Only `steer` input is materialised; `queue` rows stay inbox-only and the
    // runtime promotes them when the current turn settles. Writing the message
    // here would leak the text into the running turn's next request and run
    // the prompt twice.
    let mut message_data = None;
    if delivery == Delivery::Steer {
        let mut data = payload.clone();
        if let Value::Object(map) = &mut data {
            map.insert("time".to_owned(), json!({ "created": now_millis() }));
        }
        message_data = Some(data);
    }

    let item = store
        .enqueue(NewInbox {
            id: id.clone(),
            session_id: session_id.to_owned(),
            kind: "user".to_owned(),
            payload,
            delivery,
        })
        .await?;

    if let Some(data) = message_data
        && let Err(error) = store
            .append_message(NewMessage {
                id: id.clone(),
                session_id: session_id.to_owned(),
                kind: "user".to_owned(),
                data,
                time_created: now_millis(),
            })
            .await
    {
        let _ = store.cancel_inbox(session_id, &id).await;
        return Err(error.into());
    }

    publish_enqueued(state, session_id, &id, &item).await?;
    notify(state, session_id).await?;
    Ok(item)
}

/// Wake the runtime for a session that just received input.
pub(crate) async fn notify(state: &AppState, session_id: &str) -> Result<(), ErrorResponse> {
    if let Some(runtime) = state.runtime() {
        runtime.notify(session_id).await.map_err(runtime_error)?;
    }
    Ok(())
}

/// `POST /api/session/:sid/synthetic`
pub async fn synthetic(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<SyntheticBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.text.trim().is_empty() {
        return Err(ApiError::new(400, "BadRequestError", "text must not be empty").into());
    }
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;

    let delivery = delivery_from_wire(body.delivery.as_deref())?;
    let id = body
        .id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| MessageId::new().into_inner());
    if store.get_message(&session_id, &id).await?.is_some() {
        return Err(ApiError::conflict(format!(
            "Synthetic message ID conflicts with an existing durable record: {id}"
        ))
        .into());
    }

    let mut payload = Map::new();
    payload.insert("text".to_owned(), Value::String(body.text));
    if let Some(description) = body.description {
        payload.insert("description".to_owned(), Value::String(description));
    }
    if let Some(metadata) = body.metadata {
        payload.insert("metadata".to_owned(), Value::Object(metadata));
    }

    // Synthetic input is never materialised here, whatever its delivery: the
    // runtime promotes the inbox row (mid-turn for `steer`, at settle for
    // `queue`), so a queued row cannot leak into the running turn.
    let item = store
        .enqueue(NewInbox {
            id: id.clone(),
            session_id: session_id.clone(),
            kind: "synthetic".to_owned(),
            payload: Value::Object(payload),
            delivery,
        })
        .await?;
    publish_enqueued(&state, &session_id, &id, &item).await?;
    notify(&state, &session_id).await?;
    Ok(Json(json!({ "data": item })))
}

/// Admit one `compaction` inbox row and wake the runtime.
///
/// Shared by `POST /compact`, `POST /command {"name":"compress"}` and the
/// raw `/compress` prompt alias, so all three produce exactly the same durable
/// row and event ordering as the upstream manual-compaction endpoint.
async fn admit_compaction(
    state: &AppState,
    session_id: &str,
    id: Option<String>,
    delivery: Option<String>,
) -> Result<Value, ErrorResponse> {
    let store = require_store(state)?;
    store
        .get_session(session_id)
        .await?
        .ok_or_else(|| session_not_found(session_id))?;

    let delivery = delivery_from_wire(delivery.as_deref())?;
    let id = id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| MessageId::new().into_inner());
    let item = store
        .enqueue(NewInbox {
            id: id.clone(),
            session_id: session_id.to_owned(),
            kind: "compaction".to_owned(),
            payload: json!({}),
            delivery,
        })
        .await?;
    publish_enqueued(state, session_id, &id, &item).await?;
    notify(state, session_id).await?;
    Ok(item)
}

/// `POST /api/session/:sid/compact`
pub async fn compact(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<CompactBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let item = admit_compaction(&state, &session_id, body.id, body.delivery).await?;
    Ok(Json(json!({ "data": item })))
}

/// `POST /api/session/:sid/command`
pub async fn command(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<CommandBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let Some(name) = body.name.filter(|name| !name.trim().is_empty()) else {
        return Err(ApiError::new(400, "BadRequestError", "name must not be empty").into());
    };
    let config = require_config(&state)?;
    // Builtin `/compress` (upstream calls the same operation `/compact`).
    // A markdown command named `compress` keeps precedence.
    if name.eq_ignore_ascii_case("compress") && config.command(&name).is_none() {
        admit_compaction(&state, &session_id, None, body.delivery).await?;
        return Ok(no_content());
    }
    let Some(command) = config.command(&name) else {
        return Err(ApiError::not_found(
            "CommandNotFoundError",
            format!("Command not found: {name}"),
        )
        .with("command", json!(name))
        .into());
    };
    let arguments = body.text.unwrap_or_default();
    let text = if command.template.contains("$ARGUMENTS") {
        command.template.replace("$ARGUMENTS", &arguments)
    } else if arguments.trim().is_empty() {
        command.template.clone()
    } else {
        format!("{}\n\n{}", command.template.trim_end(), arguments)
    };
    if text.trim().is_empty() {
        return Err(ApiError::new(400, "BadRequestError", "text must not be empty").into());
    }
    let mut extra = Map::new();
    if let Some(files) = body.files {
        extra.insert("files".to_owned(), Value::Array(files));
    }
    if let Some(agents) = body.agents {
        extra.insert("agents".to_owned(), Value::Array(agents));
    }
    if let Some(skills) = body.skills {
        extra.insert("skills".to_owned(), Value::Array(skills));
    }
    admit(&state, &session_id, None, text, extra, body.delivery).await?;
    Ok(no_content())
}

/// `POST /api/experimental/session/:sid/wait`
pub async fn wait(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    params: Result<Query<WaitParams>, QueryRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    let Some(runtime) = state.runtime() else {
        return Err(runtime_unavailable().into());
    };
    let timeout = Duration::from_secs_f64(params.timeout.unwrap_or(30.0).clamp(0.0, 600.0));
    runtime
        .wait_idle(&session_id, timeout)
        .await
        .map_err(runtime_error)?;
    Ok(no_content())
}

pub(crate) async fn publish_enqueued(
    state: &AppState,
    session_id: &str,
    id: &str,
    item: &Value,
) -> Result<(), ErrorResponse> {
    let item: InboxItem = serde_json::from_value(item.clone()).map_err(|error| {
        tracing::error!(error = %error, "inbox row is not a wire item");
        ErrorResponse::from(ApiError::internal("Internal error"))
    })?;
    let payload = SessionInboxEnqueued {
        session_id: SessionId::from_wire(session_id),
        inbox_id: MessageId::from_wire(id),
        item,
    };
    publish_durable(
        state,
        session_id,
        SessionInboxEnqueued::TYPE,
        payload.data(),
        SessionInboxEnqueued::VERSION,
    )
    .await?;
    Ok(())
}

fn created_event_data(session: &Value, version: &str) -> Value {
    let id = session
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let directory = session
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let mut map = Map::new();
    map.insert("sessionID".to_owned(), Value::String(id.clone()));
    map.insert(
        "projectID".to_owned(),
        session
            .get("projectID")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new())),
    );
    map.insert("location".to_owned(), json!({ "directory": directory }));
    for key in [
        "subpath",
        "parentID",
        "title",
        "agent",
        "model",
        "metadata",
        "permissions",
    ] {
        if let Some(value) = session.get(key) {
            map.insert(key.to_owned(), value.clone());
        }
    }
    map.insert("slug".to_owned(), Value::String(slug()));
    map.insert("version".to_owned(), Value::String(version.to_owned()));
    Value::Object(map)
}

/// Upstream-style random adjective-noun slug.
fn slug() -> String {
    use rand::Rng as _;
    const ADJECTIVES: [&str; 8] = [
        "brave", "calm", "clever", "cosmic", "gentle", "neon", "quiet", "swift",
    ];
    const NOUNS: [&str; 8] = [
        "cabin", "comet", "falcon", "harbor", "meadow", "nebula", "river", "tiger",
    ];
    let mut rng = rand::rng();
    format!(
        "{}-{}",
        ADJECTIVES[rng.random_range(0..ADJECTIVES.len())],
        NOUNS[rng.random_range(0..NOUNS.len())]
    )
}
