//! Server request and notification dispatch.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::rpc::{RequestId, RpcError, RpcMessage};
use crate::uri::uri_to_path;

use super::Inner;
use super::io::schedule_debounce;
use super::json::configuration_value;

impl Inner {
    pub(super) async fn dispatch(self: &Arc<Self>, message: RpcMessage) {
        match message {
            RpcMessage::Response { id, result, error } => {
                let Some(id) = id else {
                    return;
                };
                let Some(sender) = self.pending.lock().remove(&id) else {
                    return;
                };
                let outcome = match error {
                    Some(error) => Err(RpcError::Server {
                        code: error.code,
                        message: error.message,
                    }),
                    None => Ok(result.unwrap_or(Value::Null)),
                };
                let _ = sender.send(outcome);
            }
            RpcMessage::Request { id, method, params } => {
                self.handle_server_request(id, &method, &params).await;
            }
            RpcMessage::Notification { method, params } => {
                self.handle_notification(&method, &params);
            }
        }
    }

    async fn handle_server_request(&self, id: RequestId, method: &str, params: &Value) {
        let response = match method {
            "workspace/configuration" => {
                let items = params.get("items").and_then(Value::as_array);
                let values: Vec<Value> = items.map_or_else(Vec::new, |items| {
                    items
                        .iter()
                        .map(|item| {
                            let section = item
                                .get("section")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            configuration_value(&self.settings, section)
                        })
                        .collect()
                });
                json!({ "jsonrpc": "2.0", "id": id, "result": values })
            }
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "window/showMessageRequest" => {
                json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null })
            }
            "workspace/workspaceFolders" => {
                json!({ "jsonrpc": "2.0", "id": id, "result": [self.workspace_folder()] })
            }
            other => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            }),
        };
        if let Err(error) = self.write(&response).await {
            tracing::debug!(error = %error, "failed to answer server request");
        }
    }

    fn handle_notification(self: &Arc<Self>, method: &str, params: &Value) {
        match method {
            "textDocument/publishDiagnostics" => {
                let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                    return;
                };
                let Some(path) = uri_to_path(uri) else {
                    return;
                };
                // Canonicalize so cache keys match `touch_file`'s canonical
                // paths (notably Windows verbatim prefixes).
                let path = std::fs::canonicalize(&path).unwrap_or(path);
                let Some(items) = params.get("diagnostics").and_then(Value::as_array) else {
                    return;
                };
                let version = params.get("version").and_then(Value::as_i64);
                let accepted = self
                    .diagnostics
                    .lock()
                    .put_push(&path, version, items.clone());
                if !accepted {
                    return;
                }
                self.bump_diagnostics();
                self.debounce_pending.lock().insert(path);
                schedule_debounce(Arc::clone(self));
            }
            "window/showMessage" => {
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                tracing::warn!(
                    server = %self.server_id,
                    message = %message,
                    "language server message"
                );
            }
            "window/logMessage" | "telemetry/event" | "$/progress" | "$/logTrace" => {}
            other => {
                tracing::trace!(server = %self.server_id, method = other, "ignored notification");
            }
        }
    }
}
