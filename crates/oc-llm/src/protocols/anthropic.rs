//! Anthropic Messages `/messages` protocol.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::{
    ToolAcc, WireRequest, apply_body_overlays, finish_event, normalize_finish, text_of, u64_at,
};
use crate::event::{ContentPart, LlmEvent, LlmMessage, LlmRequest, Role, Usage};
use crate::sse::SseEvent;

const DEFAULT_MAX_TOKENS: u64 = 32_000;
const MAX_CACHE_BREAKPOINTS: usize = 4;

fn ephemeral_cache_control() -> Value {
    json!({"type": "ephemeral"})
}

pub(crate) fn request(request: &LlmRequest) -> WireRequest {
    let model = &request.model;
    let settings = model.settings();
    let mut messages: Vec<Value> = Vec::new();
    for message in &request.messages {
        lower_message(message, &mut messages);
    }
    let max_tokens = request.options.max_output_tokens.unwrap_or_else(|| {
        let limit = model.model.limit.output;
        if limit == 0 {
            DEFAULT_MAX_TOKENS
        } else {
            limit.clamp(1, DEFAULT_MAX_TOKENS)
        }
    });
    let mut remaining_breakpoints = MAX_CACHE_BREAKPOINTS;
    let mut object = Map::new();
    object.insert("model".to_string(), json!(model.model_id()));
    object.insert("max_tokens".to_string(), json!(max_tokens));
    object.insert("stream".to_string(), json!(true));
    if !request.tools.is_empty() {
        let mut tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })
            })
            .collect();
        if remaining_breakpoints > 0
            && let Some(last_tool) = tools.last_mut().and_then(Value::as_object_mut)
        {
            last_tool.insert("cache_control".to_string(), ephemeral_cache_control());
            remaining_breakpoints -= 1;
        }
        object.insert("tools".to_string(), Value::Array(tools));
        if let Some(choice) = &request.options.tool_choice {
            object.insert("tool_choice".to_string(), tool_choice(choice));
        }
    }
    let mut system_blocks: Vec<Value> = request
        .system
        .iter()
        .filter(|part| !part.trim().is_empty())
        .map(|part| json!({"type": "text", "text": part}))
        .collect();
    if !system_blocks.is_empty() {
        if system_blocks.len() > 1
            && remaining_breakpoints > 2
            && let Some(first_block) = system_blocks.first_mut().and_then(Value::as_object_mut)
        {
            first_block.insert("cache_control".to_string(), ephemeral_cache_control());
            remaining_breakpoints -= 1;
        }
        if remaining_breakpoints > 0
            && let Some(last_block) = system_blocks.last_mut().and_then(Value::as_object_mut)
            && !last_block.contains_key("cache_control")
        {
            last_block.insert("cache_control".to_string(), ephemeral_cache_control());
            remaining_breakpoints -= 1;
        }
        object.insert("system".to_string(), Value::Array(system_blocks));
    }
    apply_message_cache_breakpoints(&mut messages, remaining_breakpoints);
    object.insert("messages".to_string(), Value::Array(messages));
    if let Some(temperature) = request.options.temperature {
        object.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.options.top_p {
        object.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(thinking) = settings.get("thinking") {
        object.insert("thinking".to_string(), anthropic_settings(thinking));
    }
    if let Some(effort) = settings.get("effort").and_then(Value::as_str) {
        object.insert("output_config".to_string(), json!({"effort": effort}));
    }
    let mut body = Value::Object(object);
    apply_body_overlays(&mut body, model);
    WireRequest {
        path: "/messages".to_string(),
        body,
    }
}

fn tool_choice(choice: &str) -> Value {
    match choice {
        "auto" => json!({"type": "auto"}),
        "none" => json!({"type": "none"}),
        "required" => json!({"type": "any"}),
        name => json!({"type": "tool", "name": name}),
    }
}

/// Place remaining `cache_control: {"type": "ephemeral"}` breakpoints on the
/// conversation tail:
/// 1. When at least 2 breakpoints remain, mark the previous `"user"` turn
///    boundary (which matches the prefix written on the immediately preceding
///    step/turn) so multi-step loops get an immediate cache hit.
/// 2. Mark the final message in `messages` so the next step/turn hits the newly
///    extended prefix.
fn apply_message_cache_breakpoints(messages: &mut [Value], mut remaining: usize) {
    if remaining == 0 || messages.is_empty() {
        return;
    }
    let last_index = messages.len() - 1;
    if remaining >= 2 && last_index > 0 {
        let prev_user = messages[..last_index]
            .iter()
            .rposition(|message| message.get("role").and_then(Value::as_str) == Some("user"));
        if let Some(index) = prev_user
            && mark_last_cacheable_block(&mut messages[index])
        {
            remaining -= 1;
        }
    }
    if remaining >= 1 {
        let _ = mark_last_cacheable_block(&mut messages[last_index]);
    }
}

fn mark_last_cacheable_block(message: &mut Value) -> bool {
    let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return false;
    };
    for block in content.iter_mut().rev() {
        let Some(object) = block.as_object_mut() else {
            continue;
        };
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let cacheable = match kind {
            "text" => object
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty()),
            "tool_use" | "tool_result" | "image" | "document" => true,
            _ => false,
        };
        if cacheable {
            object
                .entry("cache_control".to_string())
                .or_insert_with(ephemeral_cache_control);
            return true;
        }
    }
    false
}

fn anthropic_settings(thinking: &Value) -> Value {
    let Some(object) = thinking.as_object() else {
        return thinking.clone();
    };
    let mut output = Map::new();
    for (key, value) in object {
        let key = match key.as_str() {
            "budgetTokens" => "budget_tokens",
            "display" => "display",
            "type" => "type",
            _ => key.as_str(),
        };
        output.insert(key.to_string(), value.clone());
    }
    Value::Object(output)
}

fn lower_message(message: &LlmMessage, output: &mut Vec<Value>) {
    match message.role {
        Role::System => {
            // System content outside `system` is folded into the first user turn;
            // the runtime always supplies system separately.
            output.push(json!({"role": "user", "content": [{"type": "text", "text": text_of(&message.content)}]}));
        }
        Role::User => {
            let content = lower_content(&message.content);
            let has_media_only = message.content.iter().all(|part| {
                matches!(
                    part,
                    ContentPart::Image { .. } | ContentPart::Document { .. }
                )
            });
            if has_media_only
                && let Some(last) = output.last_mut()
                && last.get("role").and_then(Value::as_str) == Some("user")
                && last
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_result")
                        })
                    })
                && let Some(blocks) = last.get_mut("content").and_then(Value::as_array_mut)
            {
                blocks.extend(content);
                return;
            }
            output.push(json!({"role": "user", "content": content}));
        }
        Role::Assistant => {
            let content = lower_content(&message.content);
            output.push(json!({"role": "assistant", "content": content}));
        }
        Role::Tool => {
            let mut blocks = Vec::new();
            for part in &message.content {
                if let ContentPart::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                    ..
                } = part
                {
                    let mut block = Map::new();
                    block.insert("type".to_string(), json!("tool_result"));
                    block.insert("tool_use_id".to_string(), json!(tool_call_id));
                    block.insert("content".to_string(), json!(content));
                    if *is_error {
                        block.insert("is_error".to_string(), json!(true));
                    }
                    blocks.push(Value::Object(block));
                }
            }
            // Consecutive tool results belong to one user message.
            if let Some(last) = output.last_mut()
                && last.get("role").and_then(Value::as_str) == Some("user")
                && last
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|content| {
                        content.iter().any(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_result")
                        })
                    })
                && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
            {
                content.extend(blocks);
                return;
            }
            output.push(json!({"role": "user", "content": blocks}));
        }
    }
}

fn lower_content(content: &[ContentPart]) -> Vec<Value> {
    let mut blocks = Vec::new();
    for part in content {
        match part {
            ContentPart::Text { text } => blocks.push(json!({"type": "text", "text": text})),
            ContentPart::Image { media_type, data } => blocks.push(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data},
            })),
            ContentPart::Document {
                media_type,
                data,
                filename,
            } => {
                let mut source = Map::new();
                source.insert("type".to_string(), json!("base64"));
                source.insert("media_type".to_string(), json!(media_type));
                source.insert("data".to_string(), json!(data));
                let mut block = Map::new();
                block.insert("type".to_string(), json!("document"));
                block.insert("source".to_string(), Value::Object(source));
                if let Some(filename) = filename {
                    block.insert("title".to_string(), json!(filename));
                }
                blocks.push(Value::Object(block));
            }
            ContentPart::ToolCall { id, name, input } => {
                let input = if input.is_object() {
                    input.clone()
                } else {
                    json!({"input": input})
                };
                blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                }));
            }
            ContentPart::ToolResult {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
                let mut block = Map::new();
                block.insert("type".to_string(), json!("tool_result"));
                block.insert("tool_use_id".to_string(), json!(tool_call_id));
                block.insert("content".to_string(), json!(content));
                if *is_error {
                    block.insert("is_error".to_string(), json!(true));
                }
                blocks.push(Value::Object(block));
            }
        }
    }
    blocks
}

#[derive(Debug, Default)]
struct TextBlock {
    text: String,
    started: bool,
}

/// Anthropic Messages streaming state.
#[derive(Debug, Default)]
pub(crate) struct Decoder {
    text: BTreeMap<u64, TextBlock>,
    tools: BTreeMap<u64, ToolAcc>,
    tool_calls: u64,
    usage: Usage,
    finish_reason: Option<String>,
    pub(crate) done: bool,
}

impl Decoder {
    #[allow(
        clippy::too_many_lines,
        reason = "Anthropic content-block lifecycle is one state machine"
    )]
    pub(crate) fn event(&mut self, event: &SseEvent) -> Vec<LlmEvent> {
        if self.done {
            return Vec::new();
        }
        let data = event.data.trim();
        if data.is_empty() || data == "null" || data == "[DONE]" || data == ": keepalive" {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => {
                return vec![LlmEvent::ProviderError {
                    message: format!("invalid Anthropic stream event: {error}"),
                }];
            }
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .unwrap_or_default();
        let mut events = Vec::new();
        match kind {
            "message_start" => {
                if let Some(usage) = value
                    .get("message")
                    .and_then(|message| message.get("usage"))
                {
                    self.merge_usage(usage);
                }
            }
            "content_block_start" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                let Some(block) = value.get("content_block") else {
                    return events;
                };
                match block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text" => {
                        let block_entry = self.text.entry(index).or_default();
                        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                        if !block_entry.started {
                            block_entry.started = true;
                            events.push(LlmEvent::TextStart);
                        }
                        if !text.is_empty() {
                            block_entry.text.push_str(text);
                            events.push(LlmEvent::TextDelta {
                                delta: text.to_string(),
                            });
                        }
                    }
                    "tool_use" | "server_tool_use" => {
                        let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        let mut tool = ToolAcc {
                            id: id.to_string(),
                            name: name.to_string(),
                            args: String::new(),
                            started: false,
                        };
                        if let Some(input) = block.get("input")
                            && !input.is_null()
                            && (!input.is_object()
                                || input.as_object().is_some_and(|input| !input.is_empty()))
                        {
                            tool.args = input.to_string();
                        }
                        events.extend(tool.start_events());
                        self.tools.insert(index, tool);
                    }
                    "thinking" => {
                        if let Some(text) = block.get("thinking").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            events.push(LlmEvent::ReasoningDelta {
                                delta: text.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                let Some(delta) = value.get("delta") else {
                    return events;
                };
                match delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text_delta" => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if !text.is_empty() {
                            let block = self.text.entry(index).or_default();
                            if !block.started {
                                block.started = true;
                                events.push(LlmEvent::TextStart);
                            }
                            block.text.push_str(text);
                            events.push(LlmEvent::TextDelta {
                                delta: text.to_string(),
                            });
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            events.push(LlmEvent::ReasoningDelta {
                                delta: text.to_string(),
                            });
                        }
                    }
                    "input_json_delta" => {
                        let fragment = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !fragment.is_empty()
                            && let Some(tool) = self.tools.get_mut(&index)
                        {
                            tool.args.push_str(fragment);
                            if tool.started {
                                events.push(LlmEvent::ToolInputDelta {
                                    id: tool.id.clone(),
                                    delta: fragment.to_string(),
                                });
                            } else {
                                events.extend(tool.start_events());
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(block) = self.text.remove(&index)
                    && block.started
                {
                    events.push(LlmEvent::TextEnd { text: block.text });
                }
                if let Some(tool) = self.tools.remove(&index) {
                    if tool.ready() {
                        self.tool_calls += 1;
                    }
                    events.extend(tool.close_events());
                }
            }
            "message_delta" => {
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.finish_reason = Some(reason.to_string());
                }
                if let Some(usage) = value.get("usage") {
                    self.merge_usage(usage);
                }
            }
            "message_stop" => return self.finish(),
            "error" => {
                events.push(LlmEvent::ProviderError {
                    message: error_message(&value),
                });
            }
            _ => {}
        }
        events
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
        let text = std::mem::take(&mut self.text);
        for (_, block) in text {
            if block.started {
                events.push(LlmEvent::TextEnd { text: block.text });
            }
        }
        let tools = std::mem::take(&mut self.tools);
        let had_open_tools = !tools.is_empty();
        for (_, tool) in tools {
            if tool.ready() {
                self.tool_calls += 1;
            }
            events.extend(tool.close_events());
        }
        let has_tools = self.tool_calls > 0 || had_open_tools;
        events.push(finish_event(
            normalize_finish(self.finish_reason.as_deref(), has_tools),
            std::mem::take(&mut self.usage),
        ));
        events
    }

    fn merge_usage(&mut self, usage: &Value) {
        let non_cached = u64_at(usage, &["input_tokens"]);
        let cache_read = u64_at(usage, &["cache_read_input_tokens"]);
        let cache_write = u64_at(usage, &["cache_creation_input_tokens"]);
        let output = u64_at(usage, &["output_tokens"]);
        let reasoning = u64_at(usage, &["output_tokens_details", "thinking_tokens"]);
        if non_cached.is_some() {
            self.usage.non_cached_input_tokens = non_cached;
        }
        if cache_read.is_some() {
            self.usage.cache_read_input_tokens = cache_read;
        }
        if cache_write.is_some() {
            self.usage.cache_write_input_tokens = cache_write;
        }
        if output.is_some() {
            self.usage.output_tokens = output;
        }
        if reasoning.is_some() {
            self.usage.reasoning_tokens = reasoning;
        }
        let input = self.usage.non_cached_input_tokens.unwrap_or(0)
            + self.usage.cache_read_input_tokens.unwrap_or(0)
            + self.usage.cache_write_input_tokens.unwrap_or(0);
        self.usage.input_tokens = Some(input);
        self.usage.total_tokens = Some(input + self.usage.output_tokens.unwrap_or(0));
        self.usage.requests = Some(1);
    }
}

fn error_message(value: &Value) -> String {
    if let Some(error) = value.get("error") {
        let kind = error.get("type").and_then(Value::as_str);
        let message = error.get("message").and_then(Value::as_str);
        return match (kind, message) {
            (Some(kind), Some(message)) => format!("{kind}: {message}"),
            (_, Some(message)) => message.to_string(),
            (Some(kind), None) => kind.to_string(),
            (None, None) => error.to_string(),
        };
    }
    "Anthropic stream error".to_string()
}
