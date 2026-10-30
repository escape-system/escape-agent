//! Pure token estimation and cost helpers.
//!
//! The estimator follows the runtime's anchor rule: a caller that knows a measured
//! prefix passes its input-token count and only the messages after that anchor are
//! estimated. Text is ~4 characters per token; an image is 1500 tokens and a PDF is
//! 2000 tokens.

use serde_json::Value;

use crate::catalog::ModelInfo;
use crate::event::{ContentPart, LlmMessage, ToolDef};

/// Tokens an image attachment is charged.
pub const IMAGE_TOKENS: u64 = 1_500;
/// Tokens a PDF attachment is charged.
pub const PDF_TOKENS: u64 = 2_000;
/// Characters per token used for text estimation.
pub const CHARS_PER_TOKEN: u64 = 4;

/// Estimate tokens for one content part.
#[must_use]
pub fn estimate_part_tokens(part: &ContentPart) -> u64 {
    match part {
        ContentPart::Text { text } => text_tokens(text),
        ContentPart::Image { .. } => IMAGE_TOKENS,
        // Documents are almost always PDFs on this path; the runtime prices any
        // document attachment the same way rather than decoding page structure.
        ContentPart::Document { .. } => PDF_TOKENS,
        ContentPart::ToolCall { name, input, .. } => {
            text_tokens(name) + estimate_json_tokens(input) + 4
        }
        ContentPart::ToolResult { content, .. } => text_tokens(content) + 4,
    }
}

/// Estimate tokens for a message's content.
#[must_use]
pub fn estimate_message_tokens(message: &LlmMessage) -> u64 {
    message
        .content
        .iter()
        .map(estimate_part_tokens)
        .sum::<u64>()
        + 4
}

/// Estimate tokens for a contiguous run of messages.
#[must_use]
pub fn estimate_messages_tokens(messages: &[LlmMessage]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Estimate a prompt from an optional measured anchor.
///
/// `anchor` is the measured `input + cache.read + cache.write` of the last assistant
/// response. Only `after` is estimated and added, exactly like the runtime's
/// context-pressure calculation.
#[must_use]
pub fn estimate_prompt_tokens(anchor: Option<u64>, after: &[LlmMessage]) -> u64 {
    anchor.unwrap_or(0) + estimate_messages_tokens(after)
}

/// Estimate a full request: system prompts, messages, and tool schemas.
#[must_use]
pub fn estimate_request_tokens(
    system: &[String],
    messages: &[LlmMessage],
    tools: &[ToolDef],
) -> u64 {
    let system_tokens: u64 = system.iter().map(|text| text_tokens(text)).sum();
    let tool_tokens: u64 = tools
        .iter()
        .map(|tool| {
            text_tokens(&tool.name)
                + text_tokens(&tool.description)
                + estimate_json_tokens(&tool.input_schema)
        })
        .sum();
    system_tokens + estimate_messages_tokens(messages) + tool_tokens
}

/// Cost of a usage record using a model's context bands.
#[must_use]
pub fn model_cost(model: &ModelInfo, usage: &crate::event::Usage) -> f64 {
    if let Some(cost) = usage.cost {
        return cost;
    }
    crate::catalog::calculate_cost(&model.cost, usage)
}

fn text_tokens(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    chars.div_ceil(CHARS_PER_TOKEN)
}

fn estimate_json_tokens(value: &Value) -> u64 {
    text_tokens(&value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_image_and_pdf_weights() {
        assert_eq!(estimate_part_tokens(&ContentPart::text("")), 0);
        assert_eq!(estimate_part_tokens(&ContentPart::text("abcd")), 1);
        assert_eq!(estimate_part_tokens(&ContentPart::text("abcde")), 2);
        assert_eq!(
            estimate_part_tokens(&ContentPart::image("image/png", "AAAA")),
            IMAGE_TOKENS
        );
        assert_eq!(
            estimate_part_tokens(&ContentPart::document("application/pdf", "AAAA", None)),
            PDF_TOKENS
        );
    }

    #[test]
    fn anchor_only_estimates_tail() {
        let tail = vec![LlmMessage::user("abcdefgh")];
        assert_eq!(estimate_prompt_tokens(Some(10_000), &tail), 10_006);
        assert_eq!(estimate_prompt_tokens(None, &tail), 6);
    }
}
