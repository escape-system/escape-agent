//! Gemini `:streamGenerateContent` protocol.

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Map, Value, json};

use super::{WireRequest, apply_body_overlays, finish_event, json_error, normalize_finish, u64_at};
use crate::event::{ContentPart, LlmEvent, LlmMessage, LlmRequest, Role, Usage};
use crate::sse::SseEvent;

pub(crate) fn request(request: &LlmRequest) -> WireRequest {
    let model = &request.model;
    let settings = model.settings();
    let mut contents: Vec<Value> = Vec::new();
    let mut tool_names: Map<String, Value> = Map::new();
    for message in &request.messages {
        for part in &message.content {
            if let ContentPart::ToolCall { id, name, .. } = part {
                tool_names.insert(id.clone(), json!(name));
            }
        }
    }
    for message in &request.messages {
        lower_message(message, &tool_names, &mut contents);
    }

    let mut generation = Map::new();
    if let Some(max_tokens) = request.options.max_output_tokens {
        generation.insert("maxOutputTokens".to_string(), json!(max_tokens));
    }
    if let Some(temperature) = request.options.temperature {
        generation.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.options.top_p {
        generation.insert("topP".to_string(), json!(top_p));
    }
    if let Some(thinking) = settings.get("thinkingConfig") {
        generation.insert("thinkingConfig".to_string(), thinking.clone());
    }

    let mut body = Map::new();
    body.insert("contents".to_string(), Value::Array(contents));
    if !request.system.is_empty() {
        body.insert(
            "systemInstruction".to_string(),
            json!({"parts": request.system.iter().map(|text| json!({"text": text})).collect::<Vec<_>>()}),
        );
    }
    if !request.tools.is_empty() {
        body.insert(
            "tools".to_string(),
            json!([{
                "functionDeclarations": request.tools.iter().map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parametersJsonSchema": tool.input_schema,
                })).collect::<Vec<_>>(),
            }]),
        );
        if let Some(choice) = &request.options.tool_choice {
            body.insert("toolConfig".to_string(), tool_config(choice));
        }
    }
    if !generation.is_empty() {
        body.insert("generationConfig".to_string(), Value::Object(generation));
    }
    let mut body = Value::Object(body);
    apply_body_overlays(&mut body, model);

    let id = utf8_percent_encode(model.model_id(), NON_ALPHANUMERIC);
    WireRequest {
        path: format!("/models/{id}:streamGenerateContent?alt=sse"),
        body,
    }
}

fn tool_config(choice: &str) -> Value {
    match choice {
        "none" => json!({"functionCallingConfig": {"mode": "NONE"}}),
        "required" => json!({"functionCallingConfig": {"mode": "ANY"}}),
        "auto" => json!({"functionCallingConfig": {"mode": "AUTO"}}),
        name => json!({
            "functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": [name]}
        }),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one lowering table maps every role and content part to Gemini contents"
)]
fn lower_message(message: &LlmMessage, tool_names: &Map<String, Value>, output: &mut Vec<Value>) {
    match message.role {
        Role::System => {
            let text = message
                .content
                .iter()
                .filter_map(ContentPart::as_text)
                .collect::<Vec<_>>()
                .join("\n");
            output.push(json!({"role": "user", "parts": [{"text": text}]}));
        }
        Role::User => {
            let mut parts = Vec::new();
            for part in &message.content {
                match part {
                    ContentPart::Text { text } => parts.push(json!({"text": text})),
                    ContentPart::Image { media_type, data }
                    | ContentPart::Document {
                        media_type, data, ..
                    } => parts.push(json!({
                        "inlineData": {"mimeType": media_type, "data": data}
                    })),
                    ContentPart::ToolResult { .. } | ContentPart::ToolCall { .. } => {}
                }
            }
            output.push(json!({"role": "user", "parts": parts}));
        }
        Role::Assistant => {
            let mut parts = Vec::new();
            for part in &message.content {
                match part {
                    ContentPart::Text { text } => parts.push(json!({"text": text})),
                    ContentPart::Image { media_type, data }
                    | ContentPart::Document {
                        media_type, data, ..
                    } => parts.push(json!({
                        "inlineData": {"mimeType": media_type, "data": data}
                    })),
                    ContentPart::ToolCall { name, input, id } => {
                        let mut call = Map::new();
                        call.insert("name".to_string(), json!(name));
                        call.insert("args".to_string(), input.clone());
                        if !id.is_empty() {
                            call.insert("id".to_string(), json!(id));
                        }
                        parts.push(json!({"functionCall": call}));
                    }
                    ContentPart::ToolResult { .. } => {}
                }
            }
            output.push(json!({"role": "model", "parts": parts}));
        }
        Role::Tool => {
            let mut new_parts = Vec::new();
            for part in &message.content {
                if let ContentPart::ToolResult {
                    tool_call_id,
                    name,
                    content,
                    ..
                } = part
                {
                    let name = name
                        .clone()
                        .or_else(|| {
                            tool_names
                                .get(tool_call_id)
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "tool".to_string());
                    new_parts.push(json!({
                        "functionResponse": {
                            "name": name,
                            "response": {"name": name, "content": content},
                        }
                    }));
                }
            }
            // Gemini requires all responses to a parallel batch in one user turn.
            if let Some(last) = output.last_mut()
                && last.get("role").and_then(Value::as_str) == Some("user")
                && last
                    .get("parts")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|part| part.get("functionResponse").is_some())
                    })
                && let Some(parts) = last.get_mut("parts").and_then(Value::as_array_mut)
            {
                parts.extend(new_parts);
                return;
            }
            output.push(json!({"role": "user", "parts": new_parts}));
        }
    }
}

/// Gemini streaming state.
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
    #[allow(
        clippy::too_many_lines,
        reason = "Gemini parts are decoded in one per-chunk state machine"
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
                    message: format!("invalid Gemini stream event: {error}"),
                }];
            }
        };
        if let Some(message) = json_error(&value) {
            return vec![LlmEvent::ProviderError { message }];
        }
        if let Some(usage) = value.get("usageMetadata") {
            self.usage = map_usage(usage);
        }
        let mut events = Vec::new();
        let Some(candidate) = value
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
        else {
            return events;
        };
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        else {
            return events;
        };
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if text.is_empty() {
                    continue;
                }
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    events.push(LlmEvent::ReasoningDelta {
                        delta: text.to_string(),
                    });
                } else {
                    if !self.text_started {
                        self.text_started = true;
                        events.push(LlmEvent::TextStart);
                    }
                    self.text.push_str(text);
                    events.push(LlmEvent::TextDelta {
                        delta: text.to_string(),
                    });
                }
                continue;
            }
            if let Some(call) = part.get("functionCall") {
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map_or_else(
                        || {
                            self.generated_tools += 1;
                            format!("tool_{}", self.generated_tools)
                        },
                        str::to_string,
                    );
                let input = call.get("args").cloned().unwrap_or_else(|| json!({}));
                let args = input.to_string();
                events.push(LlmEvent::ToolInputStart {
                    id: id.clone(),
                    name: name.clone(),
                });
                if !args.is_empty() && args != "{}" {
                    events.push(LlmEvent::ToolInputDelta {
                        id: id.clone(),
                        delta: args.clone(),
                    });
                }
                events.push(LlmEvent::ToolInputEnd {
                    id: id.clone(),
                    text: args,
                });
                events.push(LlmEvent::ToolCall { id, name, input });
                self.tool_calls += 1;
            }
        }
        events
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

pub(crate) fn map_usage(usage: &Value) -> Usage {
    let input = u64_at(usage, &["promptTokenCount"]);
    let cached = u64_at(usage, &["cachedContentTokenCount"]);
    let thoughts = u64_at(usage, &["thoughtsTokenCount"]);
    let visible = u64_at(usage, &["candidatesTokenCount"]);
    let output = visible.map(|visible| visible + thoughts.unwrap_or(0));
    Usage {
        input_tokens: input,
        output_tokens: output,
        non_cached_input_tokens: input.map(|input| input.saturating_sub(cached.unwrap_or(0))),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: thoughts,
        total_tokens: u64_at(usage, &["totalTokenCount"]),
        cost: usage.get("cost").and_then(Value::as_f64),
        requests: Some(1),
    }
}
