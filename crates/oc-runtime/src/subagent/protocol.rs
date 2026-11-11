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

/// Configured upper bound on the child's final text embedded in the
/// parent-visible `<subagent …>` report. `0` means unlimited, which is the
/// default: the full report is replayed (measured worst case `28_728` chars).
pub(crate) const UNLIMITED_REPORT_CHARS: usize = 0;

fn xml_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
}

/// Renders the child's final text into the parent-visible report body.
///
/// A zero `limit` ([`UNLIMITED_REPORT_CHARS`]) returns the text unchanged; so
/// does any text at or under a non-zero `limit`. Longer text keeps the first
/// `limit`-worth of characters (moved back to a line boundary when that drops
/// little) and names the child so the parent can call `digest` for the rest.
/// Truncation is character-based, so a multi-byte character is never split.
pub(super) fn render_subagent_report(session: &str, text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if limit == UNLIMITED_REPORT_CHARS || total <= limit {
        return text.to_owned();
    }
    let head: String = text.chars().take(limit).collect();
    let cut = match head.rfind('\n') {
        Some(byte) if head[..byte].chars().count() + 200 >= limit => head[..byte].chars().count(),
        _ => limit,
    };
    let kept: String = text.chars().take(cut).collect();
    let omitted = total - cut;
    format!(
        "{kept}\n[truncated: {omitted} chars omitted; full report in sessionID={session}; use digest with sessionID={session} for a summary]"
    )
}

pub(crate) fn envelope(
    state: &str,
    session: &str,
    description: Option<&str>,
    text: &str,
    limit: usize,
) -> String {
    let text = render_subagent_report(session, text, limit);
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
    limit: usize,
) -> String {
    let text = render_subagent_report(session, text, limit);
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

fn outcome_state(outcome: Option<&str>) -> Option<&'static str> {
    match outcome {
        Some("failed") => Some("failed"),
        Some("interrupted") => Some("cancelled"),
        // `step_limit` is the stored literal for a completed turn whose step
        // budget ended it; it is a qualified success, never a failure.
        Some("succeeded" | "step_limit") => Some("completed"),
        _ => None,
    }
}

/// Terminal state of the child's current turn, or `None` when no terminal
/// outcome is recorded for it.
///
/// `durable` is the session row's `idle_outcome`. The child's own `settle`
/// writes it together with the idle message, and the parent-side wait only ever
/// downgrades it to a failure, so the row is authoritative; the idle message is
/// consulted only for states the row does not carry. A failure from either
/// source wins over a success: the safe direction for a parent that plans on
/// the result. Callers must never read `None` as success — it means the child
/// is not known to have settled.
pub(super) fn child_state(messages: &[Value], durable: Option<&str>) -> Option<&'static str> {
    let idle = messages
        .iter()
        .rev()
        .find(|message| message.get("type").and_then(Value::as_str) == Some("idle"))
        .and_then(|message| message.get("outcome").and_then(Value::as_str));
    match (outcome_state(durable), outcome_state(idle)) {
        (Some("failed"), _) | (_, Some("failed")) => Some("failed"),
        (Some("cancelled"), _) | (_, Some("cancelled")) => Some("cancelled"),
        (Some("completed"), _) | (_, Some("completed")) => Some("completed"),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::{UNLIMITED_REPORT_CHARS, child_state, envelope, render_subagent_report};
    use serde_json::json;

    #[test]
    fn child_state_never_defaults_to_completed() {
        // No recorded outcome is not success: the child has not settled.
        assert_eq!(child_state(&[], None), None);
        // The durable row wins; a stale idle message cannot resurrect success.
        let idle = vec![json!({"type": "idle", "outcome": "succeeded"})];
        assert_eq!(child_state(&idle, Some("failed")), Some("failed"));
        assert_eq!(child_state(&idle, Some("interrupted")), Some("cancelled"));
        // The idle message is still read when the row has no outcome yet.
        assert_eq!(child_state(&idle, None), Some("completed"));
        let failed = vec![json!({"type": "idle", "outcome": "failed"})];
        assert_eq!(child_state(&failed, None), Some("failed"));
        let interrupted = vec![json!({"type": "idle", "outcome": "interrupted"})];
        assert_eq!(child_state(&interrupted, None), Some("cancelled"));
        // An unrecognised value is not success either.
        let unknown = vec![json!({"type": "idle", "outcome": "weird"})];
        assert_eq!(child_state(&unknown, None), None);
    }

    #[test]
    fn report_at_or_below_cap_is_byte_identical() {
        let body = "a".repeat(3000);
        assert!(body.len() < 4000);
        assert_eq!(render_subagent_report("ses_test", &body, 4000), body);
    }

    #[test]
    fn report_unlimited_keeps_the_whole_text() {
        let body = "x".repeat(10_000);
        let out = render_subagent_report("ses_test", &body, UNLIMITED_REPORT_CHARS);
        assert_eq!(out, body);
        assert!(!out.contains("truncated"));
    }

    #[test]
    fn report_default_settings_are_unlimited() {
        assert_eq!(
            crate::subagent::settings::SubagentSettings::default().report_char_limit,
            UNLIMITED_REPORT_CHARS
        );
    }

    #[test]
    fn report_above_cap_truncates_and_names_the_child() {
        let body = "a".repeat(10_000);
        let out = render_subagent_report("ses_test", &body, 4000);
        let marker = "\n[truncated: 6000 chars omitted; full report in sessionID=ses_test; use digest with sessionID=ses_test for a summary]";
        assert_eq!(out, format!("{}{marker}", &body[..4000]));
        assert!(out.contains("6000 chars omitted"));
        assert!(out.contains("sessionID=ses_test"));
        assert!(out.starts_with(&body[..4000]));
    }

    #[test]
    fn report_cut_moves_back_to_a_line_boundary() {
        let body = format!("{}\n{}", "a".repeat(3950), "b".repeat(6000));
        let out = render_subagent_report("ses_test", &body, 4000);
        assert!(out.contains("[truncated:"), "{out}");
        assert!(out.starts_with(&"a".repeat(3950)));
        assert!(out.chars().take(3950).all(|c| c == 'a'));
        assert_eq!(out.chars().nth(3950), Some('\n'));
        assert_eq!(body.chars().count(), 9951);
        assert!(
            out.contains(&format!("{} chars omitted", body.chars().count() - 3950)),
            "{out}"
        );
    }

    #[test]
    fn report_truncation_never_splits_multibyte() {
        let body = "ş".repeat(5000);
        let out = render_subagent_report("ses_test", &body, 4000);
        assert_eq!(out.chars().take(4000).collect::<String>(), "ş".repeat(4000));
        assert!(out.contains("1000 chars omitted"));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn recovered_child_report_is_capped() {
        // Boot recovery (`recover.rs`) reports a settled child through this
        // exact call: the shared builder with no description (recovery has no
        // description to pass). Assert on the rendered string, not the const.
        let body = "x".repeat(10_000);
        let rendered = envelope("failed", "ses_recover", None, &body, 4000);
        let open = "<subagent sessionID=\"ses_recover\" state=\"failed\">";
        let close = "</subagent>";
        let marker = "\n[truncated: 6000 chars omitted; full report in sessionID=ses_recover; use digest with sessionID=ses_recover for a summary]";
        assert!(rendered.starts_with(open), "{rendered}");
        assert!(rendered.ends_with(close), "{rendered}");
        let inner = &rendered[open.len()..rendered.len() - close.len()];
        assert!(inner.starts_with(&body[..4000]));
        assert!(inner.ends_with(marker), "{}", &inner[3950..]);
        assert!(inner.contains("truncated:"));
        assert_eq!(inner.chars().count(), 4000 + marker.chars().count());
        let unlimited = envelope("failed", "ses_recover", None, &body, UNLIMITED_REPORT_CHARS);
        assert_eq!(
            unlimited,
            format!("<subagent sessionID=\"ses_recover\" state=\"failed\">{body}</subagent>")
        );
    }
}
