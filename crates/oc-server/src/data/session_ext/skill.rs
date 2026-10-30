//! `POST /api/experimental/session/{sid}/skill`: activates a configured skill by
//! enqueueing its body as a synthetic prompt.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use oc_store::{Delivery, NewInbox};
use oc_types::{ApiError, MessageId};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::data::session::{notify, publish_enqueued};
use crate::data::{no_content, require_config, require_store, schema_error};
use crate::error::ErrorResponse;

use super::common::require_session;

/// `POST /api/experimental/session/:sid/skill`
#[derive(Debug, Deserialize)]
pub struct SkillBody {
    id: String,
    #[serde(default)]
    resume: Option<bool>,
}

/// `POST /api/experimental/session/:sid/skill`
pub async fn skill(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<SkillBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let (skill_name, skill_id, skill_content) = {
        let config = require_config(&state)?;
        let skill = config.skill(&body.id).ok_or_else(|| {
            ApiError::not_found(
                "SkillNotFoundError",
                format!("Skill not found: {}", body.id),
            )
            .with("skill", json!(body.id))
        })?;
        (skill.name.clone(), skill.id.clone(), skill.content.clone())
    };
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;

    // The runtime owns loaded skill state; a synthetic inbox item carrying the
    // skill body is the durable hand-off it consumes like any other prompt.
    let text = format!(
        "<skill_content name=\"{skill_name}\">\n{}\n</skill_content>",
        skill_content.trim_end()
    );
    let id = MessageId::new().into_inner();
    let item = store
        .enqueue(NewInbox {
            id: id.clone(),
            session_id: session_id.clone(),
            kind: "synthetic".to_owned(),
            payload: json!({
                "text": text,
                "description": format!("Activate skill {skill_id}"),
            }),
            delivery: Delivery::Queue,
        })
        .await?;
    publish_enqueued(&state, &session_id, &id, &item).await?;
    if body.resume != Some(false) {
        notify(&state, &session_id).await?;
    }
    Ok(no_content())
}
