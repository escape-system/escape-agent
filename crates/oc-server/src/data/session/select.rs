//! Agent/model selection and moving a session to another directory.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use oc_store::SessionPatch;
use oc_types::{
    ApiError, EventPayload, ModelRef, SessionAgentSelected, SessionId, SessionModelSelected,
    SessionMoved,
};
use serde_json::{Value, json};

use super::bodies::{AgentBody, ModelBody, MoveBody};
use crate::AppState;
use crate::data::events::publish_durable;
use crate::data::location;
use crate::data::{no_content, require_config, require_store, schema_error, session_not_found};
use crate::error::ErrorResponse;

/// Normalize a variant: absent, empty and `"default"` select the same model.
fn normalized_variant(variant: Option<&str>) -> Option<&str> {
    variant.filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("default"))
}

fn same_effective_model(previous: &ModelRef, next: &ModelRef) -> bool {
    previous.provider_id == next.provider_id
        && previous.id == next.id
        && normalized_variant(previous.variant.as_deref())
            == normalized_variant(next.variant.as_deref())
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
    // A client (or a stored row) may still name a pre-rename agent id such as
    // `build`, `plan` or `reviewer`. Resolve it through the alias so the
    // canonical id is what gets validated, persisted and published.
    let agent = oc_config::normalize_agent_id(body.agent.trim()).to_owned();
    if config.agent(&agent).is_none() {
        return Err(
            ApiError::not_found("AgentNotFoundError", format!("Agent not found: {agent}"))
                .with("agentID", json!(&agent))
                .into(),
        );
    }
    let previous = existing
        .get("agent")
        .and_then(Value::as_str)
        .map(str::to_owned);

    store
        .patch_session(
            &session_id,
            SessionPatch {
                agent: Some(agent.clone()),
                ..SessionPatch::default()
            },
        )
        .await?;

    let payload = SessionAgentSelected {
        session_id: SessionId::from_wire(session_id.as_str()),
        agent,
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
    let Json(mut body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.model.id.is_empty() || body.model.provider_id.is_empty() {
        return Err(
            ApiError::new(400, "BadRequestError", "model requires id and providerID").into(),
        );
    }
    body.model.variant = normalized_variant(body.model.variant.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("default"))
        .map(str::to_owned);
    if let Some(catalog) = state.catalog()
        && catalog
            .model(&body.model.provider_id, &body.model.id)
            .is_none()
        && !(body.model.provider_id == "google"
            && state.antigravity().is_some_and(|ag| {
                let reference = match body.model.variant.as_deref() {
                    Some(variant) => {
                        format!("{}/{}#{variant}", body.model.provider_id, body.model.id)
                    }
                    None => format!("{}/{}", body.model.provider_id, body.model.id),
                };
                ag.resolve_catalog_model(catalog.as_ref(), &reference)
                    .is_some()
                    || ag
                        .antigravity_model_infos()
                        .iter()
                        .any(|m| m.id == body.model.id || m.model_id == body.model.id)
            }))
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

    // The explicit client choice is always persisted, but repeating the same
    // effective selection (variant absent/empty/"default") must not spam the
    // session timeline with `session.model.selected` notices.
    if previous
        .as_ref()
        .is_some_and(|prev| same_effective_model(prev, &body.model))
    {
        tracing::debug!(session = %session_id, "model selection unchanged; skipping session.model.selected event");
        return Ok(no_content());
    }
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
