//! Experimental subagent routes: tree view and peer messages.
//!
//! The tree is a bounded breadth-first walk of `Store::children` starting at
//! the requested session's root, capped at [`MAX_NODES`].
//!
//! The peer-message route is addressed to the target session and the HTTP
//! client is trusted; it only enqueues input. Ancestry and write authorization
//! are enforced by the tool layer (`talk` / `session_write`), not here. The
//! item is published as the normal `session.inbox.enqueued` event.

use std::collections::VecDeque;

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use oc_store::{NewInbox, SessionNodeRow};
use oc_types::{ApiError, MessageId};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::session::{notify, publish_enqueued};
use crate::data::{
    delivery_from_wire, query_error, require_session_root, require_store, schema_error,
    session_not_found,
};
use crate::error::ErrorResponse;

/// Hard cap on nodes returned by the tree route.
pub const MAX_NODES: usize = 200;

/// `GET /api/experimental/subagent/tree?sessionID=`
#[derive(Debug, Deserialize)]
pub struct TreeParams {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

/// `POST /api/experimental/subagent/:sid/message`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageBody {
    text: String,
    delivery: Option<String>,
}

/// `GET /api/experimental/subagent/tree?sessionID=`
pub async fn tree(
    State(state): State<AppState>,
    params: Result<Query<TreeParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let session_id = params
        .session_id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("sessionID is required"))?;
    let root = require_session_root(&state, &session_id).await?;
    let store = require_store(&state)?;
    let Some(root_node) = store.session_node(&root).await? else {
        return Err(session_not_found(&session_id).into());
    };

    let mut nodes = Vec::new();
    let mut queue: VecDeque<SessionNodeRow> = VecDeque::new();
    queue.push_back(root_node);
    while let Some(node) = queue.pop_front() {
        if nodes.len() >= MAX_NODES {
            break;
        }
        let children = store.children(&node.id).await?;
        nodes.push(node_value(&node));
        for child in children {
            if nodes.len() + queue.len() >= MAX_NODES {
                break;
            }
            queue.push_back(child);
        }
    }
    Ok(Json(json!({
        "data": {
            "root": root,
            "nodes": nodes,
        }
    })))
}

/// `POST /api/experimental/subagent/:sid/message`
pub async fn message(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<MessageBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.text.trim().is_empty() {
        return Err(ApiError::bad_request("text must not be empty").into());
    }
    let delivery = delivery_from_wire(body.delivery.as_deref())?;
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;

    let id = MessageId::new().into_inner();
    let mut payload = Map::new();
    payload.insert("text".to_owned(), Value::String(body.text));
    payload.insert("peer".to_owned(), Value::Bool(true));
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

fn node_value(node: &SessionNodeRow) -> Value {
    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(node.id.clone()));
    if let Some(parent_id) = &node.parent_id {
        map.insert("parentID".to_owned(), Value::String(parent_id.clone()));
    }
    map.insert("agent".to_owned(), Value::String(node.agent.clone()));
    if let Some(model) = &node.model {
        map.insert("model".to_owned(), Value::String(model.clone()));
    }
    map.insert("state".to_owned(), Value::String(node.status.clone()));
    map.insert("depth".to_owned(), json!(node.depth));
    map.insert("role".to_owned(), Value::String(node.role.clone()));
    map.insert("spawnAllowed".to_owned(), Value::Bool(node.spawn_allowed));
    map.insert("childCount".to_owned(), json!(node.child_count));
    Value::Object(map)
}
