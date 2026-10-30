//! Saved permission rule listing and maintenance under `/api/permission/saved`.

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::{no_content, query_error, require_store, schema_error};
use crate::error::ErrorResponse;

/// `GET /api/permission/saved?projectID=`
#[derive(Debug, Default, Deserialize)]
pub struct SavedPermissionParams {
    #[serde(default, rename = "projectID")]
    project_id: Option<String>,
}

/// `PATCH /api/permission/saved/:id`
#[derive(Debug, Default, Deserialize)]
pub struct SavedPermissionPatch {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

fn permission_not_found(id: &str) -> ApiError {
    ApiError::not_found(
        "PermissionSavedNotFoundError",
        format!("Saved permission not found: {id}"),
    )
    .with("id", json!(id))
}

/// `GET /api/permission/saved`
pub async fn saved_permissions(
    State(state): State<AppState>,
    params: Result<Query<SavedPermissionParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    let rows = store
        .list_saved_permissions(params.project_id.as_deref())
        .await?;
    Ok(Json(json!({ "data": rows })))
}

/// `PATCH /api/permission/saved/:id`
pub async fn saved_permission_update(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<SavedPermissionPatch>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let updated = store
        .update_saved_permission(&id, None, body.action.as_deref(), body.resource.as_deref())
        .await?;
    let updated = updated.ok_or_else(|| permission_not_found(&id))?;
    Ok(Json(json!({ "data": updated })))
}

/// `DELETE /api/permission/saved/:id`
pub async fn saved_permission_delete(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    store.delete_saved_permission(&id).await?;
    Ok(no_content())
}
