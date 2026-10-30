//! MCP transport event handler: roots, elicitation, notifications and close.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use serde_json::{Value, json};
use url::Url;

use crate::error::RpcError;
use crate::protocol::codes;
use crate::transport::TransportEventHandler;

use super::lock;
use super::types::{ElicitationHandler, McpChange, McpStatus};
use super::{Inner, McpClient};

pub(super) struct ClientHandler {
    inner: Weak<Inner>,
    generation: u64,
    directory: PathBuf,
    elicitation: Option<Arc<dyn ElicitationHandler>>,
}

impl std::fmt::Debug for ClientHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientHandler")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl ClientHandler {
    pub(super) fn new(inner: &Arc<Inner>, generation: u64) -> Self {
        Self {
            inner: Arc::downgrade(inner),
            generation,
            directory: inner.directory.clone(),
            elicitation: inner.elicitation.clone(),
        }
    }

    fn roots(&self) -> Value {
        let uri = Url::from_file_path(&self.directory).map_or_else(
            |()| {
                format!(
                    "file://{}",
                    self.directory.to_string_lossy().replace('\\', "/")
                )
            },
            |url| url.to_string(),
        );
        json!({"roots": [{"uri": uri}]})
    }
}

#[async_trait]
impl TransportEventHandler for ClientHandler {
    async fn on_request(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        match method {
            "roots/list" => Ok(self.roots()),
            "ping" => Ok(json!({})),
            "elicitation/create" => match (&self.elicitation, method) {
                (Some(handler), _) => {
                    let server = self
                        .inner
                        .upgrade()
                        .map_or_else(String::new, |inner| inner.server.clone());
                    handler
                        .create(&server, params.unwrap_or(Value::Null))
                        .await
                        .map_err(|error| RpcError {
                            code: codes::INTERNAL_ERROR,
                            message: error.to_string(),
                            data: None,
                        })
                }
                (None, _) => Err(RpcError {
                    code: codes::METHOD_NOT_FOUND,
                    message: "elicitation is not supported".to_owned(),
                    data: None,
                }),
            },
            other => Err(RpcError {
                code: codes::METHOD_NOT_FOUND,
                message: format!("unsupported method: {other}"),
                data: None,
            }),
        }
    }

    async fn on_notification(&self, method: &str, _params: Option<Value>) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.generation.load(Ordering::SeqCst) != self.generation {
            return;
        }
        let change = match method {
            "notifications/tools/list_changed" => McpChange::Tools,
            "notifications/prompts/list_changed" => McpChange::Prompts,
            "notifications/resources/list_changed" => McpChange::Resources,
            other => {
                tracing::debug!(server = %inner.server, method = %other, "MCP notification");
                return;
            }
        };
        inner.emit_change(change);
        let client = McpClient {
            inner: inner.clone(),
        };
        tokio::spawn(async move {
            let result = match change {
                McpChange::Tools => client.refresh_tools().await.map(|_| ()),
                McpChange::Prompts => client.refresh_prompts().await.map(|_| ()),
                McpChange::Resources => client.refresh_resources().await.map(|_| ()),
            };
            if let Err(error) = result {
                tracing::warn!(server = %client.inner.server, %error, "background MCP catalog refresh failed");
            }
        });
    }

    async fn on_close(&self, reason: String) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.generation.load(Ordering::SeqCst) != self.generation
            || inner.closed.load(Ordering::SeqCst)
        {
            return;
        }
        {
            let mut state = lock(&inner.state);
            state.clear_catalog();
        }
        inner.set_status(McpStatus::Failed {
            error: reason.clone(),
        });
        tracing::warn!(server = %inner.server, %reason, "MCP connection closed");
    }

    fn on_error(&self, message: String) {
        if let Some(inner) = self.inner.upgrade() {
            tracing::warn!(server = %inner.server, %message, "MCP transport error");
        }
    }
}
