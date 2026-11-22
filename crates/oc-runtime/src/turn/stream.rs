use std::sync::Arc;

use futures_util::StreamExt;
use oc_llm::{LlmMessage, LlmOptions, LlmRequest};
use oc_store::NewMessage;
use oc_types::message::{AssistantPart, FinishReason, StructuredError, ToolState};
use oc_types::payload::{
    SessionStepEnded, SessionStepFailed, SessionStepStarted, SessionStepStreamed, SessionTextDelta,
    SessionTextEnded, SessionTextStarted, SessionToolCalled, SessionToolInputDelta,
    SessionToolInputEnded, SessionToolInputStarted, SessionUsageUpdated,
};
use oc_types::{MessageId, ModelRef, Tokens};

use crate::runtime::{CancelToken, Runtime, RuntimeError};

use super::assistant::{AssistantState, ordinal_u32};
use super::tools::strip_expectation;
use super::types::{
    PendingToolCall, ResolvedTurn, StepAttempt, StepRun, StreamFailure, finish_reason,
    tokens_from_usage,
};

/// Turn a raw provider message into a typed failure when the text names a known
/// class.
///
/// Provider streams report some failures only as opaque text. Classifying it
/// here means a retired model, an exhausted quota or a rate limit is recorded
/// against the model (and can be acted on) instead of surfacing as an
/// unclassifiable `Provider` error. Unrecognized text keeps the historical
/// `Provider` shape byte for byte.
pub(super) fn classified_provider_error(message: &str) -> oc_llm::LlmError {
    let kind = oc_llm::classify_provider_message(message);
    if kind == oc_llm::FailureKind::Unknown {
        oc_llm::LlmError::Provider {
            message: message.to_owned(),
        }
    } else {
        oc_llm::LlmError::Failure {
            kind,
            message: message.to_owned(),
        }
    }
}

/// Normalize any [`oc_llm::LlmError`] (pre-stream or in-band) into a typed
/// [`oc_llm::LlmError::Failure`] when its failure class is known so key rotation
/// and model fallback handle transport, timeout, and transient HTTP errors
/// consistently instead of aborting the turn.
fn classify_llm_error(error: oc_llm::LlmError) -> oc_llm::LlmError {
    match error {
        oc_llm::LlmError::Provider { message } => classified_provider_error(&message),
        oc_llm::LlmError::Transport { message } => oc_llm::LlmError::Failure {
            kind: oc_llm::FailureKind::Transport,
            message,
        },
        oc_llm::LlmError::Timeout { phase } => oc_llm::LlmError::Failure {
            kind: oc_llm::FailureKind::Transport,
            message: format!("request timed out while {phase}"),
        },
        oc_llm::LlmError::Http {
            status,
            message,
            retry_after_ms,
        } => {
            let msg_kind = oc_llm::classify_provider_message(&message);
            let mapped = if msg_kind == oc_llm::FailureKind::Unknown {
                match status {
                    401 | 403 => Some(oc_llm::FailureKind::Auth),
                    429 => Some(oc_llm::FailureKind::RateLimit),
                    408 | 425 | 500..=599 => Some(oc_llm::FailureKind::Transport),
                    _ => None,
                }
            } else {
                Some(msg_kind)
            };
            if let Some(kind) = mapped {
                oc_llm::LlmError::Failure {
                    kind,
                    message: format!("HTTP {status}: {message}"),
                }
            } else {
                oc_llm::LlmError::Http {
                    status,
                    message,
                    retry_after_ms,
                }
            }
        }
        other => other,
    }
}

/// Upper bound on credential attempts for one model candidate.
///
/// A pool larger than this is a configuration accident; the cap keeps one step
/// from turning into an unbounded provider loop while still covering every
/// realistic backup-key count (contract §3).
const MAX_KEY_ATTEMPTS: usize = 8;

/// The failure class that justifies rotating to another credential, if any.
///
/// Only key-level classes rotate: `Auth`, `Quota` and `RateLimit` say the
/// credential cannot serve now, while `ModelUnavailable` / `InvalidRequest` /
/// `EmptyResponse` / `Unknown` say nothing about the key (a different key would
/// repeat the same answer), so those keep today's model-chain behavior.
/// `Http` statuses are mapped as a defensive fallback for transports that report
/// the status before it is classified.
fn key_failure_kind(failure: &StreamFailure) -> Option<oc_llm::FailureKind> {
    match &failure.error {
        oc_llm::LlmError::Failure { kind, .. } => matches!(
            kind,
            oc_llm::FailureKind::Auth | oc_llm::FailureKind::Quota | oc_llm::FailureKind::RateLimit
        )
        .then_some(*kind),
        oc_llm::LlmError::Http { status, .. } => match status {
            401..=403 => Some(oc_llm::FailureKind::Auth),
            429 => Some(oc_llm::FailureKind::RateLimit),
            _ => None,
        },
        _ => None,
    }
}

impl Runtime {
    pub(super) async fn persist_assistant(
        &self,
        session: &str,
        state: &AssistantState,
    ) -> Result<(), RuntimeError> {
        self.inner
            .config
            .store
            .update_message(session, state.id.as_str(), state.data())
            .await?;
        Ok(())
    }

    async fn fail_step(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        mut state: AssistantState,
        error: StructuredError,
        tokens: Option<Tokens>,
        cost: f64,
    ) -> Result<(), RuntimeError> {
        state.error = Some(error.clone());
        state.finish = Some(FinishReason::Error);
        state.completed = Some(oc_types::now_millis());
        state.tokens = tokens.clone();
        state.cost = Some(cost);
        self.persist_assistant(session, &state).await?;
        self.emit(
            session,
            &turn.directory,
            SessionStepFailed {
                session_id: Runtime::session_id(session),
                assistant_message_id: state.id.clone(),
                error,
                finish: Some(FinishReason::Error),
                raw_finish: state.raw_finish.clone(),
                native: None,
                cost: Some(cost),
                tokens,
                snapshot: None,
                files: None,
            },
        )
        .await?;
        Ok(())
    }

    /// Stream one step using only the resolved model (rotating healthy pool
    /// keys on that same model via `stream_model_attempts`). Automatic fallback
    /// to other models in `models.chain` is disabled so errors surface directly
    /// to the caller/main agent.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn stream_step(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        messages: &[LlmMessage],
        final_call: bool,
        cancel: &Arc<CancelToken>,
    ) -> Result<StepRun, RuntimeError> {
        let mut reuse: Option<AssistantState> = None;
        let attempt = self
            .stream_model_attempts(
                session,
                turn,
                &turn.model,
                &turn.model_ref,
                messages,
                final_call,
                cancel,
                &mut reuse,
            )
            .await?;
        let failure = match attempt {
            StepAttempt::Done(run) => return Ok(run),
            StepAttempt::Retry(failure) => failure,
        };
        let StreamFailure {
            error,
            state,
            kind: _,
            reusable: _,
        } = failure;
        let original = RuntimeError::Llm(error);
        let tokens = state.tokens.clone();
        let cost = state.cost.unwrap_or(0.0);
        let structured = original.structured();
        self.fail_step(session, turn, state, structured, tokens, cost)
            .await?;
        Err(original)
    }

    /// One model candidate with credential rotation.
    ///
    /// The pool is ordered by [`oc_llm::KeyPool::select`] (active key first,
    /// cooling keys removed). Each key gets one attempt on the SAME model and the
    /// SAME assistant row: a key-level failure marks the key, moves the durable
    /// primary pointer when the dead key was primary, and retries immediately.
    ///
    /// A key rotation is not a model fallback: no `session.model.fallback` event
    /// is published here, and `models.chain` is untouched. When every key is
    /// exhausted the last failure is returned to [`Runtime::stream_step`],
    /// which surfaces it to the caller without switching models.
    /// `MAX_KEY_ATTEMPTS` bounds the work even if a pool somehow reports more
    /// keys than that.
    #[allow(clippy::too_many_arguments)]
    async fn stream_model_attempts(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        model: &oc_llm::ResolvedModel,
        model_ref: &ModelRef,
        messages: &[LlmMessage],
        final_call: bool,
        cancel: &Arc<CancelToken>,
        reuse: &mut Option<AssistantState>,
    ) -> Result<StepAttempt, RuntimeError> {
        let provider = model.provider.id.clone();
        let now_ms = oc_types::now_millis();
        let hook_owns_pool = self
            .inner
            .config
            .client
            .hook()
            .is_some_and(|hook| hook.handles(&LlmRequest::new(model.clone())));
        let pool = if hook_owns_pool {
            Vec::new()
        } else {
            self.credential_pool(&provider, now_ms).await
        };
        // An empty pool means "no stored credential and no environment key": the
        // client resolves the provider's environment on its own, so the request
        // is attempted exactly once with `api_key: None` — today's behavior.
        let attempts: Vec<Option<oc_llm::PoolKey>> = if pool.is_empty() {
            vec![None]
        } else {
            pool.into_iter().take(MAX_KEY_ATTEMPTS).map(Some).collect()
        };
        let total = attempts.len();
        let mut last: Option<StreamFailure> = None;
        for (index, candidate) in attempts.into_iter().enumerate() {
            let api_key = candidate.as_ref().map(|entry| entry.key.clone());
            let attempt = self
                .stream_once(
                    session,
                    turn,
                    model,
                    model_ref,
                    messages,
                    final_call,
                    cancel,
                    reuse.take(),
                    api_key,
                )
                .await?;
            let failure = match attempt {
                StepAttempt::Done(run) => return Ok(StepAttempt::Done(run)),
                StepAttempt::Retry(failure) => failure,
            };
            let Some(credential) = candidate.as_ref() else {
                // No key to rotate away from: keep the historical behavior.
                return Ok(StepAttempt::Retry(failure));
            };
            let rotated = key_failure_kind(&failure).is_some();
            if !rotated {
                return Ok(StepAttempt::Retry(failure));
            }
            let kind = failure.kind;
            self.mark_key_failure(&provider, credential.credential_id.as_deref(), kind, now_ms)
                .await;
            let last_attempt = index + 1 >= total;
            // The row is reusable only while nothing reached the provider
            // surface; once a key did stream output the row is durable as a
            // failed attempt (`reusable == false`) and the next key starts a new
            // one. Either way the *step* stays the same and no model switch is
            // announced.
            if !failure.reusable {
                if last_attempt {
                    last = Some(failure);
                    break;
                }
                let original = RuntimeError::Llm(failure.error);
                let tokens = failure.state.tokens.clone();
                let cost = failure.state.cost.unwrap_or(0.0);
                let structured = original.structured();
                self.fail_step(session, turn, failure.state, structured, tokens, cost)
                    .await?;
                *reuse = None;
                continue;
            }
            if last_attempt {
                last = Some(failure);
                break;
            }
            *reuse = Some(failure.state);
        }
        Ok(StepAttempt::Retry(last.expect(
            "a pool with at least one usable key yields a failure or a success",
        )))
    }

    /// Record a key-level failure and keep the durable primary pointer healthy.
    ///
    /// The store's pool is the source of truth for order, so the pointer only
    /// moves when the failing key *was* primary: a restart would otherwise pick
    /// the dead key first again. `set_credential_primary` reuses
    /// `activate_credential`'s transaction, so kill -9 safety is unchanged.
    async fn mark_key_failure(
        &self,
        provider: &str,
        credential_id: Option<&str>,
        kind: oc_llm::FailureKind,
        now_ms: i64,
    ) {
        self.key_pool().mark(provider, credential_id, kind, now_ms);
        let Some(failed) = credential_id else {
            return;
        };
        let Ok(rows) = self.inner.config.store.credential_pool(provider).await else {
            return;
        };
        let Some(primary) = rows.first() else {
            return;
        };
        if primary.id != failed {
            return;
        }
        let Some(next) = rows.iter().find(|row| row.id != failed) else {
            return;
        };
        match self
            .inner
            .config
            .store
            .set_credential_primary(provider, &next.id)
            .await
        {
            Ok(true) => {}
            Ok(false) => tracing::debug!(
                provider,
                credential = %next.id,
                "rotation could not move the primary credential"
            ),
            Err(error) => tracing::warn!(
                %error,
                provider,
                credential = %next.id,
                "rotation could not move the primary credential"
            ),
        }
    }

    /// One request attempt. A classifiable failure is returned to the caller
    /// instead of being recorded, so [`Runtime::stream_step`] can decide whether
    /// the chain advances; every other failure keeps the historical
    /// fail-then-error behavior.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    async fn stream_once(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        model: &oc_llm::ResolvedModel,
        model_ref: &ModelRef,
        messages: &[LlmMessage],
        final_call: bool,
        cancel: &Arc<CancelToken>,
        reuse: Option<AssistantState>,
        api_key: Option<String>,
    ) -> Result<StepAttempt, RuntimeError> {
        let created = oc_types::now_millis();
        // Nothing streamed on the previous attempt: the row is rewritten for
        // the new model instead of appending a second assistant message.
        let mut state = if let Some(mut state) = reuse {
            state.model = model_ref.clone();
            self.persist_assistant(session, &state).await?;
            state
        } else {
            let state = AssistantState::new(
                MessageId::new(),
                turn.agent_id.clone(),
                model_ref.clone(),
                created,
            );
            self.inner
                .config
                .store
                .append_message(NewMessage {
                    id: state.id.as_str().to_owned(),
                    session_id: session.to_owned(),
                    kind: "assistant".to_owned(),
                    data: state.data(),
                    time_created: created,
                })
                .await?;
            self.emit(
                session,
                &turn.directory,
                SessionStepStarted {
                    session_id: Runtime::session_id(session),
                    assistant_message_id: state.id.clone(),
                    agent: turn.agent_id.clone(),
                    model: model_ref.clone(),
                    snapshot: None,
                    started: created,
                },
            )
            .await?;
            state
        };
        let id = state.id.clone();

        let options = LlmOptions {
            api_key,
            tool_choice: final_call.then(|| "none".to_owned()),
            cache_key: Some(session.to_owned()),
            ..LlmOptions::default()
        };
        let request = LlmRequest {
            model: model.clone(),
            system: turn.system.clone(),
            messages: messages.to_vec(),
            tools: turn.tools.clone(),
            options,
        };

        let stream_result = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = self.inner.model.stream(request) => Some(result),
        };
        let Some(stream_result) = stream_result else {
            self.schedule_finalizer(session.to_owned(), turn.directory.clone(), state)
                .await;
            return Err(RuntimeError::Cancelled);
        };
        let mut stream = match stream_result {
            Ok(stream) => stream,
            Err(error) => {
                // Classify all pre-stream provider/transport/timeout/HTTP errors
                // uniformly so fallback/rotation recovers reliably.
                let error = classify_llm_error(error);
                // Copy the class out before moving `error`: `FailureKind` is
                // `Copy`, so the borrow ends with the `if let` arm.
                if let oc_llm::LlmError::Failure { kind, .. } = &error {
                    let kind = *kind;
                    // Nothing reached the provider surface yet, so the row is
                    // still reusable for the next candidate.
                    return Ok(StepAttempt::Retry(StreamFailure {
                        error,
                        state,
                        kind,
                        reusable: true,
                    }));
                }
                let runtime_error = RuntimeError::Llm(error);
                self.fail_step(session, turn, state, runtime_error.structured(), None, 0.0)
                    .await?;
                return Err(runtime_error);
            }
        };
        let mut calls = Vec::new();
        let mut provider_error = None;
        loop {
            let event = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.schedule_finalizer(session.to_owned(), turn.directory.clone(), state)
                        .await;
                    return Err(RuntimeError::Cancelled);
                }
                event = stream.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            match event {
                oc_llm::LlmEvent::StepStart => {}
                oc_llm::LlmEvent::TextStart => {
                    self.close_reasoning(session, turn, &mut state).await?;
                    self.mark_streamed(session, turn, &mut state).await?;
                    let (ordinal, created_part) = state.ensure_text();
                    if created_part {
                        self.emit(
                            session,
                            &turn.directory,
                            SessionTextStarted {
                                session_id: Runtime::session_id(session),
                                assistant_message_id: id.clone(),
                                ordinal: ordinal_u32(ordinal),
                            },
                        )
                        .await?;
                    }
                }
                oc_llm::LlmEvent::TextDelta { delta } => {
                    self.mark_streamed(session, turn, &mut state).await?;
                    state.append_text(&delta);
                    self.emit(
                        session,
                        &turn.directory,
                        SessionTextDelta {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            ordinal: ordinal_u32(state.open_text.unwrap_or(0)),
                            delta,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::TextEnd { text } => {
                    self.mark_streamed(session, turn, &mut state).await?;
                    let (ordinal, _) = state.ensure_text();
                    if let Some(AssistantPart::Text(part)) = state.content.get_mut(ordinal) {
                        part.text.clone_from(&text);
                    }
                    state.open_text = None;
                    self.persist_assistant(session, &state).await?;
                    self.emit(
                        session,
                        &turn.directory,
                        SessionTextEnded {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            ordinal: ordinal_u32(ordinal),
                            text,
                            native: None,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ReasoningDelta { delta } => {
                    self.mark_streamed(session, turn, &mut state).await?;
                    let (ordinal, created_part) = state.ensure_reasoning();
                    if created_part {
                        self.emit(
                            session,
                            &turn.directory,
                            oc_types::payload::SessionReasoningStarted {
                                session_id: Runtime::session_id(session),
                                assistant_message_id: id.clone(),
                                ordinal: ordinal_u32(ordinal),
                                native: None,
                            },
                        )
                        .await?;
                    }
                    state.append_reasoning(&delta);
                    self.emit(
                        session,
                        &turn.directory,
                        oc_types::payload::SessionReasoningDelta {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            ordinal: ordinal_u32(ordinal),
                            delta,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ToolInputStart { id: call_id, name } => {
                    if final_call {
                        continue;
                    }
                    self.close_reasoning(session, turn, &mut state).await?;
                    self.mark_streamed(session, turn, &mut state).await?;
                    state.ensure_tool(&call_id, &name);
                    self.persist_assistant(session, &state).await?;
                    self.emit(
                        session,
                        &turn.directory,
                        SessionToolInputStarted {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            id: call_id,
                            name,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ToolInputDelta { id: call_id, delta } => {
                    if final_call {
                        continue;
                    }
                    self.mark_streamed(session, turn, &mut state).await?;
                    state.append_tool_input(&call_id, &delta);
                    self.emit(
                        session,
                        &turn.directory,
                        SessionToolInputDelta {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            id: call_id,
                            delta,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ToolInputEnd { id: call_id, text } => {
                    if final_call {
                        continue;
                    }
                    state.set_tool_streamed(&call_id, text.clone());
                    self.persist_assistant(session, &state).await?;
                    self.emit(
                        session,
                        &turn.directory,
                        SessionToolInputEnded {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            id: call_id,
                            text,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ToolCall {
                    id: call_id,
                    name,
                    input,
                } => {
                    if final_call {
                        continue;
                    }
                    self.close_reasoning(session, turn, &mut state).await?;
                    self.mark_streamed(session, turn, &mut state).await?;
                    let created_tool = state.tool_index(&call_id).is_none();
                    let index = state.ensure_tool(&call_id, &name);
                    let (clean, expectation, invalid) = strip_expectation(&name, input);
                    if created_tool {
                        let input_text =
                            serde_json::to_string(&clean).unwrap_or_else(|_| "{}".to_owned());
                        self.emit(
                            session,
                            &turn.directory,
                            SessionToolInputStarted {
                                session_id: Runtime::session_id(session),
                                assistant_message_id: id.clone(),
                                id: call_id.clone(),
                                name: name.clone(),
                            },
                        )
                        .await?;
                        self.emit(
                            session,
                            &turn.directory,
                            SessionToolInputEnded {
                                session_id: Runtime::session_id(session),
                                assistant_message_id: id.clone(),
                                id: call_id.clone(),
                                text: input_text,
                            },
                        )
                        .await?;
                    }
                    if let Some(AssistantPart::Tool(tool)) = state.content.get_mut(index) {
                        tool.name.clone_from(&name);
                        tool.state = ToolState::Running {
                            input: clean.clone(),
                            metadata: None,
                        };
                        tool.executed = Some(false);
                    }
                    self.persist_assistant(session, &state).await?;
                    self.emit(
                        session,
                        &turn.directory,
                        SessionToolCalled {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            id: call_id.clone(),
                            input: clean.clone(),
                            executed: false,
                            native: None,
                        },
                    )
                    .await?;
                    calls.push(PendingToolCall {
                        index,
                        id: call_id,
                        name,
                        input: clean,
                        expectation,
                        invalid,
                    });
                }
                oc_llm::LlmEvent::Finish { reason, usage } => {
                    state.finish = Some(finish_reason(&reason));
                    state.raw_finish = Some(reason);
                    let tokens = tokens_from_usage(&usage);
                    state.tokens = Some(tokens.clone());
                    let cost = oc_llm::model_cost(&model.model, &usage);
                    state.cost = Some(cost);
                    self.inner
                        .config
                        .store
                        .add_session_usage(
                            session,
                            cost,
                            tokens.input,
                            tokens.output,
                            tokens.reasoning,
                            tokens.cache.read,
                            tokens.cache.write,
                        )
                        .await?;
                    self.persist_assistant(session, &state).await?;
                    self.emit(
                        session,
                        &turn.directory,
                        SessionUsageUpdated {
                            session_id: Runtime::session_id(session),
                            cost,
                            tokens,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ProviderError { message } => {
                    provider_error = Some(message);
                    break;
                }
            }
        }

        self.close_reasoning(session, turn, &mut state).await?;
        self.close_text(session, turn, &mut state).await?;
        if let Some(message) = provider_error {
            // A classifiable in-band provider/transport error advances the model
            // chain instead of killing the step. If zero output was streamed
            // yet, the assistant row stays reusable (`reusable == true`).
            let error = classified_provider_error(&message);
            if let oc_llm::LlmError::Failure { kind, .. } = &error {
                let kind = *kind;
                let reusable = state.streamed.is_none();
                return Ok(StepAttempt::Retry(StreamFailure {
                    error,
                    state,
                    kind,
                    reusable,
                }));
            }
            let error = StructuredError::new("ProviderError", message.clone());
            let tokens = state.tokens.clone().unwrap_or_default();
            let cost = state.cost.unwrap_or(0.0);
            self.fail_step(session, turn, state, error, Some(tokens), cost)
                .await?;
            return Err(RuntimeError::Llm(classified_provider_error(&message)));
        }
        if cancel.is_cancelled() {
            self.schedule_finalizer(session.to_owned(), turn.directory.clone(), state)
                .await;
            return Err(RuntimeError::Cancelled);
        }
        if state.finish.is_none() {
            let message = "stream closed before finish event";
            let error = classified_provider_error(message);
            if let oc_llm::LlmError::Failure { kind, .. } = &error {
                let kind = *kind;
                let reusable = state.streamed.is_none();
                return Ok(StepAttempt::Retry(StreamFailure {
                    error,
                    state,
                    kind,
                    reusable,
                }));
            }
            let structured = StructuredError::new("ProviderError", message.to_owned());
            let tokens = state.tokens.clone().unwrap_or_default();
            let cost = state.cost.unwrap_or(0.0);
            self.fail_step(session, turn, state, structured, Some(tokens), cost)
                .await?;
            return Err(RuntimeError::Llm(error));
        }
        self.persist_assistant(session, &state).await?;
        Ok(StepAttempt::Done(StepRun {
            tokens: state.tokens.clone().unwrap_or_default(),
            cost: state.cost.unwrap_or(0.0),
            state,
            calls,
        }))
    }

    async fn mark_streamed(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
    ) -> Result<(), RuntimeError> {
        if state.streamed.is_some() {
            return Ok(());
        }
        state.streamed = Some(oc_types::now_millis());
        self.emit(
            session,
            &turn.directory,
            SessionStepStreamed {
                session_id: Runtime::session_id(session),
                assistant_message_id: state.id.clone(),
            },
        )
        .await
    }

    async fn close_text(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
    ) -> Result<(), RuntimeError> {
        let Some((index, text)) = state.close_text() else {
            return Ok(());
        };
        self.persist_assistant(session, state).await?;
        self.emit(
            session,
            &turn.directory,
            SessionTextEnded {
                session_id: Runtime::session_id(session),
                assistant_message_id: state.id.clone(),
                ordinal: ordinal_u32(index),
                text,
                native: None,
            },
        )
        .await
    }

    async fn close_reasoning(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
    ) -> Result<(), RuntimeError> {
        let Some((index, text)) = state.close_reasoning() else {
            return Ok(());
        };
        self.persist_assistant(session, state).await?;
        self.emit(
            session,
            &turn.directory,
            oc_types::payload::SessionReasoningEnded {
                session_id: Runtime::session_id(session),
                assistant_message_id: state.id.clone(),
                ordinal: ordinal_u32(index),
                text,
                native: None,
            },
        )
        .await
    }

    pub(super) async fn finish_step(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        step: &mut StepRun,
    ) -> Result<(), RuntimeError> {
        step.state.completed = Some(oc_types::now_millis());
        step.state.finish.get_or_insert(FinishReason::Unknown);
        let tokens = step
            .state
            .tokens
            .clone()
            .unwrap_or_else(|| step.tokens.clone());
        step.state.tokens = Some(tokens.clone());
        let cost = step.state.cost.unwrap_or(step.cost);
        step.state.cost = Some(cost);
        self.persist_assistant(session, &step.state).await?;
        self.emit(
            session,
            &turn.directory,
            SessionStepEnded {
                session_id: Runtime::session_id(session),
                assistant_message_id: step.state.id.clone(),
                finish: step.state.finish.unwrap_or(FinishReason::Unknown),
                raw_finish: step.state.raw_finish.clone(),
                native: None,
                cost,
                tokens,
                snapshot: None,
                files: None,
            },
        )
        .await
    }
}
