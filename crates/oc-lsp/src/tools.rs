//! `lsp` and `lsp_diagnostics` tool operations.
//!
//! Input positions are one-based; LSP positions are zero-based. All results
//! are JSON: raw LSP responses for navigation/hover/symbols, and the bounded
//! diagnostics array for the diagnostics operations.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::LspError;
use crate::position::user_position_to_offset;
use crate::supervisor::LspSupervisor;
use crate::uri::path_to_uri;

/// Maximum number of workspace symbols returned to the model.
pub const WORKSPACE_SYMBOL_LIMIT: usize = 50;
/// Maximum severity-1 errors appended to edit/write output.
pub const ERROR_TEXT_LIMIT: usize = 20;

/// Supported `lsp` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspOperation {
    /// `textDocument/definition`
    GoToDefinition,
    /// `textDocument/references`
    FindReferences,
    /// `textDocument/hover`
    Hover,
    /// `textDocument/documentSymbol`
    DocumentSymbol,
    /// `workspace/symbol`
    WorkspaceSymbol,
    /// `textDocument/implementation`
    GoToImplementation,
    /// `textDocument/prepareCallHierarchy`
    PrepareCallHierarchy,
    /// `callHierarchy/incomingCalls`
    IncomingCalls,
    /// `callHierarchy/outgoingCalls`
    OutgoingCalls,
    /// Cached diagnostics (also exposed as `lsp_diagnostics`).
    Diagnostics,
}

impl LspOperation {
    /// Parse the wire operation name. Snake-case aliases are accepted.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim();
        match normalized {
            "goToDefinition" | "go_to_definition" | "definition" => Some(Self::GoToDefinition),
            "findReferences" | "find_references" | "references" => Some(Self::FindReferences),
            "hover" => Some(Self::Hover),
            "documentSymbol" | "document_symbol" | "documentSymbols" => Some(Self::DocumentSymbol),
            "workspaceSymbol" | "workspace_symbol" | "workspaceSymbols" => {
                Some(Self::WorkspaceSymbol)
            }
            "goToImplementation" | "go_to_implementation" | "implementation" => {
                Some(Self::GoToImplementation)
            }
            "prepareCallHierarchy" | "prepare_call_hierarchy" => Some(Self::PrepareCallHierarchy),
            "incomingCalls" | "incoming_calls" => Some(Self::IncomingCalls),
            "outgoingCalls" | "outgoing_calls" => Some(Self::OutgoingCalls),
            "diagnostics" => Some(Self::Diagnostics),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::GoToDefinition => "goToDefinition",
            Self::FindReferences => "findReferences",
            Self::Hover => "hover",
            Self::DocumentSymbol => "documentSymbol",
            Self::WorkspaceSymbol => "workspaceSymbol",
            Self::GoToImplementation => "goToImplementation",
            Self::PrepareCallHierarchy => "prepareCallHierarchy",
            Self::IncomingCalls => "incomingCalls",
            Self::OutgoingCalls => "outgoingCalls",
            Self::Diagnostics => "diagnostics",
        }
    }
}

/// Input accepted by the `lsp` tool.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspToolInput {
    /// Operation name.
    pub operation: String,
    /// Absolute (or workspace-relative) file path.
    #[serde(default)]
    pub file_path: Option<String>,
    /// One-based line.
    #[serde(default)]
    pub line: Option<u32>,
    /// One-based UTF-16 character offset.
    #[serde(default)]
    pub character: Option<u32>,
    /// Optional symbol query for `workspaceSymbol`.
    #[serde(default)]
    pub query: Option<String>,
}

impl LspToolInput {
    /// Parse from a JSON object.
    pub fn from_value(value: Value) -> Result<Self, LspError> {
        serde_json::from_value(value).map_err(|error| LspError::InvalidParams(error.to_string()))
    }
}

/// Diagnostics produced after an edit, ready for the tool result.
#[derive(Debug, Clone)]
pub struct EditDiagnostics {
    /// Severity-1 error text to append to the model output, when any.
    pub text: Option<String>,
    /// Full diagnostics JSON for `metadata.diagnostics`.
    pub metadata: Value,
}

/// Run one `lsp` tool operation.
pub async fn execute(supervisor: &LspSupervisor, input: &LspToolInput) -> Result<Value, LspError> {
    let operation = LspOperation::parse(&input.operation)
        .ok_or_else(|| LspError::Unsupported(input.operation.clone()))?;

    if operation == LspOperation::Diagnostics {
        let file = resolve_path(supervisor, &required_file(input)?);
        return Ok(supervisor.diagnostics_json(&file).await);
    }
    if operation == LspOperation::WorkspaceSymbol {
        return workspace_symbol(supervisor, input).await;
    }

    let file = resolve_path(supervisor, &required_file(input)?);
    let line = input.line.unwrap_or(1);
    let character = input.character.unwrap_or(1);
    let client = supervisor.client_for_file(&file).await?;
    client.touch_file(&file, false).await?;
    let canonical = canonical_or_owned(&file);
    let uri = path_to_uri(&canonical);
    let position = client.wire_position(&canonical, line, character).await?;
    let position_json = position.to_json();
    let text_document = json!({ "uri": uri });

    match operation {
        LspOperation::GoToDefinition => {
            client
                .request(
                    "textDocument/definition",
                    json!({ "textDocument": text_document, "position": position_json }),
                )
                .await
        }
        LspOperation::FindReferences => {
            client
                .request(
                    "textDocument/references",
                    json!({
                        "textDocument": text_document,
                        "position": position_json,
                        "context": { "includeDeclaration": true }
                    }),
                )
                .await
        }
        LspOperation::Hover => {
            client
                .request(
                    "textDocument/hover",
                    json!({ "textDocument": text_document, "position": position_json }),
                )
                .await
        }
        LspOperation::DocumentSymbol => {
            client
                .request(
                    "textDocument/documentSymbol",
                    json!({ "textDocument": text_document }),
                )
                .await
        }
        LspOperation::GoToImplementation => {
            client
                .request(
                    "textDocument/implementation",
                    json!({ "textDocument": text_document, "position": position_json }),
                )
                .await
        }
        LspOperation::PrepareCallHierarchy => {
            client
                .request(
                    "textDocument/prepareCallHierarchy",
                    json!({ "textDocument": text_document, "position": position_json }),
                )
                .await
        }
        LspOperation::IncomingCalls | LspOperation::OutgoingCalls => {
            let prepared = client
                .request(
                    "textDocument/prepareCallHierarchy",
                    json!({ "textDocument": text_document, "position": position_json }),
                )
                .await?;
            let Some(item) = prepared.as_array().and_then(|items| items.first()).cloned() else {
                return Ok(json!([]));
            };
            let method = if operation == LspOperation::IncomingCalls {
                "callHierarchy/incomingCalls"
            } else {
                "callHierarchy/outgoingCalls"
            };
            client.request(method, json!({ "item": item })).await
        }
        LspOperation::WorkspaceSymbol | LspOperation::Diagnostics => unreachable!(),
    }
}

async fn workspace_symbol(
    supervisor: &LspSupervisor,
    input: &LspToolInput,
) -> Result<Value, LspError> {
    let (client, query) = match (&input.file_path, &input.query) {
        (_, Some(query)) if !query.trim().is_empty() => (
            match &input.file_path {
                Some(path) => {
                    supervisor
                        .client_for_file(&resolve_path(supervisor, Path::new(path)))
                        .await?
                }
                None => supervisor.client_for_workspace().await?,
            },
            query.trim().to_owned(),
        ),
        (Some(path), _) => {
            let file = resolve_path(supervisor, Path::new(path));
            let text = std::fs::read_to_string(&file).map_err(LspError::Io)?;
            let line = input.line.unwrap_or(1);
            let character = input.character.unwrap_or(1);
            let (_, offset) = user_position_to_offset(&text, line, character);
            let query = identifier_at(&text, offset).ok_or_else(|| {
                LspError::InvalidParams(
                    "workspaceSymbol requires `query` or an identifier at the given position"
                        .into(),
                )
            })?;
            (supervisor.client_for_file(&file).await?, query)
        }
        (None, _) => {
            return Err(LspError::InvalidParams(
                "workspaceSymbol requires `query` or `filePath` with `line` and `character`".into(),
            ));
        }
    };

    let result = client
        .request("workspace/symbol", json!({ "query": query }))
        .await?;
    let symbols = result.as_array().cloned().unwrap_or_default();
    let truncated = symbols.len() > WORKSPACE_SYMBOL_LIMIT;
    let symbols: Vec<Value> = symbols.into_iter().take(WORKSPACE_SYMBOL_LIMIT).collect();
    Ok(json!({ "symbols": symbols, "truncated": truncated }))
}

/// Cached diagnostics for `path` as a JSON array (`lsp_diagnostics`).
pub async fn diagnostics(supervisor: &LspSupervisor, path: &Path) -> Result<Value, LspError> {
    Ok(supervisor.diagnostics_json(path).await)
}

/// Wait for diagnostics after an edit and prepare the model text/metadata.
pub async fn edit_diagnostics(
    supervisor: &LspSupervisor,
    path: &Path,
) -> Result<Option<EditDiagnostics>, LspError> {
    let Some(cached) = supervisor.touch_and_wait(path).await? else {
        return Ok(None);
    };
    let merged = cached.merged();
    if merged.is_empty() {
        return Ok(None);
    }
    Ok(Some(EditDiagnostics {
        text: format_errors(path, &merged),
        metadata: Value::Array(merged),
    }))
}

/// Render severity-1 diagnostics in the documented tool-output shape.
#[must_use]
pub fn format_errors(path: &Path, diagnostics: &[Value]) -> Option<String> {
    let errors: Vec<&Value> = diagnostics
        .iter()
        .filter(|item| item.get("severity").and_then(Value::as_i64) == Some(1))
        .take(ERROR_TEXT_LIMIT)
        .collect();
    if errors.is_empty() {
        return None;
    }
    let mut text = String::from("LSP errors detected in this file:\n");
    let _ = writeln!(text, "<diagnostics file=\"{}\">", path.display());
    for error in errors {
        let range = error.get("range");
        let line = range
            .and_then(|range| range.pointer("/start/line"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        let character = range
            .and_then(|range| range.pointer("/start/character"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let _ = writeln!(text, "ERROR [{line}:{character}] {message}");
    }
    text.push_str("</diagnostics>");
    Some(text)
}

fn required_file(input: &LspToolInput) -> Result<PathBuf, LspError> {
    let operation = match LspOperation::parse(&input.operation) {
        Some(operation) => operation.name(),
        None => input.operation.as_str(),
    };
    input
        .file_path
        .as_deref()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| LspError::InvalidParams(format!("`filePath` is required for {operation}")))
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_path(supervisor: &LspSupervisor, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        supervisor.workspace_root().join(path)
    }
}

fn identifier_at(text: &str, offset: usize) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let mut start = offset;
    while start > 0 {
        let Some(previous) = text[..start].chars().next_back() else {
            break;
        };
        if is_identifier_char(previous) {
            start -= previous.len_utf8();
        } else {
            break;
        }
    }
    let mut end = offset;
    while end < text.len() {
        let Some(next) = text[end..].chars().next() else {
            break;
        };
        if is_identifier_char(next) {
            end += next.len_utf8();
        } else {
            break;
        }
    }
    (start < end).then(|| text[start..end].to_owned())
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_operations() {
        for (name, operation) in [
            ("goToDefinition", LspOperation::GoToDefinition),
            ("findReferences", LspOperation::FindReferences),
            ("hover", LspOperation::Hover),
            ("documentSymbol", LspOperation::DocumentSymbol),
            ("workspaceSymbol", LspOperation::WorkspaceSymbol),
            ("goToImplementation", LspOperation::GoToImplementation),
            ("prepareCallHierarchy", LspOperation::PrepareCallHierarchy),
            ("incomingCalls", LspOperation::IncomingCalls),
            ("outgoingCalls", LspOperation::OutgoingCalls),
        ] {
            assert_eq!(LspOperation::parse(name), Some(operation), "{name}");
        }
        assert_eq!(
            LspOperation::parse("go_to_definition"),
            Some(LspOperation::GoToDefinition)
        );
        assert!(LspOperation::parse("rename").is_none());
    }

    #[test]
    fn formats_only_severity_one_up_to_the_limit() {
        let path = Path::new("/a.rs");
        let mut items = Vec::new();
        for index in 0..30 {
            items.push(json!({
                "range": {"start": {"line": index, "character": 2}, "end": {"line": index, "character": 4}},
                "severity": 1,
                "message": format!("bad {index}"),
            }));
        }
        items.push(json!({
            "range": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 2}},
            "severity": 2,
            "message": "warning",
        }));
        let text = format_errors(path, &items).unwrap();
        assert!(
            text.starts_with("LSP errors detected in this file:\n<diagnostics file=\"/a.rs\">")
        );
        assert!(text.ends_with("</diagnostics>"));
        assert!(!text.contains("warning"));
        assert_eq!(text.matches("ERROR [").count(), ERROR_TEXT_LIMIT);
        assert!(text.contains("ERROR [1:3] bad 0"));
    }

    #[test]
    fn no_errors_means_no_text() {
        let items = vec![json!({"severity": 2, "message": "warn"})];
        assert!(format_errors(Path::new("/a.rs"), &items).is_none());
        assert!(format_errors(Path::new("/a.rs"), &[]).is_none());
    }

    #[test]
    fn finds_identifiers_at_the_cursor() {
        let text = "let alpha_beta = foo(1);";
        let offset = text.find("alpha_beta").unwrap() + 3;
        assert_eq!(identifier_at(text, offset).as_deref(), Some("alpha_beta"));
        assert_eq!(identifier_at(text, 0).as_deref(), Some("let"));
        assert!(identifier_at("  ()  ", 2).is_none());
    }

    #[test]
    fn input_requires_file_path_for_positional_operations() {
        let input = LspToolInput {
            operation: "hover".into(),
            file_path: None,
            line: Some(1),
            character: Some(1),
            query: None,
        };
        let error = required_file(&input).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid LSP arguments: `filePath` is required for hover"
        );
    }
}
