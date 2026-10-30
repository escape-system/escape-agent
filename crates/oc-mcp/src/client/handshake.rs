//! MCP initialize handshake and protocol fallback.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::config::{LEGACY_PROTOCOL_VERSION, Protocol};
use crate::error::{McpError, McpResult};
use crate::transport::Transport;

use super::lock;
use super::{InitResult, Inner, ServerInfo};

pub(super) fn host_environment() -> std::collections::HashMap<String, String> {
    std::env::vars().collect()
}

pub(super) fn apply_init(inner: &Arc<Inner>, init: InitResult) {
    let mut state = lock(&inner.state);
    state.protocol_version = Some(init.protocol_version);
    state.capabilities = init.capabilities;
    state.server_info = init.server_info;
    state.instructions = init.instructions;
}

fn capabilities(inner: &Inner) -> Value {
    let mut capabilities = Map::new();
    capabilities.insert("roots".to_owned(), json!({}));
    capabilities.insert("tools".to_owned(), json!({"listChanged": true}));
    capabilities.insert("prompts".to_owned(), json!({"listChanged": true}));
    capabilities.insert("resources".to_owned(), json!({"listChanged": true}));
    if inner.elicitation.is_some() {
        capabilities.insert("elicitation".to_owned(), json!({"form": {}, "url": {}}));
    }
    Value::Object(capabilities)
}

fn is_protocol_rejection(error: &McpError) -> bool {
    matches!(
        error,
        McpError::Rpc(_) | McpError::Protocol(_) | McpError::Closed | McpError::Exited(_)
    ) || matches!(error, McpError::Http { status, .. } if *status == 400 || *status == 404)
}

fn unsupported_message(error: &McpError) -> String {
    let supported = match error {
        McpError::Rpc(rpc) => rpc
            .data
            .as_ref()
            .and_then(|data| data.get("supported"))
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            }),
        _ => None,
    };
    match supported {
        Some(supported) if !supported.is_empty() => {
            format!("{error}; the server supports {supported}")
        }
        _ => error.to_string(),
    }
}

async fn initialize_once(
    inner: &Arc<Inner>,
    transport: &Arc<dyn Transport>,
    version: &str,
) -> McpResult<InitResult> {
    let params = json!({
        "protocolVersion": version,
        "capabilities": capabilities(inner),
        "clientInfo": {
            "name": inner.client_info.name,
            "version": inner.client_info.version,
        }
    });
    let result = transport
        .request("initialize", Some(params), inner.timeouts.startup())
        .await?;
    let Some(object) = result.as_object() else {
        return Err(McpError::Protocol(
            "initialize result is not an object".to_owned(),
        ));
    };
    let protocol_version = object
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::Protocol("initialize result has no protocolVersion".to_owned()))?
        .to_owned();
    let server_info = object.get("serverInfo").and_then(|value| {
        let name = value.get("name")?.as_str()?.to_owned();
        let version = value.get("version")?.as_str()?.to_owned();
        let title = value
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned);
        Some(ServerInfo {
            name,
            version,
            title,
        })
    });
    let instructions = object
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    Ok(InitResult {
        protocol_version,
        capabilities: object
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| json!({})),
        server_info,
        instructions,
    })
}

pub(super) async fn handshake(
    inner: &Arc<Inner>,
    transport: &Arc<dyn Transport>,
) -> McpResult<InitResult> {
    let initial = inner.protocol.initial_version();
    match initialize_once(inner, transport, initial).await {
        Ok(init) => {
            if inner.protocol == Protocol::V2026_07_28 && init.protocol_version != initial {
                return Err(McpError::Protocol(format!(
                    "MCP server negotiated {} but protocol 2026-07-28 was required",
                    init.protocol_version
                )));
            }
            Ok(init)
        }
        Err(error)
            if inner.protocol.allows_legacy_fallback()
                && initial != LEGACY_PROTOCOL_VERSION
                && is_protocol_rejection(&error) =>
        {
            tracing::info!(
                server = %inner.server,
                error = %error,
                "MCP 2026-07-28 probe rejected; falling back to the legacy initialize handshake"
            );
            initialize_once(inner, transport, LEGACY_PROTOCOL_VERSION)
                .await
                .map_err(|fallback| {
                    McpError::Protocol(format!(
                        "{}; legacy fallback failed: {}",
                        unsupported_message(&error),
                        fallback
                    ))
                })
        }
        Err(error) => Err(error),
    }
}
