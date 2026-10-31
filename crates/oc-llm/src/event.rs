//! Request messages, tool definitions, stream events, and usage accounting.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::ResolvedModel;

/// Conversation role in a provider request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// One piece of message content. Providers map these to their own wire shapes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    /// Base64 image bytes plus MIME type (`image/png`, ...).
    Image {
        media_type: String,
        data: String,
    },
    /// Base64 document bytes (PDF) plus MIME type.
    Document {
        media_type: String,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
    /// Assistant tool invocation, replayed on subsequent turns.
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// Tool result sent back to the provider.
    ToolResult {
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

impl ContentPart {
    #[must_use]
    pub fn text(value: impl Into<String>) -> Self {
        ContentPart::Text { text: value.into() }
    }

    #[must_use]
    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        ContentPart::Image {
            media_type: media_type.into(),
            data: data.into(),
        }
    }

    #[must_use]
    pub fn document(
        media_type: impl Into<String>,
        data: impl Into<String>,
        filename: Option<String>,
    ) -> Self {
        ContentPart::Document {
            media_type: media_type.into(),
            data: data.into(),
            filename,
        }
    }

    #[must_use]
    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        ContentPart::ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    #[must_use]
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: Option<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        ContentPart::ToolResult {
            tool_call_id: tool_call_id.into(),
            name,
            content: content.into(),
            is_error,
        }
    }

    /// Text payload when this part is text, for provider string bodies.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentPart::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// One conversation message. Content is kept as parts so images and PDFs survive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

impl LlmMessage {
    #[must_use]
    pub fn new(role: Role, content: Vec<ContentPart>) -> Self {
        Self { role, content }
    }

    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, vec![ContentPart::text(text)])
    }

    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::new(Role::Assistant, vec![ContentPart::text(text)])
    }

    #[must_use]
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: Option<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::new(
            Role::Tool,
            vec![ContentPart::tool_result(
                tool_call_id,
                name,
                content,
                is_error,
            )],
        )
    }
}

/// Tool definition sent to the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDef {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// Per-call timeouts. `header` covers connect + response headers, `chunk` covers one SSE
/// read, and `total` (when set) bounds the whole call including retries.
#[derive(Debug, Clone)]
pub struct Timeouts {
    pub header: Duration,
    pub chunk: Duration,
    pub total: Option<Duration>,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            header: Duration::from_secs(300),
            chunk: Duration::from_secs(900),
            total: None,
        }
    }
}

/// Provider-independent request options.
#[derive(Debug, Clone, Default)]
pub struct LlmOptions {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_output_tokens: Option<u64>,
    /// Explicit credential. When absent the client checks the provider's env vars.
    pub api_key: Option<String>,
    /// Extra request headers. Provider/model/variant overlays are applied first, these last.
    pub headers: BTreeMap<String, String>,
    /// `auto`, `none`, `required`, or a specific tool name.
    pub tool_choice: Option<String>,
    /// Optional session / cache affinity key used for sticky KV-cache routing.
    pub cache_key: Option<String>,
    pub timeouts: Option<Timeouts>,
}

/// Fully resolved request handed to [`crate::LlmClient`].
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: ResolvedModel,
    pub system: Vec<String>,
    pub messages: Vec<LlmMessage>,
    pub tools: Vec<ToolDef>,
    pub options: LlmOptions,
}

impl LlmRequest {
    #[must_use]
    pub fn new(model: ResolvedModel) -> Self {
        Self {
            model,
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            options: LlmOptions::default(),
        }
    }
}

/// Token accounting for a provider response.
///
/// `input_tokens` and `output_tokens` are inclusive totals; the cache and reasoning
/// fields are non-overlapping subsets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// USD the endpoint itself reported, when it reports a charge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// Number of provider requests that contributed to this usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<u64>,
}

impl Usage {
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total_tokens
            .unwrap_or_else(|| self.input_tokens.unwrap_or(0) + self.output_tokens.unwrap_or(0))
    }

    /// Output tokens that are not reasoning, clamped at zero.
    #[must_use]
    pub fn visible_output_tokens(&self) -> u64 {
        self.output_tokens
            .unwrap_or(0)
            .saturating_sub(self.reasoning_tokens.unwrap_or(0))
    }
}

/// Typed stream event. Field names follow `docs/17-interfaces.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LlmEvent {
    StepStart,
    TextStart,
    TextDelta {
        delta: String,
    },
    TextEnd {
        text: String,
    },
    ReasoningDelta {
        delta: String,
    },
    ToolInputStart {
        id: String,
        name: String,
    },
    ToolInputDelta {
        id: String,
        delta: String,
    },
    ToolInputEnd {
        id: String,
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    Finish {
        reason: String,
        usage: Usage,
    },
    ProviderError {
        message: String,
    },
}

/// Boxed event stream returned by [`crate::LlmClient::stream`].
pub type LlmStream = BoxStream<'static, LlmEvent>;
