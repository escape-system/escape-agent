//! Sidecar notifications, registration, callbacks, and RPC error mapping.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::{debug, info, warn};

use super::protocol::{self, CallbackParams, Incoming, LogParams, RegisterParams, UpdateParams};
use super::{Inner, SidecarLog, SidecarPlugin};
use crate::domain::Domain;
use crate::hook::Hook;
use crate::info::{PluginFeatures, PluginSource, PluginState};
use crate::rpc::{RpcError, RpcErrorObject};

pub(super) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn rpc_error_from_object(error: RpcErrorObject, method: &str) -> RpcError {
    match error.code {
        -32601 => RpcError::MethodNotFound {
            namespace: String::new(),
            method: method.to_string(),
        },
        -32602 => RpcError::InvalidInput {
            message: error.message,
        },
        _ => RpcError::Failed {
            message: error.message,
            data: error.data,
        },
    }
}

pub(super) fn handle_incoming(inner: &Arc<Inner>, incoming: Incoming) {
    match incoming {
        Incoming::Response { id, outcome } => {
            if let Some(sender) = inner.pending.lock().remove(&id) {
                let _ = sender.send(Ok(outcome));
            } else {
                debug!(target: "oc_plugin::sidecar", id, "response for unknown request");
            }
        }
        Incoming::Notification { method, params } => match method.as_str() {
            protocol::NOTIFY_REGISTER => handle_register(inner, params),
            protocol::NOTIFY_UPDATE => handle_update(inner, params),
            protocol::NOTIFY_LOG => handle_log(inner, params),
            other => debug!(target: "oc_plugin::sidecar", method = other, "unknown notification"),
        },
        Incoming::Request { id, method, params } => {
            if method == protocol::METHOD_CALLBACK {
                spawn_callback(inner, id, params);
            } else {
                let error = RpcError::MethodNotFound {
                    namespace: "host".to_string(),
                    method: method.clone(),
                }
                .to_object();
                let _ = inner
                    .outbound
                    .send(protocol::error_response(id, &error).to_string());
            }
        }
    }
}

fn handle_register(inner: &Arc<Inner>, params: Value) {
    let parsed: RegisterParams = match serde_json::from_value(params) {
        Ok(parsed) => parsed,
        Err(error) => {
            inner.protocol_error(&format!("invalid register params: {error}"));
            return;
        }
    };
    let features = parsed.features.unwrap_or_else(PluginFeatures::server);
    let source = parsed.source.unwrap_or_else(|| PluginSource::Package {
        target: parsed.id.clone(),
        version: None,
        outdated: None,
        updating: None,
    });
    let mut state = PluginState::Active;
    let mut hooks = Vec::new();
    for name in &parsed.hooks {
        match name.parse::<Hook>() {
            Ok(hook) => hooks.push(hook),
            Err(_) => state = PluginState::failed(format!("unsupported feature: {name}")),
        }
    }
    let mut transforms = Vec::new();
    for name in &parsed.transforms {
        match name.parse::<Domain>() {
            Ok(domain) => transforms.push(domain),
            Err(_) => state = PluginState::failed(format!("unsupported feature: {name}")),
        }
    }
    let mut plugins = inner.plugins.lock();
    if plugins.iter().any(|plugin| plugin.id == parsed.id) {
        state = PluginState::failed("duplicate id");
    }
    plugins.push(SidecarPlugin {
        id: parsed.id,
        features,
        hooks,
        transforms,
        state,
        source,
    });
}

fn handle_update(inner: &Arc<Inner>, params: Value) {
    let parsed: UpdateParams = match serde_json::from_value(params) {
        Ok(parsed) => parsed,
        Err(error) => {
            inner.protocol_error(&format!("invalid update params: {error}"));
            return;
        }
    };
    let mut plugins = inner.plugins.lock();
    if let Some(plugin) = plugins.iter_mut().find(|plugin| plugin.id == parsed.id) {
        plugin.state = parsed.state;
    } else {
        debug!(
            target: "oc_plugin::sidecar",
            id = %parsed.id,
            "update for unknown plugin"
        );
    }
}

fn handle_log(inner: &Arc<Inner>, params: Value) {
    let parsed: LogParams = match serde_json::from_value(params) {
        Ok(parsed) => parsed,
        Err(error) => {
            inner.protocol_error(&format!("invalid log params: {error}"));
            return;
        }
    };
    match parsed.level.as_str() {
        "debug" => {
            debug!(target: "oc_plugin::sidecar", plugin = parsed.plugin.as_deref().unwrap_or(""), "{}", parsed.message);
        }
        "warn" => {
            warn!(target: "oc_plugin::sidecar", plugin = parsed.plugin.as_deref().unwrap_or(""), "{}", parsed.message);
        }
        "error" => {
            warn!(target: "oc_plugin::sidecar", plugin = parsed.plugin.as_deref().unwrap_or(""), level = "error", "{}", parsed.message);
        }
        _ => {
            info!(target: "oc_plugin::sidecar", plugin = parsed.plugin.as_deref().unwrap_or(""), "{}", parsed.message);
        }
    }
    inner.push_log(SidecarLog {
        level: parsed.level,
        message: parsed.message,
        plugin: parsed.plugin,
    });
}

fn spawn_callback(inner: &Arc<Inner>, id: Value, params: Value) {
    let inner = Arc::clone(inner);
    tokio::spawn(async move {
        let parsed: CallbackParams = match serde_json::from_value(params) {
            Ok(parsed) => parsed,
            Err(error) => {
                let object = RpcError::InvalidInput {
                    message: format!("invalid callback params: {error}"),
                }
                .to_object();
                let _ = inner
                    .outbound
                    .send(protocol::error_response(id, &object).to_string());
                return;
            }
        };
        let host = inner.host.read().clone();
        let response = match host {
            None => Err(RpcError::Unavailable {
                message: "no host service registered".to_string(),
            }),
            Some(host) => {
                let deadline = inner.config.request_timeout;
                match tokio::time::timeout(
                    deadline,
                    host.callback(&parsed.request_id, &parsed.method, &parsed.args),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(RpcError::Timeout {
                        method: parsed.method.clone(),
                        timeout_ms: millis(deadline),
                    }),
                }
            }
        };
        let frame = match response {
            Ok(value) => protocol::ok_response(id, value),
            Err(error) => protocol::error_response(id, &error.to_object()),
        };
        let _ = inner.outbound.send(frame.to_string());
    });
}
