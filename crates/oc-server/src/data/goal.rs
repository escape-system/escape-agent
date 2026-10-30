//! Experimental goal routes: current goal, set, patch and history.
//!
//! A goal belongs to the root of a session tree. Every route resolves the
//! requested session through [`oc_store::Store::session_node`], so a subagent session
//! addresses the same goal as its root and an unknown session is a 404.
//!
//! `PATCH /goal/:id` returns `409 ConflictError` for a reactivation
//! (`done`/`failed`/`cancelled` → `active`) as well as for the store's
//! one-active-goal-per-root rule.

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use oc_store::{GoalPatch, NewGoal};
use oc_types::{ApiError, GoalId, now_millis};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::{query_error, require_session_root, require_store, schema_error};
use crate::error::ErrorResponse;

/// `GET /api/experimental/goal?sessionID=`
#[derive(Debug, Deserialize)]
pub struct SessionParams {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

/// `POST /api/experimental/goal`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetBody {
    #[serde(rename = "sessionID")]
    session_id: String,
    objective: String,
    #[serde(default)]
    criteria: Vec<String>,
    #[serde(default)]
    model: Option<String>,
}

/// `PATCH /api/experimental/goal/:id`
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchBody {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    rounds: Option<u32>,
    #[serde(default)]
    stalls: Option<u32>,
    #[serde(default)]
    last_progress: Option<i64>,
    #[serde(default)]
    evidence: Option<Vec<String>>,
}

/// `GET /api/experimental/goal?sessionID=`
pub async fn get(
    State(state): State<AppState>,
    params: Result<Query<SessionParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let session_id = require_session_query(params.session_id)?;
    let root = require_session_root(&state, &session_id).await?;
    let store = require_store(&state)?;
    let goal = store.get_goal(&root).await?;
    Ok(Json(json!({ "data": goal })))
}

/// `POST /api/experimental/goal`
pub async fn set(
    State(state): State<AppState>,
    body: Result<Json<SetBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.objective.trim().is_empty() {
        return Err(ApiError::bad_request("objective must not be empty").into());
    }
    let root = require_session_root(&state, &body.session_id).await?;
    let store = require_store(&state)?;
    let model = match body.model {
        Some(model) => Some(model),
        None => session_model(store, &body.session_id).await?,
    };
    let goal = store
        .set_goal(NewGoal {
            id: GoalId::new().into_inner(),
            root_id: root,
            objective: body.objective,
            criteria: body.criteria,
            model,
            time_created: now_millis(),
        })
        .await?;
    Ok(Json(json!({ "data": goal })))
}

/// `PATCH /api/experimental/goal/:id?sessionID=`
///
/// `sessionID` disambiguates the current status before an `active` transition.
/// Without it an activation is rejected (409): the route cannot tell a
/// terminal goal from a resumable one, and reviving `done`/`failed`/`cancelled`
/// goals is invalid. The store's one-active-goal rule is enforced either way.
pub async fn patch(
    Path(id): Path<String>,
    State(state): State<AppState>,
    params: Result<Query<SessionParams>, QueryRejection>,
    body: Result<Json<PatchBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;

    if body.status.as_deref() == Some("active") {
        let Some(session_id) = params
            .session_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
        else {
            // Without a session there is no way to read the current status, and
            // the only documented activation transition is from
            // `paused`/`blocked`; reject rather than reviving a terminal goal.
            return Err(ApiError::conflict(format!(
                "cannot activate goal {id} without sessionID; terminal goals are not reactivatable"
            ))
            .into());
        };
        let root = require_session_root(&state, session_id).await?;
        if let Some(current) = store
            .list_goals(&root)
            .await?
            .into_iter()
            .find(|goal| goal.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            let previous = current
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(previous, "done" | "failed" | "cancelled") {
                return Err(ApiError::conflict(format!(
                    "cannot reactivate goal {id}: {previous} is terminal"
                ))
                .into());
            }
        }
    }

    let updated = store
        .update_goal(
            &id,
            GoalPatch {
                status: body.status,
                rounds: body.rounds,
                stalls: body.stalls,
                last_progress: body.last_progress,
                evidence: body.evidence,
            },
        )
        .await?;
    let updated = updated.ok_or_else(|| {
        ErrorResponse::from(ApiError::not_found(
            "NotFoundError",
            format!("Goal not found: {id}"),
        ))
    })?;
    Ok(Json(json!({ "data": updated })))
}

/// `GET /api/experimental/goal/history?sessionID=`
pub async fn history(
    State(state): State<AppState>,
    params: Result<Query<SessionParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let session_id = require_session_query(params.session_id)?;
    let root = require_session_root(&state, &session_id).await?;
    let store = require_store(&state)?;
    let goals = store.list_goals(&root).await?;
    Ok(Json(json!({ "data": goals })))
}

fn require_session_query(session_id: Option<String>) -> Result<String, ErrorResponse> {
    session_id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("sessionID is required").into())
}

/// Compact `provider/id#variant` reference from the session's stored model.
async fn session_model(
    store: &oc_store::Store,
    session_id: &str,
) -> Result<Option<String>, ErrorResponse> {
    let session = store.get_session(session_id).await?.unwrap_or(Value::Null);
    Ok(compact_model(session.get("model")))
}

fn compact_model(model: Option<&Value>) -> Option<String> {
    let model = model?;
    let provider = model.get("providerID")?.as_str()?;
    let id = model.get("id")?.as_str()?;
    match model
        .get("variant")
        .and_then(Value::as_str)
        .filter(|variant| !variant.is_empty())
    {
        Some(variant) => Some(format!("{provider}/{id}#{variant}")),
        None => Some(format!("{provider}/{id}")),
    }
}
