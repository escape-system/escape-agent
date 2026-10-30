use oc_types::payload::EventPayload;
use serde::Serialize;
use serde_json::Value;

/// `subagent.peer.message` (ephemeral).
///
/// Published by `talk` after the peer inbox row is durable, so a UI can show
/// the message before the target session starts running. Peer messages are
/// not user messages and are never replayed from the event log.
#[derive(Debug, Serialize)]
pub(super) struct SubagentPeerMessage {
    #[serde(rename = "sessionID")]
    pub(super) session_id: String,
    pub(super) from: String,
    pub(super) agent: String,
    pub(super) kind: String,
    pub(super) text: String,
    #[serde(rename = "requestReply")]
    pub(super) request_reply: bool,
    #[serde(rename = "messageID")]
    pub(super) message_id: String,
}

impl EventPayload for SubagentPeerMessage {
    const TYPE: &'static str = "subagent.peer.message";
    const DURABLE: bool = false;
    const VERSION: u32 = 1;
}

fn xml_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
}

pub(super) fn envelope(
    state: &str,
    session: &str,
    description: Option<&str>,
    text: &str,
) -> String {
    match description {
        Some(description) => format!(
            "<subagent sessionID=\"{session}\" state=\"{state}\" description=\"{}\">{text}</subagent>",
            xml_attr(description)
        ),
        None => format!("<subagent sessionID=\"{session}\" state=\"{state}\">{text}</subagent>"),
    }
}

pub(super) fn envelope_reason(
    state: &str,
    session: &str,
    description: Option<&str>,
    reason: &str,
    text: &str,
) -> String {
    match description {
        Some(description) => format!(
            "<subagent sessionID=\"{session}\" state=\"{state}\" description=\"{}\" reason=\"{}\">{text}</subagent>",
            xml_attr(description),
            xml_attr(reason)
        ),
        None => format!(
            "<subagent sessionID=\"{session}\" state=\"{state}\" reason=\"{}\">{text}</subagent>",
            xml_attr(reason)
        ),
    }
}

pub(super) fn message_text(message: &Value) -> String {
    if let Some(text) = message.get("text").and_then(Value::as_str) {
        return text.to_owned();
    }
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text") => part.get("text").and_then(Value::as_str),
                    _ => None,
                })
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

pub(super) fn final_assistant_text(messages: &[Value]) -> String {
    for message in messages.iter().rev() {
        if message.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let text = message_text(message);
        if !text.trim().is_empty() {
            return text;
        }
        if let Some(error) = message
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            return error.to_owned();
        }
    }
    String::new()
}

pub(super) fn child_state(messages: &[Value]) -> String {
    for message in messages.iter().rev() {
        if message.get("type").and_then(Value::as_str) != Some("idle") {
            continue;
        }
        return match message.get("outcome").and_then(Value::as_str) {
            Some("failed") => "failed".to_owned(),
            Some("interrupted") => "cancelled".to_owned(),
            _ => "completed".to_owned(),
        };
    }
    "completed".to_owned()
}

/// Messages appended after the prompt marker. The marker is absent until the
/// child actor promotes the inbox row, so a pre-execution idle can never be
/// mistaken for completion.
pub(super) fn after_prompt<'a>(messages: &'a [Value], prompt_id: &str) -> (bool, &'a [Value]) {
    match messages
        .iter()
        .position(|message| message.get("id").and_then(Value::as_str) == Some(prompt_id))
    {
        Some(index) => (true, &messages[index + 1..]),
        None => (false, &[]),
    }
}

/// The final assistant message failed without a settle idle row.
pub(super) fn assistant_failure(tail: &[Value]) -> Option<(String, String)> {
    let message = tail
        .iter()
        .rev()
        .find(|message| message.get("type").and_then(Value::as_str) == Some("assistant"))?;
    let error = message.get("error");
    let error_type = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let finish = message
        .get("finish")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if finish != "error" && error.is_none() {
        return None;
    }
    let state = if error_type.starts_with("MessageAborted") {
        "cancelled"
    } else {
        "failed"
    };
    let text = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| final_assistant_text(std::slice::from_ref(message)));
    Some((state.to_owned(), text))
}
