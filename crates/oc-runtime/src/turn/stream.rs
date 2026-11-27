use std::sync::Arc;

use futures_util::StreamExt;
use oc_llm::{LlmMessage, LlmOptions, LlmRequest};
use oc_store::NewMessage;
use oc_types::message::{AssistantPart, FinishReason, StructuredError, ToolState};
use oc_types::payload::{
    SessionRequestEnvelope, SessionStepEnded, SessionStepFailed, SessionStepStarted,
    SessionStepStreamed, SessionTextDelta, SessionTextEnded, SessionTextStarted, SessionToolCalled,
    SessionToolInputDelta, SessionToolInputEnded, SessionToolInputStarted, SessionUsageUpdated,
};
use oc_types::{MessageId, ModelRef, Tokens};

use crate::runtime::{CancelToken, Runtime, RuntimeError};

use super::assistant::{AssistantState, ordinal_u32};
use super::request_envelope::RequestEnvelopeSeed;
use super::tools::strip_expectation;
use super::types::{
    PendingToolCall, ResolvedTurn, StepAttempt, StepRun, StreamFailure, finish_reason,
    tokens_from_usage,
};

async fn recv_eager_progress(
    eager: &mut Option<super::tools::EagerToolBatch>,
) -> Option<(usize, String, serde_json::Map<String, serde_json::Value>)> {
    match eager.as_mut() {
        Some(batch) => batch.recv_progress().await,
        None => std::future::pending().await,
    }
}

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
pub(super) fn classify_llm_error(error: oc_llm::LlmError) -> oc_llm::LlmError {
    match error {
        oc_llm::LlmError::Provider { message } => classified_provider_error(&message),
        oc_llm::LlmError::PolicyBlocked { message } => oc_llm::LlmError::Failure {
            kind: oc_llm::FailureKind::PolicyBlocked,
            message,
        },
        oc_llm::LlmError::QuotaTerminal { message } => oc_llm::LlmError::Failure {
            kind: oc_llm::FailureKind::Quota,
            message,
        },
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

/// `true` when `base_request` is the tool-less final call of a step budget.
///
/// The turn loop expresses a final call by setting `tool_choice: "none"` on the
/// request it hands down (see `run_steps`), so the request itself is the single
/// source of truth for the mode — there is no second, driftable parameter.
fn final_call(base_request: &LlmRequest) -> bool {
    base_request.options.tool_choice.as_deref() == Some("none")
}

/// Upper bound on credential attempts for one model candidate.
pub(super) const MAX_KEY_ATTEMPTS: usize = 8;

/// Maximum bounded rounds for transient `Transport` / `EmptyResponse` recovery
/// (covers both single-key setups and full-pool transient stream drops).
pub(super) const TRANSPORT_MAX_ATTEMPTS: usize = 3;

const TRANSPORT_RETRY_BASE_MS: u64 = 600;

fn is_transport_retryable(error: &oc_llm::LlmError) -> bool {
    matches!(
        error,
        oc_llm::LlmError::Failure {
            kind: oc_llm::FailureKind::Transport | oc_llm::FailureKind::EmptyResponse,
            ..
        } | oc_llm::LlmError::Transport { .. }
            | oc_llm::LlmError::Timeout { .. }
    )
}

/// The failure class that justifies rotating or retrying a credential/attempt.
///
/// Key-level classes (`Auth`, `Quota`, `RateLimit`) and transient transport/empty
/// stream failures rotate across remaining healthy keys (and transport/empty
/// failures additionally retry with bounded backoff when all keys exhaust).
pub(super) fn is_key_rotatable(error: &oc_llm::LlmError) -> Option<oc_llm::FailureKind> {
    match error {
        oc_llm::LlmError::Failure { kind, .. } => matches!(
            kind,
            oc_llm::FailureKind::Auth
                | oc_llm::FailureKind::Quota
                | oc_llm::FailureKind::RateLimit
                | oc_llm::FailureKind::Transport
                | oc_llm::FailureKind::EmptyResponse
        )
        .then_some(*kind),
        oc_llm::LlmError::Http { status, .. } => match status {
            401..=403 => Some(oc_llm::FailureKind::Auth),
            429 => Some(oc_llm::FailureKind::RateLimit),
            408 | 425 | 500..=599 => Some(oc_llm::FailureKind::Transport),
            _ => None,
        },
        oc_llm::LlmError::Transport { .. } | oc_llm::LlmError::Timeout { .. } => {
            Some(oc_llm::FailureKind::Transport)
        }
        _ => None,
    }
}

fn key_failure_kind(failure: &StreamFailure) -> Option<oc_llm::FailureKind> {
    is_key_rotatable(&failure.error).or_else(|| {
        matches!(
            failure.kind,
            oc_llm::FailureKind::Auth
                | oc_llm::FailureKind::Quota
                | oc_llm::FailureKind::RateLimit
                | oc_llm::FailureKind::Transport
                | oc_llm::FailureKind::EmptyResponse
        )
        .then_some(failure.kind)
    })
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
        envelope: RequestEnvelopeSeed,
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
                envelope,
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
            retry_after_ms: _,
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
    async fn prepare_retry_draft(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        failure: StreamFailure,
        transport_retryable: bool,
    ) -> Result<Option<AssistantState>, RuntimeError> {
        if failure.reusable {
            return Ok(Some(failure.state));
        }
        let has_tools = failure
            .state
            .content
            .iter()
            .any(|part| matches!(part, AssistantPart::Tool(_)));
        if transport_retryable && !has_tools {
            let mut state = failure.state;
            state.clear_draft_for_retry();
            self.persist_assistant(session, &state).await?;
            return Ok(Some(state));
        }
        let original = RuntimeError::Llm(failure.error);
        let tokens = failure.state.tokens.clone();
        let cost = failure.state.cost.unwrap_or(0.0);
        let structured = original.structured();
        self.fail_step(session, turn, failure.state, structured, tokens, cost)
            .await?;
        Ok(None)
    }

    async fn sleep_transport_backoff(
        &self,
        session: &str,
        provider: &str,
        round: usize,
        retry_after_ms: Option<u64>,
        error_text: &str,
        cancel: &Arc<CancelToken>,
    ) -> Result<(), RuntimeError> {
        let base_ms = TRANSPORT_RETRY_BASE_MS * u64::try_from(round + 1).unwrap_or(1);
        let hinted_ms = retry_after_ms.map_or(0, |ms| ms.min(60_000));
        let wait_ms = base_ms.max(hinted_ms);
        tracing::warn!(
            session = %session,
            provider = %provider,
            attempt = round + 1,
            wait_ms,
            error = %error_text,
            "retrying stream step after transient transport/empty failure"
        );
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(RuntimeError::Cancelled),
            () = self.inner.shutdown.cancelled() => Err(RuntimeError::Shutdown),
            () = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => Ok(()),
        }
    }

    fn build_step_request(
        &self,
        turn: &ResolvedTurn,
        session: &str,
        model: &oc_llm::ResolvedModel,
        model_ref: &ModelRef,
        messages: &[LlmMessage],
        final_call: bool,
    ) -> LlmRequest {
        let merged = self.inner.config.config.merged();
        let thinking_cap = merged
            .pointer(&format!("/agents/{}/thinking_cap", turn.agent_id))
            .or_else(|| merged.pointer("/models/thinking_cap"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let store = merged
            .pointer(&format!("/providers/{}/store", model.provider.id))
            .or_else(|| merged.pointer("/models/store"))
            .and_then(serde_json::Value::as_bool);
        let settings_thinking = model
            .settings()
            .get("thinkingConfig")
            .and_then(|cfg| cfg.get("thinkingLevel"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let timeouts = oc_llm::Timeouts::for_variant(
            model_ref.variant.as_deref(),
            settings_thinking.as_deref(),
        );
        LlmRequest {
            model: model.clone(),
            system: turn.system.clone(),
            messages: messages.to_vec(),
            tools: turn.tools.clone(),
            options: LlmOptions {
                api_key: None,
                credential_id: model_ref.credential_id.clone(),
                temperature: turn.agent_request.temperature,
                top_p: turn.agent_request.top_p,
                headers: turn.agent_request.headers.clone(),
                tool_choice: final_call.then(|| "none".to_owned()),
                cache_key: Some(session.to_owned()),
                store,
                thinking_cap,
                timeouts: Some(timeouts),
                ..LlmOptions::default()
            },
        }
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
        envelope: RequestEnvelopeSeed,
    ) -> Result<StepAttempt, RuntimeError> {
        let provider = model.provider.id.clone();
        let now_ms = oc_types::now_millis();
        // The request is built once, here, so the transport-hook probe below and
        // the dispatch in `stream_once` cannot disagree about it.
        let base_request =
            self.build_step_request(turn, session, model, model_ref, messages, final_call);
        let hook_owns_pool = self
            .inner
            .config
            .client
            .hook()
            .is_some_and(|hook| hook.handles(&base_request));
        let pool = if hook_owns_pool {
            Vec::new()
        } else {
            self.credential_pool_for(&provider, model_ref.credential_id.as_deref(), now_ms)
                .await
        };
        let attempts: Vec<Option<oc_llm::PoolKey>> = if pool.is_empty() {
            vec![None]
        } else {
            pool.into_iter().take(MAX_KEY_ATTEMPTS).map(Some).collect()
        };
        let total = attempts.len();
        let mut eligible = vec![true; total];
        let mut last: Option<StreamFailure> = None;

        'rounds: for transport_round in 0..TRANSPORT_MAX_ATTEMPTS {
            let round_now_ms = oc_types::now_millis();
            let active_indices: Vec<usize> = (0..total).filter(|&i| eligible[i]).collect();
            if active_indices.is_empty() {
                break;
            }
            let active_len = active_indices.len();
            for (pos, index) in active_indices.into_iter().enumerate() {
                let candidate = &attempts[index];
                let api_key = candidate.as_ref().map(|entry| entry.secret().to_owned());
                let credential_id = candidate
                    .as_ref()
                    .and_then(|entry| entry.credential_id.clone());
                let attempt = self
                    .stream_once(
                        session,
                        turn,
                        model_ref,
                        &base_request,
                        cancel,
                        reuse.take(),
                        api_key,
                        credential_id,
                        envelope.clone(),
                    )
                    .await?;
                let failure = match attempt {
                    StepAttempt::Done(run) => return Ok(StepAttempt::Done(run)),
                    StepAttempt::Retry(failure) => failure,
                };
                let retry_after_ms = failure.retry_after_ms;
                // Explicit `Retry-After` / `RetryInfo` server hints on 429 are bounded-retried
                // here by the single retry owner after `sleep_transport_backoff`, whereas
                // unhinted rate-limit failures rotate keys without blind loops.
                let transport_retryable = is_transport_retryable(&failure.error)
                    || (failure.kind == oc_llm::FailureKind::RateLimit
                        && retry_after_ms.is_some_and(|ms| ms <= 60_000));
                let error_text = failure.error.to_string();
                if let Some(credential) = candidate.as_ref() {
                    let Some(kind) = key_failure_kind(&failure) else {
                        return Ok(StepAttempt::Retry(failure));
                    };
                    if !transport_retryable {
                        eligible[index] = false;
                    }
                    self.mark_key_failure(
                        &provider,
                        credential.credential_id.as_deref(),
                        kind,
                        round_now_ms,
                    )
                    .await;
                } else if !transport_retryable {
                    return Ok(StepAttempt::Retry(failure));
                }

                let more_keys = pos + 1 < active_len;
                let more_transport_rounds =
                    eligible.iter().any(|&ok| ok) && (transport_round + 1 < TRANSPORT_MAX_ATTEMPTS);
                if !more_keys && (!transport_retryable || !more_transport_rounds) {
                    last = Some(failure);
                    break 'rounds;
                }

                *reuse = self
                    .prepare_retry_draft(session, turn, failure, transport_retryable)
                    .await?;

                if !more_keys && more_transport_rounds {
                    self.sleep_transport_backoff(
                        session,
                        &provider,
                        transport_round,
                        retry_after_ms,
                        &error_text,
                        cancel,
                    )
                    .await?;
                    continue 'rounds;
                }
            }
        }
        Ok(StepAttempt::Retry(last.expect(
            "a pool with at least one usable key yields a failure or a success",
        )))
    }

    /// Record a key-level failure and keep the durable primary pointer healthy.
    ///
    /// Only permanent key-level failures (`Auth`, `Quota`) rewrite the durable
    /// `credential.active` pointer in SQLite; transient `RateLimit`, `Transport`,
    /// and `EmptyResponse` failures stay in-memory in `KeyPool`.
    pub(super) async fn mark_key_failure(
        &self,
        provider: &str,
        credential_id: Option<&str>,
        kind: oc_llm::FailureKind,
        now_ms: i64,
    ) {
        self.key_pool().mark(provider, credential_id, kind, now_ms);
        if !matches!(kind, oc_llm::FailureKind::Auth | oc_llm::FailureKind::Quota) {
            return;
        }
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
        model_ref: &ModelRef,
        base_request: &LlmRequest,
        cancel: &Arc<CancelToken>,
        reuse: Option<AssistantState>,
        api_key: Option<String>,
        credential_id: Option<String>,
        envelope: RequestEnvelopeSeed,
    ) -> Result<StepAttempt, RuntimeError> {
        let created = oc_types::now_millis();
        // Record the actual credential selected for this attempt on the
        // assistant message so per-key usage rollups track key rotation too.
        let effective_model = ModelRef {
            credential_id: credential_id
                .clone()
                .or_else(|| model_ref.credential_id.clone()),
            ..model_ref.clone()
        };
        // Nothing streamed on the previous attempt: the row is rewritten for
        // the new model instead of appending a second assistant message.
        let mut state = if let Some(mut state) = reuse {
            state.model = effective_model.clone();
            self.persist_assistant(session, &state).await?;
            state
        } else {
            let state = AssistantState::new(
                MessageId::new(),
                turn.agent_id.clone(),
                effective_model.clone(),
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
                    model: effective_model,
                    snapshot: None,
                    started: created,
                },
            )
            .await?;
            // The row (and its `session.step.started`) is committed and the
            // exact request is fully materialised. Record it before the first
            // byte is requested, so a crash mid-stream still leaves the request
            // recoverable from the log. `emit` appends durably before
            // publishing. One envelope per assistant row: a key rotation reuses
            // this row, so it does not record a second one.
            self.emit(
                session,
                &turn.directory,
                SessionRequestEnvelope {
                    session_id: Runtime::session_id(session),
                    assistant_message_id: state.id.clone(),
                    step: envelope.step,
                    provider: base_request.model.provider.id.clone(),
                    model: base_request.model.model.id.clone(),
                    tools: base_request
                        .tools
                        .iter()
                        .map(|tool| tool.name.clone())
                        .collect(),
                    system_digest: oc_types::payload::digest_sequence(
                        base_request.system.iter().map(String::as_str),
                    ),
                    base_messages: envelope.base_messages,
                    base_digest: envelope.base_digest,
                    injections: envelope.injections,
                    created: oc_types::now_millis(),
                },
            )
            .await?;
            state
        };
        let id = state.id.clone();

        let request = LlmRequest {
            options: LlmOptions {
                api_key,
                credential_id: credential_id.or_else(|| base_request.options.credential_id.clone()),
                ..base_request.options.clone()
            },
            ..base_request.clone()
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
                let retry_after_ms = error.retry_after_ms();
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
                        retry_after_ms,
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
        let mut eager: Option<super::tools::EagerToolBatch> = None;
        let mut prewarm_started = false;
        loop {
            let event = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    if let Some(mut batch) = eager.take() {
                        batch.abort_only(cancel.reason()).await;
                    }
                    self.clear_subagent_prewarm(session);
                    self.schedule_finalizer(session.to_owned(), turn.directory.clone(), state)
                        .await;
                    return Err(RuntimeError::Cancelled);
                }
                Some((p_index, p_call_id, p_meta)) = recv_eager_progress(&mut eager) => {
                    self.apply_tool_progress(session, turn, &mut state, p_index, p_call_id, p_meta)
                        .await?;
                    continue;
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
                    if final_call(base_request) {
                        continue;
                    }
                    if name == "subagent" && !prewarm_started {
                        prewarm_started = true;
                        self.prewarm_subagent_caller(session);
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
                    if final_call(base_request) {
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
                    if final_call(base_request) {
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
                    if final_call(base_request) {
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
                    if let Some(last_call) = calls.last()
                        && last_call.name == "subagent"
                        && last_call.invalid.is_none()
                    {
                        if !prewarm_started {
                            prewarm_started = true;
                            self.prewarm_subagent_caller(session);
                        }
                        if eager.is_none() {
                            match super::tools::EagerToolBatch::new(self, session, cancel).await {
                                Ok(batch) => eager = Some(batch),
                                Err(error) => {
                                    tracing::debug!(session = %session, %error, "eager subagent init skipped");
                                }
                            }
                        }
                        if let Some(batch) = &mut eager {
                            batch.try_spawn_subagent(self, turn, &id, last_call);
                        }
                    }
                }
                oc_llm::LlmEvent::Finish { reason, usage } => {
                    state.finish = Some(finish_reason(&reason));
                    state.raw_finish = Some(reason);
                    let tokens = tokens_from_usage(&usage);
                    state.tokens = Some(tokens.clone());
                    let cost = oc_llm::model_cost(&base_request.model.model, &usage);
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
            if let Some(mut batch) = eager.take() {
                batch.abort_only(oc_types::InterruptReason::Shutdown).await;
            }
            self.clear_subagent_prewarm(session);
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
                    retry_after_ms: None,
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
            if let Some(mut batch) = eager.take() {
                batch.abort_only(cancel.reason()).await;
            }
            self.clear_subagent_prewarm(session);
            self.schedule_finalizer(session.to_owned(), turn.directory.clone(), state)
                .await;
            return Err(RuntimeError::Cancelled);
        }
        if state.finish.is_none() {
            if let Some(mut batch) = eager.take() {
                batch.abort_only(oc_types::InterruptReason::Shutdown).await;
            }
            self.clear_subagent_prewarm(session);
            let message = "stream closed before finish event";
            let error = classified_provider_error(message);
            if let oc_llm::LlmError::Failure { kind, .. } = &error {
                let kind = *kind;
                let reusable = state.streamed.is_none();
                return Ok(StepAttempt::Retry(StreamFailure {
                    error,
                    state,
                    kind,
                    retry_after_ms: None,
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
        if let Some(batch) = &mut eager {
            for (p_index, p_call_id, p_meta) in batch.drain_progress() {
                self.apply_tool_progress(session, turn, &mut state, p_index, p_call_id, p_meta)
                    .await?;
            }
        }
        if calls.is_empty() && prewarm_started {
            self.clear_subagent_prewarm(session);
        }
        self.persist_assistant(session, &state).await?;
        Ok(StepAttempt::Done(StepRun {
            tokens: state.tokens.clone().unwrap_or_default(),
            cost: state.cost.unwrap_or(0.0),
            state,
            calls,
            eager,
        }))
    }

    /// Publishes the durable `session.step.streamed` for this row.
    ///
    /// The event reports that the step produced model output, and its evidence is
    /// the `time.streamed` stamp on the assistant row. A durable event must never
    /// describe uncommitted state, so the stamp is persisted *first* and the
    /// event only goes out once that write succeeded. Nothing is reordered on the
    /// wire: the caller emits `session.text.started`/`session.text.delta`/… after
    /// this returns, exactly as before.
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
        self.persist_assistant(session, state).await?;
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

        let duration_ms = step
            .state
            .completed
            .unwrap_or_default()
            .saturating_sub(step.state.created);
        let prompt_total = tokens.input.saturating_add(tokens.cache.read);
        let cache_hit_pct = if prompt_total > 0 {
            (tokens.cache.read.saturating_mul(100)) / prompt_total
        } else {
            0
        };
        let finish_str = match step.state.finish {
            Some(FinishReason::Stop) => "stop",
            Some(FinishReason::Length) => "length",
            Some(FinishReason::ToolCalls) => "tool-calls",
            Some(FinishReason::ContentFilter) => "content-filter",
            Some(FinishReason::Error) => "error",
            Some(FinishReason::Unknown) | None => "unknown",
        };
        // Payload-free OTel GenAI Semantic Conventions telemetry (`gen_ai.*`).
        tracing::info!(
            "gen_ai.system" = %turn.model_ref.provider_id,
            "gen_ai.operation.name" = "chat",
            "gen_ai.request.model" = %turn.model_ref.id,
            "gen_ai.response.finish_reasons" = %finish_str,
            "gen_ai.usage.input_tokens" = tokens.input,
            "gen_ai.usage.output_tokens" = tokens.output,
            "gen_ai.usage.cache_read.input_tokens" = tokens.cache.read,
            "gen_ai.usage.reasoning_tokens" = tokens.reasoning,
            "gen_ai.client.operation.duration_ms" = duration_ms,
            "gen_ai.usage.cost_usd" = cost,
            cache_hit_pct,
            session = %session,
            "gen_ai.chat.completed"
        );
        let cred_slot = turn.model_ref.credential_id.as_deref().unwrap_or("default");
        self.inner.config.client.0.rate_limiter().record_spend(
            &turn.model_ref.provider_id,
            cred_slot,
            cost,
        );

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
