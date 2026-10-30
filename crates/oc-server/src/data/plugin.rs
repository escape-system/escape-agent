//! `/api/plugin`, `/api/rpc/:rpcID/:method` and the plugin update routes.
//!
//! Only native plugins are visible here. When no registry is attached the list
//! route is truthful: an empty inventory if nothing is configured, and a `503`
//! when `plugins` is configured but the host is missing (fail closed).
//!
//! RPC dispatches through [`oc_plugin::PluginRegistry::invoke`]. GUI bundles and the npm
//! package installer are not part of this host: a GUI request is a truthful
//! `404`, and an update request fails with `503` plus a per-target failure
//! instead of pretending the install succeeded.

use axum::Json;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use oc_plugin::{
    PluginCheckRequest, PluginInfo, PluginListEnvelope, PluginSource, PluginUpdateRequest, RpcError,
};
use oc_types::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::location;
use crate::data::query_error;
use crate::data::session::LocationQuery;
use crate::error::ErrorResponse;

fn plugins_configured(state: &AppState) -> bool {
    state
        .config()
        .is_some_and(|config| match config.merged().get("plugins") {
            Some(Value::Array(items)) => !items.is_empty(),
            Some(Value::Object(entries)) => !entries.is_empty(),
            Some(Value::String(target)) => !target.trim().is_empty(),
            Some(Value::Null) | None => false,
            Some(_) => true,
        })
}

/// Inventory rows, or `None` when the host is genuinely absent but configured.
fn inventory(state: &AppState) -> Result<Vec<PluginInfo>, ErrorResponse> {
    if let Some(registry) = state.plugins() {
        return Ok(registry.inventory());
    }
    if plugins_configured(state) {
        return Err(ApiError::service_unavailable("plugin host not attached").into());
    }
    Ok(Vec::new())
}

fn plugin_not_found(id: &str) -> ApiError {
    ApiError::not_found(
        "PluginNotFoundError",
        format!("Plugin GUI bundle not found: {id}"),
    )
    .with("id", json!(id))
}

/// `GET /api/plugin`
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<PluginListEnvelope>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let location = serde_json::to_value(resolved.reference()).unwrap_or(Value::Null);
    let rows = inventory(&state)?;
    Ok(Json(PluginListEnvelope::new(location, rows)))
}

/// `GET /api/plugin/gui?id=`
#[derive(Debug, Default, Deserialize)]
pub struct GuiQuery {
    /// Plugin id whose GUI bundle should be read.
    pub id: Option<String>,
    /// Requested working directory.
    #[serde(rename = "location[directory]")]
    pub directory: Option<String>,
}

pub async fn gui(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<GuiQuery>, QueryRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let Some(id) = params
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return Err(ApiError::bad_request("Missing required query parameter: id").into());
    };
    let _ = location::resolve(&state, &headers, None, params.directory.as_deref());
    let Some(registry) = state.plugins() else {
        if plugins_configured(&state) {
            return Err(ApiError::service_unavailable("plugin host not attached").into());
        }
        return Err(plugin_not_found(id).into());
    };
    // The native registry tracks the `gui` feature flag but this host does not
    // load `./gui-experimental` bundles, so every lookup is truthfully absent.
    let message = if registry.info(id).is_some() {
        format!("Plugin GUI bundle not available: {id}")
    } else {
        format!("Plugin not found: {id}")
    };
    Err(ApiError::not_found("PluginNotFoundError", message)
        .with("id", json!(id))
        .into())
}

/// `POST /api/plugin/check`
///
/// Package update metadata requires a package-manager query, which this host
/// does not perform. The truthful response is the current inventory (no
/// `outdated` flags are invented), optionally filtered to one target.
pub async fn check(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
    body: Option<Json<PluginCheckRequest>>,
) -> Result<Json<PluginListEnvelope>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let location = serde_json::to_value(resolved.reference()).unwrap_or(Value::Null);
    let mut rows = inventory(&state)?;
    if let Some(Json(request)) = body
        && let Some(target) = request.target.as_deref().map(str::trim)
        && !target.is_empty()
    {
        rows.retain(|info| matches_target(info, target));
    }
    Ok(Json(PluginListEnvelope::new(location, rows)))
}

fn matches_target(info: &PluginInfo, target: &str) -> bool {
    if info.id == target {
        return true;
    }
    matches!(&info.source, PluginSource::Package { target: package, .. } if package.as_str() == target)
}

/// `POST /api/plugin/update`
///
/// No installer is attached, so every requested target fails truthfully with
/// `503`; an empty request is a no-op `204`. The failure message names each
/// target, keeping the body inside the declared `ServiceUnavailableError`
/// schema (no invented success and no extra fields).
pub async fn update(Json(body): Json<PluginUpdateRequest>) -> Result<Response, ErrorResponse> {
    if body.targets.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let targets = body.targets.join(", ");
    Err(ApiError::service_unavailable(format!(
        "plugin installer not available; failed targets: {targets}"
    ))
    .with("service", json!("plugin-installer"))
    .into())
}

/// `POST /api/rpc/:rpcID/:method`
pub async fn rpc(
    State(state): State<AppState>,
    AxumPath((rpc_id, method)): AxumPath<(String, String)>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, ErrorResponse> {
    let Some(registry) = state.plugins() else {
        return Err(ApiError::service_unavailable("plugin RPC host not attached").into());
    };
    let input = body
        .and_then(|Json(value)| value.get("input").cloned())
        .unwrap_or(Value::Null);
    let output = registry
        .invoke(&rpc_id, &method, input)
        .map_err(rpc_failure)?;
    if output.is_null() {
        Ok(Json(json!({})))
    } else {
        Ok(Json(json!({ "output": output })))
    }
}

fn rpc_failure(error: RpcError) -> ErrorResponse {
    let failure = match error {
        RpcError::NamespaceNotFound { namespace } => {
            ApiError::not_found("RpcError", format!("RPC is unavailable: {namespace}"))
                .with("type", json!("rpc.unavailable"))
                .with("data", json!({"namespace": namespace}))
        }
        RpcError::MethodNotFound { namespace, method } => ApiError::not_found(
            "RpcError",
            format!("Unknown RPC method: {namespace}.{method}"),
        )
        .with("type", json!("rpc.method_not_found"))
        .with("data", json!({"namespace": namespace, "method": method})),
        RpcError::InvalidInput { message } => {
            ApiError::new(400, "RpcError", message).with("type", json!("rpc.invalid_input"))
        }
        RpcError::Failed { message, data } => {
            let failure =
                ApiError::new(500, "RpcInternalError", message).with("type", json!("rpc.internal"));
            match data {
                Some(data) => failure.with("data", data),
                None => failure,
            }
        }
        RpcError::Timeout { method, timeout_ms } => ApiError::new(
            503,
            "RpcError",
            format!("rpc {method} timed out after {timeout_ms} ms"),
        )
        .with("type", json!("rpc.timeout"))
        .with("data", json!({"method": method, "timeoutMs": timeout_ms})),
        RpcError::Unavailable { message } => {
            ApiError::new(503, "RpcError", message).with("type", json!("rpc.unavailable"))
        }
    };
    failure.into()
}

/// Routes owned by this module.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/plugin", get(list))
        .route("/api/plugin/gui", get(gui))
        .route("/api/plugin/check", post(check))
        .route("/api/plugin/update", post(update))
        .route("/api/rpc/{rpc_id}/{method}", post(rpc))
}
