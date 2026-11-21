use oc_llm::LlmMessage;
use serde_json::Value;

use crate::prompt;
use crate::runtime::{Runtime, RuntimeError};
use crate::tokens;

use super::types;
use super::types::ResolvedTurn;

/// Token pressure for one session, used by compaction and observer advice.
///
/// `estimated` is the same anchor-aware estimate the turn loop uses,
/// `ceiling` is [`tokens::ceiling`] and `trigger` is [`tokens::trigger`], so
/// observers and the threshold path cannot disagree on the numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextPressure {
    pub(crate) estimated: u64,
    pub(crate) ceiling: u64,
    pub(crate) trigger: u64,
}

impl Runtime {
    /// Token pressure for one session, used by compaction and observer advice.
    ///
    /// Cheap and non-failing for the caller: any session/model resolve error
    /// becomes `Ok(None)` so an observer tick never fails on a stale session.
    pub(crate) async fn context_pressure(
        &self,
        session: &str,
    ) -> Result<Option<ContextPressure>, RuntimeError> {
        let Ok(Some(value)) = self.inner.config.store.get_session(session).await else {
            return Ok(None);
        };
        let Ok(info) = Runtime::parse_session(&value) else {
            return Ok(None);
        };
        let directory = info.location.directory.clone();
        let Ok(turn) = self.resolve_turn(&info, &directory).await else {
            return Ok(None);
        };
        if turn.context_window == 0 {
            return Ok(None);
        }
        let context = self.load_context(session).await?;
        let estimate = self.estimate_turn_tokens(session, &turn, &context).await?;
        let compaction = self.compaction_config();
        let ceiling = tokens::ceiling(turn.context_window, compaction.ceiling_tokens);
        Ok(Some(ContextPressure {
            estimated: estimate,
            ceiling,
            trigger: tokens::trigger(ceiling, turn.context_window, compaction.buffer),
        }))
    }

    pub(super) async fn estimate_turn_tokens(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        context: &[LlmMessage],
    ) -> Result<u64, RuntimeError> {
        let heuristic = oc_llm::estimate_request_tokens(&turn.system, context, &turn.tools);
        let stored = self.list_all_messages(session).await?;
        let boundary = stored.iter().rposition(|message| {
            message.get("type").and_then(Value::as_str) == Some("compaction")
                && message.get("status").and_then(Value::as_str) == Some("completed")
        });
        let epoch_start = boundary.map_or(0, |index| index + 1);
        let active = &stored[epoch_start..];
        let anchor = active
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| {
                if message.get("type").and_then(Value::as_str) != Some("assistant") {
                    return None;
                }
                let provider = message
                    .get("model")
                    .and_then(|model| model.get("providerID"))
                    .and_then(Value::as_str)?;
                if provider != turn.model_ref.provider_id {
                    return None;
                }
                let tokens_obj = message.get("tokens")?;
                let prompt_tokens = types::prompt_tokens_from_store(tokens_obj)?;
                let output = tokens_obj
                    .get("output")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let reasoning = tokens_obj
                    .get("reasoning")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                Some((
                    index,
                    types::anchor_measured(prompt_tokens, output, reasoning),
                ))
            });
        let Some((anchor_index, measured)) = anchor else {
            return Ok(heuristic);
        };
        let anchor_converted =
            prompt::messages_from_store(std::slice::from_ref(&active[anchor_index]));
        let tool_results_tokens: u64 = anchor_converted
            .iter()
            .filter(|msg| msg.role != oc_llm::Role::Assistant)
            .map(oc_llm::estimate_message_tokens)
            .sum();
        let after_converted = prompt::messages_from_store(&active[anchor_index + 1..]);
        let after_tokens =
            tool_results_tokens.saturating_add(oc_llm::estimate_messages_tokens(&after_converted));
        let anchored =
            oc_llm::estimate_prompt_tokens(Some(measured), &[]).saturating_add(after_tokens);
        Ok(heuristic.max(anchored))
    }
}
