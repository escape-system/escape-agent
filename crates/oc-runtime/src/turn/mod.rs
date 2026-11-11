//! The turn loop: prompt promotion, streaming, tools, compaction and settlement.
//!
//! Durable message rows are written before the events that describe them. The
//! assistant message is appended once per step and rewritten at text end, tool
//! call, tool result and step end; deltas are never persisted.

use std::sync::Arc;

use oc_llm::LlmMessage;
use oc_store::NewMessage;
use oc_types::payload::{
    SessionExecutionFailed, SessionExecutionInterrupted, SessionExecutionStepLimit,
    SessionExecutionSucceeded, SessionIdle, SessionInboxDelivered, SessionStatusUpdated,
};
use oc_types::{InterruptReason, MessageId, SessionStatus};
use serde_json::{Map, Value, json};

use crate::prompt::{self, MAX_STEPS_PROMPT};
use crate::runtime::{ActorHandle, CancelToken, Runtime, RuntimeError, TerminalState};
use crate::tokens;

mod assistant;
mod compaction;
mod finalize;
mod permissions;
mod repetition;
mod session;
mod stall;
mod stats;
mod stream;
mod tools;
mod types;
mod verify;

pub(crate) use tools::report_tool_progress;
pub(crate) use types::{PendingToolCall, ToolOutcome};

use assistant::assistant_text;
use stats::TurnStats;
use stats::collect_tool_lines;
use types::{ExecutionOutcome, ResolvedTurn};

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

/// Lowercase wire value of an [`InterruptReason`] (`user`, `shutdown`,
/// `superseded`, `inactivity`), used for the durable `session_v2.idle_reason`.
fn interrupt_reason_str(reason: InterruptReason) -> &'static str {
    match reason {
        InterruptReason::User => "user",
        InterruptReason::Shutdown => "shutdown",
        InterruptReason::Superseded => "superseded",
        InterruptReason::Inactivity => "inactivity",
    }
}

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
    // From this commit onward the session is durably busy: every exit from this
    // function must publish the terminal pair, including the actor failure path
    // that fires when an `Err` escapes the drain loop.
    handle.terminal.begin(true);
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
                    &handle.terminal,
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
                &handle.terminal,
            )
            .await?;
        return Ok(());
    }

    let mut stats = TurnStats::default();
    let outcome = runtime
        .run_steps(session, &turn, cancel, &mut stats)
        .await?;
    // A step-limited turn still answered on its final, tool-less request; it is
    // treated like a success for memory/scribe, but settles with the qualifier.
    let answered = matches!(
        outcome,
        ExecutionOutcome::Succeeded | ExecutionOutcome::StepLimited { .. }
    );
    if answered {
        runtime.touch_memory(&turn.memory_ids).await;
    }
    runtime
        .settle(session, &directory, outcome, &handle.terminal)
        .await?;
    if answered {
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
        let mut pending_verify_nudge: Option<String> = None;
        let mut pending_stall_notice: Option<String> = None;
        let mut stall_guard = stall::StallGuard::default();
        let mut step_number = 0_usize;
        let compaction = self.compaction_config();
        let ceiling = if turn.context_window == 0 {
            u64::MAX
        } else {
            // The ceiling is derived from the model's real window; the old
            // hard-coded 150k cap is gone (`docs/04-agent-runtime.md`).
            tokens::ceiling(turn.context_window, compaction.ceiling_tokens)
        };
        let guard_armed = compaction.auto || compaction.ceiling_tokens.is_some();
        let trigger = if turn.context_window == 0 {
            u64::MAX
        } else {
            tokens::trigger(ceiling, turn.context_window, compaction.buffer)
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
            if auto_compactions < 3 && guard_armed && last_compacted_len != Some(context.len()) {
                let estimate = self.estimate_turn_tokens(session, turn, &context).await?;
                if estimate > trigger {
                    // `guard` is only reachable with an explicit ceiling and
                    // `auto=false`; everything else keeps the historical
                    // `auto` reason. The wire reason stays `auto` for both
                    // (`event_reason`).
                    let reason = if compaction.auto { "auto" } else { "guard" };
                    tracing::info!(
                        session = %session,
                        estimate,
                        trigger,
                        ceiling,
                        reason,
                        "threshold compaction"
                    );
                    match self.compact(session, turn, &context, cancel, reason).await {
                        Ok(()) => {
                            context = self.load_context(session).await?;
                            auto_compactions += 1;
                            last_compacted_len = Some(context.len());
                        }
                        Err(RuntimeError::Cancelled) => {
                            return Ok(ExecutionOutcome::Interrupted(cancel.reason()));
                        }
                        Err(error) => {
                            tracing::warn!(
                                session = %session,
                                %error,
                                "auto-compaction failed; continuing turn without wedging"
                            );
                            last_compacted_len = Some(context.len());
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
            if let Some(nudge) = pending_verify_nudge.take() {
                messages.push(LlmMessage::user(nudge));
            }
            if let Some(notice) = pending_stall_notice.take() {
                messages.push(LlmMessage::user(notice));
            }
            if final_call {
                messages.push(LlmMessage::user(MAX_STEPS_PROMPT));
            } else if turn.max_steps >= 10 {
                let remaining = turn.max_steps - step_number;
                if remaining == 5 || remaining == 1 {
                    messages.push(LlmMessage::user(format!(
                        "<system-reminder>\nStep budget notice: {remaining} tool-enabled step(s) remain \
                         before the hard step limit ({}/{}) disables tools. Finish any pending \
                         verification or git commit now and prepare your final summary.\n</system-reminder>",
                        step_number + 1,
                        turn.max_steps
                    )));
                }
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
                for call in &step.calls {
                    if let Some(warning) = stall_guard.record_and_check(&call.name, &call.input) {
                        pending_stall_notice = Some(warning);
                        break;
                    }
                }
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
            collect_tool_lines(stats, &step.state, step_number);
            if !final_call
                && step.calls.is_empty()
                && step.state.finish == Some(oc_types::message::FinishReason::Length)
                && length_continuations < 2
            {
                let text = assistant_text(&step.state);
                if repetition::is_repetition_dominated(&text) {
                    tracing::warn!(
                        session = %session,
                        "suppressed length continuation because response is repetition-dominated"
                    );
                } else {
                    length_continuations += 1;
                    pending_output_continuation = true;
                    step_number += 1;
                    context = self.load_context(session).await?;
                    continue;
                }
            }
            if !final_call
                && step.calls.is_empty()
                && verify::verify_on_stop_enabled(&self.inner.config.config)
            {
                let needs_verification = !stats.changed_code_files.is_empty()
                    && match (stats.last_verification_step, stats.last_code_mutation_step) {
                        (Some(v), Some(m)) => v < m,
                        (None, Some(_)) => true,
                        _ => false,
                    };
                if needs_verification && stats.verify_nudges < 2 {
                    stats.verify_nudges += 1;
                    pending_verify_nudge =
                        Some(verify::build_verification_nudge(&stats.changed_code_files));
                    step_number += 1;
                    context = self.load_context(session).await?;
                    continue;
                }
            }
            if final_call || step.calls.is_empty() {
                stats.assistant_text = assistant_text(&step.state);
                if final_call && step_number > 0 {
                    // The budget, not the model, ended the turn: tools were
                    // disabled for this final request. The outcome carries the
                    // consumed budget so settle can qualify the terminal and
                    // persist the honest session outcome.
                    return Ok(ExecutionOutcome::StepLimited {
                        steps: u64::try_from(turn.max_steps).unwrap_or(u64::MAX),
                    });
                }
                return Ok(ExecutionOutcome::Succeeded);
            }
            step_number += 1;
            context = self.load_context(session).await?;
        }
    }

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
        let Ok(turn) = self.resolve_turn(&info, &directory) else {
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

    async fn settle(
        &self,
        session: &str,
        directory: &str,
        outcome: ExecutionOutcome,
        terminal: &TerminalState,
    ) -> Result<(), RuntimeError> {
        match &outcome {
            ExecutionOutcome::Failed(_) => self.note_observer_execution(session, true).await,
            ExecutionOutcome::Interrupted(_) => self.note_observer_execution(session, false).await,
            ExecutionOutcome::Succeeded | ExecutionOutcome::StepLimited { .. } => {}
        }
        let now = oc_types::now_millis();
        // The *stored* outcome is the honest internal literal `step_limit`
        // (a completed turn whose step budget ended it), while the *wire*
        // terminal stays `session.execution.succeeded` and the idle message
        // keeps the literal `succeeded`: `SessionOutcome` has exactly three
        // literals, so a fourth one on a wire event/message would break the
        // generated-types contract. `oc_types::SessionInfo` reads an unknown
        // stored outcome as absent instead of failing, which is what lets the
        // next execution of this session parse at all.
        let (stored, name, error, reason) = match &outcome {
            ExecutionOutcome::Succeeded => ("succeeded", "succeeded", None, None),
            ExecutionOutcome::StepLimited { .. } => ("step_limit", "succeeded", None, None),
            ExecutionOutcome::Failed(error) => ("failed", "failed", Some(error.clone()), None),
            ExecutionOutcome::Interrupted(reason) => (
                "interrupted",
                "interrupted",
                None,
                Some(interrupt_reason_str(*reason)),
            ),
        };
        // A step-limited answer keeps the step-limit grammar as its reason;
        // the store persists it next to the `step_limit` outcome.
        let reason = match &outcome {
            ExecutionOutcome::StepLimited { steps } => Some(oc_types::step_limit_reason(*steps)),
            _ => reason.map(str::to_owned),
        };
        // Hoisted out of the variant before `publish_outcome_terminal` consumes
        // the outcome. The qualifier is emitted only with the first terminal.
        let step_limit = match &outcome {
            ExecutionOutcome::StepLimited { steps } => Some(*steps),
            _ => None,
        };
        self.clear_background_request(session);
        // The idle row and its event are committed/published before the outcome
        // event, so a crash between them leaves a session without an
        // `execution.*` terminal rather than a terminal followed by a busy row.
        self.publish_idle_terminal(
            session,
            directory,
            now,
            stored,
            name,
            error.as_ref(),
            reason.as_deref(),
            terminal,
        )
        .await?;
        let first_outcome = !terminal.outcome_published();
        self.publish_outcome_terminal(session, directory, outcome, terminal)
            .await?;
        // The qualifier is published strictly after the terminal event is
        // durable, so its sequence number orders it after `succeeded`. Guarded
        // by the same idempotency flag as the terminal itself.
        if first_outcome && let Some(steps) = step_limit {
            self.emit(
                session,
                directory,
                SessionExecutionStepLimit {
                    session_id: session.to_owned(),
                    steps,
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Writes the idle row, then publishes `session.idle` and
    /// `session.status{idle}`.
    ///
    /// `stored` is the internal `session_v2.idle_outcome` literal (it may be
    /// `step_limit`, which the wire does not know); `name` is the wire literal
    /// for the persisted idle message. Both are written in the same step, so the
    /// durable row and the transcript can never disagree about *that* a turn
    /// settled — only about the internal qualifier.
    ///
    /// Idempotent: the terminal guard records that the pair was published so a
    /// later recovery pass (the actor failure path) does not duplicate the
    /// ephemeral events.
    // Settle helper: the flat parameter list mirrors the persisted idle row and
    // its event fields one-for-one; grouping it into a struct would only add
    // ceremony at the two call sites.
    #[allow(clippy::too_many_arguments)]
    async fn publish_idle_terminal(
        &self,
        session: &str,
        directory: &str,
        now: i64,
        stored: &str,
        name: &str,
        error: Option<&oc_types::StructuredError>,
        reason: Option<&str>,
        terminal: &TerminalState,
    ) -> Result<(), RuntimeError> {
        if terminal.idle_published() {
            return Ok(());
        }
        self.inner
            .config
            .store
            .mark_session_idle_with_reason(session, now, stored, reason)
            .await?;
        let mut idle = Map::new();
        idle.insert("time".to_owned(), json!({ "created": now }));
        idle.insert("outcome".to_owned(), Value::String(name.to_owned()));
        if let Some(error) = error {
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
        terminal.mark_idle();
        Ok(())
    }

    /// Publishes the `session.execution.*` outcome exactly once per turn.
    async fn publish_outcome_terminal(
        &self,
        session: &str,
        directory: &str,
        outcome: ExecutionOutcome,
        terminal: &TerminalState,
    ) -> Result<(), RuntimeError> {
        if terminal.outcome_published() {
            return Ok(());
        }
        match outcome {
            // A step-limited turn keeps the same wire terminal as a voluntary
            // stop; the qualifier is `session.execution.step_limit`, emitted by
            // `settle` after this event is durable.
            ExecutionOutcome::Succeeded | ExecutionOutcome::StepLimited { .. } => {
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
        terminal.mark_outcome();
        Ok(())
    }

    /// Terminal repair after an error escaped the drain loop.
    ///
    /// The turn died before `settle` could run, so two things are missing: the
    /// assistant/tool rows the step left open, and the terminal event pair.
    /// Repair the rows first (durable), then publish `session.idle` and
    /// `session.execution.failed` — in that order, matching [`settle`] — so a
    /// consumer never sees a terminal execution event for a still-busy session.
    ///
    /// Idempotent with a settle that already ran: the shared [`TerminalState`]
    /// guard suppresses duplicate `session.idle` / `execution.*` publications
    /// and a fully settled turn has no open rows to repair.
    pub(crate) async fn finalize_actor_failure(
        &self,
        session: &str,
        error: &RuntimeError,
        terminal: &TerminalState,
    ) {
        let directory = match self.inner.config.store.get_session(session).await {
            Ok(Some(value)) => value
                .pointer("/location/directory")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            Ok(None) => String::new(),
            Err(lookup) => {
                tracing::warn!(session = %session, error = %lookup, "actor failure: session lookup failed");
                String::new()
            }
        };
        if let Err(repair) = self.repair_open_turn_rows(session).await {
            tracing::error!(
                session = %session,
                error = %repair,
                "actor failure: could not repair the rows left open by the stopped turn"
            );
        }
        if !terminal.is_owed() {
            return;
        }
        self.note_observer_execution(session, true).await;
        self.clear_background_request(session);
        let structured = error.structured();
        let now = oc_types::now_millis();
        if let Err(idle) = self
            .publish_idle_terminal(
                session,
                &directory,
                now,
                "failed",
                "failed",
                Some(&structured),
                None,
                terminal,
            )
            .await
        {
            // No committed idle row means the busy flag may still be set and no
            // terminal can be published ahead of it.
            tracing::error!(session = %session, error = %idle, "actor failure: session.idle was not published");
            return;
        }
        if let Err(failed) = self
            .publish_outcome_terminal(
                session,
                &directory,
                ExecutionOutcome::Failed(structured),
                terminal,
            )
            .await
        {
            tracing::error!(session = %session, error = %failed, "actor failure: session.execution.failed was not published");
        }
    }

    /// Closes every assistant/tool/compaction row the crashed turn left open,
    /// reusing the boot-recovery repair so the two paths cannot drift.
    async fn repair_open_turn_rows(&self, session: &str) -> Result<(), RuntimeError> {
        for message in self.list_all_messages(session).await? {
            let repaired = match message.get("type").and_then(Value::as_str) {
                Some("assistant") => self.repair_assistant(&message).await?,
                Some("compaction") => crate::recover::repair_compaction(&message),
                _ => None,
            };
            let Some(body) = repaired else {
                continue;
            };
            let id = message
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            self.inner
                .config
                .store
                .update_message(session, id, body)
                .await?;
        }
        Ok(())
    }
}
