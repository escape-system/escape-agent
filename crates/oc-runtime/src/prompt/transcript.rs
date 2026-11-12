use oc_llm::{ContentPart, LlmMessage, Role};
use oc_types::message::{AssistantPart, PromptFileAttachment, ToolContent, ToolState, UserMessage};
use serde_json::Value;

/// Convert stored wire messages into provider messages.
///
/// The last completed compaction is the context boundary: its summary becomes
/// the first user message and everything before it is dropped, except the
/// retained tail named by `keepID`. Assistant tool calls are replayed with
/// their results so the model sees why a tool ran.
#[must_use]
pub fn messages_from_store(messages: &[Value]) -> Vec<LlmMessage> {
    let boundary = messages.iter().rposition(|message| {
        message_type(message) == Some("compaction") && compaction_completed(message)
    });
    let mut out = Vec::new();
    let mut start = 0;
    if let Some(index) = boundary {
        if let Some(summary) = messages[index].get("summary").and_then(Value::as_str) {
            out.push(LlmMessage::user(summary));
        }
        if let Some(keep_id) = messages[index].get("keepID").and_then(Value::as_str)
            && let Some(keep) = messages[..index]
                .iter()
                .position(|message| message.get("id").and_then(Value::as_str) == Some(keep_id))
        {
            push_messages(&messages[keep..index], &mut out);
        }
        start = index + 1;
    }
    push_messages(&messages[start..], &mut out);
    out
}

fn push_messages(messages: &[Value], out: &mut Vec<LlmMessage>) {
    for message in messages {
        match message_type(message) {
            Some("user") => {
                out.push(LlmMessage::new(Role::User, user_parts(message)));
            }
            Some("synthetic" | "system" | "skill") => {
                if let Some(text) = message.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    out.push(LlmMessage::user(text.to_owned()));
                }
            }
            Some("peer") => {
                let text = message
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    let from = message.get("from").and_then(Value::as_str).unwrap_or("?");
                    let agent = message.get("agent").and_then(Value::as_str).unwrap_or("?");
                    let kind = message
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("note");
                    out.push(LlmMessage::user(format!(
                        "<peer from=\"{from}\" agent=\"{agent}\" kind=\"{kind}\">{text}</peer>"
                    )));
                }
            }
            Some("assistant") => assistant_messages(message, out),
            Some(_) | None => {}
        }
    }
}

fn user_parts(message: &Value) -> Vec<ContentPart> {
    let mut parts: Vec<ContentPart> = Vec::new();
    if let Ok(user) = serde_json::from_value::<UserMessage>(message.clone()) {
        if !user.text.is_empty() {
            parts.push(ContentPart::text(user.text));
        }
        if let Some(files) = user.files {
            for file in files {
                parts.extend(attachment_parts(&file));
            }
        }
    } else if let Some(text) = message.get("text").and_then(Value::as_str)
        && !text.is_empty()
    {
        parts.push(ContentPart::text(text.to_owned()));
    }
    if parts.is_empty() {
        parts.push(ContentPart::text(String::new()));
    }
    parts
}

fn attachment_parts(file: &PromptFileAttachment) -> Vec<ContentPart> {
    let data = strip_data_url(&file.data);
    if file.mime.starts_with("image/") {
        return vec![ContentPart::image(file.mime.clone(), data)];
    }
    if file.mime == "application/pdf" {
        return vec![ContentPart::document(
            file.mime.clone(),
            data,
            file.name.clone(),
        )];
    }
    let name = file.name.as_deref().unwrap_or("attachment");
    vec![ContentPart::text(format!(
        "[attached file: {name} ({}), {} bytes]",
        file.mime,
        data.len()
    ))]
}

fn strip_data_url(data: &str) -> String {
    if let Some((_, rest)) = data.split_once(',')
        && data.starts_with("data:")
    {
        return rest.to_owned();
    }
    data.to_owned()
}

fn assistant_messages(message: &Value, out: &mut Vec<LlmMessage>) {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    let Ok(parts) = serde_json::from_value::<Vec<AssistantPart>>(Value::Array(content.clone()))
    else {
        return;
    };
    let mut calls: Vec<ContentPart> = Vec::new();
    let mut results: Vec<LlmMessage> = Vec::new();
    let mut attachments: Vec<ContentPart> = Vec::new();
    for part in parts {
        match part {
            AssistantPart::Text(text) => {
                if !text.text.is_empty() {
                    calls.push(ContentPart::text(text.text));
                }
            }
            AssistantPart::Reasoning(_) => {}
            AssistantPart::Tool(tool) => match &tool.state {
                ToolState::Completed { input, content, .. } => {
                    calls.push(ContentPart::tool_call(
                        tool.id.clone(),
                        tool.name.clone(),
                        Value::Object(input.clone()),
                    ));
                    results.push(LlmMessage::tool_result(
                        tool.id.clone(),
                        Some(tool.name.clone()),
                        join_content(content),
                        false,
                    ));
                    attachments.extend(tool_attachment_parts(content));
                }
                ToolState::Error {
                    input,
                    error,
                    content,
                    ..
                } => {
                    calls.push(ContentPart::tool_call(
                        tool.id.clone(),
                        tool.name.clone(),
                        Value::Object(input.clone()),
                    ));
                    let text = content
                        .as_ref()
                        .map(|content| join_content(content))
                        .filter(|text| !text.is_empty())
                        .unwrap_or_else(|| error.message.clone());
                    results.push(LlmMessage::tool_result(
                        tool.id.clone(),
                        Some(tool.name.clone()),
                        text,
                        true,
                    ));
                }
                ToolState::Running { input, .. } => {
                    calls.push(ContentPart::tool_call(
                        tool.id.clone(),
                        tool.name.clone(),
                        Value::Object(input.clone()),
                    ));
                    results.push(LlmMessage::tool_result(
                        tool.id.clone(),
                        Some(tool.name.clone()),
                        "Tool execution is still running.",
                        false,
                    ));
                }
                ToolState::Streaming { .. } => {}
            },
        }
    }
    if !calls.is_empty() {
        out.push(LlmMessage::new(Role::Assistant, calls));
    }
    out.extend(results);
    if !attachments.is_empty() {
        out.push(LlmMessage::new(Role::User, attachments));
    }
}

fn join_content(content: &[ToolContent]) -> String {
    content
        .iter()
        .map(|part| match part {
            ToolContent::Text { text } => text.clone(),
            ToolContent::File { uri, mime, name } => {
                format_tool_file_summary(uri, mime, name.as_deref())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_tool_file_summary(uri: &str, mime: &str, name: Option<&str>) -> String {
    if uri.starts_with("data:") {
        let payload = strip_data_url(uri);
        let approx_bytes = payload.len() * 3 / 4;
        match name.filter(|name| !name.is_empty()) {
            Some(name) => format!("[file {mime} ({name}): {approx_bytes} bytes]"),
            None => format!("[file {mime}: {approx_bytes} bytes]"),
        }
    } else {
        format!("[file {mime}: {uri}]")
    }
}

fn tool_attachment_parts(content: &[ToolContent]) -> Vec<ContentPart> {
    let mut parts = Vec::new();
    for item in content {
        let ToolContent::File { uri, mime, name } = item else {
            continue;
        };
        if !uri.starts_with("data:") {
            continue;
        }
        let data = strip_data_url(uri);
        if mime.starts_with("image/") {
            parts.push(ContentPart::image(mime.clone(), data));
        } else if mime == "application/pdf" {
            parts.push(ContentPart::document(mime.clone(), data, name.clone()));
        }
    }
    parts
}

fn message_type(message: &Value) -> Option<&str> {
    message.get("type").and_then(Value::as_str)
}

/// Last user message text in a provider transcript.
#[must_use]
pub fn last_user_text(messages: &[LlmMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(ContentPart::as_text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn compaction_completed(message: &Value) -> bool {
    message.get("status").and_then(Value::as_str) == Some("completed")
        && message
            .get("summary")
            .and_then(Value::as_str)
            .is_some_and(|summary| !summary.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn running_tool_part_emits_paired_tool_result() {
        let messages = vec![json!({
            "type": "assistant",
            "id": "msg_a1",
            "content": [{
                "type": "tool",
                "id": "call_run_1",
                "name": "shell",
                "time": { "created": 1 },
                "state": {
                    "status": "running",
                    "input": {"command": "cargo test"}
                }
            }]
        })];
        let llm = messages_from_store(&messages);
        assert_eq!(llm.len(), 2);
        assert_eq!(llm[0].role, Role::Assistant);
        assert_eq!(llm[1].role, Role::Tool);
        assert!(matches!(
            &llm[1].content[0],
            ContentPart::ToolResult { tool_call_id, .. } if tool_call_id == "call_run_1"
        ));
    }
}
