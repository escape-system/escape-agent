//! `/api/lsp` status and diagnostics routes plus the LSP event bridge.
//!
//! The supervisor spawns language servers lazily on file use. `GET /api/lsp`
//! therefore never forces a spawn: it reports live slots and adds a `cold` row
//! for every configured server that has no client yet.
//!
//! `bridge_events` forwards `lsp.updated` and `lsp.client.diagnostics` onto
//! the server's SSE bus. The task holds only the broadcast receiver, never the
//! supervisor, so it exits when the last supervisor handle is dropped at
//! shutdown (the channel closes).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use oc_lsp::LspSupervisor;
use oc_types::{ApiError, EventEnvelope, LocationEnvelope};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::AppState;
use crate::data::location::{self, RequestLocation};
use crate::data::query_error;
use crate::data::session::LocationQuery;
use crate::error::ErrorResponse;

/// Forward supervisor events onto the SSE bus until the supervisor is dropped.
pub(crate) fn bridge_events(
    supervisor: &LspSupervisor,
    bus: &broadcast::Sender<Arc<EventEnvelope>>,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("no Tokio runtime; LSP events will not reach the SSE bus");
        return;
    };
    let mut events = supervisor.subscribe();
    let bus = bus.clone();
    handle.spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    bus.send(Arc::new(EventEnvelope::now(event.name(), event.payload())))
                        .ok();
                }
                Err(RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "LSP event bridge lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// `GET /api/lsp/diagnostics?path=`
#[derive(Debug, Default, Deserialize)]
pub struct DiagnosticsQuery {
    /// Absolute or location-relative file path.
    pub path: Option<String>,
    /// Requested working directory.
    #[serde(rename = "location[directory]")]
    pub directory: Option<String>,
}

fn require_supervisor(state: &AppState) -> Result<&Arc<LspSupervisor>, ApiError> {
    state
        .lsp()
        .ok_or_else(|| ApiError::service_unavailable("LSP supervisor not attached"))
}

fn envelope(location: &RequestLocation, data: Value) -> Json<LocationEnvelope<Value>> {
    Json(LocationEnvelope::new(location.reference(), data))
}

/// `GET /api/lsp`
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let supervisor = require_supervisor(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());

    let config = supervisor.config();
    let root = supervisor.workspace_root().to_string_lossy().into_owned();
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for status in supervisor.statuses().await {
        seen.insert(status.id.clone());
        let mut row = json!({
            "id": status.id,
            "name": status.name,
            "root": status.root,
            "status": status.status,
        });
        if let Some(rss) = status.rss_bytes {
            row["rssBytes"] = json!(rss);
        }
        rows.push(row);
    }
    for server in config.active_servers() {
        if seen.insert(server.id.clone()) {
            rows.push(json!({
                "id": server.id,
                "name": server.name,
                "root": root,
                "status": "cold",
            }));
        }
    }
    rows.sort_by(|left, right| {
        let left_key = (
            left["id"].as_str().unwrap_or_default(),
            left["root"].as_str().unwrap_or_default(),
        );
        let right_key = (
            right["id"].as_str().unwrap_or_default(),
            right["root"].as_str().unwrap_or_default(),
        );
        left_key.cmp(&right_key)
    });
    Ok(envelope(&resolved, Value::Array(rows)))
}

/// `GET /api/lsp/diagnostics?path=`
pub async fn diagnostics(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<DiagnosticsQuery>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let supervisor = require_supervisor(&state)?;
    let Some(raw) = params
        .path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
    else {
        return Err(ErrorResponse::from(ApiError::bad_request(
            "Missing required query parameter: path",
        )));
    };
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let mut file = PathBuf::from(raw);
    if !file.is_absolute() {
        file = resolved.path().join(file);
    }
    let root = supervisor.workspace_root();
    let file = std::fs::canonicalize(&file)
        .ok()
        .filter(|path| path.starts_with(root))
        .ok_or_else(|| ErrorResponse::from(not_found(raw)))?;
    if supervisor.config().servers_for_file(&file).is_empty() {
        return Err(ErrorResponse::from(not_found(raw)));
    }
    let diagnostics = supervisor.diagnostics_json(&file).await;
    // The supervisor exposes no "is this file open" query; an empty merge is
    // exactly an unknown/unopened path or one with nothing cached.
    if diagnostics.as_array().is_none_or(Vec::is_empty) {
        return Err(ErrorResponse::from(not_found(raw)));
    }
    Ok(Json(diagnostics))
}

fn not_found(path: &str) -> ApiError {
    ApiError::not_found(
        "NotFoundError",
        format!("No LSP diagnostics for path: {path}"),
    )
}

/// `POST /api/lsp/reload` — restart every client and clear diagnostics.
///
/// A configuration snapshot is reloaded from the attached `ConfigService` when
/// present; otherwise the supervisor's current snapshot is reused.
pub async fn reload(State(state): State<AppState>) -> Result<Response, ErrorResponse> {
    let supervisor = require_supervisor(&state)?;
    let config = state
        .config()
        .map(|config| oc_lsp::LspConfig::from_config(config.merged()));
    supervisor.reload_config(config).await;
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

/// Routes owned by this module.
pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/lsp", axum::routing::get(list))
        .route("/api/lsp/diagnostics", axum::routing::get(diagnostics))
        .route("/api/lsp/reload", axum::routing::post(reload))
}
