//! Newline-framed JSON-RPC 2.0 for the JS sidecar.
//!
//! # Frames
//!
//! One JSON object per line, UTF-8, `\n` terminated, at most
//! [`MAX_FRAME_BYTES`] per frame. Optional members are omitted, never `null`.
//!
//! # Host → sidecar requests
//!
//! ```jsonc
//! {"jsonrpc":"2.0","id":1,"method":"load","params":{"targets":["pkg@1.2.3"]}}
//! {"jsonrpc":"2.0","id":2,"method":"reload","params":{"ids":["pkg"]}}
//! {"jsonrpc":"2.0","id":3,"method":"hook","params":{"hook":"tool.execute.before","event":{...}}}
//! {"jsonrpc":"2.0","id":4,"method":"transform","params":{"domain":"agent","doc":{...}}}
//! {"jsonrpc":"2.0","id":5,"method":"rpc","params":{"namespace":"ns","method":"m","input":{}}}
//! {"jsonrpc":"2.0","id":6,"method":"dispose","params":{"id":"pkg"}}
//! {"jsonrpc":"2.0","id":7,"method":"plugin.shutdown"}
//! ```
//!
//! `hook` resolves to the mutated event or `null` when untouched; an error is
//! meaningful only for `tool.execute.before`. `transform` resolves to the
//! mutated document. `rpc` resolves to the method output.
//!
//! # Sidecar → host notifications
//!
//! ```jsonc
//! {"jsonrpc":"2.0","method":"register","params":{"id":"pkg","features":{"server":true,"rpc":true},
//!   "hooks":["tool.execute.before"],"transforms":["agent"],"source":{"type":"package","target":"pkg"}}}
//! {"jsonrpc":"2.0","method":"update","params":{"id":"pkg","state":{"status":"failed","error":"…"}}}
//! {"jsonrpc":"2.0","method":"log","params":{"level":"info","message":"…","plugin":"pkg"}}
//! ```
//!
//! # Sidecar → host request (callback)
//!
//! ```jsonc
//! {"jsonrpc":"2.0","id":41,"method":"callback",
//!  "params":{"requestId":"r-1","method":"session.read","args":{...}}}
//! ```
//!
//! The host correlates `requestId`, enforces the request deadline and answers
//! with a normal JSON-RPC response.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::info::{PluginFeatures, PluginSource, PluginState};
use crate::rpc::RpcErrorObject;

/// JSON-RPC version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// Maximum frame size accepted in either direction (8 MiB).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Method: load plugin targets.
pub const METHOD_LOAD: &str = "load";
/// Method: reload plugin ids.
pub const METHOD_RELOAD: &str = "reload";
/// Method: dispatch a hook.
pub const METHOD_HOOK: &str = "hook";
/// Method: transform a document.
pub const METHOD_TRANSFORM: &str = "transform";
/// Method: dispatch an RPC call.
pub const METHOD_RPC: &str = "rpc";
/// Method: dispose a plugin.
pub const METHOD_DISPOSE: &str = "dispose";
/// Method: graceful shutdown.
pub const METHOD_SHUTDOWN: &str = "plugin.shutdown";

/// Notification: a plugin registered.
pub const NOTIFY_REGISTER: &str = "register";
/// Notification: plugin state changed.
pub const NOTIFY_UPDATE: &str = "update";
/// Notification: plugin log line.
pub const NOTIFY_LOG: &str = "log";
/// Request: plugin calls a host service.
pub const METHOD_CALLBACK: &str = "callback";

/// An invalid frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ProtocolError {
    /// What was wrong.
    pub message: String,
}

impl ProtocolError {
    /// Build a protocol error.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// A parsed incoming frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Response to a host request.
    Response {
        /// Request id the host allocated.
        id: u64,
        /// `result` or `error`.
        outcome: Exchange,
    },
    /// Request from the sidecar (callback) expecting a response.
    Request {
        /// JSON-RPC id, echoed verbatim.
        id: Value,
        /// Method name.
        method: String,
        /// Request params.
        params: Value,
    },
    /// Fire-and-forget notification.
    Notification {
        /// Method name.
        method: String,
        /// Notification params.
        params: Value,
    },
}

/// Result half of an exchange: either the method output or a JSON-RPC error.
#[derive(Debug, Clone, PartialEq)]
pub enum Exchange {
    /// Successful result (may be `null`).
    Ok(Value),
    /// JSON-RPC error object.
    Error(RpcErrorObject),
}

/// `register` notification params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterParams {
    /// Plugin id.
    pub id: String,
    /// Entrypoints; defaults to `server`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<PluginFeatures>,
    /// Hook names the plugin handles.
    #[serde(default)]
    pub hooks: Vec<String>,
    /// Transform domains the plugin handles.
    #[serde(default)]
    pub transforms: Vec<String>,
    /// Where the plugin came from; defaults to a package named after `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PluginSource>,
}

/// `update` notification params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateParams {
    /// Plugin id.
    pub id: String,
    /// New state.
    pub state: PluginState,
}

/// `log` notification params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogParams {
    /// `debug`, `info`, `warn` or `error`.
    pub level: String,
    /// Message text.
    pub message: String,
    /// Owning plugin, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
}

/// `callback` request params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallbackParams {
    /// Host correlation id supplied by the plugin.
    #[serde(rename = "requestId")]
    pub request_id: String,
    /// Host service method.
    pub method: String,
    /// Method arguments.
    #[serde(default)]
    pub args: Value,
}

/// Host → sidecar request frame.
#[derive(Debug, Clone, Serialize)]
struct OutgoingRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<&'a Value>,
}

/// Serialize a host request frame (without the trailing newline).
#[must_use]
pub fn request_frame(id: u64, method: &str, params: Option<&Value>) -> String {
    serde_json::to_string(&OutgoingRequest {
        jsonrpc: JSONRPC_VERSION,
        id,
        method,
        params,
    })
    .unwrap_or_else(|error| {
        // Only reachable for non-string keys / NaN, which `Value` cannot hold.
        tracing::error!(target: "oc_plugin::sidecar", %error, "failed to encode request frame");
        String::new()
    })
}

/// Successful response frame.
#[must_use]
pub fn ok_response(id: Value, result: Value) -> Value {
    let mut object = Map::new();
    object.insert("jsonrpc".into(), Value::from(JSONRPC_VERSION));
    object.insert("id".into(), id);
    object.insert("result".into(), result);
    Value::Object(object)
}

/// Error response frame.
#[must_use]
pub fn error_response(id: Value, error: &RpcErrorObject) -> Value {
    let mut object = Map::new();
    object.insert("jsonrpc".into(), Value::from(JSONRPC_VERSION));
    object.insert("id".into(), id);
    object.insert(
        "error".into(),
        serde_json::to_value(error).unwrap_or_else(|_| {
            Value::Object(Map::from_iter([
                ("code".into(), Value::from(error.code)),
                ("message".into(), Value::from(error.message.clone())),
            ]))
        }),
    );
    Value::Object(object)
}

impl fmt::Display for Incoming {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Response { id, .. } => write!(formatter, "response {id}"),
            Self::Request { method, .. } => write!(formatter, "request {method}"),
            Self::Notification { method, .. } => write!(formatter, "notification {method}"),
        }
    }
}

/// Parse one frame.
pub fn parse_incoming(line: &str) -> Result<Incoming, ProtocolError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| ProtocolError::new(format!("invalid json: {error}")))?;
    let Some(object) = value.as_object() else {
        return Err(ProtocolError::new("frame is not a JSON object"));
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
        return Err(ProtocolError::new("missing jsonrpc 2.0 marker"));
    }
    let id = object.get("id").cloned();
    let method = match object.get("method") {
        Some(Value::String(method)) => Some(method.clone()),
        Some(_) => return Err(ProtocolError::new("method must be a string")),
        None => None,
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    match (id, method) {
        (Some(id), Some(method)) => Ok(Incoming::Request { id, method, params }),
        (None, Some(method)) => Ok(Incoming::Notification { method, params }),
        (Some(id), None) => {
            let result = object.get("result").cloned();
            let error = object.get("error").cloned();
            let id = id
                .as_u64()
                .ok_or_else(|| ProtocolError::new("response id must be an unsigned integer"))?;
            match (result, error) {
                (Some(result), None) => Ok(Incoming::Response {
                    id,
                    outcome: Exchange::Ok(result),
                }),
                (None, Some(error)) => {
                    let error: RpcErrorObject = serde_json::from_value(error)
                        .map_err(|error| ProtocolError::new(format!("invalid error: {error}")))?;
                    Ok(Incoming::Response {
                        id,
                        outcome: Exchange::Error(error),
                    })
                }
                _ => Err(ProtocolError::new(
                    "response must carry exactly one of result/error",
                )),
            }
        }
        (None, None) => Err(ProtocolError::new("frame has neither id nor method")),
    }
}
