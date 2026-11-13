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

/// Host post-edit diagnostics hook powered by [`LspSupervisor`].
#[derive(Debug, Clone)]
pub struct LspEditDiagnosticsHook {
    supervisor: Arc<LspSupervisor>,
}

impl LspEditDiagnosticsHook {
    #[must_use]
    pub fn new(supervisor: Arc<LspSupervisor>) -> Self {
        Self { supervisor }
    }
}

#[async_trait::async_trait]
impl oc_runtime::EditDiagnosticsHook for LspEditDiagnosticsHook {
    async fn after_edit(
        &self,
        root: &std::path::Path,
        files: &[String],
    ) -> oc_runtime::PostEditDiagnostics {
        let mut texts = Vec::new();
        let mut all_diagnostics = Vec::new();
        let mut error_count = 0usize;

        for rel in files.iter().take(6) {
            let candidate = PathBuf::from(rel);
            let abs = if candidate.is_absolute() {
                candidate
            } else {
                root.join(candidate)
            };
            if let Ok(Some(diag)) = oc_lsp::tools::edit_diagnostics(&self.supervisor, &abs).await {
                if let Some(items) = diag.metadata.as_array() {
                    for item in items {
                        if item.get("severity").and_then(Value::as_i64) == Some(1) {
                            error_count += 1;
                        }
                        all_diagnostics.push(item.clone());
                    }
                }
                if let Some(formatted) = diag.text
                    && !formatted.trim().is_empty()
                {
                    texts.push(formatted);
                }
            }
        }

        let formatted = (!texts.is_empty()).then(|| {
            format!(
                "<lsp_diagnostics>\n{}\n</lsp_diagnostics>",
                texts.join("\n")
            )
        });
        let metadata = (!all_diagnostics.is_empty()).then_some(Value::Array(all_diagnostics));

        oc_runtime::PostEditDiagnostics {
            formatted,
            error_count,
            metadata,
        }
    }
}

/// Compiler-resolved semantic oracle forwarding `code_map` precision queries to
/// [`LspSupervisor`].
#[derive(Debug, Clone)]
pub struct LspSemanticOracle {
    supervisor: Arc<LspSupervisor>,
}

impl LspSemanticOracle {
    #[must_use]
    pub fn new(supervisor: Arc<LspSupervisor>) -> Self {
        Self { supervisor }
    }
}

#[async_trait::async_trait]
impl oc_tools::code_map::SemanticPrecisionOracle for LspSemanticOracle {
    async fn query(&self, req: oc_tools::code_map::SemanticRequest) -> Result<Value, String> {
        use oc_tools::code_map::SemanticRequest;
        let input = match req {
            SemanticRequest::Definition {
                file_path,
                line,
                character,
            } => LspToolInput {
                operation: "goToDefinition".to_owned(),
                file_path: Some(file_path),
                line: Some(line),
                character: Some(character),
                query: None,
            },
            SemanticRequest::References {
                file_path,
                line,
                character,
            } => LspToolInput {
                operation: "findReferences".to_owned(),
                file_path: Some(file_path),
                line: Some(line),
                character: Some(character),
                query: None,
            },
            SemanticRequest::Implementations {
                file_path,
                line,
                character,
            } => LspToolInput {
                operation: "goToImplementation".to_owned(),
                file_path: Some(file_path),
                line: Some(line),
                character: Some(character),
                query: None,
            },
            SemanticRequest::IncomingCalls {
                file_path,
                line,
                character,
            } => LspToolInput {
                operation: "incomingCalls".to_owned(),
                file_path: Some(file_path),
                line: Some(line),
                character: Some(character),
                query: None,
            },
            SemanticRequest::OutgoingCalls {
                file_path,
                line,
                character,
            } => LspToolInput {
                operation: "outgoingCalls".to_owned(),
                file_path: Some(file_path),
                line: Some(line),
                character: Some(character),
                query: None,
            },
            SemanticRequest::WorkspaceSymbol { query } => LspToolInput {
                operation: "workspaceSymbol".to_owned(),
                file_path: None,
                line: None,
                character: None,
                query: Some(query),
            },
            SemanticRequest::Diagnostics { file_path } => LspToolInput {
                operation: "diagnostics".to_owned(),
                file_path: Some(file_path),
                line: None,
                character: None,
                query: None,
            },
        };
        self.supervisor
            .execute_tool(&input)
            .await
            .map_err(|error| error.to_string())
    }
}
