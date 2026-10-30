//! Experimental schedule routes.
//!
//! `Schedule.Info` is persisted by `oc-store`; this module only decodes the
//! wire request, resolves the owning session and shapes the `{data}` envelope.
//! Timing semantics (duration/cron parsing, time zones, the first due instant,
//! `ends` limits and resume recomputation) belong to the store so the scheduler
//! and the HTTP surface agree exactly.
//!
//! Routes:
//! - `GET /api/experimental/schedule?sessionID=` lists schedules.
//! - `POST /api/experimental/schedule` creates one.
//! - `GET/PATCH/DELETE /api/experimental/schedule/{id}` read, update or remove.
//! - `POST /api/experimental/schedule/{id}/pause` and `.../resume` toggle.
//!
//! Every handler requires an attached [`oc_store::Store`] and answers the
//! documented `503 ServiceUnavailableError` otherwise. An unknown session is
//! `404 SessionNotFoundError`; an unknown schedule is
//! `404 ScheduleNotFoundError`; invalid durations, cron expressions, time zones
//! or bodies are `400 BadRequestError`/`SchemaError`.

use axum::Json;
use axum::Router;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use oc_store::{
    NewSchedule, ScheduleAction, ScheduleEnds, SchedulePatch, ScheduleWhen, Store, StoreError,
};
use oc_types::{ApiError, now_millis};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::{no_content, query_error, require_store, schema_error, session_not_found};
use crate::error::ErrorResponse;

/// The schedule routes, mounted under the protected layer.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/experimental/schedule", get(list).post(create))
        .route(
            "/api/experimental/schedule/{id}",
            get(read).patch(patch).delete(remove),
        )
        .route("/api/experimental/schedule/{id}/pause", post(pause))
        .route("/api/experimental/schedule/{id}/resume", post(resume))
}

/// `GET /api/experimental/schedule?sessionID=`
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

/// The `when` object callers write: exactly one trigger plus an optional zone.
#[derive(Debug, Default, Deserialize)]
pub struct WhenInput {
    #[serde(default)]
    r#in: Option<String>,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    every: Option<String>,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    tz: Option<String>,
}

/// `ends` limits a repeating schedule.
#[derive(Debug, Default, Deserialize)]
pub struct EndsInput {
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    times: Option<u32>,
}

/// `POST /api/experimental/schedule`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBody {
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(default)]
    name: Option<String>,
    when: WhenInput,
    #[serde(default)]
    ends: Option<EndsInput>,
    prompt: String,
}

/// `PATCH /api/experimental/schedule/:id`
#[derive(Debug, Default, Deserialize)]
pub struct PatchBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    when: Option<WhenInput>,
    #[serde(default)]
    ends: Option<EndsInput>,
    #[serde(default)]
    prompt: Option<String>,
}

/// `GET /api/experimental/schedule?sessionID=`
pub async fn list(
    State(state): State<AppState>,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    let session_id = params
        .session_id
        .as_deref()
        .filter(|id| !id.trim().is_empty());
    if let Some(session_id) = session_id {
        ensure_session(store, session_id).await?;
    }
    let schedules = store.list_schedules(session_id).await?;
    Ok(Json(json!({ "data": schedules })))
}

/// `POST /api/experimental/schedule`
pub async fn create(
    State(state): State<AppState>,
    body: Result<Json<CreateBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    require_prompt(&body.prompt)?;
    let when = convert_when(body.when)?;
    let ends = body.ends.map(convert_ends);
    let action = ScheduleAction::Prompt {
        prompt: body.prompt,
    };
    ensure_session(store, &body.session_id).await?;
    let session_id = body.session_id.clone();
    let schedule = store
        .create_schedule(NewSchedule {
            session_id: body.session_id,
            name: body.name,
            when,
            ends,
            action,
            time_created: now_millis(),
        })
        .await
        .map_err(|error| match error {
            // The store re-checks the session; keep the route's documented tag.
            StoreError::NotFound(_) => ErrorResponse::from(session_not_found(&session_id)),
            other => ErrorResponse::from(other),
        })?;
    Ok(Json(json!({ "data": schedule })))
}

/// `GET /api/experimental/schedule/:id`
pub async fn read(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    let schedule = store
        .get_schedule(&id)
        .await?
        .ok_or_else(|| ErrorResponse::from(schedule_not_found(&id)))?;
    Ok(Json(json!({ "data": schedule })))
}

/// `PATCH /api/experimental/schedule/:id`
pub async fn patch(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<PatchBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    if let Some(prompt) = body.prompt.as_deref() {
        require_prompt(prompt)?;
    }
    let patch = SchedulePatch {
        name: body.name,
        when: body.when.map(convert_when).transpose()?,
        ends: body.ends.map(convert_ends),
        prompt: body.prompt,
    };
    let schedule = store
        .update_schedule(&id, patch)
        .await?
        .ok_or_else(|| ErrorResponse::from(schedule_not_found(&id)))?;
    Ok(Json(json!({ "data": schedule })))
}

/// `DELETE /api/experimental/schedule/:id`
pub async fn remove(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    if !store.delete_schedule(&id).await? {
        return Err(schedule_not_found(&id).into());
    }
    Ok(no_content())
}

/// `POST /api/experimental/schedule/:id/pause`
pub async fn pause(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    let schedule = store
        .pause_schedule(&id)
        .await?
        .ok_or_else(|| ErrorResponse::from(schedule_not_found(&id)))?;
    Ok(Json(json!({ "data": schedule })))
}

/// `POST /api/experimental/schedule/:id/resume`
pub async fn resume(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    let schedule = store
        .resume_schedule(&id)
        .await?
        .ok_or_else(|| ErrorResponse::from(schedule_not_found(&id)))?;
    Ok(Json(json!({ "data": schedule })))
}

/// A session that the store does not know is the documented 404.
async fn ensure_session(store: &Store, session_id: &str) -> Result<(), ErrorResponse> {
    if store.session_node(session_id).await?.is_none() {
        return Err(session_not_found(session_id).into());
    }
    Ok(())
}

/// `Schedule.When` from the caller's one-of trigger object.
///
/// Exactly one of `in`, `at`, `every` or `cron` is required. Duration, instant
/// and cron validation happen in the store when the schedule is created.
fn convert_when(input: WhenInput) -> Result<ScheduleWhen, ApiError> {
    let triggers = [
        ("in", input.r#in),
        ("at", input.at),
        ("every", input.every),
        ("cron", input.cron),
    ];
    let mut present = triggers
        .iter()
        .filter_map(|(name, value)| value.clone().map(|value| (*name, value)));
    let Some((name, value)) = present.next() else {
        return Err(ApiError::bad_request(
            "when must contain exactly one of in, at, every or cron",
        ));
    };
    if present.next().is_some() {
        return Err(ApiError::bad_request(
            "when must contain exactly one of in, at, every or cron",
        ));
    }
    let tz = input.tz;
    Ok(match name {
        "in" => ScheduleWhen::In { r#in: value, tz },
        "at" => ScheduleWhen::At { at: value, tz },
        "every" => ScheduleWhen::Every { every: value, tz },
        _ => ScheduleWhen::Cron { cron: value, tz },
    })
}

/// `Schedule.Ends` from the caller's optional limits object.
fn convert_ends(input: EndsInput) -> ScheduleEnds {
    ScheduleEnds {
        after: input.after,
        at: input.at,
        times: input.times,
    }
}

/// A prompt is the whole action, so whitespace alone is not a message.
fn require_prompt(prompt: &str) -> Result<(), ApiError> {
    if prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }
    Ok(())
}

fn schedule_not_found(id: &str) -> ApiError {
    ApiError::not_found("ScheduleNotFoundError", format!("Schedule not found: {id}"))
        .with("scheduleID", json!(id))
}
