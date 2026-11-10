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
    pub(super) memory_supplement: Option<String>,
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
    /// The turn answered on its final, tool-less request because the step
    /// budget ran out. The wire terminal stays `succeeded`; `steps` is the
    /// budget the turn consumed.
    StepLimited {
        steps: u64,
    },
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
        // `TokenUsage.Info.input` is the non-cached part only; cache is carried
        // separately so `input + cache.read + cache.write` is the prompt size.
        // `Tokens::prompt_tokens` is the inclusive helper.
        input: non_cached,
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

/// Inclusive prompt size from a stored `tokens` object: non-cached `input` plus
/// cache reads/writes. `None` means the row carries no measured prompt at all,
/// so it cannot anchor an estimate (a fully cached step still can: `input == 0`).
pub(super) fn prompt_tokens_from_store(tokens: &Value) -> Option<u64> {
    let input = tokens.get("input").and_then(Value::as_u64)?;
    let cache_read = tokens
        .get("cache")
        .and_then(|cache| cache.get("read"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = tokens
        .get("cache")
        .and_then(|cache| cache.get("write"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let prompt = input.saturating_add(cache_read).saturating_add(cache_write);
    (prompt > 0).then_some(prompt)
}

/// Anchor measurement: prompt tokens plus the step output (minus reasoning,
/// which is already inside output). Mirrors the pre-existing arithmetic.
pub(super) fn anchor_measured(prompt_tokens: u64, output: u64, reasoning: u64) -> u64 {
    prompt_tokens.saturating_add(output.saturating_sub(reasoning))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_llm::Usage;
    use serde_json::json;

    #[test]
    fn mapper_keeps_input_non_cached_and_prompt_inclusive() {
        let usage = Usage {
            input_tokens: Some(467_593),
            non_cached_input_tokens: Some(393),
            cache_read_input_tokens: Some(467_200),
            cache_write_input_tokens: Some(0),
            ..Usage::default()
        };
        let tokens = tokens_from_usage(&usage);
        assert_eq!(tokens.input, 393);
        assert_eq!(tokens.prompt_tokens(), 467_593);
        assert_eq!(tokens.cache.read, 467_200);
        assert_eq!(tokens.cache.write, 0);
    }

    #[test]
    fn estimate_anchor_uses_prompt_tokens_not_collapsing_to_heuristic() {
        // Cache-heavy anchor: `input` is tiny but the real prompt is large.
        let cache_heavy = json!({
            "input": 393,
            "output": 10,
            "reasoning": 0,
            "cache": {"read": 467_200, "write": 0},
        });
        let prompt = prompt_tokens_from_store(&cache_heavy).expect("cache-heavy anchor");
        assert_eq!(prompt, 467_593);
        assert_eq!(anchor_measured(prompt, 10, 0), 467_603);

        // Fully cached anchor: non-cached input is zero, but it still measures.
        let fully_cached = json!({
            "input": 0,
            "output": 5,
            "reasoning": 0,
            "cache": {"read": 100_000, "write": 0},
        });
        let prompt = prompt_tokens_from_store(&fully_cached).expect("fully cached anchor");
        assert_eq!(prompt, 100_000);
        assert_eq!(anchor_measured(prompt, 5, 0), 100_005);

        // Only a genuinely empty row is rejected, so the heuristic is used.
        let empty = json!({"input": 0, "cache": {"read": 0, "write": 0}});
        assert_eq!(prompt_tokens_from_store(&empty), None);
    }
}
