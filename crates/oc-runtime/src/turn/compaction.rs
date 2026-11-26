use std::fmt::Write;
use std::sync::Arc;

use futures_util::StreamExt;
use oc_llm::{LlmMessage, LlmOptions, LlmRequest};
use oc_store::NewMessage;
use oc_types::message::StructuredError;
use oc_types::payload::{
    CompactionTrigger, SessionCompactionEnded, SessionCompactionFailed, SessionCompactionStarted,
    SessionStatusUpdated,
};
use oc_types::{MessageId, ModelRef, SessionStatus};
use serde_json::{Value, json};

use crate::runtime::{CancelToken, Runtime, RuntimeError};
use crate::subagent::settings::models_policy;
use crate::tokens;

use super::types::ResolvedTurn;

impl Runtime {
    /// Runs one `compaction` inbox row: resolve the session turn, load the
    /// context and call [`Runtime::compact`].
    ///
    /// The inbox row is consumed durably BEFORE the attempt, so a crash, an
    /// error or a cancellation cannot replay the same compaction forever: the
    /// actor's drain loop treats a surviving row as fresh work and
    /// `runtime/actor.rs` respawns on a drain error. Consumption errors are
    /// logged and never propagated (they used to kill the actor); the row is
    /// retried once after the attempt as a best-effort cleanup. Turn-level
    /// failures are already persisted as a failed compaction message; they are
    /// logged here and reported as a clean outcome because the actor must keep
    /// serving the session.
    pub(crate) async fn run_manual_compaction(
        &self,
        session: &str,
        item: &Value,
        cancel: &Arc<CancelToken>,
    ) -> Result<(), RuntimeError> {
        let note = item
            .pointer("/payload/note")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let reason = item
            .pointer("/payload/reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("manual");
        let trigger = trigger_for_reason(reason).unwrap_or(CompactionTrigger::Manual);
        let inbox_id = item.get("id").and_then(Value::as_str).map(str::to_owned);
        let store = &self.inner.config.store;
        if let Some(id) = &inbox_id {
            match store.consume_inbox(session, id).await {
                Ok(true) => {
                    // The badge retires on the same `inboxID` the compaction
                    // event will name, so a client can correlate the two.
                    if let Some(directory) = self.session_directory(session).await
                        && let Err(error) = self
                            .emit(
                                session,
                                &directory,
                                oc_types::payload::SessionInboxDelivered {
                                    session_id: Runtime::session_id(session),
                                    inbox_id: MessageId::from_wire(id.clone()),
                                },
                            )
                            .await
                    {
                        tracing::warn!(
                            session = %session,
                            inbox_id = %id,
                            error = %error,
                            "could not publish session.inbox.delivered for a consumed compaction row"
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        session = %session,
                        inbox_id = %id,
                        error = %error,
                        "could not consume the compaction inbox row before running it"
                    );
                }
            }
        }
        let request = CompactionRequest::new(trigger)
            .with_note(note.as_deref())
            .with_inbox(inbox_id.as_deref());
        let outcome = self.compact_from_inbox(session, request, cancel).await;
        if let Some(id) = &inbox_id {
            match store.consume_inbox(session, id).await {
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    session = %session,
                    inbox_id = %id,
                    error = %error,
                    "could not retire the compaction inbox row after the attempt"
                ),
            }
        }
        match outcome {
            Ok(()) => {
                tracing::debug!(session = %session, "manual compaction completed");
            }
            Err(RuntimeError::Cancelled) => {
                tracing::debug!(session = %session, "manual compaction cancelled");
            }
            Err(error) => {
                tracing::warn!(
                    session = %session,
                    error = %error,
                    "manual compaction failed"
                );
            }
        }
        Ok(())
    }

    async fn compact_from_inbox(
        &self,
        session: &str,
        mut request: CompactionRequest<'_>,
        cancel: &Arc<CancelToken>,
    ) -> Result<(), RuntimeError> {
        let info_value = self
            .inner
            .config
            .store
            .get_session(session)
            .await?
            .ok_or_else(|| RuntimeError::SessionNotFound(session.to_owned()))?;
        let info = Runtime::parse_session(&info_value)?;
        let directory = info.location.directory.clone();
        let mut turn = self.resolve_turn(&info, &directory).await?;
        if let Some(handle) = self
            .inner
            .actors
            .lock()
            .expect("actors")
            .get(session)
            .cloned()
            && let Some(set) = handle
                .instructions
                .lock()
                .expect("instruction set")
                .as_ref()
        {
            let has_memory = set.source("oc/memory").is_some();
            turn.system = crate::prompt::system_prefix(
                turn.agent_system.as_deref(),
                &turn.tool_names,
                &turn.directory,
                has_memory,
                set.initial(),
            );
        }
        let context = self.load_context(session).await?;
        // Explain the decision with the numbers the turn loop would have used:
        // an inbox-triggered compaction otherwise carries no token evidence.
        if request.estimate_tokens.is_none() && turn.context_window > 0 {
            let estimate = self.estimate_turn_tokens(session, &turn, &context).await?;
            let compaction = self.compaction_config();
            let ceiling = tokens::ceiling(turn.context_window, compaction.ceiling_tokens);
            let trigger = tokens::trigger(ceiling, turn.context_window, compaction.buffer);
            request = request.with_pressure(estimate, trigger, Some(ceiling));
        }
        let owns_status = self.begin_compaction_activity(session, &directory).await;
        let outcome = self
            .compact_with_note(session, &turn, &context, cancel, request)
            .await;
        self.end_compaction_activity(session, &directory, owns_status)
            .await;
        outcome
    }

    pub(super) async fn compact(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        context: &[LlmMessage],
        cancel: &Arc<CancelToken>,
        request: CompactionRequest<'_>,
    ) -> Result<(), RuntimeError> {
        self.compact_with_note(session, turn, context, cancel, request)
            .await
    }

    /// Marks the session busy for the whole compaction when no execution owns
    /// the status, so the UI cannot show `idle` while a manual/observer
    /// compaction runs. Returns whether this call owns the status.
    ///
    /// `time_compacting` has no store writer today; `time_busy`/`time_idle` are
    /// the durable pair the wire status is derived from, so a compaction that
    /// starts from an idle session marks itself busy and clears it after.
    async fn begin_compaction_activity(&self, session: &str, directory: &str) -> bool {
        let store = &self.inner.config.store;
        let value = match store.get_session(session).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(session = %session, error = %error, "could not read the session before compaction");
                return false;
            }
        };
        let busy = value
            .as_ref()
            .and_then(|value| value.pointer("/time/busy"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let idle = value
            .as_ref()
            .and_then(|value| value.pointer("/time/idle"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if busy > idle {
            return false;
        }
        if let Err(error) = store
            .mark_session_busy(session, oc_types::now_millis())
            .await
        {
            tracing::warn!(session = %session, error = %error, "could not mark the session busy for compaction");
            return false;
        }
        if let Err(error) = self
            .emit(
                session,
                directory,
                SessionStatusUpdated {
                    session_id: Runtime::session_id(session),
                    status: SessionStatus::Busy,
                },
            )
            .await
        {
            tracing::warn!(session = %session, error = %error, "could not publish busy status for compaction");
        }
        true
    }

    /// Clears the busy state a manual/observer compaction owned.
    async fn end_compaction_activity(&self, session: &str, directory: &str, owns: bool) {
        if !owns {
            return;
        }
        let store = &self.inner.config.store;
        if let Err(error) = store
            .mark_session_idle(session, oc_types::now_millis(), "succeeded")
            .await
        {
            tracing::warn!(session = %session, error = %error, "could not clear the compaction busy state");
            return;
        }
        if let Err(error) = self
            .emit(
                session,
                directory,
                SessionStatusUpdated {
                    session_id: Runtime::session_id(session),
                    status: SessionStatus::Idle,
                },
            )
            .await
        {
            tracing::warn!(session = %session, error = %error, "could not publish idle status after compaction");
        }
    }

    async fn session_directory(&self, session: &str) -> Option<String> {
        self.inner
            .config
            .store
            .get_session(session)
            .await
            .ok()
            .flatten()
            .and_then(|value| {
                value
                    .pointer("/location/directory")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
    }

    /// Summarize `context` and persist the compaction.
    ///
    /// Model reporting: the completed compaction row and the
    /// `session.compaction.ended` payload both name the model that ACTUALLY
    /// produced the summary — the chain candidate that answered, which is not
    /// `turn.model_ref` when the chain advanced. That field is telemetry about
    /// what happened; it never means "the session's configured model". A
    /// compaction does not rewrite `session.model`, so the observer/repair path
    /// must read the configured model from the session row and use the
    /// compaction's `model` only to describe the summary's author.
    ///
    /// `request` carries the true cause ([`CompactionTrigger`]), the admitting
    /// inbox row (so `session.compaction.started` names the same id
    /// `session.inbox.delivered` retired) and the token evidence behind the
    /// decision. `reason` is derived from the trigger and kept for upstream
    /// compatibility only.
    #[allow(clippy::too_many_lines)]
    async fn compact_with_note(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        context: &[LlmMessage],
        cancel: &Arc<CancelToken>,
        request: CompactionRequest<'_>,
    ) -> Result<(), RuntimeError> {
        let CompactionRequest {
            trigger,
            note,
            inbox_id,
            estimate_tokens,
            trigger_tokens,
            ceiling_tokens,
        } = request;
        let reason = event_reason(trigger);
        let created = oc_types::now_millis();
        let id = MessageId::new();
        let keep_tokens = self.compaction_keep_tokens();
        let keep_id = self.compaction_keep_id(session, keep_tokens).await?;
        let mut running = json!({
            "time": {"created": created},
            "status": "running",
            "reason": reason,
            "trigger": trigger,
            "summary": ""
        });
        if let Some(keep_id) = &keep_id {
            running["keepID"] = json!(keep_id);
        }
        if let Some(inbox_id) = inbox_id {
            running["inboxID"] = json!(inbox_id);
        }
        if let Some(estimate) = estimate_tokens {
            running["estimateTokens"] = json!(estimate);
        }
        if let Some(trigger_tokens) = trigger_tokens {
            running["triggerTokens"] = json!(trigger_tokens);
        }
        if let Some(ceiling_tokens) = ceiling_tokens {
            running["ceilingTokens"] = json!(ceiling_tokens);
        }
        self.inner
            .config
            .store
            .append_message(NewMessage {
                id: id.as_str().to_owned(),
                session_id: session.to_owned(),
                kind: "compaction".to_owned(),
                data: running,
                time_created: created,
            })
            .await?;
        self.emit(
            session,
            &turn.directory,
            SessionCompactionStarted {
                session_id: Runtime::session_id(session),
                reason: reason.to_owned(),
                input_id: Some(id.clone()),
                inbox_id: inbox_id.map(|id| MessageId::from_wire(id.to_owned())),
                trigger: Some(trigger),
                estimate_tokens,
                trigger_tokens,
                ceiling_tokens,
            },
        )
        .await?;

        let system = if turn.system.is_empty() {
            crate::prompt::system_parts(
                turn.agent_system.as_deref(),
                &turn.tool_names,
                &turn.directory,
            )
        } else {
            turn.system.clone()
        };
        // The summary request carries the live box index so the model points at
        // keys instead of restating stable content (`docs/25-context-boxes.md`).
        let root_id = self
            .inner
            .config
            .store
            .session_node(session)
            .await?
            .map_or_else(|| session.to_owned(), |node| node.root_id);
        let box_index = match self.box_index_rows_for(&root_id, session).await {
            Ok(rows) => crate::context_box::render_box_index(
                &root_id,
                &rows,
                crate::context_box::BOX_INDEX_MAX,
            ),
            Err(error) => {
                tracing::warn!(
                    session = %session,
                    error = %error,
                    "could not render the box index for the compaction prompt"
                );
                String::new()
            }
        };
        let summary_instruction = compaction_summary_instruction_with_note(
            keep_id.as_deref(),
            trigger_wire(trigger),
            keep_tokens,
            &box_index,
            note,
        );
        let mut messages = context.to_vec();
        messages.push(LlmMessage::user(summary_instruction.clone()));
        // Compaction selects a model from `models.chains.compaction` (else
        // `models.chain`), then the turn's current model. Automatic model
        // fallback is disabled: it makes one attempt, and a candidate the
        // catalog cannot run is skipped without spending it. A key-level
        // failure still marks that key ([`Runtime::mark_compaction_key`]), so
        // the *next* turn picks another one instead of paying for the same dead
        // key again.
        let policy = models_policy(&self.inner.config.config);
        let candidates = self.compaction_candidates(turn);
        let mut attempted = 0_usize;
        let mut produced = None;
        for reference in &candidates {
            if attempted >= COMPACTION_MAX_ATTEMPTS {
                break;
            }
            // A reference the catalog cannot run is skipped without spending
            // the single attempt, so a retired entry does not stall compaction.
            let Ok(model) = self
                .inner
                .config
                .client
                .resolve_model(&self.inner.config.catalog.current(), reference)
            else {
                tracing::debug!(
                    session = %session,
                    reference = %reference,
                    "skipping an unusable compaction model candidate"
                );
                continue;
            };
            attempted = attempted.saturating_add(1);
            let model_ref = ModelRef {
                id: model.model.id.clone(),
                provider_id: model.model.provider_id.clone(),
                variant: model.variant.as_ref().map(|variant| variant.id.clone()),
                credential_id: (turn.model_ref.provider_id == model.model.provider_id)
                    .then(|| turn.model_ref.credential_id.clone())
                    .flatten(),
            };
            let attempt = CompactionAttempt {
                session,
                turn,
                model: &model,
                context,
                messages: &messages,
                system: &system,
                cancel,
            };
            match self.compaction_attempt(&attempt).await {
                CompactionOutcome::Produced { summary, usage } => {
                    produced = Some((summary, usage, model_ref));
                    break;
                }
                CompactionOutcome::Cancelled => {
                    self.fail_compaction(
                        session,
                        &turn.directory,
                        request,
                        &id,
                        created,
                        "compaction was cancelled",
                    )
                    .await?;
                    return Err(RuntimeError::Cancelled);
                }
                CompactionOutcome::Empty => {
                    let can_fallback = attempted < COMPACTION_MAX_ATTEMPTS
                        && policy.fallback().allows(oc_llm::FailureKind::EmptyResponse);
                    if can_fallback {
                        self.model_health().mark(
                            &model_ref.to_reference(),
                            oc_llm::FailureKind::EmptyResponse,
                            oc_types::now_millis(),
                            None,
                        );
                        tracing::warn!(
                            session = %session,
                            model = %model_ref.to_reference(),
                            "compaction returned empty summary; trying fallback candidate"
                        );
                        continue;
                    }
                    self.fail_compaction(
                        session,
                        &turn.directory,
                        request,
                        &id,
                        created,
                        "the summary was empty",
                    )
                    .await?;
                    return Err(RuntimeError::Config(
                        "compaction returned an empty summary".to_owned(),
                    ));
                }
                CompactionOutcome::Failed(failure) => {
                    // Record against the key that failed so the next turn
                    // rotates away from a dead key on the same model.
                    match failure.record {
                        RecordPolicy::ModelAndKey | RecordPolicy::KeyOnly => {
                            self.mark_compaction_key(
                                &model.provider.id,
                                failure.credential.as_ref(),
                                &failure.error,
                            );
                        }
                    }
                    let last = attempted >= COMPACTION_MAX_ATTEMPTS;
                    if last || !policy.fallback().allows(failure.kind) {
                        self.fail_compaction(
                            session,
                            &turn.directory,
                            request,
                            &id,
                            created,
                            &failure.message,
                        )
                        .await?;
                        return Err(RuntimeError::Llm(failure.error));
                    }
                    tracing::debug!(
                        session = %session,
                        model = %model_ref.to_reference(),
                        kind = failure.kind.as_str(),
                        "compaction advanced to the next model candidate"
                    );
                }
            }
        }
        let Some((summary, usage, model_ref)) = produced else {
            self.fail_compaction(
                session,
                &turn.directory,
                request,
                &id,
                created,
                "no compaction model candidate could be attempted",
            )
            .await?;
            return Err(RuntimeError::Config(
                "no compaction model candidate could be attempted".to_owned(),
            ));
        };
        // `model` is the model that ACTUALLY produced this summary, which is not
        // necessarily `turn.model_ref` when the chain advanced. It describes what
        // happened; `session.model` stays untouched by a compaction, so the
        // session's configured model is read from the session row, never from
        // this compaction field.
        let mut completed = json!({
            "time": {"created": created},
            "status": "completed",
            "reason": reason,
            "trigger": trigger,
            "model": model_ref,
            "summary": summary
        });
        if let Some(keep_id) = &keep_id {
            completed["keepID"] = json!(keep_id);
        }
        if let Some(inbox_id) = inbox_id {
            completed["inboxID"] = json!(inbox_id);
        }
        if let Some(estimate) = estimate_tokens {
            completed["estimateTokens"] = json!(estimate);
        }
        if let Some(trigger_tokens) = trigger_tokens {
            completed["triggerTokens"] = json!(trigger_tokens);
        }
        if let Some(ceiling_tokens) = ceiling_tokens {
            completed["ceilingTokens"] = json!(ceiling_tokens);
        }
        self.inner
            .config
            .store
            .update_message(session, id.as_str(), completed)
            .await?;
        // Expire temporary boxes only after the completed row is durable and
        // before `session.compaction.ended` is published. A failure is logged
        // and must not fail the compaction itself.
        //
        // The expiry is scoped to the compacting session and its descendants
        // (R3): a sibling subtree's temporary boxes are not part of this
        // compaction and must survive it.
        let owners = self.compaction_owner_sessions(session).await;
        if let Err(error) = self
            .inner
            .config
            .store
            .expire_temporary_boxes_for(&root_id, &owners)
            .await
        {
            tracing::warn!(
                session = %session,
                error = %error,
                "could not expire temporary context boxes after compaction"
            );
        }
        let tokens = usage.as_ref().map(super::types::tokens_from_usage);
        self.emit(
            session,
            &turn.directory,
            SessionCompactionEnded {
                session_id: Runtime::session_id(session),
                reason: reason.to_owned(),
                model: Some(model_ref.clone()),
                native: None,
                text: summary,
                cost: usage.as_ref().and_then(|usage| usage.cost),
                tokens,
                inbox_id: inbox_id.map(|id| MessageId::from_wire(id.to_owned())),
                trigger: Some(trigger),
                before_tokens: estimate_tokens,
                after_tokens: self
                    .post_compaction_estimate(session, turn, estimate_tokens)
                    .await,
            },
        )
        .await?;
        Ok(())
    }

    /// Token estimate after the completed compaction row is durable.
    ///
    /// `before_tokens` is the pre-compaction estimate that crossed the trigger;
    /// this reads the transcript the next request will actually use and reports
    /// the honest post state, so the pair explains the reduction instead of
    /// restating the input.
    async fn post_compaction_estimate(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        before: Option<u64>,
    ) -> Option<u64> {
        before?;
        let context = self.load_context(session).await.ok()?;
        self.estimate_turn_tokens(session, turn, &context)
            .await
            .ok()
    }

    /// Ordered model references for one compaction.
    ///
    /// `models.chains.compaction` (else `models.chain`) keeps its configured
    /// order, and the turn's current model is appended last so a document without
    /// a compaction chain compacts on exactly the model the session already uses.
    /// The list is deduplicated by reference string, preserving the FIRST
    /// occurrence: a chain that names the session's own model keeps it at its
    /// chain position and the appended turn model is dropped instead.
    ///
    /// Nothing is filtered by resolvability here. Resolving is the attempt loop's
    /// job, exactly as in the turn loop's chain walk: a retired entry is skipped
    /// there without spending one of the two attempts, and letting the stream
    /// call itself fail is what makes an unusable reference reportable instead of
    /// silently shrinking the candidate list to nothing.
    fn compaction_candidates(&self, turn: &ResolvedTurn) -> Vec<String> {
        let policy = models_policy(&self.inner.config.config);
        let mut candidates: Vec<String> = Vec::new();
        let push = |reference: &str, candidates: &mut Vec<String>| {
            if !candidates.iter().any(|known| known == reference) {
                candidates.push(reference.to_owned());
            }
        };
        for reference in policy.chain_for("compaction") {
            push(reference, &mut candidates);
        }
        push(&turn.model_ref.to_reference(), &mut candidates);
        candidates
    }

    /// One compaction attempt against a single model candidate.
    ///
    /// Preserves the historical request shape: one summary request with
    /// `tool_choice: none`, one selected key (compaction does not rotate keys
    /// mid-attempt), and a single shortened retry when the request itself
    /// overflows the context window. The shortened retry stays on the SAME
    /// candidate: the model did not fail, the prompt was too large.
    async fn compaction_attempt(&self, attempt: &CompactionAttempt<'_>) -> CompactionOutcome {
        let (stream, credential) = match self.open_compaction_stream(attempt).await {
            Ok(opened) => opened,
            Err(outcome) => return *outcome,
        };
        read_compaction_summary(stream, attempt.cancel, credential).await
    }

    /// Open the summary stream for one candidate, selecting exactly one key.
    ///
    /// A request that overflows the provider's context window is retried once on
    /// the SAME candidate with masked tool output and inline media: the model did
    /// not fail, the prompt was too large. Any other open failure is classified
    /// here and reported against the model AND the key, because it says the model
    /// could not serve the request at all. The selected key is returned on
    /// success so an in-band failure can still be recorded against it.
    async fn open_compaction_stream(
        &self,
        attempt: &CompactionAttempt<'_>,
    ) -> Result<(oc_llm::LlmStream, Option<oc_llm::PoolKey>), Box<CompactionOutcome>> {
        let CompactionAttempt {
            session,
            turn,
            model,
            context: _,
            messages,
            system,
            cancel,
        } = *attempt;
        let preferred_key = (turn.model_ref.provider_id == model.provider.id)
            .then_some(turn.model_ref.credential_id.as_deref())
            .flatten();
        let pool = self
            .credential_pool_for(&model.provider.id, preferred_key, oc_types::now_millis())
            .await;
        let credential = pool.into_iter().next();
        let api_key = credential.as_ref().map(|entry| entry.key.clone());
        let request = LlmRequest {
            model: model.clone(),
            system: system.to_vec(),
            messages: messages.to_vec(),
            tools: turn.tools.clone(),
            options: LlmOptions {
                api_key: api_key.clone(),
                credential_id: credential
                    .as_ref()
                    .and_then(|entry| entry.credential_id.clone())
                    .or_else(|| preferred_key.map(ToOwned::to_owned)),
                tool_choice: Some("none".to_owned()),
                cache_key: Some(session.to_owned()),
                ..LlmOptions::default()
            },
        };
        let stream_result = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = self.inner.model.stream(request) => Some(result),
        };
        let Some(stream_result) = stream_result else {
            return Err(Box::new(CompactionOutcome::Cancelled));
        };
        match stream_result {
            Ok(stream) => Ok((stream, credential)),
            Err(error) if is_context_overflow_message(&error.to_string()) => {
                self.retry_compaction_overflow(attempt, api_key, credential)
                    .await
            }
            Err(error) => Err(Box::new(CompactionOutcome::Failed(compaction_failure(
                Self::classify_compaction_open_error(&error),
                credential,
                RecordPolicy::ModelAndKey,
            )))),
        }
    }

    async fn retry_compaction_overflow(
        &self,
        attempt: &CompactionAttempt<'_>,
        api_key: Option<String>,
        credential: Option<oc_llm::PoolKey>,
    ) -> Result<(oc_llm::LlmStream, Option<oc_llm::PoolKey>), Box<CompactionOutcome>> {
        let CompactionAttempt {
            session,
            turn,
            model,
            context,
            messages,
            system,
            cancel,
        } = *attempt;
        let instr = messages.last().cloned().unwrap_or_else(|| {
            LlmMessage::user(compaction_summary_instruction(None, "overflow", 0, ""))
        });
        let instr_tokens = oc_llm::estimate_messages_tokens(std::slice::from_ref(&instr));
        let sys_tokens: u64 = system
            .iter()
            .map(|s| u64::try_from(s.len() / 4).unwrap_or(0))
            .sum();
        let max_input_tokens = if model.model.limit.context > 0 {
            (model.model.limit.context * 7 / 10)
                .saturating_sub(instr_tokens.saturating_add(sys_tokens))
                .max(2_048)
        } else {
            0
        };
        let mut shortened = shorten_messages_for_compaction_to_limit(context, max_input_tokens);
        shortened.push(instr);
        let retry = LlmRequest {
            model: model.clone(),
            system: system.to_vec(),
            messages: shortened,
            tools: turn.tools.clone(),
            options: LlmOptions {
                api_key,
                tool_choice: Some("none".to_owned()),
                cache_key: Some(session.to_owned()),
                ..LlmOptions::default()
            },
        };
        let retry_result = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = self.inner.model.stream(retry) => Some(result),
        };
        match retry_result {
            Some(Ok(stream)) => Ok((stream, credential)),
            Some(Err(error)) => Err(Box::new(CompactionOutcome::Failed(compaction_failure(
                Self::classify_compaction_open_error(&error),
                credential,
                RecordPolicy::ModelAndKey,
            )))),
            None => Err(Box::new(CompactionOutcome::Cancelled)),
        }
    }

    fn classify_compaction_open_error(error: &oc_llm::LlmError) -> oc_llm::LlmError {
        match error {
            oc_llm::LlmError::Provider { message } => {
                super::stream::classified_provider_error(message)
            }
            oc_llm::LlmError::Failure { kind, message } => oc_llm::LlmError::Failure {
                kind: *kind,
                message: message.clone(),
            },
            other => oc_llm::LlmError::Provider {
                message: other.to_string(),
            },
        }
    }

    /// Record a key-level compaction failure so the next turn skips that key.
    /// Compaction keeps the single-request shape (it reuses one shortened
    /// prompt), so it never rotates mid-attempt: a `Quota`/`Auth`/`RateLimit`
    /// failure only cools the key inside [`oc_llm::KeyPool`]. The durable primary
    /// pointer is left alone here — a turn step owns that decision
    /// (`Runtime::mark_key_failure`) so a compaction never rewrites credential
    /// order on its own.
    fn mark_compaction_key(
        &self,
        provider: &str,
        credential: Option<&oc_llm::PoolKey>,
        error: &oc_llm::LlmError,
    ) {
        let Some(credential) = credential else {
            return;
        };
        let kind = match error {
            oc_llm::LlmError::Failure { kind, .. } => *kind,
            oc_llm::LlmError::Http { status, .. } => match status {
                401..=403 => oc_llm::FailureKind::Auth,
                429 => oc_llm::FailureKind::RateLimit,
                _ => return,
            },
            _ => return,
        };
        self.key_pool().mark(
            provider,
            credential.credential_id.as_deref(),
            kind,
            oc_types::now_millis(),
        );
    }

    /// Recent-context budget preserved across a compaction (`compaction.keep.tokens`).
    fn compaction_keep_tokens(&self) -> u64 {
        self.compaction_config().keep_tokens
    }

    /// `compaction` settings from the merged config document.
    ///
    /// `auto` is honored by the turn loop; the default is `false`, so
    /// threshold compaction runs only when the document opts in.
    pub(super) fn compaction_config(&self) -> crate::compaction::CompactionConfig {
        crate::compaction::CompactionConfig::from_merged(self.inner.config.config.merged())
    }

    /// The compacting session plus every descendant, breadth-first.
    ///
    /// A compaction expires only the temporary boxes owned by this subtree
    /// (R3). The session itself is always included, even when the tree lookup
    /// fails, so the box expiry still covers the caller; a lookup error is
    /// logged and never fails the compaction.
    async fn compaction_owner_sessions(&self, session: &str) -> Vec<String> {
        let mut owners = vec![session.to_owned()];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        seen.insert(session.to_owned());
        queue.push_back(session.to_owned());
        while let Some(parent) = queue.pop_front() {
            let children = match self.inner.config.store.children(&parent).await {
                Ok(children) => children,
                Err(error) => {
                    tracing::warn!(
                        session = %session,
                        parent = %parent,
                        error = %error,
                        "could not list child sessions while scoping box expiry"
                    );
                    continue;
                }
            };
            for child in children {
                if !seen.insert(child.id.clone()) {
                    continue;
                }
                queue.push_back(child.id.clone());
                owners.push(child.id);
            }
        }
        owners
    }

    /// Oldest stored message to keep verbatim after a compaction.
    ///
    /// The summary replaces older history; the tail keeps the active turn (or
    /// the most recent `keep_tokens` budget) so the next request does not lose
    /// the prompt that triggered compaction. `None` means "summary only".
    async fn compaction_keep_id(
        &self,
        session: &str,
        keep_tokens: u64,
    ) -> Result<Option<String>, RuntimeError> {
        if keep_tokens == 0 {
            return Ok(None);
        }
        let messages = self.list_all_messages(session).await?;
        let boundary = messages.iter().rposition(|message| {
            message.get("type").and_then(Value::as_str) == Some("compaction")
                && message.get("status").and_then(Value::as_str) == Some("completed")
        });
        let start = boundary.map_or(0, |index| index + 1);
        let candidates: Vec<&Value> = messages[start..]
            .iter()
            .filter(|message| {
                matches!(
                    message.get("type").and_then(Value::as_str),
                    Some(
                        "user"
                            | "synthetic"
                            | "system"
                            | "skill"
                            | "peer"
                            | "observer"
                            | "assistant"
                    )
                )
            })
            .collect();
        if candidates.is_empty() {
            return Ok(None);
        }
        let costs: Vec<u64> = candidates
            .iter()
            .map(|message| {
                let converted = crate::prompt::messages_from_store(std::slice::from_ref(*message));
                oc_llm::estimate_messages_tokens(&converted)
            })
            .collect();
        let total: u64 = costs.iter().sum();
        let mut keep_start = candidates.len();
        if total <= keep_tokens {
            // Keep the active turn: the last user/peer message and everything
            // after it. Keeping the entire small history would duplicate the
            // summary without reducing the prompt.
            keep_start = candidates
                .iter()
                .rposition(|message| {
                    matches!(
                        message.get("type").and_then(Value::as_str),
                        Some("user" | "synthetic" | "peer" | "observer")
                    )
                })
                .unwrap_or(candidates.len().saturating_sub(1));
        } else {
            let mut accumulated = 0_u64;
            for index in (0..candidates.len()).rev() {
                let next = accumulated.saturating_add(costs[index]);
                if next > keep_tokens {
                    // Only a fresh user/peer/synthetic/observer prompt at the very end of
                    // the transcript may exceed `keep_tokens` on its own; an
                    // oversized assistant/tool message must be summarized rather
                    // than replayed verbatim into the post-compaction context.
                    let is_trigger_prompt = accumulated == 0
                        && matches!(
                            candidates[index].get("type").and_then(Value::as_str),
                            Some("user" | "synthetic" | "peer" | "observer")
                        );
                    if !is_trigger_prompt {
                        break;
                    }
                }
                accumulated = next;
                keep_start = index;
                if accumulated >= keep_tokens {
                    break;
                }
            }
        }
        Ok(candidates
            .get(keep_start)
            .and_then(|message| message.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    async fn fail_compaction(
        &self,
        session: &str,
        directory: &str,
        request: CompactionRequest<'_>,
        id: &MessageId,
        created: i64,
        message: &str,
    ) -> Result<(), RuntimeError> {
        let CompactionRequest {
            trigger, inbox_id, ..
        } = request;
        let reason = event_reason(trigger);
        let mut failed = json!({
            "time": {"created": created},
            "status": "failed",
            "reason": reason,
            "trigger": trigger,
            "error": {
                "type": "CompactionError",
                "message": message
            }
        });
        if let Some(inbox_id) = inbox_id {
            failed["inboxID"] = json!(inbox_id);
        }
        self.inner
            .config
            .store
            .update_message(session, id.as_str(), failed)
            .await?;
        self.emit(
            session,
            directory,
            SessionCompactionFailed {
                session_id: Runtime::session_id(session),
                reason: reason.to_owned(),
                error: StructuredError::new("CompactionError", message),
                input_id: Some(id.clone()),
                cost: None,
                tokens: None,
                inbox_id: inbox_id.map(|id| MessageId::from_wire(id.to_owned())),
                trigger: Some(trigger),
            },
        )
        .await?;
        Ok(())
    }
}

/// Build the compaction summary instruction, including the preservation
/// guidance required by `docs/29-step-budget.md`.
///
/// A compaction must not silently drop the state the step-budget decision and
/// the next turn depend on: the selected model/variant, tool state (pending and
/// cancelled calls, lanes, diagnostics), open goals (id/status/criteria/next
/// step) and the exact `keepID` boundary with its reason/threshold context. The
/// boundary is only named when one was computed; the overflow retry passes no
/// keep ID because it rebuilds a shortened context.
fn compaction_summary_instruction(
    keep_id: Option<&str>,
    reason: &str,
    keep_tokens: u64,
    box_index: &str,
) -> String {
    compaction_summary_instruction_with_note(keep_id, reason, keep_tokens, box_index, None)
}

fn compaction_summary_instruction_with_note(
    keep_id: Option<&str>,
    reason: &str,
    keep_tokens: u64,
    box_index: &str,
    note: Option<&str>,
) -> String {
    let mut instruction = format!(
        "{}\n\nPreserve across this compaction: the selected model and variant; tool state \
         (pending and cancelled calls, lanes and diagnostics); open goals (id, status, criteria \
         and next step); and the exact keepID boundary plus the reason and threshold context that \
         made this compaction necessary. Compaction reason: {reason}; configured keep window: \
         {keep_tokens} tokens.",
        crate::compaction::summary_prompt(None)
    );
    if let Some(note) = note.map(str::trim).filter(|s| !s.is_empty()) {
        let _ = write!(instruction, "\nFocus / handoff note: {note}");
    }
    if let Some(keep_id) = keep_id {
        let _ = write!(instruction, "\nExact keepID: {keep_id}.");
    }
    if !box_index.is_empty() {
        let _ = write!(
            instruction,
            "\n\nDurable context boxes of this session tree. Reference their keys instead of \
             restating stable content; temporary boxes expire now. Load details with `box_read` \
             when needed:\n{box_index}"
        );
    }
    instruction
}

/// Read one summary stream to completion.
///
/// A stream that ends without text is [`CompactionOutcome::Empty`]; an in-band
/// provider frame is classified and recorded against the KEY only, because the
/// request was accepted and the model itself may still be fine.
async fn read_compaction_summary(
    mut stream: oc_llm::LlmStream,
    cancel: &Arc<CancelToken>,
    credential: Option<oc_llm::PoolKey>,
) -> CompactionOutcome {
    let mut summary = String::new();
    let mut usage = None;
    let mut failure = None;
    loop {
        let event = tokio::select! {
            biased;
            () = cancel.cancelled() => return CompactionOutcome::Cancelled,
            event = stream.next() => event,
        };
        let Some(event) = event else {
            break;
        };
        match event {
            oc_llm::LlmEvent::TextDelta { delta } => summary.push_str(&delta),
            oc_llm::LlmEvent::TextEnd { text } => summary = text,
            oc_llm::LlmEvent::Finish { usage: finish, .. } => usage = Some(finish),
            oc_llm::LlmEvent::ProviderError { message } => {
                failure = Some(message);
                break;
            }
            _ => {}
        }
    }
    if let Some(message) = failure {
        let error = super::stream::classified_provider_error(&message);
        // An in-band quota/auth frame on the summary request still costs the
        // key: record it so the next turn rotates away from it.
        return CompactionOutcome::Failed(compaction_failure(
            error,
            credential,
            RecordPolicy::KeyOnly,
        ));
    }
    if summary.trim().is_empty() {
        return CompactionOutcome::Empty;
    }
    CompactionOutcome::Produced { summary, usage }
}

/// Classify one failed attempt and keep the provider text verbatim for the row.
fn compaction_failure(
    error: oc_llm::LlmError,
    credential: Option<oc_llm::PoolKey>,
    record: RecordPolicy,
) -> CompactionFailure {
    let message = match &error {
        oc_llm::LlmError::Failure { message, .. } => message.clone(),
        other => other.to_string(),
    };
    CompactionFailure {
        kind: failure_kind(&error),
        message,
        error,
        credential,
        record,
    }
}

/// Everything one compaction attempt needs.
///
/// Grouped because the attempt is one request/response exchange over a fixed
/// context; carrying seven positional parameters made every call site and
/// signature change a risk.
struct CompactionAttempt<'a> {
    /// Session being compacted (used for the key's `cache_key` and event ids).
    session: &'a str,
    /// Resolved turn: supplies the tool list and the event directory.
    turn: &'a ResolvedTurn,
    /// Candidate model for this attempt.
    model: &'a oc_llm::ResolvedModel,
    /// Unshortened conversation, replayed once more if the request overflows.
    context: &'a [LlmMessage],
    /// Conversation plus the summary instruction.
    messages: &'a [LlmMessage],
    /// System prompt parts.
    system: &'a [String],
    /// Cancellation for the stream open and every read.
    cancel: &'a Arc<CancelToken>,
}

/// Maximum model candidates one compaction may attempt (automatic fallback
/// between models is disabled).
const COMPACTION_MAX_ATTEMPTS: usize = 1;

/// What a compaction failure is recorded against.
///
/// A failure on the stream-open path says the model could not serve at all, so
/// it is recorded against the model AND the key; an in-band frame arrives after
/// the request was accepted and only costs the key. `Nothing` exists so the
/// caller's intent is explicit rather than implied by an absent field.
enum RecordPolicy {
    /// Mark the model unhealthy and cool the key.
    ModelAndKey,
    /// Cool the key only; the model may still be fine.
    KeyOnly,
}

/// One compaction attempt's classified failure.
struct CompactionFailure {
    /// Class the fallback policy decides on.
    kind: oc_llm::FailureKind,
    /// Provider text, preserved verbatim for the failed row.
    message: String,
    /// Original error, surfaced unchanged when the attempt is the last one.
    error: oc_llm::LlmError,
    /// Key the request used, if any; cooled by [`Runtime::mark_compaction_key`].
    credential: Option<oc_llm::PoolKey>,
    /// What to record this failure against.
    record: RecordPolicy,
}

/// Result of one compaction attempt against one model candidate.
enum CompactionOutcome {
    /// The model produced a non-empty summary.
    Produced {
        summary: String,
        usage: Option<oc_llm::Usage>,
    },
    /// The execution was cancelled before or during the stream.
    Cancelled,
    /// The stream completed without any text.
    Empty,
    /// The request failed in a classifiable way.
    Failed(CompactionFailure),
}

/// Classify an [`oc_llm::LlmError`] into the class the fallback policy reads.
///
/// [`oc_llm::LlmError::Failure`] is already classified. The remaining variants
/// map onto their closest class so an untyped transport error can still advance
/// a chain entry that a `Failure` would have advanced; anything with no
/// provider meaning stays `Unknown`, which no policy advances.
fn failure_kind(error: &oc_llm::LlmError) -> oc_llm::FailureKind {
    use oc_llm::FailureKind;
    match error {
        oc_llm::LlmError::Failure { kind, .. } => *kind,
        oc_llm::LlmError::Http { status, .. } => match status {
            401..=403 => FailureKind::Auth,
            429 => FailureKind::RateLimit,
            408 | 500..=599 => FailureKind::Transport,
            _ => FailureKind::Unknown,
        },
        oc_llm::LlmError::Timeout { .. } | oc_llm::LlmError::Transport { .. } => {
            FailureKind::Transport
        }
        oc_llm::LlmError::Provider { message } => oc_llm::classify_provider_message(message),
        oc_llm::LlmError::InvalidRequest { .. } => FailureKind::InvalidRequest,
        _ => FailureKind::Unknown,
    }
}

/// Compaction `reason` for one trigger.
///
/// `reason` keeps the upstream `auto`/`manual` vocabulary for compatibility
/// while [`trigger`](CompactionTrigger) names the true cause. `manual` and
/// `command` are user intent, everything the guard/overflow path raises is
/// `auto`; the distinction between guard, overflow and observer now lives in
/// `trigger` instead of being collapsed away.
fn trigger_wire(trigger: CompactionTrigger) -> &'static str {
    match trigger {
        CompactionTrigger::Guard => "guard",
        CompactionTrigger::Overflow => "overflow",
        CompactionTrigger::Manual => "manual",
        CompactionTrigger::Observer => "observer",
        CompactionTrigger::Command => "command",
    }
}

fn event_reason(trigger: CompactionTrigger) -> &'static str {
    match trigger {
        CompactionTrigger::Manual | CompactionTrigger::Command => "manual",
        CompactionTrigger::Guard | CompactionTrigger::Overflow | CompactionTrigger::Observer => {
            "auto"
        }
    }
}

/// The trigger a stored inbox `reason` names, if it names one.
///
/// Queue rows are written by several owners (`/compact`, the tool, the
/// observer, `/compress`); an unknown or absent reason is a manual request,
/// which is what every legacy row meant.
fn trigger_for_reason(reason: &str) -> Option<CompactionTrigger> {
    CompactionTrigger::from_reason(reason)
}

/// Everything one compaction run needs besides the context it summarizes.
///
/// Grouped so the trigger, the admitting inbox row and the token evidence
/// travel together: the started/ended/failed payloads must name the same
/// `inboxID` the client's queued badge is keyed by, and `before_tokens` must be
/// the estimate that crossed the trigger rather than re-derived later.
#[derive(Debug, Clone, Copy)]
pub(super) struct CompactionRequest<'a> {
    /// True cause of this compaction.
    pub(super) trigger: CompactionTrigger,
    /// Optional focused handoff note.
    pub(super) note: Option<&'a str>,
    /// Inbox row that admitted this run, when there is one.
    pub(super) inbox_id: Option<&'a str>,
    /// Token estimate that crossed the trigger.
    pub(super) estimate_tokens: Option<u64>,
    /// Trigger threshold in force.
    pub(super) trigger_tokens: Option<u64>,
    /// Configured hard ceiling, when one applied.
    pub(super) ceiling_tokens: Option<u64>,
}

impl<'a> CompactionRequest<'a> {
    pub(super) fn new(trigger: CompactionTrigger) -> Self {
        Self {
            trigger,
            note: None,
            inbox_id: None,
            estimate_tokens: None,
            trigger_tokens: None,
            ceiling_tokens: None,
        }
    }

    pub(super) fn with_note(mut self, note: Option<&'a str>) -> Self {
        self.note = note;
        self
    }

    pub(super) fn with_inbox(mut self, inbox_id: Option<&'a str>) -> Self {
        self.inbox_id = inbox_id;
        self
    }

    /// Attach the token evidence behind the decision. `ceiling` is `None` when
    /// the model has no bounded window, so the optional field is omitted rather
    /// than serialized as a meaningless number.
    pub(super) fn with_pressure(
        mut self,
        estimate: u64,
        trigger: u64,
        ceiling: Option<u64>,
    ) -> Self {
        self.estimate_tokens = Some(estimate);
        self.trigger_tokens = Some(trigger);
        self.ceiling_tokens = ceiling;
        self
    }
}

pub(super) fn is_context_overflow_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("prompt is too long")
        || lower.contains("context window")
        || lower.contains("too many tokens")
        || lower.contains("input is too long")
        || lower.contains("exceeds the model's maximum context")
}

/// Mask oversized tool outputs, huge text/tool-call bodies, and inline media
/// when a compaction request itself exceeds the provider's context window.
#[cfg(test)]
pub(super) fn shorten_messages_for_compaction(messages: &[LlmMessage]) -> Vec<LlmMessage> {
    shorten_messages_for_compaction_to_limit(messages, 0)
}

pub(super) fn shorten_messages_for_compaction_to_limit(
    messages: &[LlmMessage],
    max_input_tokens: u64,
) -> Vec<LlmMessage> {
    const HEAD_CHARS: usize = 1_000;
    const TAIL_CHARS: usize = 500;
    const MAX_TOOL_CHARS: usize = 2_000;
    const MAX_TEXT_CHARS: usize = 12_000;
    const TEXT_HEAD_CHARS: usize = 4_000;
    const TEXT_TAIL_CHARS: usize = 4_000;
    const MAX_TOOL_INPUT_CHARS: usize = 4_000;

    let mut shortened: Vec<LlmMessage> = messages
        .iter()
        .map(|message| {
            let content = message
                .content
                .iter()
                .map(|part| match part {
                    oc_llm::ContentPart::ToolResult {
                        tool_call_id,
                        name,
                        content,
                        is_error,
                    } if content.chars().count() > MAX_TOOL_CHARS => {
                        let total = content.chars().count();
                        let head: String = content.chars().take(HEAD_CHARS).collect();
                        let tail: String = content.chars().skip(total - TAIL_CHARS).collect();
                        let omitted = total.saturating_sub(HEAD_CHARS + TAIL_CHARS);
                        oc_llm::ContentPart::ToolResult {
                            tool_call_id: tool_call_id.clone(),
                            name: name.clone(),
                            content: format!(
                                "{head}\n...[omitted {omitted} chars during compaction]...\n{tail}"
                            ),
                            is_error: *is_error,
                        }
                    }
                    oc_llm::ContentPart::Text { text } if text.chars().count() > MAX_TEXT_CHARS => {
                        let total = text.chars().count();
                        let head: String = text.chars().take(TEXT_HEAD_CHARS).collect();
                        let tail: String = text.chars().skip(total - TEXT_TAIL_CHARS).collect();
                        let omitted = total.saturating_sub(TEXT_HEAD_CHARS + TEXT_TAIL_CHARS);
                        oc_llm::ContentPart::text(format!(
                            "{head}\n...[omitted {omitted} chars during compaction]...\n{tail}"
                        ))
                    }
                    oc_llm::ContentPart::ToolCall { id, name, input } => {
                        let serialized = serde_json::to_string(input).unwrap_or_default();
                        if serialized.chars().count() > MAX_TOOL_INPUT_CHARS {
                            let preview: String = serialized.chars().take(HEAD_CHARS).collect();
                            oc_llm::ContentPart::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                input: json!({
                                    "_truncated": true,
                                    "_preview": preview,
                                }),
                            }
                        } else {
                            oc_llm::ContentPart::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            }
                        }
                    }
                    oc_llm::ContentPart::Image { media_type, data } => {
                        oc_llm::ContentPart::text(format!(
                            "[omitted {media_type} attachment ({} chars) during compaction]",
                            data.len()
                        ))
                    }
                    oc_llm::ContentPart::Document {
                        media_type, data, ..
                    } => oc_llm::ContentPart::text(format!(
                        "[omitted {media_type} document ({} chars) during compaction]",
                        data.len()
                    )),
                    other => other.clone(),
                })
                .collect();
            LlmMessage::new(message.role, content)
        })
        .collect();

    if max_input_tokens > 0 && shortened.len() > 3 {
        let mut omitted_turns = 0_usize;
        while shortened.len() > 3 && oc_llm::estimate_messages_tokens(&shortened) > max_input_tokens
        {
            shortened.remove(1);
            omitted_turns += 1;
        }
        if omitted_turns > 0 {
            shortened.insert(
                1,
                LlmMessage::user(format!(
                    "[{omitted_turns} earlier conversation message(s) omitted to fit the \
                     compaction context limit]"
                )),
            );
        }
    }
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_prompt_preserves_budget_and_goal_context() {
        let prompt = compaction_summary_instruction(
            Some("msg_keep"),
            "manual",
            15_000,
            "ses_root key=plan (stable)",
        );
        for needle in [
            "model and variant",
            "pending and cancelled calls, lanes and diagnostics",
            "open goals (id, status, criteria and next step)",
            "Exact keepID: msg_keep",
            "Compaction reason: manual",
            "15000 tokens",
            "ses_root key=plan (stable)",
        ] {
            assert!(prompt.contains(needle), "missing {needle:?} in:\n{prompt}");
        }

        assert!(prompt.contains("compact_context") || !prompt.is_empty());
        let focused = compaction_summary_instruction_with_note(
            Some("msg_keep"),
            "manual",
            15_000,
            "ses_root key=plan (stable)",
            Some("keep SQLite migration 0013 status"),
        );
        assert!(focused.contains("Focus / handoff note: keep SQLite migration 0013 status"));

        let retry = compaction_summary_instruction(None, "overflow", 0, "");
        assert!(retry.contains("Compaction reason: overflow"));
        assert!(!retry.contains("Exact keepID:"));
        assert!(!retry.contains("ses_root key=plan"));
    }

    #[test]
    fn shorten_messages_to_limit_caps_large_text_and_drops_middle_turns() {
        let mut msgs = vec![LlmMessage::user("Objective: finish the rewrite cleanly")];
        for i in 0..20 {
            msgs.push(LlmMessage::assistant(format!(
                "Step {i}: {}",
                "a".repeat(20_000)
            )));
        }
        msgs.push(LlmMessage::user("Latest state right now"));

        let bounded = shorten_messages_for_compaction_to_limit(&msgs, 12_000);
        assert!(
            oc_llm::estimate_messages_tokens(&bounded) <= 12_000 || bounded.len() <= 4,
            "bounded tokens should fit target budget"
        );
        assert_eq!(
            bounded[0].content[0].as_text(),
            Some("Objective: finish the rewrite cleanly")
        );
        assert_eq!(
            bounded.last().and_then(|m| m.content[0].as_text()),
            Some("Latest state right now")
        );
    }

    #[test]
    fn shorten_messages_for_compaction_truncates_oversized_last_tool_result_and_media() {
        let huge = "x".repeat(100_000);
        let messages = vec![
            LlmMessage::user("read the screenshot"),
            LlmMessage::tool_result("call_1", Some("read".to_owned()), huge, false),
            LlmMessage::new(
                oc_llm::Role::User,
                vec![oc_llm::ContentPart::image("image/png", "A".repeat(50_000))],
            ),
        ];
        let shortened = shorten_messages_for_compaction(&messages);
        let oc_llm::ContentPart::ToolResult { content, .. } = &shortened[1].content[0] else {
            panic!("expected ToolResult");
        };
        assert!(
            content.chars().count() < 2_000,
            "oversized last tool result must be shortened during compaction fallback, got {} chars",
            content.chars().count()
        );
        assert!(content.contains("omitted 98500 chars during compaction"));
        assert_eq!(
            shortened[2].content[0].as_text(),
            Some("[omitted image/png attachment (50000 chars) during compaction]")
        );
    }
}
