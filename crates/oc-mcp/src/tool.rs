//! MCP tool registration, schema patching and result mapping.

use serde_json::{Map, Value, json};

use crate::error::{McpError, McpResult};

/// Text from one MCP result is capped at 1 MiB.
pub const MAX_RESULT_TEXT_BYTES: usize = 1024 * 1024;
/// One media part is capped at 15 MiB.
pub const MAX_RESULT_MEDIA_BYTES: usize = 15 * 1024 * 1024;
/// Maximum length of `expectation.why`.
const WHY_MAX_CHARS: usize = 160;

/// Sanitizes one name segment (`[^A-Za-z0-9_-]` → `_`).
#[must_use]
pub fn sanitize(segment: &str) -> String {
    segment
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Registry namespace for a server: its sanitized name.
#[must_use]
pub fn namespace(server: &str) -> String {
    sanitize(server)
}

/// Model-facing tool name: `<server>_<tool>`, both sanitized.
#[must_use]
pub fn effective_name(server: &str, tool: &str) -> String {
    format!("{}_{}", sanitize(server), sanitize(tool))
}

/// A validated tool advertised by an MCP server.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub server: String,
    pub namespace: String,
    /// Name as the MCP server knows it.
    pub name: String,
    /// Name the model sees.
    pub effective_name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
}

impl McpTool {
    /// Validates one `tools/list` entry. Invalid entries are rejected so the
    /// caller can drop them with a warning while keeping the rest.
    pub fn from_wire(server: &str, value: &Value) -> McpResult<Self> {
        let Some(object) = value.as_object() else {
            return Err(McpError::Protocol("tool is not an object".to_owned()));
        };
        let Some(name) = object.get("name").and_then(Value::as_str) else {
            return Err(McpError::Protocol("tool has no name".to_owned()));
        };
        if name.trim().is_empty() {
            return Err(McpError::Protocol("tool name is empty".to_owned()));
        }
        let Some(input_schema) = object.get("inputSchema").filter(|value| value.is_object()) else {
            return Err(McpError::Protocol(format!(
                "tool \"{name}\" has no object inputSchema"
            )));
        };
        if let Some(description) = object.get("description")
            && !description.is_string()
            && !description.is_null()
        {
            return Err(McpError::Protocol(format!(
                "tool \"{name}\" has a non-string description"
            )));
        }
        let output_schema = match object.get("outputSchema") {
            Some(value) if value.is_object() => Some(value.clone()),
            Some(value) if !value.is_null() => {
                return Err(McpError::Protocol(format!(
                    "tool \"{name}\" has a non-object outputSchema"
                )));
            }
            _ => None,
        };
        Ok(Self {
            server: server.to_owned(),
            namespace: namespace(server),
            name: name.to_owned(),
            effective_name: effective_name(server, name),
            description: object
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            input_schema: input_schema.clone(),
            output_schema,
        })
    }

    /// The JSON Schema shown to the model: the server schema with the
    /// required `expectation` property patched in.
    #[must_use]
    pub fn schema(&self) -> Value {
        patch_expectation(&self.input_schema)
    }

    /// Effective name used for permissions.
    #[must_use]
    pub fn permission_action(&self) -> &str {
        &self.effective_name
    }

    /// Calls this tool through `client`, stripping `expectation` first.
    pub async fn execute(
        &self,
        client: &crate::client::McpClient,
        mut arguments: Map<String, Value>,
    ) -> McpResult<McpToolOutput> {
        let _ = arguments.remove("expectation");
        let result = client
            .call_tool(&self.name, Value::Object(arguments))
            .await?;
        map_call_result(
            &self.server,
            &self.name,
            &result,
            self.output_schema.is_some(),
        )
    }
}

fn expectation_schema() -> Value {
    json!({
        "type": "object",
        "description": "What this call expects and when the observer may intervene.",
        "properties": {
            "why": {
                "type": "string",
                "maxLength": WHY_MAX_CHARS,
                "description": "One sentence: action and expected outcome."
            },
            "expect_seconds": {
                "type": "integer",
                "minimum": 1,
                "maximum": 3600
            },
            "intervene_after_seconds": {
                "type": "integer",
                "minimum": 3,
                "maximum": 3600
            }
        },
        "required": ["why", "expect_seconds", "intervene_after_seconds"],
        "additionalProperties": false
    })
}

/// Patches a JSON Schema with the required `expectation` property.
#[must_use]
pub fn patch_expectation(schema: &Value) -> Value {
    let mut patched = schema.clone();
    let Some(object) = patched.as_object_mut() else {
        return json!({
            "type": "object",
            "properties": {"expectation": expectation_schema()},
            "required": ["expectation"]
        });
    };
    object
        .entry("type")
        .or_insert_with(|| Value::String("object".to_owned()));
    let properties = object
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    if !properties.is_object() {
        *properties = Value::Object(Map::new());
    }
    if let Some(properties) = properties.as_object_mut() {
        properties.insert("expectation".to_owned(), expectation_schema());
    }
    let required = object
        .entry("required")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !required.is_array() {
        *required = Value::Array(Vec::new());
    }
    if let Some(required) = required.as_array_mut()
        && !required.iter().any(|item| item == "expectation")
    {
        required.push(Value::String("expectation".to_owned()));
    }
    patched
}

/// Removes `expectation` from tool arguments.
pub fn strip_expectation(arguments: &mut Map<String, Value>) -> Option<Value> {
    arguments.remove("expectation")
}

/// One mapped content part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpContent {
    /// Text content.
    Text { text: String },
    /// Base64 media content.
    Media {
        data: String,
        mime_type: String,
        uri: Option<String>,
    },
}

impl McpContent {
    /// Text of this part, when it is text.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            Self::Media { .. } => None,
        }
    }
}

/// Mapped `tools/call` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolOutput {
    pub content: Vec<McpContent>,
    /// `structuredContent`, when the server sent it.
    pub structured: Option<Value>,
}

impl McpToolOutput {
    /// Concatenated text parts joined with a newline.
    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(McpContent::as_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `metadata.structured` for the runtime, when the server sent structured content.
    #[must_use]
    pub fn metadata(&self) -> Option<Value> {
        self.structured
            .as_ref()
            .map(|structured| serde_json::json!({"structured": structured}))
    }
}

/// Maps a raw `tools/call` result per docs/09.
///
/// `isError` becomes [`McpError::ToolReturnedError`] carrying the joined text.
#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive mapping of the MCP content union reads better as a single match"
)]
pub fn map_call_result(
    server: &str,
    tool: &str,
    result: &Value,
    has_output_schema: bool,
) -> McpResult<McpToolOutput> {
    let Some(object) = result.as_object() else {
        return Err(McpError::Protocol(
            "tools/call result is not an object".to_owned(),
        ));
    };
    let is_error = object
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let structured = object
        .get("structuredContent")
        .filter(|value| !value.is_null())
        .cloned();
    let mut content = Vec::new();
    let mut text_budget = MAX_RESULT_TEXT_BYTES;

    let parts = match object.get("content") {
        Some(Value::Array(parts)) => parts.as_slice(),
        Some(Value::Null) | None => &[],
        Some(_) => {
            return Err(McpError::Protocol(
                "tools/call content is not an array".to_owned(),
            ));
        }
    };

    for part in parts {
        let Some(part) = part.as_object() else {
            tracing::warn!(%server, %tool, "skipping non-object MCP content part");
            continue;
        };
        let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "text" => {
                let Some(text) = part.get("text").and_then(Value::as_str) else {
                    return Err(McpError::Protocol(
                        "text content part has no text".to_owned(),
                    ));
                };
                push_text(&mut content, &mut text_budget, text);
            }
            "image" | "audio" => {
                let (Some(data), Some(mime)) = (
                    part.get("data").and_then(Value::as_str),
                    part.get("mimeType").and_then(Value::as_str),
                ) else {
                    return Err(McpError::Protocol(format!(
                        "{kind} content part is missing data or mimeType"
                    )));
                };
                push_media(
                    &mut content,
                    data,
                    mime,
                    part.get("uri").and_then(Value::as_str),
                );
            }
            "resource_link" => {
                let Some(uri) = part.get("uri").and_then(Value::as_str) else {
                    return Err(McpError::Protocol(
                        "resource_link content part has no uri".to_owned(),
                    ));
                };
                push_text(&mut content, &mut text_budget, uri);
            }
            "resource" => {
                let Some(resource) = part.get("resource").and_then(Value::as_object) else {
                    return Err(McpError::Protocol(
                        "resource content part has no resource object".to_owned(),
                    ));
                };
                let uri = resource
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(text) = resource.get("text").and_then(Value::as_str) {
                    push_text(&mut content, &mut text_budget, text);
                } else if let (Some(blob), Some(mime)) = (
                    resource.get("blob").and_then(Value::as_str),
                    resource.get("mimeType").and_then(Value::as_str),
                ) {
                    push_media(&mut content, blob, mime, Some(uri));
                } else {
                    push_text(&mut content, &mut text_budget, uri);
                }
            }
            other => {
                tracing::warn!(%server, %tool, kind = %other, "skipping unsupported MCP content part");
            }
        }
    }

    if is_error {
        let message = content
            .iter()
            .filter_map(McpContent::as_text)
            .collect::<Vec<_>>()
            .join("\n");
        let message = message.trim();
        return Err(McpError::ToolReturnedError {
            server: server.to_owned(),
            tool: tool.to_owned(),
            message: if message.is_empty() {
                "MCP tool returned an error".to_owned()
            } else {
                message.to_owned()
            },
        });
    }

    if let Some(structured) = &structured
        && !has_output_schema
        && !content.iter().any(|part| part.as_text().is_some())
    {
        let text = serde_json::to_string(structured).map_err(|error| {
            McpError::Protocol(format!("structuredContent is not JSON: {error}"))
        })?;
        push_text(&mut content, &mut text_budget, &text);
    }

    Ok(McpToolOutput {
        content,
        structured,
    })
}

fn push_text(content: &mut Vec<McpContent>, budget: &mut usize, text: &str) {
    if *budget == 0 {
        return;
    }
    let (text, truncated) = truncate_utf8(text, *budget);
    *budget = budget.saturating_sub(text.len());
    let mut owned = text.to_owned();
    if truncated {
        owned.push_str("\n[output truncated at 1 MiB]");
    }
    content.push(McpContent::Text { text: owned });
}

fn push_media(content: &mut Vec<McpContent>, data: &str, mime: &str, uri: Option<&str>) {
    // Base64 expands bytes by 4/3; estimate the decoded size for the limit.
    let decoded_bytes = data.len().saturating_mul(3) / 4;
    if decoded_bytes > MAX_RESULT_MEDIA_BYTES {
        content.push(McpContent::Text {
            text: format!(
                "[media omitted: {mime} part is {decoded_bytes} bytes, exceeding the 15 MiB limit]"
            ),
        });
        return;
    }
    content.push(McpContent::Media {
        data: data.to_owned(),
        mime_type: mime.to_owned(),
        uri: uri.map(str::to_owned),
    });
}

fn truncate_utf8(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(namespace("my/server!"), "my_server_");
        assert_eq!(
            effective_name("gh.srv", "list-repos!"),
            "gh_srv_list-repos_"
        );
        assert_eq!(effective_name("srv", "already_ok-1"), "srv_already_ok-1");
    }

    #[test]
    fn patches_schema_with_expectation() {
        let schema =
            json!({"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]});
        let patched = patch_expectation(&schema);
        assert!(patched["properties"]["expectation"].is_object());
        assert!(
            patched["required"]
                .as_array()
                .unwrap()
                .contains(&json!("expectation"))
        );
        // idempotent
        let twice = patch_expectation(&patched);
        assert_eq!(
            twice["required"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|value| *value == "expectation")
                .count(),
            1
        );
        assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
    }

    fn tool() -> McpTool {
        McpTool::from_wire(
            "srv",
            &json!({"name": "t", "description": "d", "inputSchema": {"type": "object"}}),
        )
        .expect("tool")
    }

    #[test]
    fn rejects_invalid_tool_schemas() {
        for value in [
            json!("nope"),
            json!({"inputSchema": {"type": "object"}}),
            json!({"name": "", "inputSchema": {"type": "object"}}),
            json!({"name": "x"}),
            json!({"name": "x", "inputSchema": []}),
            json!({"name": "x", "inputSchema": {}, "description": 4}),
            json!({"name": "x", "inputSchema": {}, "outputSchema": "nope"}),
        ] {
            assert!(
                McpTool::from_wire("srv", &value).is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn maps_every_content_kind() {
        let result = json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "data": "aGk=", "mimeType": "image/png"},
                {"type": "audio", "data": "aGk=", "mimeType": "audio/wav"},
                {"type": "resource_link", "uri": "file:///tmp/x"},
                {"type": "resource", "resource": {"uri": "file:///a", "text": "inline"}},
                {"type": "resource", "resource": {"uri": "file:///b", "blob": "aGk=", "mimeType": "application/octet-stream"}},
                {"type": "resource", "resource": {"uri": "file:///c"}},
                {"type": "something-new"}
            ]
        });
        let output = map_call_result("srv", "t", &result, true).expect("mapped");
        assert_eq!(
            output.content,
            vec![
                McpContent::Text {
                    text: "hello".to_owned()
                },
                McpContent::Media {
                    data: "aGk=".to_owned(),
                    mime_type: "image/png".to_owned(),
                    uri: None
                },
                McpContent::Media {
                    data: "aGk=".to_owned(),
                    mime_type: "audio/wav".to_owned(),
                    uri: None
                },
                McpContent::Text {
                    text: "file:///tmp/x".to_owned()
                },
                McpContent::Text {
                    text: "inline".to_owned()
                },
                McpContent::Media {
                    data: "aGk=".to_owned(),
                    mime_type: "application/octet-stream".to_owned(),
                    uri: Some("file:///b".to_owned())
                },
                McpContent::Text {
                    text: "file:///c".to_owned()
                },
            ]
        );
        assert_eq!(output.text(), "hello\nfile:///tmp/x\ninline\nfile:///c");
    }

    #[test]
    fn structured_content_becomes_json_text_without_output_schema() {
        let result = json!({"content": [], "structuredContent": {"count": 2}});
        let output = map_call_result("srv", "t", &result, false).expect("mapped");
        assert_eq!(output.structured, Some(json!({"count": 2})));
        assert_eq!(output.metadata(), Some(json!({"structured": {"count": 2}})));
        assert_eq!(output.text(), r#"{"count":2}"#);
        let with_schema = map_call_result("srv", "t", &result, true).expect("mapped");
        assert_eq!(with_schema.text(), "");
    }

    #[test]
    fn is_error_becomes_typed_error_with_joined_text() {
        let result = json!({
            "content": [
                {"type": "text", "text": " first "},
                {"type": "image", "data": "aGk=", "mimeType": "image/png"},
                {"type": "text", "text": "second"}
            ],
            "isError": true
        });
        let error = map_call_result("srv", "t", &result, true).expect_err("error");
        assert!(matches!(
            error,
            McpError::ToolReturnedError { message, .. } if message == "first \nsecond"
        ));
        let empty = json!({"content": [], "isError": true});
        let error = map_call_result("srv", "t", &empty, true).expect_err("error");
        assert!(error.to_string().contains("MCP tool returned an error"));
    }

    #[test]
    fn truncates_long_text_at_char_boundary() {
        let long = "é".repeat(MAX_RESULT_TEXT_BYTES);
        let result = json!({"content": [{"type": "text", "text": long}]});
        let output = map_call_result("srv", "t", &result, true).expect("mapped");
        let text = output.text();
        assert!(text.len() <= MAX_RESULT_TEXT_BYTES + 64);
        assert!(text.contains("truncated"));
    }

    #[test]
    fn executes_stripping_expectation() {
        let mut arguments = Map::from_iter([
            ("q".to_owned(), json!("x")),
            ("expectation".to_owned(), json!({"why": "test"})),
        ]);
        let stripped = strip_expectation(&mut arguments);
        assert!(stripped.is_some());
        assert!(!arguments.contains_key("expectation"));
        assert_eq!(
            tool().schema()["properties"]["expectation"]["type"],
            "object"
        );
    }
}
