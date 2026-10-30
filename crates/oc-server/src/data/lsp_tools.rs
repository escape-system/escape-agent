//! Runtime tool provider for the LSP supervisor.
//!
//! Two tools, matching `docs/11-lsp.md` and `docs/12-tools.md`:
//!
//! - `lsp {operation, filePath?, line?, character?, query?}` — navigation,
//!   hover, symbols and call hierarchy (1-based positions);
//! - `lsp_diagnostics {path}` — the cached diagnostics array for a path.
//!
//! Both return JSON as their model-facing text. The supervisor owns spawning,
//! file sync and caching; these adapters only parse and format.

use std::path::PathBuf;
use std::sync::Arc;

use oc_lsp::{LspSupervisor, tools::LspToolInput};
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use oc_types::SessionInfo;
use serde_json::{Value, json};

/// `lsp` tool: LSP operations over the supervisor.
#[derive(Debug)]
pub struct LspTool {
    supervisor: Arc<LspSupervisor>,
}

impl LspTool {
    #[must_use]
    pub fn new(supervisor: Arc<LspSupervisor>) -> Self {
        Self { supervisor }
    }
}

fn json_text(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

#[async_trait::async_trait]
impl Tool for LspTool {
    fn name(&self) -> &'static str {
        "lsp"
    }

    fn description(&self) -> &'static str {
        "Query a language server: goToDefinition, findReferences, hover, documentSymbol, \
         workspaceSymbol, goToImplementation, prepareCallHierarchy, incomingCalls, \
         outgoingCalls, or diagnostics. Positions are 1-based."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": [
                        "goToDefinition",
                        "findReferences",
                        "hover",
                        "documentSymbol",
                        "workspaceSymbol",
                        "goToImplementation",
                        "prepareCallHierarchy",
                        "incomingCalls",
                        "outgoingCalls",
                        "diagnostics"
                    ],
                    "description": "LSP request to run."
                },
                "filePath": {
                    "type": "string",
                    "description": "Absolute or workspace-relative file path."
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "One-based line."
                },
                "character": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "One-based UTF-16 character offset."
                },
                "query": {
                    "type": "string",
                    "description": "Symbol query for workspaceSymbol."
                }
            },
            "required": ["operation"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let input =
            LspToolInput::from_value(input).map_err(|error| ToolError::new(error.to_string()))?;
        let value = self
            .supervisor
            .execute_tool(&input)
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        Ok(ToolResult::text(json_text(&value)))
    }
}

/// `lsp_diagnostics` tool: cached diagnostics for one path.
#[derive(Debug)]
pub struct LspDiagnosticsTool {
    supervisor: Arc<LspSupervisor>,
}

impl LspDiagnosticsTool {
    #[must_use]
    pub fn new(supervisor: Arc<LspSupervisor>) -> Self {
        Self { supervisor }
    }
}

#[async_trait::async_trait]
impl Tool for LspDiagnosticsTool {
    fn name(&self) -> &'static str {
        "lsp_diagnostics"
    }

    fn description(&self) -> &'static str {
        "Return the cached LSP diagnostics for a file as JSON."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or workspace-relative file path."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let path = match &input {
            Value::Object(object) => object
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_owned),
            _ => None,
        }
        .ok_or_else(|| ToolError::new("lsp_diagnostics requires a path string"))?;
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            ctx.directory.join(path)
        };
        let value = self
            .supervisor
            .execute_diagnostics_tool(&path)
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        Ok(ToolResult::text(json_text(&value)))
    }
}

/// Tool provider backed by the attached LSP supervisor.
#[derive(Debug)]
pub struct LspToolProvider {
    supervisor: Arc<LspSupervisor>,
}

impl LspToolProvider {
    #[must_use]
    pub fn new(supervisor: Arc<LspSupervisor>) -> Self {
        Self { supervisor }
    }
}

impl oc_runtime::ToolProvider for LspToolProvider {
    fn tools(&self, _session: &SessionInfo, _agent: &str) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(LspTool::new(Arc::clone(&self.supervisor))),
            Arc::new(LspDiagnosticsTool::new(Arc::clone(&self.supervisor))),
        ]
    }
}
