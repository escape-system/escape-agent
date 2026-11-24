//! Building blocks for the log-only `session.request.envelope` event.
//!
//! The turn loop materialises one request as `base messages` (a pure function of
//! the `session_message` rows, via [`crate::prompt::messages_from_store`]) plus
//! a handful of volatile splices that cannot be recomputed from durable state.
//! This module renders the base messages canonically so the recorded
//! `base_digest` can be recomputed by any consumer from the rows alone, and it
//! collects the splices as `(kind, text)` pairs so they can be replayed
//! byte-for-byte.
//!
//! The canonical rendering is documented on
//! [`oc_types::payload::SessionRequestEnvelope`]'s module and implemented by
//! [`canonical_message_of`]; the two must stay in step. Rendering rules:
//!
//! | part | rendering |
//! |---|---|
//! | text | the text itself |
//! | image | `[image <media_type> <data.len()> bytes]` |
//! | document | `[document <media_type> <data.len()> bytes <filename or ->]` |
//! | tool call | `[tool_call <id> <name> <input json>]` |
//! | tool result | `[tool_result <tool_call_id> <name or -> <content> error=<bool>]` |
//!
//! The role name is the `Role` wire string (`user`, `assistant`, `tool`).

use oc_llm::{ContentPart, LlmMessage, Role};
use oc_types::payload::RequestInjection;

/// Kind of the memory supplement splice.
pub(super) const KIND_MEMORY_SUPPLEMENT: &str = "memory-supplement";
/// Kind of the output-limit continuation splice.
pub(super) const KIND_OUTPUT_CONTINUATION: &str = "output-continuation";
/// Kind of the verification nudge splice.
pub(super) const KIND_VERIFY_NUDGE: &str = "verify-nudge";
/// Kind of the stall notice splice.
pub(super) const KIND_STALL_NOTICE: &str = "stall-notice";
/// Kind of the context-pressure reminder splice.
pub(super) const KIND_CONTEXT_PRESSURE: &str = "context-pressure";
/// Kind of the step-budget warning splice.
pub(super) const KIND_STEP_BUDGET: &str = "step-budget";
/// Kind of the tool-less final-call prompt splice ([`crate::prompt::MAX_STEPS_PROMPT`]).
pub(super) const KIND_MAX_STEPS: &str = "max-steps";

/// Everything the envelope needs that only the turn loop knows.
///
/// The request-building layers add the provider, model, tool order, system
/// digest and assistant id, so this seed is carried unchanged from `run_steps`
/// down to the single moment the request exists.
#[derive(Debug, Clone)]
pub(super) struct RequestEnvelopeSeed {
    /// Zero-based step ordinal inside this execution.
    pub(super) step: u32,
    /// How many messages the request derived from the durable rows.
    pub(super) base_messages: u32,
    /// Canonical digest of those messages, in order.
    pub(super) base_digest: String,
    /// The splices appended on top, in request order.
    pub(super) injections: Vec<RequestInjection>,
}

impl RequestEnvelopeSeed {
    /// Digest the base messages of one request.
    pub(super) fn new(step: u32, base: &[LlmMessage], injections: Vec<RequestInjection>) -> Self {
        Self {
            step,
            base_messages: u32::try_from(base.len()).unwrap_or(u32::MAX),
            base_digest: {
                // `digest_sequence` borrows; the rendered messages are built
                // first so the borrow lives long enough.
                let rendered: Vec<String> = base.iter().map(canonical_message_of).collect();
                oc_types::payload::digest_sequence(rendered.iter().map(String::as_str))
            },
            injections,
        }
    }
}

/// Canonical rendering of one provider message, for `base_digest`.
#[must_use]
pub(super) fn canonical_message_of(message: &LlmMessage) -> String {
    let parts: Vec<String> = message.content.iter().map(render_part).collect();
    oc_types::payload::canonical_message(role_name(message.role), &parts)
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn render_part(part: &ContentPart) -> String {
    match part {
        ContentPart::Text { text } => text.clone(),
        ContentPart::Image { media_type, data } => {
            format!("[image {media_type} {} bytes]", data.len())
        }
        ContentPart::Document {
            media_type,
            data,
            filename,
        } => format!(
            "[document {media_type} {} bytes {}]",
            data.len(),
            filename.as_deref().unwrap_or("-")
        ),
        ContentPart::ToolCall { id, name, input } => format!(
            "[tool_call {id} {name} {}]",
            serde_json::to_string(input).unwrap_or_default()
        ),
        ContentPart::ToolResult {
            tool_call_id,
            name,
            content,
            is_error,
        } => format!(
            "[tool_result {tool_call_id} {} {content} error={is_error}]",
            name.as_deref().unwrap_or("-")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> LlmMessage {
        LlmMessage::user(text)
    }

    #[test]
    fn base_digest_changes_with_the_base_conversation_only() {
        let base = vec![user("hello"), LlmMessage::assistant("hi")];
        let seed = RequestEnvelopeSeed::new(0, &base, Vec::new());
        assert_eq!(seed.base_messages, 2);
        let same = RequestEnvelopeSeed::new(0, &base, Vec::new());
        assert_eq!(seed.base_digest, same.base_digest);

        let appended = RequestEnvelopeSeed::new(
            0,
            &base,
            vec![RequestInjection::new(KIND_STALL_NOTICE, "notice")],
        );
        assert_eq!(
            appended.base_messages, seed.base_messages,
            "a splice is not a base message"
        );
        assert_eq!(
            appended.base_digest, seed.base_digest,
            "the base digest covers the derived rows, not the splices"
        );

        let extended = vec![user("hello"), LlmMessage::assistant("hi"), user("more")];
        assert_ne!(
            RequestEnvelopeSeed::new(0, &extended, Vec::new()).base_digest,
            seed.base_digest
        );
    }

    #[test]
    fn canonical_rendering_is_stable_and_part_aware() {
        let message = LlmMessage::new(
            Role::Assistant,
            vec![
                ContentPart::text("hi"),
                ContentPart::tool_call("call_1", "read", json!({"path": "a.txt"})),
            ],
        );
        let rendered = canonical_message_of(&message);
        assert_eq!(
            rendered,
            "assistant\u{1e}hi\u{1f}[tool_call call_1 read {\"path\":\"a.txt\"}]"
        );
        assert_eq!(
            rendered,
            canonical_message_of(&message),
            "rendering is deterministic"
        );

        let tool = LlmMessage::tool_result("call_1", Some("read".to_owned()), "ok", false);
        assert_eq!(
            canonical_message_of(&tool),
            "tool\u{1e}[tool_result call_1 read ok error=false]"
        );
        let unlabelled = LlmMessage::tool_result("call_1", None, "ok", true);
        assert_eq!(
            canonical_message_of(&unlabelled),
            "tool\u{1e}[tool_result call_1 - ok error=true]"
        );
    }

    #[test]
    fn attachments_render_by_metadata_not_by_payload() {
        let message = LlmMessage::new(
            Role::User,
            vec![
                ContentPart::image("image/png", "AAAA"),
                ContentPart::document("application/pdf", "BBBBBB", Some("spec.pdf".to_owned())),
            ],
        );
        assert_eq!(
            canonical_message_of(&message),
            "user\u{1e}[image image/png 4 bytes]\u{1f}[document application/pdf 6 bytes spec.pdf]"
        );
    }
}
