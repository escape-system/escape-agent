//! MCP catalog refresh and calls: tools, prompts, resources and ping.

use serde_json::{Value, json};

use crate::error::{McpError, McpResult};
use crate::tool::McpTool;

use super::McpClient;
use super::lock;

impl McpClient {
    /// Refreshes and returns the tool catalog, dropping invalid tools with a
    /// warning.
    pub async fn refresh_tools(&self) -> McpResult<Vec<McpTool>> {
        let raw = if self.supports("tools") {
            self.list_pages("tools/list", "tools").await?
        } else {
            Vec::new()
        };
        let mut tools = Vec::new();
        for value in raw {
            match McpTool::from_wire(&self.inner.server, &value) {
                Ok(tool) => tools.push(tool),
                Err(error) => {
                    let name = value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("<unknown>");
                    tracing::warn!(
                        server = %self.inner.server,
                        tool = %name,
                        %error,
                        "skipping invalid MCP tool"
                    );
                }
            }
        }
        {
            let mut state = lock(&self.inner.state);
            state.tools.clone_from(&tools);
        }
        Ok(tools)
    }

    /// Alias for [`Self::refresh_tools`].
    pub async fn list_tools(&self) -> McpResult<Vec<McpTool>> {
        self.refresh_tools().await
    }

    /// Cached tools (no request).
    #[must_use]
    pub fn tools(&self) -> Vec<McpTool> {
        lock(&self.inner.state).tools.clone()
    }

    /// Calls a tool with the server's own tool name.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> McpResult<Value> {
        let timeout = self.inner.timeouts.execution();
        let params = json!({"name": name, "arguments": arguments});
        self.request_recovering("tools/call", Some(params), timeout)
            .await
            .map_err(|error| match error {
                McpError::ToolReturnedError { .. } => error,
                McpError::Timeout(after) => McpError::ToolCall {
                    server: self.inner.server.clone(),
                    tool: name.to_owned(),
                    message: format!("MCP tool timed out after {}s", after.as_secs()),
                },
                other => McpError::ToolCall {
                    server: self.inner.server.clone(),
                    tool: name.to_owned(),
                    message: other.to_string(),
                },
            })
    }

    /// Refreshes and returns the prompt catalog.
    pub async fn refresh_prompts(&self) -> McpResult<Vec<Value>> {
        let prompts = if self.supports("prompts") {
            self.list_pages("prompts/list", "prompts").await?
        } else {
            Vec::new()
        };
        {
            let mut state = lock(&self.inner.state);
            state.prompts.clone_from(&prompts);
        }
        Ok(prompts)
    }

    /// Cached prompts.
    #[must_use]
    pub fn prompts(&self) -> Vec<Value> {
        lock(&self.inner.state).prompts.clone()
    }

    /// Gets one prompt.
    pub async fn get_prompt(&self, name: &str, arguments: Option<Value>) -> McpResult<Value> {
        let timeout = self.inner.timeouts.execution();
        let params = json!({"name": name, "arguments": arguments.unwrap_or_else(|| json!({}))});
        self.request_recovering("prompts/get", Some(params), timeout)
            .await
    }

    /// Refreshes resource and resource-template catalogs.
    pub async fn refresh_resources(&self) -> McpResult<Vec<Value>> {
        if !self.supports("resources") {
            let mut state = lock(&self.inner.state);
            state.resources.clear();
            state.templates.clear();
            return Ok(Vec::new());
        }
        let resources = self.list_pages("resources/list", "resources").await?;
        // Some servers declare resources without implementing template listing.
        let templates = match self
            .list_pages("resources/templates/list", "resourceTemplates")
            .await
        {
            Ok(templates) => templates,
            Err(error) => {
                tracing::debug!(server = %self.inner.server, %error, "MCP resource templates unavailable");
                Vec::new()
            }
        };
        let mut state = lock(&self.inner.state);
        state.resources.clone_from(&resources);
        state.templates = templates;
        drop(state);
        Ok(resources)
    }

    /// Cached resources.
    #[must_use]
    pub fn resources(&self) -> Vec<Value> {
        lock(&self.inner.state).resources.clone()
    }

    /// Cached resource templates.
    #[must_use]
    pub fn resource_templates(&self) -> Vec<Value> {
        lock(&self.inner.state).templates.clone()
    }

    /// Reads one resource; `None` when the server has no resources capability.
    pub async fn read_resource(&self, uri: &str) -> McpResult<Option<Value>> {
        if !self.supports("resources") {
            return Ok(None);
        }
        let timeout = self.inner.timeouts.execution();
        self.request_recovering("resources/read", Some(json!({"uri": uri})), timeout)
            .await
            .map(Some)
    }

    /// Pings the server.
    pub async fn ping(&self) -> McpResult<Value> {
        self.request_recovering("ping", None, self.inner.timeouts.catalog())
            .await
    }
}
