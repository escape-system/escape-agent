//! OpenAI-compatible `/chat/completions` protocol.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::{
    ToolAcc, WireRequest, apply_body_overlays, data_url, finish_event, json_error,
    normalize_finish, text_of, u64_at,
};
use crate::event::{ContentPart, LlmEvent, LlmMessage, LlmRequest, Role, Usage};
use crate::sse::SseEvent;

pub(crate) fn request(request: &LlmRequest) -> WireRequest {
    let model = &request.model;
    let settings = model.settings();
    let mut messages = Vec::new();
    for system in &request.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        lower_message(message, &mut messages);
    }

    let mut object = Map::new();
    object.insert("model".to_string(), json!(model.model_id()));
    object.insert("stream".to_string(), json!(true));
    object.insert("stream_options".to_string(), json!({"include_usage": true}));
    object.insert("messages".to_string(), Value::Array(messages));
    if !request.tools.is_empty() {
        object.insert(
            "tools".to_string(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema,
                            }
                        })
                    })
                    .collect(),
            ),
        );
        if let Some(choice) = &request.options.tool_choice {
            object.insert("tool_choice".to_string(), tool_choice(choice));
        }
    }
    if let Some(temperature) = request.options.temperature {
        object.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.options.top_p {
        object.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(max_tokens) = request.options.max_output_tokens {
        let field = model
            .model
            .compatibility
            .as_ref()
            .and_then(|compatibility| compatibility.max_tokens_field.as_deref())
            .unwrap_or("max_tokens");
        object.insert(field.to_string(), json!(max_tokens));
    }
    if let Some(effort) = settings.get("reasoningEffort").and_then(Value::as_str) {
        object.insert("reasoning_effort".to_string(), json!(effort));
    }
    if let Some(cache_key) = &request.options.cache_key
        && !cache_key.is_empty()
        && (model.provider.id == "openai"
            || settings
                .get("promptCacheKey")
                .and_then(Value::as_bool)
                .unwrap_or(false))
    {
        object.insert("prompt_cache_key".to_string(), json!(cache_key));
    }
    let mut body = Value::Object(object);
    apply_body_overlays(&mut body, model);
    WireRequest {
        path: "/chat/completions".to_string(),
        body,
    }
}

fn tool_choice(choice: &str) -> Value {
    match choice {
        "auto" | "none" | "required" => json!(choice),
        name => json!({"type": "function", "function": {"name": name}}),
    }
}

fn lower_message(message: &LlmMessage, output: &mut Vec<Value>) {
    match message.role {
        Role::System => output.push(json!({
            "role": "system",
            "content": text_of(&message.content),
        })),
        Role::User => {
            let mut parts = Vec::new();
            let mut only_text = true;
            for part in &message.content {
                match part {
                    ContentPart::Text { text } => parts.push(json!({"type": "text", "text": text})),
                    ContentPart::Image { media_type, data } => {
                        only_text = false;
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": {"url": data_url(media_type, data)},
                        }));
                    }
                    ContentPart::Document {
                        media_type,
                        data,
                        filename,
                    } => {
                        only_text = false;
                        parts.push(json!({
                            "type": "file",
                            "file": {
                                "filename": filename.clone().unwrap_or_else(|| "document.pdf".to_string()),
                                "file_data": data_url(media_type, data),
                            }
                        }));
                    }
                    ContentPart::ToolCall { .. } | ContentPart::ToolResult { .. } => {}
                }
            }
            let content = if only_text {
                Value::String(text_of(&message.content))
            } else {
                Value::Array(parts)
            };
            output.push(json!({"role": "user", "content": content}));
        }
        Role::Assistant => {
            let text = text_of(&message.content);
            let calls: Vec<Value> = message
                .content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::ToolCall { id, name, input } => Some(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": input.to_string(),
                        }
                    })),
                    _ => None,
                })
                .collect();
            let mut object = Map::new();
            object.insert("role".to_string(), json!("assistant"));
            object.insert(
                "content".to_string(),
                if text.is_empty() {
                    Value::Null
                } else {
                    Value::String(text)
                },
            );
            if !calls.is_empty() {
                object.insert("tool_calls".to_string(), Value::Array(calls));
            }
            output.push(Value::Object(object));
        }
        Role::Tool => {
            for part in &message.content {
                if let ContentPart::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } = part
                {
                    output.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_call_id,
                        "content": content,
                    }));
                }
            }
        }
    }
}

/// `OpenAI` Chat streaming state.
#[derive(Debug, Default)]
pub(crate) struct Decoder {
    text: String,
    text_started: bool,
    tools: BTreeMap<u64, ToolAcc>,
    tool_order: Vec<u64>,
    finish_reason: Option<String>,
    usage: Usage,
    pub(crate) done: bool,
}

impl Decoder {
    #[allow(
        clippy::too_many_lines,
        reason = "one state machine handles text, reasoning, and split tool deltas per SSE frame"
    )]
    pub(crate) fn event(&mut self, event: &SseEvent) -> Vec<LlmEvent> {
        if self.done {
            return Vec::new();
        }
        let data = event.data.trim();
        if data.is_empty() || data == "null" || data == ": keepalive" {
            return Vec::new();
        }
        if data == "[DONE]" {
            return self.finish();
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => {
                return vec![LlmEvent::ProviderError {
                    message: format!("invalid OpenAI stream event: {error}"),
                }];
            }
        };
        if let Some(message) = json_error(&value) {
            return vec![LlmEvent::ProviderError { message }];
        }
        if let Some(usage) = value.get("usage") {
            self.usage = map_usage(usage);
        }
        let mut events = Vec::new();
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return events;
        };
        if choice.get("usage").is_some_and(|usage| !usage.is_null()) {
            self.usage = map_usage(&choice["usage"]);
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return events;
        };

        if let Some(content) = delta.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            if !self.text_started {
                self.text_started = true;
                events.push(LlmEvent::TextStart);
            }
            self.text.push_str(content);
            events.push(LlmEvent::TextDelta {
                delta: content.to_string(),
            });
        }
        let reasoning = ["reasoning_content", "reasoning", "reasoning_text"]
            .iter()
            .find_map(|field| delta.get(*field).and_then(Value::as_str));
        if let Some(reasoning) = reasoning
            && !reasoning.is_empty()
        {
            events.push(LlmEvent::ReasoningDelta {
                delta: reasoning.to_string(),
            });
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(position as u64);
                if !self.tools.contains_key(&index) {
                    self.tool_order.push(index);
                }
                let tool = self.tools.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str)
                    && !id.is_empty()
                {
                    tool.id = id.to_string();
                }
                let function = call.get("function");
                if let Some(name) = function
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    && !name.is_empty()
                {
                    tool.name = name.to_string();
                }
                let fragment = function
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let was_started = tool.started;
                if !fragment.is_empty() {
                    tool.args.push_str(fragment);
                }
                if !was_started {
                    events.extend(tool.start_events());
                } else if !fragment.is_empty() {
                    events.push(LlmEvent::ToolInputDelta {
                        id: tool.id.clone(),
                        delta: fragment.to_string(),
                    });
                }
            }
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
        if self.text_started {
            events.push(LlmEvent::TextEnd {
                text: std::mem::take(&mut self.text),
            });
        }
        let order = std::mem::take(&mut self.tool_order);
        let has_tools = !self.tools.is_empty();
        for index in order {
            if let Some(tool) = self.tools.remove(&index) {
                events.extend(tool.close_events());
            }
        }
        events.push(finish_event(
            normalize_finish(self.finish_reason.as_deref(), has_tools),
            std::mem::take(&mut self.usage),
        ));
        events
    }
}

pub(crate) fn map_usage(usage: &Value) -> Usage {
    let input = u64_at(usage, &["prompt_tokens"]).or_else(|| u64_at(usage, &["input_tokens"]));
    let output =
        u64_at(usage, &["completion_tokens"]).or_else(|| u64_at(usage, &["output_tokens"]));
    let cache_read = u64_at(usage, &["prompt_tokens_details", "cached_tokens"])
        .or_else(|| u64_at(usage, &["prompt_cache_hit_tokens"]))
        .or_else(|| u64_at(usage, &["cached_tokens"]))
        .or_else(|| u64_at(usage, &["cache_read_input_tokens"]))
        .or_else(|| u64_at(usage, &["input_tokens_details", "cached_tokens"]));
    let cache_write = u64_at(usage, &["prompt_tokens_details", "cache_write_tokens"])
        .or_else(|| u64_at(usage, &["cache_created_input_tokens"]));
    let reasoning = u64_at(usage, &["completion_tokens_details", "reasoning_tokens"])
        .or_else(|| u64_at(usage, &["output_tokens_details", "reasoning_tokens"]));
    let non_cached =
        input.map(|input| input.saturating_sub(cache_read.unwrap_or(0) + cache_write.unwrap_or(0)));
    Usage {
        input_tokens: input,
        output_tokens: output,
        non_cached_input_tokens: non_cached,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: reasoning,
        total_tokens: u64_at(usage, &["total_tokens"])
            .or_else(|| input.zip(output).map(|(input, output)| input + output)),
        cost: usage.get("cost").and_then(Value::as_f64),
        requests: Some(1),
    }
}
