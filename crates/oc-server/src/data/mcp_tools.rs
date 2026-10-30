//! Runtime tool provider for MCP servers.
//!
//! [`McpToolAdapter`] turns one advertised `McpTool` into an
//! [`oc_tools::Tool`]. [`McpToolProvider`] snapshots the manager's connected
//! catalog per execution so connecting or disconnecting a server takes effect
//! without a restart. Permission filtering stays in the runtime; this provider
//! deliberately returns every connected tool.

use std::sync::Arc;

use oc_mcp::{McpClient, McpContent, McpTool};
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use oc_types::SessionInfo;
use serde_json::{Map, Value, json};

use crate::data::mcp::McpManager;

/// One MCP server tool exposed to the model.
pub struct McpToolAdapter {
    client: Arc<McpClient>,
    tool: McpTool,
    name: String,
    description: String,
}

impl std::fmt::Debug for McpToolAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpToolAdapter")
            .field("server", &self.tool.server)
            .field("name", &self.tool.effective_name)
            .finish_non_exhaustive()
    }
}

impl McpToolAdapter {
    /// Wrap one advertised tool plus the client that owns it.
    #[must_use]
    pub fn new(client: Arc<McpClient>, tool: McpTool) -> Self {
        let description = tool.description.clone().unwrap_or_else(|| {
            format!("MCP tool {} on server {}", tool.effective_name, tool.server)
        });
        Self {
            client,
            name: tool.effective_name.clone(),
            tool,
            description,
        }
    }

    /// The advertised tool (name, server, raw schemas).
    #[must_use]
    pub fn tool(&self) -> &McpTool {
        &self.tool
    }
}

#[async_trait::async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.tool.schema()
    }

    async fn execute(&self, _ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let mut arguments = match input {
            Value::Object(arguments) => arguments,
            Value::Null => Map::new(),
            _ => {
                return Err(ToolError::new("MCP tool arguments must be a JSON object"));
            }
        };
        let _ = arguments.remove("expectation");
        let output = self
            .tool
            .execute(&self.client, arguments)
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;

        let text = output.text();
        let structured = output.structured.clone();
        let output_text = if text.is_empty() {
            structured.as_ref().map(ToString::to_string).unwrap_or(text)
        } else {
            text
        };
        let content = media_content(&output.content);
        Ok(ToolResult::new(
            Some(output_text),
            content,
            output.metadata(),
        ))
    }
}

/// Media parts become file content; text parts stay as text. `None` when the
/// result is text-only so the runtime can emit its plain-text default.
fn media_content(parts: &[McpContent]) -> Option<Value> {
    if !parts
        .iter()
        .any(|part| matches!(part, McpContent::Media { .. }))
    {
        return None;
    }
    let content = parts
        .iter()
        .map(|part| match part {
            McpContent::Text { text } => json!({"type": "text", "text": text}),
            McpContent::Media {
                data,
                mime_type,
                uri,
            } => {
                let uri = uri
                    .clone()
                    .unwrap_or_else(|| format!("data:{mime_type};base64,{data}"));
                json!({"type": "file", "uri": uri, "mime": mime_type})
            }
        })
        .collect();
    Some(Value::Array(content))
}

/// Tool provider backed by the attached MCP manager.
#[derive(Debug)]
pub struct McpToolProvider {
    manager: Arc<McpManager>,
}

impl McpToolProvider {
    #[must_use]
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self { manager }
    }
}

impl oc_runtime::ToolProvider for McpToolProvider {
    fn tools(&self, _session: &SessionInfo, _agent: &str) -> Vec<Arc<dyn Tool>> {
        self.manager
            .connected_tools()
            .into_iter()
            .map(|(client, tool)| Arc::new(McpToolAdapter::new(client, tool)) as Arc<dyn Tool>)
            .collect()
    }
}
