use std::collections::BTreeMap;
use std::sync::Arc;

use oc_config::PermissionRule;
use oc_llm::{ResolvedModel, ToolDef, Usage};
use oc_tools::ToolRegistry;
use oc_types::message::{FinishReason, StructuredError, ToolContent};
use oc_types::payload::EventPayload;
use oc_types::{InterruptReason, ModelRef, SessionInfo, Tokens};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::expectation::Expectation;

use super::assistant::AssistantState;

/// Resolved session state for one execution.
#[derive(Clone, Debug)]
pub(super) struct ResolvedTurn {
    pub(super) info: SessionInfo,
    pub(super) directory: String,
    pub(super) agent_id: String,
    pub(super) model: ResolvedModel,
    pub(super) model_ref: ModelRef,
    pub(super) agent_system: Option<String>,
    pub(super) tool_names: Vec<String>,
    pub(super) registry: Arc<ToolRegistry>,
    pub(super) system: Vec<String>,
    pub(super) tools: Vec<ToolDef>,
    pub(super) memory_ids: Vec<String>,
    pub(super) max_steps: usize,
    pub(super) session_rules: Vec<PermissionRule>,
    pub(super) context_window: u64,
}

/// `session.instructions.updated` (durable, v2).
///
/// `oc-types` owns the wire payload catalog; the runtime defines this locally
/// until the payload lands there so the instruction prefix can still publish
/// deltas. [`crate::Runtime::emit`] persists the payload before publishing it.
#[derive(Debug, Serialize)]
pub(super) struct SessionInstructionsUpdated {
    #[serde(rename = "sessionID")]
    pub(super) session_id: String,
    pub(super) delta: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) text: Option<String>,
}

impl EventPayload for SessionInstructionsUpdated {
    const TYPE: &'static str = "session.instructions.updated";
    const DURABLE: bool = true;
    const VERSION: u32 = 2;
}

#[derive(Debug)]
pub(super) enum ExecutionOutcome {
    Succeeded,
    Failed(StructuredError),
    Interrupted(InterruptReason),
}

#[derive(Debug)]
pub(super) struct StepRun {
    pub(super) state: AssistantState,
    pub(super) calls: Vec<PendingToolCall>,
    pub(super) tokens: Tokens,
    pub(super) cost: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingToolCall {
    pub(super) index: usize,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(super) input: Map<String, Value>,
    pub(crate) expectation: Option<Expectation>,
    pub(super) invalid: Option<StructuredError>,
}

pub(super) enum AuthFailure {
    Cancelled,
    Denied(StructuredError),
}

#[derive(Debug)]
pub(crate) enum ToolOutcome {
    Completed {
        index: usize,
        input: Map<String, Value>,
        content: Vec<ToolContent>,
        metadata: Option<Map<String, Value>>,
        executed: bool,
    },
    Failed {
        index: usize,
        input: Map<String, Value>,
        error: StructuredError,
        content: Option<Vec<ToolContent>>,
        metadata: Option<Map<String, Value>>,
        executed: bool,
    },
    Cancelled,
}

pub(super) fn finish_reason(raw: &str) -> FinishReason {
    match raw {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "tool-calls" | "tool_use" => FinishReason::ToolCalls,
        "content_filter" | "content-filter" => FinishReason::ContentFilter,
        "error" => FinishReason::Error,
        _ => FinishReason::Unknown,
    }
}

pub(super) fn tokens_from_usage(usage: &Usage) -> Tokens {
    let cache_read = saturating_i64(usage.cache_read_input_tokens);
    let cache_write = saturating_i64(usage.cache_write_input_tokens);
    let non_cached = usage.non_cached_input_tokens.map_or_else(
        || {
            saturating_i64(usage.input_tokens)
                .saturating_sub(cache_read)
                .saturating_sub(cache_write)
        },
        |value| saturating_i64(Some(value)),
    );
    Tokens {
        input: non_cached
            .saturating_add(cache_read)
            .saturating_add(cache_write),
        output: saturating_i64(usage.output_tokens),
        reasoning: saturating_i64(usage.reasoning_tokens),
        cache: oc_types::CacheTokens {
            read: cache_read,
            write: cache_write,
        },
    }
}

pub(super) fn saturating_i64(value: Option<u64>) -> i64 {
    value.map_or(0, |value| i64::try_from(value).unwrap_or(i64::MAX))
}
