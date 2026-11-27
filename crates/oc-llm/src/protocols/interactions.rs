//! Gemini Interactions API (`POST /interactions`) for official Paid Tier
//! access to the managed Antigravity agent (`antigravity-preview-09-2026`).
//!
//! Sends `store: false` by default when `LlmOptions::store` is `None` or
//! `Some(false)` so confidential codebases opt out of standard content logging
//! (`docs/30-tos-safe-harness.md`).

use serde_json::{Map, Value, json};

use super::gemini::map_usage;
use super::{
    WireRequest, apply_body_overlays, finish_event, json_error, normalize_finish, text_of,
};
use crate::event::{ContentPart, LlmEvent, LlmMessage, LlmRequest, Role, Usage};
use crate::sse::SseEvent;

pub(crate) fn request(request: &LlmRequest) -> WireRequest {
    let model = &request.model;
    let mut input: Vec<Value> = Vec::new();
    for message in &request.messages {
        lower_interaction_message(message, &mut input);
    }

    let mut body = Map::new();
    body.insert("model".to_owned(), json!(model.model_id()));
    body.insert("input".to_owned(), Value::Array(input));
    body.insert("stream".to_owned(), json!(true));
    // Privacy default: opt out of 55-day API content storage unless explicitly requested.
    body.insert(
        "store".to_owned(),
        json!(request.options.store.unwrap_or(false)),
    );

    if !request.system.is_empty() {
        body.insert(
            "system_instruction".to_owned(),
            json!(request.system.join("\n")),
        );
    }
    if !request.tools.is_empty() {
        body.insert(
            "tools".to_owned(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema,
                        })
                    })
                    .collect(),
            ),
        );
    }
    let mut generation = Map::new();
    if let Some(max_tokens) = request.options.max_output_tokens {
        generation.insert("max_output_tokens".to_owned(), json!(max_tokens));
    }
    if let Some(temperature) = request.options.temperature {
        generation.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = request.options.top_p {
        generation.insert("top_p".to_owned(), json!(top_p));
    }
    if let Some(thinking) = model.settings().get("thinkingConfig") {
        let clamped =
            super::clamp_thinking_config(thinking, request.options.thinking_cap.as_deref());
        generation.insert("thinking_config".to_owned(), clamped);
    }
    if !generation.is_empty() {
        body.insert("generation_config".to_owned(), Value::Object(generation));
    }

    let mut body = Value::Object(body);
    apply_body_overlays(&mut body, model);

    WireRequest {
        path: "/interactions".to_owned(),
        body,
    }
}

fn lower_interaction_message(message: &LlmMessage, out: &mut Vec<Value>) {
    match message.role {
        Role::System => {
            out.push(json!({"role": "user", "content": [{"type": "text", "text": text_of(&message.content)}]}));
        }
        Role::User => {
            let mut parts = Vec::new();
            for part in &message.content {
                match part {
                    ContentPart::Text { text } => parts.push(json!({"type": "text", "text": text})),
                    ContentPart::Image { media_type, data } => parts.push(json!({
                        "type": "image",
                        "data": data,
                        "mime_type": media_type,
                    })),
                    ContentPart::Document {
                        media_type, data, ..
                    } => parts.push(json!({
                        "type": "document",
                        "data": data,
                        "mime_type": media_type,
                    })),
                    ContentPart::ToolCall { .. } | ContentPart::ToolResult { .. } => {}
                }
            }
            out.push(json!({"role": "user", "content": parts}));
        }
        Role::Assistant => {
            let mut parts = Vec::new();
            for part in &message.content {
                match part {
                    ContentPart::Text { text } => parts.push(json!({"type": "text", "text": text})),
                    ContentPart::ToolCall { id, name, input } => parts.push(json!({
                        "type": "function_call",
                        "id": id,
                        "name": name,
                        "arguments": input,
                    })),
                    ContentPart::Image { .. }
                    | ContentPart::Document { .. }
                    | ContentPart::ToolResult { .. } => {}
                }
            }
            out.push(json!({"role": "model", "content": parts}));
        }
        Role::Tool => {
            let mut parts = Vec::new();
            for part in &message.content {
                if let ContentPart::ToolResult {
                    tool_call_id,
                    name,
                    content,
                    is_error,
                } = part
                {
                    parts.push(json!({
                        "type": "function_result",
                        "call_id": tool_call_id,
                        "name": name,
                        "result": content,
                        "is_error": is_error,
                    }));
                }
            }
            out.push(json!({"role": "user", "content": parts}));
        }
    }
}

/// Interactions API SSE stream state.
#[derive(Debug, Default)]
pub(crate) struct Decoder {
    text: String,
    text_started: bool,
    finish_reason: Option<String>,
    usage: Usage,
    tool_calls: u64,
    generated_tools: u64,
    pub(crate) done: bool,
}

impl Decoder {
    pub(crate) fn event(&mut self, event: &SseEvent) -> Vec<LlmEvent> {
        if self.done {
            return Vec::new();
        }
        let data = event.data.trim();
        if data.is_empty() || data == "null" || data == "[DONE]" || data == ": keepalive" {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(error) => {
                return vec![LlmEvent::ProviderError {
                    message: format!("invalid Gemini Interactions stream event: {error}"),
                }];
            }
        };
        if let Some(message) = json_error(&value) {
            return vec![LlmEvent::ProviderError { message }];
        }
        if let Some(usage) = value
            .get("usageMetadata")
            .or_else(|| value.get("usage"))
            .or_else(|| value.pointer("/interaction/usage"))
        {
            self.usage = map_usage(usage);
        }
        if let Some(reason) = value
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
        {
            self.done = true;
            return vec![LlmEvent::ProviderError {
                message: format!(
                    "policy_blocked: prompt blocked by Gemini safety filter ({reason})"
                ),
            }];
        }

        let event_type = value
            .get("event_type")
            .or_else(|| value.get("type"))
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .unwrap_or_default();

        let mut out = Vec::new();
        let delta = value.get("delta").unwrap_or(&value);
        let kind = delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if (event_type == "content.delta"
            || event_type == "step.delta"
            || kind == "text"
            || kind == "thought")
            && let Some(text) = delta.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            self.push_text_delta(delta, kind, text, &mut out);
        } else if kind == "function_call" || delta.get("functionCall").is_some() {
            self.push_tool_call(delta.get("functionCall").unwrap_or(delta), &mut out);
        }

        if let Some(reason) = value
            .get("finish_reason")
            .or_else(|| value.get("finishReason"))
            .or_else(|| value.pointer("/interaction/status"))
            .and_then(Value::as_str)
        {
            if matches!(
                reason,
                "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII"
            ) {
                self.done = true;
                return vec![LlmEvent::ProviderError {
                    message: format!("policy_blocked: candidate blocked ({reason})"),
                }];
            }
            self.finish_reason = Some(match reason {
                "completed" => "stop".to_owned(),
                other => other.to_owned(),
            });
        }
        if matches!(
            event_type,
            "interaction.complete" | "interaction.completed" | "done"
        ) {
            out.extend(self.finish());
        }
        out
    }

    fn push_text_delta(&mut self, delta: &Value, kind: &str, text: &str, out: &mut Vec<LlmEvent>) {
        if kind == "thought" || delta.get("thought").and_then(Value::as_bool) == Some(true) {
            out.push(LlmEvent::ReasoningDelta {
                delta: text.to_owned(),
            });
            return;
        }
        if !self.text_started {
            self.text_started = true;
            out.push(LlmEvent::TextStart);
        }
        self.text.push_str(text);
        out.push(LlmEvent::TextDelta {
            delta: text.to_owned(),
        });
    }

    fn push_tool_call(&mut self, call: &Value, out: &mut Vec<LlmEvent>) {
        let name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_owned();
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map_or_else(
                || {
                    self.generated_tools += 1;
                    format!("tool_{}", self.generated_tools)
                },
                str::to_owned,
            );
        let input = call
            .get("arguments")
            .or_else(|| call.get("args"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let args = input.to_string();
        out.push(LlmEvent::ToolInputStart {
            id: id.clone(),
            name: name.clone(),
        });
        if !args.is_empty() && args != "{}" {
            out.push(LlmEvent::ToolInputDelta {
                id: id.clone(),
                delta: args.clone(),
            });
        }
        out.push(LlmEvent::ToolInputEnd {
            id: id.clone(),
            text: args,
        });
        out.push(LlmEvent::ToolCall { id, name, input });
        self.tool_calls += 1;
    }

    pub(crate) fn had_completion(&self) -> bool {
        self.done || self.finish_reason.is_some()
    }

    pub(crate) fn finish(&mut self) -> Vec<LlmEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut events = Vec::new();
        if self.text_started {
            events.push(LlmEvent::TextEnd {
                text: std::mem::take(&mut self.text),
            });
        }
        events.push(finish_event(
            normalize_finish(self.finish_reason.as_deref(), self.tool_calls > 0),
            std::mem::take(&mut self.usage),
        ));
        events
    }
}
