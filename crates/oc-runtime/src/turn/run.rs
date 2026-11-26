use std::sync::Arc;

use oc_llm::LlmMessage;
use oc_types::MessageId;
use oc_types::payload::{CompactionTrigger, RequestInjection, SessionInboxDelivered};
use serde_json::Value;

use crate::prompt::{self, MAX_STEPS_PROMPT};
use crate::runtime::{CancelToken, Runtime, RuntimeError};
use crate::tokens;

use super::SessionExecutionStepExtended;
use super::assistant::assistant_text;
use super::promote_inbox_row;
use super::request_envelope::{self, RequestEnvelopeSeed};
use super::stats::TurnStats;
use super::stats::collect_tool_lines;
use super::types::ExecutionOutcome;
use super::types::ResolvedTurn;
use super::{budget, compaction, repetition, stall, verify};

/// Automatic compactions one turn may run, threshold and overflow combined
/// (`docs/04-agent-runtime.md`: "at most 3 auto-compactions per turn").
const MAX_AUTO_COMPACTIONS_PER_TURN: usize = 3;

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
        if changed {
            self.inner
                .subagents
                .lock()
                .expect("subagent state")
                .clear_queue_ids_seen(session);
        }
        Ok(changed)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn run_steps(
        &self,
        session: &str,
        turn: &mut ResolvedTurn,
        cancel: &Arc<CancelToken>,
        stats: &mut TurnStats,
    ) -> Result<ExecutionOutcome, RuntimeError> {
        let mut context = self.load_context(session).await?;
        stats.user_text = prompt::last_user_text(&context);
        // Re-trigger key of the last automatic compaction: the token estimate
        // observed immediately after it finished. A compaction that fails to
        // shrink the prompt must not run again for the same pressure, so the
        // guard fires only when the estimate grows past that post state. A
        // context-length key could not express that: a swap of equal-length
        // messages re-armed it, and a genuinely larger prompt could match an
        // older length.
        let mut compacted_at_estimate: Option<u64> = None;
        let mut auto_compactions = 0_usize;
        let mut length_continuations = 0_usize;
        let mut pending_output_continuation = false;
        let mut pending_verify_nudge: Option<String> = None;
        let mut pending_stall_notice: Option<String> = None;
        let mut stall_guard = stall::StallGuard::default();
        let mut step_number = 0_usize;
        // Publish the live step for `observer_pass` (`RootContext.step`). Every
        // `step_number += 1` below re-publishes at the top of the next
        // iteration, so the counter always names the request in flight.
        self.set_step(session, 0);
        // The live step budget. It starts at the configured `agent.steps` plus
        // every grant the durable log already records for this session's open
        // execution window ([`super::step_grants::replay`]), so the effective cap
        // is a function of committed state rather than process memory: a restart
        // rebuilds it from the `session.execution.step_extended` rows instead of
        // silently resetting to `turn.max_steps`. `turn.max_steps` remains the
        // initial cap the ceiling and the `StepLimited` count are expressed in.
        let policy = budget::BudgetPolicy::from_merged(self.inner.config.config.merged());
        let replayed = super::step_grants::replay(self, session, turn.max_steps, &policy).await?;
        if replayed.dropped > 0 {
            tracing::warn!(
                session = %session,
                dropped = replayed.dropped,
                cap = replayed.cap,
                "recorded step grants exceed the current step_budget policy; not inherited"
            );
        }
        let mut cap = replayed.cap;
        // Grants already committed to the log count against `max_extensions` and
        // the ceiling, so a resumed turn cannot grant a second time for an
        // extension the previous process already recorded.
        let mut grants: Vec<usize> = replayed.steps;
        // Tool evidence for the whole turn, in execution order. It is
        // accumulated from the steps that actually ran tools because the
        // limit-reaching request is sent with tools disabled and the streamer
        // skips every tool event on a `final_call` (`stream.rs`), so
        // `step.state.content` at the limit branch only ever holds text.
        let mut observations: Vec<budget::ToolObservation> = Vec::new();
        let compaction = self.compaction_config();
        let guard_armed = compaction.auto || compaction.ceiling_tokens.is_some();

        loop {
            // Recompute context ceiling/trigger from `turn.context_window` on every
            // iteration so mid-turn model fallbacks enforce the active model's window.
            let ceiling = if turn.context_window == 0 {
                u64::MAX
            } else {
                // The ceiling is derived from the model's real window; the old
                // hard-coded 150k cap is gone (`docs/04-agent-runtime.md`).
                tokens::ceiling(turn.context_window, compaction.ceiling_tokens)
            };
            let trigger = if turn.context_window == 0 {
                u64::MAX
            } else {
                tokens::trigger(ceiling, turn.context_window, compaction.buffer)
            };
            let observer_grant = usize::try_from(self.take_granted_steps(session)).unwrap_or(0);
            if observer_grant > 0 && cap >= policy.min_cap {
                let headroom =
                    budget::remaining_ceiling_with_policy(&grants, turn.max_steps, &policy);
                let granted = observer_grant.min(headroom);
                if granted > 0 {
                    cap = cap.saturating_add(granted);
                    grants.push(granted);
                    tracing::info!(
                        session = %session,
                        granted,
                        cap,
                        reason = "observer grant",
                        "step budget extended by observer"
                    );
                    self.emit(
                        session,
                        &turn.directory,
                        SessionExecutionStepExtended {
                            session_id: session.to_owned(),
                            steps: granted,
                            total_steps: cap,
                            grant_index: grants.len(),
                            reason: "observer grant",
                        },
                    )
                    .await?;
                }
            }
            // Publish the step this iteration is about to run. Placing it at
            // the loop top covers all three `step_number += 1` sites below
            // (length continuation, verification nudge, normal advance) with a
            // single write instead of three.
            self.set_step(session, u32::try_from(step_number).unwrap_or(u32::MAX));
            let published_cap = u32::try_from(cap).unwrap_or(u32::MAX);
            self.set_step_budget(session, published_cap);
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
            if auto_compactions < MAX_AUTO_COMPACTIONS_PER_TURN && guard_armed {
                let estimate = self.estimate_turn_tokens(session, turn, &context).await?;
                let grew_since_compaction =
                    compacted_at_estimate.is_none_or(|previous| estimate > previous);
                if estimate > trigger && grew_since_compaction {
                    // `guard` names the ceiling path even when `compaction.auto`
                    // armed it: the trigger explains the decision, and the wire
                    // `reason` stays `auto` for compatibility.
                    tracing::info!(
                        session = %session,
                        estimate,
                        trigger,
                        ceiling,
                        "threshold compaction"
                    );
                    let request =
                        super::compaction::CompactionRequest::new(CompactionTrigger::Guard)
                            .with_pressure(
                                estimate,
                                trigger,
                                (ceiling != u64::MAX).then_some(ceiling),
                            );
                    match self.compact(session, turn, &context, cancel, request).await {
                        Ok(()) => {
                            context = self.load_context(session).await?;
                            auto_compactions += 1;
                            compacted_at_estimate = Some(estimate);
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
                            compacted_at_estimate = Some(estimate);
                        }
                    }
                }
            }

            let context_pressure_notice = if turn.context_window > 0
                && context.len() >= 6
                && turn.tool_names.iter().any(|name| name == "compact_context")
            {
                let estimate = self.estimate_turn_tokens(session, turn, &context).await?;
                let effective_trigger = trigger.min(ceiling.min(turn.context_window * 85 / 100));
                if effective_trigger > 0
                    && estimate.saturating_mul(100) >= effective_trigger.saturating_mul(65)
                {
                    let pct = ((estimate.saturating_mul(100)) / turn.context_window).min(100);
                    Some(format!(
                        "<system-reminder>\nContext budget: ~{pct}% used \
                         (~{estimate}/{} tokens; compaction trigger {effective_trigger}). \
                         Call `compact_context` with a focused handoff `note` before starting a \
                         major new phase so compaction compresses cleanly at the turn \
                         boundary.\n</system-reminder>",
                        turn.context_window
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            // Evaluate automatic step-budget extension *before* treating this
            // step as `final_call` so an eligible extension continues seamlessly
            // with tools enabled instead of first forcing a tool-less summary.
            if step_number >= cap && step_number > 0 && cap >= policy.min_cap {
                let decision = budget::evaluate_with_policy(
                    &observations,
                    stats.tool_calls,
                    &grants,
                    turn.max_steps,
                    &policy,
                );
                if let Some(granted) = decision.grant.filter(|granted| *granted > 0) {
                    cap = cap.saturating_add(granted);
                    grants.push(granted);
                    let published_cap = u32::try_from(cap).unwrap_or(u32::MAX);
                    self.set_step_budget(session, published_cap);
                    tracing::info!(
                        session = %session,
                        granted,
                        cap,
                        reason = decision.reason,
                        "step budget extended"
                    );
                    self.emit(
                        session,
                        &turn.directory,
                        SessionExecutionStepExtended {
                            session_id: session.to_owned(),
                            steps: granted,
                            total_steps: cap,
                            grant_index: grants.len(),
                            reason: decision.reason,
                        },
                    )
                    .await?;
                }
            }

            let final_call = step_number >= cap;
            // `base` is the request's pure function of the durable rows; every
            // splice below is model-visible but not recomputable, so each one is
            // recorded in `injections` and published with the request envelope.
            let base = context.clone();
            let mut injected: Vec<RequestInjection> = Vec::new();
            let mut messages = context.clone();
            if let Some(supplement) = &turn.memory_supplement {
                messages.push(LlmMessage::user(supplement.clone()));
                injected.push(RequestInjection::new(
                    request_envelope::KIND_MEMORY_SUPPLEMENT,
                    supplement.clone(),
                ));
            }
            if pending_output_continuation {
                messages.push(LlmMessage::user(prompt::CONTINUE_AFTER_OUTPUT_LIMIT_PROMPT));
                injected.push(RequestInjection::new(
                    request_envelope::KIND_OUTPUT_CONTINUATION,
                    prompt::CONTINUE_AFTER_OUTPUT_LIMIT_PROMPT,
                ));
            }
            if let Some(nudge) = pending_verify_nudge.take() {
                messages.push(LlmMessage::user(nudge.clone()));
                injected.push(RequestInjection::new(
                    request_envelope::KIND_VERIFY_NUDGE,
                    nudge,
                ));
            }
            if let Some(notice) = pending_stall_notice.take() {
                messages.push(LlmMessage::user(notice.clone()));
                injected.push(RequestInjection::new(
                    request_envelope::KIND_STALL_NOTICE,
                    notice,
                ));
            }
            if let Some(notice) = context_pressure_notice {
                messages.push(LlmMessage::user(notice.clone()));
                injected.push(RequestInjection::new(
                    request_envelope::KIND_CONTEXT_PRESSURE,
                    notice,
                ));
            }
            if final_call {
                messages.push(LlmMessage::user(MAX_STEPS_PROMPT));
                injected.push(RequestInjection::new(
                    request_envelope::KIND_MAX_STEPS,
                    MAX_STEPS_PROMPT,
                ));
            } else if cap >= policy.min_cap {
                let remaining = cap - step_number;
                let can_auto_extend =
                    budget::can_extend_with_policy(&grants, turn.max_steps, cap, &policy);
                if !can_auto_extend && remaining <= 5 {
                    let warning = format!(
                        "<system-reminder>\n{remaining} tool-enabled step(s) remain before the hard \
                         step limit ({}/{}). {}\n</system-reminder>",
                        step_number + 1,
                        cap,
                        prompt::STEP_BUDGET_WARNING
                    );
                    messages.push(LlmMessage::user(warning.clone()));
                    injected.push(RequestInjection::new(
                        request_envelope::KIND_STEP_BUDGET,
                        warning,
                    ));
                } else if can_auto_extend && remaining <= 3 {
                    let checkpoint = format!(
                        "<system-reminder>\nCheckpoint: {remaining} tool-enabled step(s) remain in the \
                         current slice ({}/{cap}); automatic extension applies while tool progress \
                         continues. Keep investigating systematically.\n</system-reminder>",
                        step_number + 1,
                    );
                    messages.push(LlmMessage::user(checkpoint.clone()));
                    injected.push(RequestInjection::new(
                        request_envelope::KIND_STEP_BUDGET,
                        checkpoint,
                    ));
                }
            }
            let envelope = RequestEnvelopeSeed::new(
                u32::try_from(step_number).unwrap_or(u32::MAX),
                &base,
                injected,
            );
            let mut step = match self
                .stream_step(session, turn, &messages, final_call, cancel, envelope)
                .await
            {
                Ok(step) => step,
                Err(RuntimeError::Cancelled) => {
                    return Ok(ExecutionOutcome::Interrupted(cancel.reason()));
                }
                Err(error)
                    if auto_compactions < MAX_AUTO_COMPACTIONS_PER_TURN
                        && compaction::is_context_overflow_message(&error.to_string()) =>
                {
                    // A provider overflow is the same automatic budget as the
                    // threshold guard: one turn can compact at most
                    // `MAX_AUTO_COMPACTIONS_PER_TURN` times in total, so a loop
                    // of overflows cannot bypass the documented cap.
                    let request = compaction::CompactionRequest::new(CompactionTrigger::Overflow);
                    match self.compact(session, turn, &context, cancel, request).await {
                        Ok(()) => {
                            auto_compactions += 1;
                            context = self.load_context(session).await?;
                            compacted_at_estimate = None;
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
                        pending_stall_notice.get_or_insert(warning);
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
            budget::observe(&mut observations, &step.state.content);
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
                    let radar = oc_tools::code_map::collect_post_edit_radar(
                        std::path::Path::new(&turn.directory),
                        &stats.changed_code_files,
                        true,
                    )
                    .await;
                    pending_verify_nudge = Some(verify::build_verification_nudge_with_radar(
                        &stats.changed_code_files,
                        Some(&radar),
                    ));
                    step_number += 1;
                    context = self.load_context(session).await?;
                    continue;
                }
            }
            if final_call || step.calls.is_empty() {
                stats.assistant_text = assistant_text(&step.state);
                if final_call && step_number > 0 {
                    // Any eligible automatic or observer extension already ran at the
                    // top of this step before `final_call` disabled tools; reaching
                    // here means the hard ceiling was genuinely exhausted.
                    return Ok(ExecutionOutcome::StepLimited {
                        steps: u64::try_from(cap).unwrap_or(u64::MAX),
                    });
                }
                return Ok(ExecutionOutcome::Succeeded);
            }
            step_number += 1;
            context = self.load_context(session).await?;
        }
    }
}
