//! Data routes: sessions, messages, inbox, config/agents, model catalog stubs
//! and the durable session log.
//!
//! Every handler decodes the wire body itself, persists through `oc-store`
//! first and only then publishes an event, exactly as `docs/02-protocol.md`
//! requires. When the runtime pieces (store, config snapshot, model catalog)
//! are not attached the routes answer `503 ServiceUnavailableError` instead of
//! pretending to work.

pub mod config;
pub mod config_patch;
pub mod events;
pub mod experimental;
pub mod fs;
pub mod goal;
pub mod inbox;
pub mod integration;
pub mod lane;
pub mod lists;
pub mod location;
pub mod log;
pub mod lsp;
pub mod lsp_tools;
pub mod mcp;
pub mod mcp_tools;
pub mod memory;
pub mod message;
pub mod model;
pub mod observer;
pub mod permission;
pub mod plugin;
pub mod project;
pub mod pty;
pub mod schedule;
pub mod session;
pub mod session_ext;
pub mod shells;
pub mod subagent;
pub mod vcs;
pub mod worktree;

use std::sync::Arc;

use axum::response::{IntoResponse, Response};
use oc_config::ConfigService;
use oc_llm::Catalog;
use oc_store::Store;
use oc_types::ApiError;

use crate::AppState;
use crate::permission::PermissionBroker;

/// The persistence layer, or the documented 503.
pub(crate) fn require_store(state: &AppState) -> Result<&Arc<Store>, ApiError> {
    state
        .store()
        .ok_or_else(|| ApiError::service_unavailable("data store not available"))
}

/// The permission broker, or the documented 503.
pub(crate) fn require_broker(state: &AppState) -> Result<&Arc<PermissionBroker>, ApiError> {
    state
        .permissions()
        .ok_or_else(|| ApiError::service_unavailable("permission broker not available"))
}

/// The memory service, or the documented 503.
pub(crate) fn require_memory(state: &AppState) -> Result<&Arc<oc_memory::MemoryService>, ApiError> {
    state
        .memory()
        .ok_or_else(|| ApiError::service_unavailable("memory service not attached"))
}

/// Resolve the root session id for `session_id`.
///
/// A session that the store does not know is `SessionNotFoundError`; a row
/// without a root (should not happen) is its own root.
pub(crate) async fn require_session_root(
    state: &AppState,
    session_id: &str,
) -> Result<String, crate::error::ErrorResponse> {
    let store = require_store(state)?;
    let node = store
        .session_node(session_id)
        .await?
        .ok_or_else(|| crate::error::ErrorResponse::from(session_not_found(session_id)))?;
    Ok(node.root_id)
}

/// The session runtime, or the documented 503.
pub(crate) fn runtime_unavailable() -> ApiError {
    ApiError::service_unavailable("session runtime not available")
}

/// Map a runtime failure onto the protocol's error tags.
pub(crate) fn runtime_error(error: oc_runtime::RuntimeError) -> ApiError {
    use oc_runtime::RuntimeError as Runtime;
    match error {
        Runtime::SessionNotFound(id) => session_not_found(&id),
        Runtime::InvalidSession(message) => ApiError::not_found("SessionNotFoundError", message),
        Runtime::AgentNotFound(id) => {
            ApiError::not_found("AgentNotFoundError", format!("Agent not found: {id}"))
                .with("agentID", serde_json::json!(id))
        }
        Runtime::ModelUnavailable(id) => {
            ApiError::not_found("ModelNotFoundError", format!("Model not available: {id}"))
        }
        Runtime::Timeout(id) => ApiError::service_unavailable(format!(
            "Timed out waiting for session {id} to become idle"
        )),
        Runtime::Shutdown => ApiError::service_unavailable("session runtime is shut down"),
        Runtime::Llm(error) => ApiError::new(502, "ProviderError", error.to_string()),
        other
        @ (Runtime::Store(_) | Runtime::Config(_) | Runtime::Cancelled | Runtime::Task(_)) => {
            tracing::error!(error = %other, "runtime request failed");
            ApiError::internal("Internal error")
        }
    }
}

/// The configuration snapshot, or the documented 503.
pub(crate) fn require_config(state: &AppState) -> Result<&Arc<ConfigService>, ApiError> {
    state
        .config()
        .ok_or_else(|| ApiError::service_unavailable("configuration service not available"))
}

/// The model catalog, or the documented 503.
pub(crate) fn require_catalog(state: &AppState) -> Result<&Arc<Catalog>, ApiError> {
    state
        .catalog()
        .ok_or_else(|| ApiError::service_unavailable("model catalog not loaded"))
}

/// `SessionNotFoundError` with the entity id attached.
pub(crate) fn session_not_found(session_id: &str) -> ApiError {
    ApiError::not_found(
        "SessionNotFoundError",
        format!("Session not found: {session_id}"),
    )
    .with("sessionID", serde_json::json!(session_id))
}

/// 204 with no body and no JSON content type.
pub(crate) fn no_content() -> Response {
    axum::http::StatusCode::NO_CONTENT.into_response()
}

/// Wrap a rejected JSON body in the protocol's tagged error.
pub(crate) fn schema_error(rejection: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        400,
        "SchemaError",
        format!("Invalid request body: {rejection}"),
    )
}

/// Rejected query parameters.
pub(crate) fn query_error(rejection: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        400,
        "SchemaError",
        format!("Invalid query parameters: {rejection}"),
    )
}

/// `{previous?, next?}` with absent anchors omitted.
pub(crate) fn cursor_value(cursor: oc_store::Cursor) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Some(previous) = cursor.previous {
        map.insert("previous".to_owned(), serde_json::Value::String(previous));
    }
    if let Some(next) = cursor.next {
        map.insert("next".to_owned(), serde_json::Value::String(next));
    }
    serde_json::Value::Object(map)
}

/// Parse the wire delivery mode (`steer` when absent).
pub(crate) fn delivery_from_wire(value: Option<&str>) -> Result<oc_store::Delivery, ApiError> {
    match value {
        None | Some("steer") => Ok(oc_store::Delivery::Steer),
        Some("queue") => Ok(oc_store::Delivery::Queue),
        Some(other) => Err(ApiError::new(
            400,
            "BadRequestError",
            format!("Unknown delivery mode {other:?}"),
        )),
    }
}
