//! Permission routes: pending requests, replies and rule evaluation.
//!
//! `GET`/`POST .../permission` describe the live broker. The evaluator route
//! only computes the configured effect; it never enqueues a request, because
//! requests are created by the runtime's `Ask` flow so their lifetime is tied
//! to an execution.

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use oc_config::{ConfigService, Effect as ConfigEffect};
use oc_store::{Order, SessionQuery};
use oc_types::permission::{PermissionReply, PermissionRule as WireRule};
use oc_types::{ApiError, LocationEnvelope, PermissionRequestId};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::session::LocationQuery;
use crate::data::{
    query_error, require_broker, require_config, require_store, schema_error, session_not_found,
};
use crate::error::ErrorResponse;

/// `POST /api/session/:sid/permission/:requestID/reply`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplyBody {
    decision: PermissionReply,
    #[serde(default)]
    message: Option<String>,
}

/// `POST /api/session/:sid/permission`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateBody {
    #[serde(default)]
    id: Option<String>,
    action: String,
    resources: Vec<String>,
    #[serde(default)]
    save: Option<Vec<String>>,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
    #[serde(default)]
    source: Option<Value>,
    #[serde(default)]
    agent: Option<String>,
}

/// `GET /api/session/:sid/permission`
pub async fn list(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    require_session(&state, &session_id).await?;
    let broker = require_broker(&state)?;
    Ok(Json(json!({ "data": broker.pending(&session_id) })))
}

/// `GET /api/session/:sid/permission/:request_id`
pub async fn get(
    Path((session_id, request_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    require_session(&state, &session_id).await?;
    let broker = require_broker(&state)?;
    let request = broker.request(&session_id, &request_id).ok_or_else(|| {
        ApiError::not_found(
            "PermissionNotFoundError",
            format!("Permission request not found: {request_id}"),
        )
        .with("requestID", json!(request_id))
    })?;
    Ok(Json(json!({ "data": request })))
}

/// `POST /api/session/:sid/permission/:request_id/reply`
pub async fn reply(
    Path((session_id, request_id)): Path<(String, String)>,
    State(state): State<AppState>,
    body: Result<Json<ReplyBody>, JsonRejection>,
) -> Result<axum::response::Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    require_session(&state, &session_id).await?;
    let broker = require_broker(&state)?;
    broker.reply(&session_id, &request_id, body.decision, body.message)?;
    Ok(crate::data::no_content())
}

/// `POST /api/session/:sid/permission`
pub async fn evaluate(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<EvaluateBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.action.trim().is_empty() {
        return Err(ApiError::new(400, "BadRequestError", "action must not be empty").into());
    }
    if body.resources.is_empty() {
        return Err(ApiError::new(400, "BadRequestError", "resources must not be empty").into());
    }
    let store = require_store(&state)?;
    let session = store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    let config = require_config(&state)?;
    let rules: Vec<WireRule> = session
        .get("permissions")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default();
    let agent = body.agent.or_else(|| {
        session
            .get("agent")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let action = body.action;
    let resources = body.resources;
    let mut effect = strictest(&resources, |resource| {
        composed_effect(config, agent.as_deref(), &rules, &action, resource)
    });
    let id = body
        .id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| PermissionRequestId::new().into_inner());
    if effect == ConfigEffect::Ask {
        let broker = require_broker(&state)?;
        // Saved `always` rules participate in this evaluation too. Otherwise a
        // client that asks about an already-saved action parks a request that
        // nothing awaits (config `deny` still wins: `strictest` returned it
        // above and this branch is only reached for `ask`).
        if broker.covers_saved(&session_id, &action, &resources).await {
            effect = ConfigEffect::Allow;
        } else {
            let mut data = Map::new();
            data.insert("id".to_owned(), json!(id));
            data.insert("sessionID".to_owned(), json!(session_id));
            data.insert("action".to_owned(), json!(action.clone()));
            data.insert("resources".to_owned(), json!(resources.clone()));
            if let Some(save) = body.save {
                data.insert("save".to_owned(), json!(save));
            }
            if let Some(metadata) = body.metadata {
                data.insert("metadata".to_owned(), Value::Object(metadata));
            }
            if let Some(source) = body.source {
                data.insert("source".to_owned(), source);
            }
            broker.enqueue(&session_id, &id, action, resources, Value::Object(data));
        }
    }
    Ok(Json(json!({
        "data": { "id": id, "effect": effect.as_str() },
    })))
}

/// `GET /api/permission/request` — location-scoped pending list.
pub async fn pending(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved =
        crate::data::location::resolve(&state, &headers, None, params.directory.as_deref());
    let broker = require_broker(&state)?;
    let store = require_store(&state)?;

    let mut requests = Vec::new();
    let mut cursor = None;
    loop {
        let page = store
            .list_sessions(SessionQuery {
                limit: 200,
                order: Order::Desc,
                directory: Some(resolved.directory.clone()),
                cursor,
                ..SessionQuery::default()
            })
            .await?;
        for session in &page.items {
            if let Some(id) = session.get("id").and_then(Value::as_str) {
                requests.extend(broker.pending(id));
            }
        }
        match page.cursor.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Array(requests),
    )))
}

async fn require_session(state: &AppState, session_id: &str) -> Result<(), ErrorResponse> {
    let store = require_store(state)?;
    if store.get_session(session_id).await?.is_none() {
        return Err(session_not_found(session_id).into());
    }
    Ok(())
}

fn composed_effect(
    config: &ConfigService,
    agent: Option<&str>,
    rules: &[WireRule],
    action: &str,
    resource: &str,
) -> ConfigEffect {
    let mut effect = config.evaluate(agent, action, resource);
    for rule in rules {
        if oc_config::wildcard_match(action, &rule.action)
            && oc_config::wildcard_match(resource, &rule.resource)
        {
            effect = match rule.effect {
                oc_types::Effect::Allow => ConfigEffect::Allow,
                oc_types::Effect::Deny => ConfigEffect::Deny,
                oc_types::Effect::Ask => ConfigEffect::Ask,
            };
        }
    }
    effect
}

fn strictest(resources: &[String], evaluate: impl Fn(&str) -> ConfigEffect) -> ConfigEffect {
    let mut result = ConfigEffect::Allow;
    for resource in resources {
        match evaluate(resource) {
            ConfigEffect::Deny => return ConfigEffect::Deny,
            ConfigEffect::Ask => result = ConfigEffect::Ask,
            ConfigEffect::Allow => {}
        }
    }
    result
}
