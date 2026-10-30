//! Experimental observer audit routes.
//!
//! Repairs are durable rows, so they are returned even when no observer engine
//! is attached (`enabled: false`). Engine-only fields (`model`, `activeStrips`,
//! `budget`) are included only when the engine is present; the audit never
//! fabricates counters.
//!
//! `POST /observer/check` forwards to the runtime's `request_goal_check`; it
//! answers `204` only after the engine ran, and `503` when either the session
//! runtime or its observer binding is missing.

use axum::Json;
use axum::Router;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::{
    no_content, query_error, require_session_root, require_store, runtime_error,
    runtime_unavailable, schema_error,
};
use crate::error::ErrorResponse;

/// `data::observer` route table.
///
/// The caller merges this into the protected router; it is not an
/// `AppState`-owning router by itself.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/experimental/observer", get(audit))
        .route("/api/experimental/observer/check", post(check))
        .route("/api/experimental/observer/repairs", get(repairs))
}

/// `GET /api/experimental/observer?sessionID=`
#[derive(Debug, Deserialize)]
pub struct SessionParams {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

/// `GET /api/experimental/observer/repairs?sessionID=&limit=`
#[derive(Debug, Default, Deserialize)]
pub struct RepairsParams {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    limit: Option<usize>,
}

/// `POST /api/experimental/observer/check`
#[derive(Debug, Deserialize)]
pub struct CheckBody {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

/// `GET /api/experimental/observer?sessionID=`
pub async fn audit(
    State(state): State<AppState>,
    params: Result<Query<SessionParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let session_id = require_session(params.session_id)?;
    let root = require_session_root(&state, &session_id).await?;
    let store = require_store(&state)?;

    let mut data = Map::new();
    let repairs = store.list_repairs(&root, 20).await?;
    match state.observer() {
        Some(engine) => {
            let config = engine.config();
            data.insert("enabled".to_owned(), Value::Bool(config.enabled));
            if let Some(model) = &config.model {
                data.insert("model".to_owned(), Value::String(model.clone()));
            }
            data.insert(
                "activeStrips".to_owned(),
                json!(engine.registry().active(&root).len()),
            );
            data.insert(
                "budget".to_owned(),
                serde_json::to_value(engine.budget(&root)).unwrap_or(Value::Null),
            );
        }
        None => {
            data.insert("enabled".to_owned(), Value::Bool(false));
        }
    }
    data.insert("repairs".to_owned(), Value::Array(repairs));
    Ok(Json(json!({ "data": data })))
}

/// `GET /api/experimental/observer/repairs?sessionID=&limit=`
pub async fn repairs(
    State(state): State<AppState>,
    params: Result<Query<RepairsParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let session_id = require_session(params.session_id)?;
    let root = require_session_root(&state, &session_id).await?;
    let store = require_store(&state)?;
    let limit = params.limit.unwrap_or(20).min(500);
    let items = store.list_repairs(&root, limit).await?;
    Ok(Json(json!({ "data": items })))
}

/// `POST /api/experimental/observer/check`
///
/// The session is resolved to its root first, so an unknown session is a
/// `404` even when no runtime is attached. The runtime then runs one forced
/// observer pass; a runtime without an observer binding is the documented
/// `503`, and a successful run is a bare `204`.
pub async fn check(
    State(state): State<AppState>,
    body: Result<Json<CheckBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let session_id = require_session(body.session_id)?;
    let root = require_session_root(&state, &session_id).await?;
    let runtime = state
        .runtime()
        .ok_or_else(|| ErrorResponse::from(runtime_unavailable()))?;

    runtime.request_goal_check(&root).await.map_err(|error| {
        ErrorResponse::from(match error {
            // `request_goal_check` only reports `Config` when no observer is
            // bound; the route must not claim a check happened.
            oc_runtime::RuntimeError::Config(message) => ApiError::service_unavailable(message),
            other => runtime_error(other),
        })
    })?;
    Ok(no_content())
}

fn require_session(session_id: Option<String>) -> Result<String, ErrorResponse> {
    session_id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("sessionID is required").into())
}
