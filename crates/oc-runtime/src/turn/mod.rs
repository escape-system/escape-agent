//! The turn loop: prompt promotion, streaming, tools, compaction and settlement.
//!
//! Durable message rows are written before the events that describe them. The
//! assistant message is appended once per step and rewritten at text end, tool
//! call, tool result and step end; deltas are never persisted.

use std::sync::Arc;

use oc_llm::LlmMessage;
use oc_store::NewMessage;
use oc_types::payload::{
    SessionExecutionFailed, SessionExecutionInterrupted, SessionExecutionSucceeded, SessionIdle,
    SessionInboxDelivered, SessionStatusUpdated,
};
use oc_types::{MessageId, SessionStatus};
use serde_json::{Map, Value, json};

use crate::prompt::{self, MAX_STEPS_PROMPT};
use crate::runtime::{ActorHandle, CancelToken, Runtime, RuntimeError};
use crate::tokens;

mod assistant;
mod compaction;
mod finalize;
mod permissions;
mod session;
mod stats;
mod stream;
mod tools;
mod types;

pub(crate) use tools::report_tool_progress;
pub(crate) use types::{PendingToolCall, ToolOutcome};

use assistant::assistant_text;
use stats::TurnStats;
use stats::collect_tool_lines;
use types::{ExecutionOutcome, ResolvedTurn};

/// Promotes one inbox row unless the host already wrote the message; a row
/// that already exists is consumed as a doorbell and reported as the sentinel
/// `Some(Value::Null)`.
async fn promote_inbox_row(
    store: &oc_store::Store,
    session: &str,
    id: &str,
) -> Result<Option<Value>, RuntimeError> {
    if store.get_message(session, id).await?.is_some() {
        store.cancel_inbox(session, id).await?;
        Ok(Some(Value::Null))
    } else {
        match store.promote_inbox(session, id).await {
            Ok(promoted) => Ok(promoted),
            Err(error) => {
                if store.get_message(session, id).await?.is_some() {
                    store.cancel_inbox(session, id).await?;
                    Ok(Some(Value::Null))
                } else {
                    Err(error.into())
                }
            }
        }
    }
}

pub(crate) async fn run_execution(
    runtime: &Runtime,
    session: &str,
    item: &Value,
    cancel: &Arc<CancelToken>,
    handle: &Arc<ActorHandle>,
) -> Result<(), RuntimeError> {
    let info_value = runtime
        .inner
        .config
        .store
        .get_session(session)
        .await?
        .ok_or_else(|| RuntimeError::SessionNotFound(session.to_owned()))?;
    let info = Runtime::parse_session(&info_value)?;
    let directory = info.location.directory.clone();
    let Some(inbox_id) = item.get("id").and_then(Value::as_str) else {
        return Ok(());
    };

    // Some hosts materialise the user row themselves before notifying. When the
    // row exists, the inbox entry is only a doorbell: consume it instead of
    // failing on the primary-key conflict.
    let promoted = promote_inbox_row(&runtime.inner.config.store, session, inbox_id).await?;
    if promoted.is_none() {
        return Ok(());
    }
    runtime
        .emit(
            session,
            &directory,
            SessionInboxDelivered {
                session_id: Runtime::session_id(session),
                inbox_id: MessageId::from_wire(inbox_id.to_owned()),
            },
        )
        .await?;
    runtime
        .inner
        .config
        .store
        .mark_session_busy(session, oc_types::now_millis())
        .await?;
    runtime
        .emit(
            session,
            &directory,
            SessionStatusUpdated {
                session_id: Runtime::session_id(session),
                status: SessionStatus::Busy,
            },
        )
        .await?;
    runtime
        .emit(
            session,
            &directory,
            oc_types::payload::SessionExecutionStarted {
                session_id: Runtime::session_id(session),
            },
        )
        .await?;

    let mut turn = match runtime.resolve_turn(&info, &directory) {
        Ok(turn) => turn,
        Err(error) => {
            runtime
                .settle(
                    session,
                    &directory,
                    ExecutionOutcome::Failed(error.structured()),
                )
                .await?;
            return Ok(());
        }
    };
    if let Err(error) = runtime
        .refresh_instructions(session, &mut turn, &info, handle)
        .await
    {
        runtime
            .settle(
                session,
                &directory,
                ExecutionOutcome::Failed(error.structured()),
            )
            .await?;
        return Ok(());
    }

    let mut stats = TurnStats::default();
    let outcome = runtime
        .run_steps(session, &turn, cancel, &mut stats)
        .await?;
    if matches!(outcome, ExecutionOutcome::Succeeded) {
        runtime.touch_memory(&turn.memory_ids).await;
    }
    let succeeded = matches!(outcome, ExecutionOutcome::Succeeded);
    runtime.settle(session, &directory, outcome).await?;
    if succeeded {
        runtime.spawn_scribe(session, &stats).await;
    }
    Ok(())
}

impl Runtime {
    /// Promotes the pending `steer` rows that can become conversation
    /// messages and publishes their delivery event.
    ///
    /// This is the mid-turn counterpart of the promotion at the top of
    /// [`run_execution`]: it follows the same durable ordering (promote, then
    /// publish `session.inbox.delivered`, then let the caller read the
    /// message) and the same conflict handling for hosts that materialised the
    /// message themselves. `queue` rows and control kinds (`compaction`,
    /// `move`) are left for the next execution. Returns whether the context
    /// must be reloaded.
    async fn promote_steers(&self, session: &str, directory: &str) -> Result<bool, RuntimeError> {
        let store = &self.inner.config.store;
        let items = store.list_inbox(session).await?;
        let mut changed = false;
        for item in items {
            if item.get("delivery").and_then(Value::as_str) != Some("steer") {
                continue;
            }
            if !matches!(
                item.get("type").and_then(Value::as_str),
                Some("user" | "synthetic" | "peer" | "observer")
            ) {
                continue;
            }
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            // A host may have written the message before notifying; the row is
            // then only a doorbell and is consumed instead of conflicting.
            let promoted = promote_inbox_row(store, session, id).await?;
            if promoted.is_none() {
                continue;
            }
            // The row is durable now; publish only after the promote commits.
            self.emit(
                session,
                directory,
                SessionInboxDelivered {
                    session_id: Runtime::session_id(session),
                    inbox_id: MessageId::from_wire(id.to_owned()),
                },
            )
            .await?;
            changed = true;
        }
        Ok(changed)
    }

    #[allow(clippy::too_many_lines)]
    async fn run_steps(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        cancel: &Arc<CancelToken>,
        stats: &mut TurnStats,
    ) -> Result<ExecutionOutcome, RuntimeError> {
        let mut context = self.load_context(session).await?;
        stats.user_text = prompt::last_user_text(&context);
        let mut last_compacted_len: Option<usize> = None;
        let mut auto_compactions = 0_usize;
        let mut overflow_compacted = false;
        let mut length_continuations = 0_usize;
        let mut pending_output_continuation = false;
        let mut step_number = 0_usize;
        let ceiling = if turn.context_window == 0 {
            u64::MAX
        } else {
            // Cap at 150_000 so models with huge context windows (e.g. 1M) do not run
            // hundreds of thousands of uncompacted tokens per step, burning tens of millions of tokens.
            tokens::compaction_ceiling(turn.context_window).min(150_000)
        };

        loop {
            if cancel.is_cancelled() {
                return Ok(ExecutionOutcome::Interrupted(cancel.reason()));
            }
            // Steer rows that arrived while the previous step ran join the
            // conversation before this request is built. `queue` rows stay in
            // the inbox; the actor promotes them as the next execution.
            //
            // Rows still pending before the first request belong to the batch
            // admitted ahead of this execution (the execution promoted one of
            // them already), so they keep their own turn. Only rows that
            // arrived during the turn are promoted mid-turn.
            if step_number > 0 && self.promote_steers(session, &turn.directory).await? {
                context = self.load_context(session).await?;
            }
            if auto_compactions < 3 && last_compacted_len != Some(context.len()) {
                let estimate = self.estimate_turn_tokens(session, turn, &context).await?;
                if estimate > ceiling {
                    match self.compact(session, turn, &context, cancel, "auto").await {
                        Ok(()) => {
                            context = self.load_context(session).await?;
                            auto_compactions += 1;
                            last_compacted_len = Some(context.len());
                        }
                        Err(RuntimeError::Cancelled) => {
                            return Ok(ExecutionOutcome::Interrupted(cancel.reason()));
                        }
                        Err(error) => {
                            return Ok(ExecutionOutcome::Failed(error.structured()));
                        }
                    }
                }
            }

            let final_call = step_number >= turn.max_steps;
            let mut messages = context.clone();
            if let Some(supplement) = &turn.memory_supplement {
                messages.push(LlmMessage::user(supplement.clone()));
            }
            if pending_output_continuation {
                messages.push(LlmMessage::user(prompt::CONTINUE_AFTER_OUTPUT_LIMIT_PROMPT));
            }
            if final_call {
                messages.push(LlmMessage::user(MAX_STEPS_PROMPT));
            }
            let mut step = match self
                .stream_step(session, turn, &messages, final_call, cancel)
                .await
            {
                Ok(step) => step,
                Err(RuntimeError::Cancelled) => {
                    return Ok(ExecutionOutcome::Interrupted(cancel.reason()));
                }
                Err(error)
                    if !overflow_compacted
                        && compaction::is_context_overflow_message(&error.to_string()) =>
                {
                    match self
                        .compact(session, turn, &context, cancel, "overflow")
                        .await
                    {
                        Ok(()) => {
                            overflow_compacted = true;
                            context = self.load_context(session).await?;
                            last_compacted_len = Some(context.len());
                            continue;
                        }
                        Err(RuntimeError::Cancelled) => {
                            return Ok(ExecutionOutcome::Interrupted(cancel.reason()));
                        }
                        Err(compact_err) => {
                            return Ok(ExecutionOutcome::Failed(compact_err.structured()));
                        }
                    }
                }
                Err(error) => {
                    return Ok(ExecutionOutcome::Failed(error.structured()));
                }
            };
            pending_output_continuation = false;

            if !final_call && !step.calls.is_empty() {
                length_continuations = 0;
                stats.tool_calls = stats.tool_calls.saturating_add(step.calls.len());
                match self
                    .execute_tools(session, turn, &mut step.state, &step.calls, cancel)
                    .await
                {
                    Ok(()) => {}
                    Err(RuntimeError::Cancelled) => {
                        return Ok(ExecutionOutcome::Interrupted(cancel.reason()));
                    }
                    Err(error) => return Err(error),
                }
            }
            self.finish_step(session, turn, &mut step).await?;
            collect_tool_lines(stats, &step.state);
            if !final_call
                && step.calls.is_empty()
                && step.state.finish == Some(oc_types::message::FinishReason::Length)
                && length_continuations < 2
            {
                length_continuations += 1;
                pending_output_continuation = true;
                step_number += 1;
                context = self.load_context(session).await?;
                continue;
            }
            if final_call || step.calls.is_empty() {
                stats.assistant_text = assistant_text(&step.state);
                return Ok(ExecutionOutcome::Succeeded);
            }
            step_number += 1;
            context = self.load_context(session).await?;
        }
    }

    async fn estimate_turn_tokens(
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
                let input = tokens_obj.get("input").and_then(Value::as_u64)?;
                if input == 0 {
                    return None;
                }
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
                    input.saturating_add(output.saturating_sub(reasoning)),
                ))
            });
        let Some((anchor_index, measured)) = anchor else {
            return Ok(heuristic);
        };
        let anchor_converted =
            prompt::messages_from_store(std::slice::from_ref(&active[anchor_index]));
        let tool_results_tokens: u64 = anchor_converted
            .iter()
            .filter(|msg| msg.role == oc_llm::Role::Tool)
            .map(oc_llm::estimate_message_tokens)
            .sum();
        let after_converted = prompt::messages_from_store(&active[anchor_index + 1..]);
        let after_tokens =
            tool_results_tokens.saturating_add(oc_llm::estimate_messages_tokens(&after_converted));
        let anchored =
            oc_llm::estimate_prompt_tokens(Some(measured), &[]).saturating_add(after_tokens);
        Ok(heuristic.max(anchored))
    }

    async fn settle(
        &self,
        session: &str,
        directory: &str,
        outcome: ExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        match &outcome {
            ExecutionOutcome::Failed(_) => self.note_observer_execution(session, true).await,
            ExecutionOutcome::Interrupted(_) => self.note_observer_execution(session, false).await,
            ExecutionOutcome::Succeeded => {}
        }
        let now = oc_types::now_millis();
        let (name, error) = match &outcome {
            ExecutionOutcome::Succeeded => ("succeeded", None),
            ExecutionOutcome::Failed(error) => ("failed", Some(error.clone())),
            ExecutionOutcome::Interrupted(_) => ("interrupted", None),
        };
        self.clear_background_request(session);
        self.inner
            .config
            .store
            .mark_session_idle(session, now, name)
            .await?;
        let mut idle = Map::new();
        idle.insert("time".to_owned(), json!({ "created": now }));
        idle.insert("outcome".to_owned(), Value::String(name.to_owned()));
        if let Some(error) = &error {
            idle.insert(
                "error".to_owned(),
                serde_json::to_value(error).expect("structured error is serializable"),
            );
        }
        let idle_id = MessageId::new();
        self.inner
            .config
            .store
            .append_message(NewMessage {
                id: idle_id.as_str().to_owned(),
                session_id: session.to_owned(),
                kind: "idle".to_owned(),
                data: Value::Object(idle),
                time_created: now,
            })
            .await?;
        self.emit(
            session,
            directory,
            SessionIdle {
                session_id: Runtime::session_id(session),
            },
        )
        .await?;
        self.emit(
            session,
            directory,
            SessionStatusUpdated {
                session_id: Runtime::session_id(session),
                status: SessionStatus::Idle,
            },
        )
        .await?;
        match outcome {
            ExecutionOutcome::Succeeded => {
                self.emit(
                    session,
                    directory,
                    SessionExecutionSucceeded {
                        session_id: Runtime::session_id(session),
                    },
                )
                .await?;
            }
            ExecutionOutcome::Failed(error) => {
                self.emit(
                    session,
                    directory,
                    SessionExecutionFailed {
                        session_id: Runtime::session_id(session),
                        error,
                    },
                )
                .await?;
            }
            ExecutionOutcome::Interrupted(reason) => {
                self.emit(
                    session,
                    directory,
                    SessionExecutionInterrupted {
                        session_id: Runtime::session_id(session),
                        reason,
                    },
                )
                .await?;
            }
        }
        Ok(())
    }
}
