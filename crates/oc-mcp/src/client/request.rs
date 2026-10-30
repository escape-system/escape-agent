//! MCP request correlation and cursor paging.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::{McpError, McpResult};

use super::McpClient;
use super::types::MAX_PAGES;

impl McpClient {
    /// Sends one request, reconnecting once when the HTTP session expired.
    pub(super) async fn request_recovering(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> McpResult<Value> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(McpError::NotConnected {
                server: self.inner.server.clone(),
                reason: "client is shut down".to_owned(),
            });
        }
        let generation = self.inner.generation.load(Ordering::SeqCst);
        let transport = self.inner.current_transport();
        match transport.request(method, params.clone(), timeout).await {
            Err(McpError::SessionExpired) => {
                tracing::info!(server = %self.inner.server, "MCP session expired, reconnecting");
                self.reconnect(generation).await?;
                self.inner
                    .current_transport()
                    .request(method, params, timeout)
                    .await
            }
            other => other,
        }
    }

    pub(super) async fn list_pages(&self, method: &str, key: &str) -> McpResult<Vec<Value>> {
        let timeout = self.inner.timeouts.catalog();
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self
                .request_recovering(method, Some(params), timeout)
                .await?;
            let page = result
                .get(key)
                .and_then(Value::as_array)
                .ok_or_else(|| McpError::Protocol(format!("{method} result has no {key} array")))?;
            items.extend(page.iter().cloned());
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned);
            if let Some(next) = &cursor
                && !seen.insert(next.clone())
            {
                return Err(McpError::Protocol(format!(
                    "{method} pagination repeated a cursor"
                )));
            }
            if cursor.is_none() {
                return Ok(items);
            }
        }
        Err(McpError::Protocol(format!(
            "{method} exceeded {MAX_PAGES} pages"
        )))
    }
}
