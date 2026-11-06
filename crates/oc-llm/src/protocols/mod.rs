//! Provider wire protocols: request construction and stream decoding.

pub(crate) mod anthropic;
pub(crate) mod gemini;
pub(crate) mod openai;

use serde_json::{Map, Value};

use crate::catalog::{Protocol, ResolvedModel};
use crate::error::LlmError;
use crate::event::{ContentPart, LlmEvent, LlmRequest, Usage};
use crate::sse::SseEvent;

/// A provider request body plus the path relative to its base URL.
#[derive(Debug)]
pub(crate) struct WireRequest {
    pub path: String,
    pub body: Value,
}

/// Build the native request for a protocol.
pub(crate) fn build(request: &LlmRequest, protocol: Protocol) -> Result<WireRequest, LlmError> {
    match protocol {
        Protocol::OpenAiChat => Ok(openai::request(request)),
        Protocol::AnthropicMessages => Ok(anthropic::request(request)),
        Protocol::Gemini => Ok(gemini::request(request)),
        Protocol::Unsupported => Err(LlmError::UnsupportedProtocol {
            provider: request.model.provider.id.clone(),
            model: request.model.model.id.clone(),
            protocol: "selected".to_string(),
        }),
    }
}

/// One protocol-specific streaming decoder.
#[derive(Debug)]
pub(crate) enum Decoder {
    OpenAi(openai::Decoder),
    Anthropic(anthropic::Decoder),
    Gemini(gemini::Decoder),
}

impl Decoder {
    pub(crate) fn new(protocol: Protocol) -> Self {
        match protocol {
            Protocol::OpenAiChat | Protocol::Unsupported => {
                Decoder::OpenAi(openai::Decoder::default())
            }
            Protocol::AnthropicMessages => Decoder::Anthropic(anthropic::Decoder::default()),
            Protocol::Gemini => Decoder::Gemini(gemini::Decoder::default()),
        }
    }

    pub(crate) fn event(&mut self, event: &SseEvent) -> Vec<LlmEvent> {
        match self {
            Decoder::OpenAi(decoder) => decoder.event(event),
            Decoder::Anthropic(decoder) => decoder.event(event),
            Decoder::Gemini(decoder) => decoder.event(event),
        }
    }

    pub(crate) fn finish(&mut self) -> Vec<LlmEvent> {
        match self {
            Decoder::OpenAi(decoder) => decoder.finish(),
            Decoder::Anthropic(decoder) => decoder.finish(),
            Decoder::Gemini(decoder) => decoder.finish(),
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        match self {
            Decoder::OpenAi(decoder) => decoder.done,
            Decoder::Anthropic(decoder) => decoder.done,
            Decoder::Gemini(decoder) => decoder.done,
        }
    }
}

/// A partially received tool call.
#[derive(Debug, Default)]
pub(crate) struct ToolAcc {
    pub id: String,
    pub name: String,
    pub args: String,
    pub started: bool,
}

impl ToolAcc {
    pub(crate) fn ready(&self) -> bool {
        !self.id.is_empty() && !self.name.is_empty()
    }

    /// Emit start (when identity is known), any accumulated argument prefix, and
    /// the final call. Used for protocols that may stream arguments.
    pub(crate) fn start_events(&mut self) -> Vec<LlmEvent> {
        if self.started || !self.ready() {
            return Vec::new();
        }
        self.started = true;
        let mut events = vec![LlmEvent::ToolInputStart {
            id: self.id.clone(),
            name: self.name.clone(),
        }];
        if !self.args.is_empty() {
            events.push(LlmEvent::ToolInputDelta {
                id: self.id.clone(),
                delta: self.args.clone(),
            });
        }
        events
    }

    pub(crate) fn close_events(mut self) -> Vec<LlmEvent> {
        let mut events = self.start_events();
        if !self.ready() {
            events.push(LlmEvent::ProviderError {
                message: "tool call stream ended without an id or name".to_string(),
            });
            return events;
        }
        let input = parse_tool_input(&self.args, &mut events);
        events.push(LlmEvent::ToolInputEnd {
            id: self.id.clone(),
            text: self.args.clone(),
        });
        events.push(LlmEvent::ToolCall {
            id: self.id,
            name: self.name,
            input,
        });
        events
    }
}

pub(crate) fn parse_tool_input(raw: &str, _events: &mut Vec<LlmEvent>) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Value::Object(Map::new());
    }
    if let Ok(value) = serde_json::from_str(trimmed) {
        return value;
    }
    if let Some(repaired) = repair_unquoted_expectation(trimmed)
        && let Ok(value) = serde_json::from_str(&repaired)
    {
        return value;
    }
    // Keep the raw payload as a string so the runtime tool dispatcher can
    // report an `InvalidToolArgumentsError` on the tool call itself instead of
    // aborting the entire session turn with a fatal `ProviderError`.
    Value::String(trimmed.to_string())
}

fn repair_unquoted_expectation(raw: &str) -> Option<String> {
    let key = "\"expectation\"";
    let key_pos = raw.find(key)?;
    let after_key = &raw[key_pos + key.len()..];
    let colon_offset = after_key.find(':')?;
    let value_start = key_pos + key.len() + colon_offset + 1;
    let rest = &raw[value_start..];
    let trimmed_rest = rest.trim_start();
    let ws_len = rest.len() - trimmed_rest.len();
    let first = trimmed_rest.chars().next()?;
    if matches!(first, '{' | '"' | '[') {
        return None;
    }
    let end_rel = trimmed_rest
        .find(", \"")
        .or_else(|| trimmed_rest.find(",\""))
        .or_else(|| trimmed_rest.rfind('}'))?;
    let unquoted = trimmed_rest[..end_rel].trim();
    let quoted = serde_json::to_string(unquoted).ok()?;
    let mut out = String::with_capacity(raw.len() + 4);
    out.push_str(&raw[..value_start + ws_len]);
    out.push_str(&quoted);
    out.push_str(&trimmed_rest[end_rel..]);
    Some(out)
}

pub(crate) fn data_url(media_type: &str, data: &str) -> String {
    format!("data:{media_type};base64,{data}")
}

pub(crate) fn text_of(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(ContentPart::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Deep-merge a JSON overlay over a base object.
pub(crate) fn merge_json(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(key) {
                    Some(existing) => merge_json(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

pub(crate) fn apply_body_overlays(body: &mut Value, model: &ResolvedModel) {
    let overlay = model.body();
    if !overlay.is_empty() {
        merge_json(body, &Value::Object(overlay));
    }
}

pub(crate) fn u64_at(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_u64()
}

pub(crate) fn json_error(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        return Some(message.to_string());
    }
    if let Some(message) = error.as_str() {
        return Some(message.to_string());
    }
    Some(error.to_string())
}

/// Normalize a raw provider finish reason.
pub(crate) fn normalize_finish(raw: Option<&str>, has_tool_calls: bool) -> String {
    match raw {
        Some("stop" | "end" | "end_turn" | "stop_sequence" | "pause_turn" | "STOP") | None => {
            if has_tool_calls {
                "tool-calls".to_string()
            } else {
                "stop".to_string()
            }
        }
        Some("length" | "max_tokens" | "model_context_window_exceeded" | "MAX_TOKENS") => {
            "length".to_string()
        }
        Some(
            "content_filter" | "refusal" | "SAFETY" | "RECITATION" | "BLOCKLIST"
            | "PROHIBITED_CONTENT" | "SPII",
        ) => "content-filter".to_string(),
        Some("function_call" | "tool_calls" | "tool_use") => "tool-calls".to_string(),
        Some("error" | "MALFORMED_RESPONSE" | "MALFORMED_FUNCTION_CALL") => "error".to_string(),
        Some(_) => "unknown".to_string(),
    }
}

pub(crate) fn finish_event(reason: String, usage: Usage) -> LlmEvent {
    LlmEvent::Finish { reason, usage }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_input_repairs_unquoted_expectation() {
        let mut events = Vec::new();
        let raw = r#"{"command": "git rev-parse HEAD", "expectation": why=Read git HEAD/origin/status; expect matching SHAs and clean tree, "expect_seconds": 15, "intervene_after_seconds": 30}"#;
        let value = parse_tool_input(raw, &mut events);
        assert!(events.is_empty());
        assert_eq!(value["command"], "git rev-parse HEAD");
        assert_eq!(
            value["expectation"],
            "why=Read git HEAD/origin/status; expect matching SHAs and clean tree"
        );
        assert_eq!(value["expect_seconds"], 15);
    }

    #[test]
    fn parse_tool_input_preserves_malformed_json_without_provider_error() {
        let mut events = Vec::new();
        let raw = r#"{"command": "git status""#;
        let value = parse_tool_input(raw, &mut events);
        assert!(
            events.is_empty(),
            "malformed tool input must not emit ProviderError"
        );
        assert_eq!(value, Value::String(raw.to_string()));
    }
}
