use oc_observer::RepairOutcome;
use oc_store::{Delivery, MessageQuery, NewInbox, Order, SessionPatch};
use oc_types::payload::SessionModelSelected;
use oc_types::{InterruptReason, MessageId, ModelRef, SessionId, now_millis};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::runtime::{Runtime, RuntimeError};

use super::repair::{RepairCommand, switch_model_target};

/// The runtime performed the command; `detail` says what it did.
fn applied(detail: String) -> RepairOutcome {
    RepairOutcome {
        applied: true,
        detail: Some(detail),
    }
}

/// The runtime answered and did **not** perform the command. `reason` is the
/// refusal the observer publishes instead of a success.
fn refused(reason: String) -> RepairOutcome {
    RepairOutcome {
        applied: false,
        detail: Some(reason),
    }
}

/// Normalize a model variant for effective-selection comparison: absent, empty
/// and `"default"` all select the same model, while real variants stay distinct.
fn normalized_variant(variant: Option<&str>) -> Option<&str> {
    variant.filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("default"))
}

/// Whether switching from `previous` to `next` changes the effective selection.
/// A change only in spelling between absent/empty/"default" is not a switch.
fn is_model_switch(previous: &ModelRef, next: &ModelRef) -> bool {
    previous.provider_id != next.provider_id
        || previous.id != next.id
        || normalized_variant(previous.variant.as_deref())
            != normalized_variant(next.variant.as_deref())
}

impl Runtime {
    /// Enqueue an observer synthetic message and notify its session.
    ///
    /// Returns whether a new row was queued: `false` means an observer
    /// synthetic was already pending and this call dropped a duplicate, so the
    /// caller must not report that it delivered anything.
    pub(crate) async fn observer_synthetic(
        &self,
        session: &str,
        text: &str,
        root: &str,
    ) -> Result<bool, RuntimeError> {
        if let Ok(pending) = self.inner.config.store.list_inbox(session).await {
            let already_pending = pending.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("synthetic")
                    && item
                        .pointer("/payload/metadata/observer")
                        .and_then(Value::as_bool)
                        == Some(true)
            });
            if already_pending {
                tracing::debug!(
                    session = %session,
                    "observer synthetic already pending in inbox; dropping duplicate"
                );
                return Ok(false);
            }
        }
        let payload = json!({
            "text": text,
            "metadata": { "observer": true, "target": root, "sessionID": session },
            "time": { "created": now_millis() },
        });
        self.enqueue_and_emit(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: session.to_owned(),
            kind: "synthetic".to_owned(),
            payload,
            delivery: Delivery::Steer,
        })
        .await?;
        self.notify_local(session).await?;
        Ok(true)
    }

    async fn observer_compaction(
        &self,
        session: &str,
        note: Option<&str>,
    ) -> Result<(), RuntimeError> {
        let mut payload = serde_json::Map::new();
        payload.insert("time".to_owned(), json!({ "created": now_millis() }));
        payload.insert("status".to_owned(), Value::String("pending".to_owned()));
        payload.insert("reason".to_owned(), Value::String("observer".to_owned()));
        if let Some(note) = note {
            payload.insert("note".to_owned(), Value::String(note.to_owned()));
        }
        self.enqueue_and_emit(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: session.to_owned(),
            kind: "compaction".to_owned(),
            payload: Value::Object(payload),
            delivery: Delivery::Queue,
        })
        .await?;
        self.notify_local(session).await
    }

    /// Apply a model switch.
    ///
    /// `Ok(Some(model))` is the model the session now uses; `Ok(None)` means the
    /// session already used it, so nothing was patched and no event was
    /// published (a caller must not report that as a switch). A catalog
    /// rejection is a [`RuntimeError::Config`]: the runtime decided, and
    /// [`Self::apply_switch_model`] turns that decision into a refusal receipt
    /// instead of an absent answer.
    async fn observer_switch_model(
        &self,
        session: &str,
        model: &str,
    ) -> Result<Option<String>, RuntimeError> {
        let value = self
            .inner
            .config
            .store
            .get_session(session)
            .await?
            .ok_or_else(|| RuntimeError::SessionNotFound(session.to_owned()))?;
        let info = Runtime::parse_session(&value)?;
        let model_ref = crate::prompt::model_ref(&Value::String(model.to_owned()))
            .ok_or_else(|| RuntimeError::Config(format!("invalid model reference {model:?}")))?;
        // A repair must never persist a model the catalog cannot run: every
        // later turn would fail at `resolve_turn` and the session would be
        // stuck until a human patches it.
        if let Err(error) = self
            .inner
            .config
            .client
            .resolve_model(&self.inner.config.catalog.current(), model)
        {
            return Err(RuntimeError::Config(format!(
                "repair model {model:?} is not in the catalog: {error}"
            )));
        }
        let previous = info.model.clone();
        // Catalog validation above is still required, but a variant spelling
        // change (absent/empty/"default") is not a real model switch: avoid a
        // redundant durable session patch and `session.model.selected` event.
        if previous
            .as_ref()
            .is_some_and(|prev| !is_model_switch(prev, &model_ref))
        {
            tracing::debug!(session = %session, model = %model, "observer switch model is a no-op; skipping patch and event");
            return Ok(None);
        }
        self.inner
            .config
            .store
            .patch_session(
                session,
                SessionPatch {
                    model: Some(
                        serde_json::to_value(&model_ref)
                            .map_err(|error| RuntimeError::Config(error.to_string()))?,
                    ),
                    ..SessionPatch::default()
                },
            )
            .await?;
        self.emit(
            session,
            &info.location.directory,
            SessionModelSelected {
                session_id: SessionId::from_wire(session.to_owned()),
                model: model_ref,
                previous,
            },
        )
        .await?;
        Ok(Some(model.to_owned()))
    }

    /// Drain repair commands and answer each one with the runtime's verdict.
    pub(crate) async fn observer_repair_loop(
        &self,
        mut receiver: UnboundedReceiver<RepairCommand>,
    ) {
        loop {
            let command = tokio::select! {
                biased;
                () = self.inner.shutdown.cancelled() => return,
                command = receiver.recv() => command,
            };
            let Some(mut command) = command else {
                return;
            };
            // Take the receipt before applying: the sender answers once, and a
            // panic in `apply` then still drops it rather than leaving the
            // caller waiting for the full ack timeout.
            let reply = command.take_reply();
            let result = self.apply_repair_command(&command).await;
            if let Some(reply) = reply {
                // Forward exactly what the runtime decided: a verdict, or the
                // reason it could not reach one. A dropped receiver means the
                // observer stopped waiting; the engine then reports the repair
                // as unconfirmed, never as applied.
                let answer = match &result {
                    Ok(outcome) => Ok(outcome.clone()),
                    Err(error) => Err(error.to_string()),
                };
                if reply.send(answer).is_err() {
                    tracing::debug!("observer repair receipt dropped: the caller stopped waiting");
                }
            }
            if let Err(error) = result {
                tracing::warn!(error = %error, "observer repair failed");
            }
        }
    }

    /// Apply one repair command and report what actually happened.
    ///
    /// `Ok(applied: true)` is reserved for commands the runtime performed.
    /// Every skip path returns `Ok(applied: false)` with the reason, so the
    /// observer can no longer audit a queued-but-unapplied repair as a success.
    /// `Err` means the runtime could not reach a decision at all (store
    /// failure, internal error); the worker forwards it as a failed receipt.
    async fn apply_repair_command(
        &self,
        command: &RepairCommand,
    ) -> Result<RepairOutcome, RuntimeError> {
        match command {
            RepairCommand::Interrupt { session_id, .. } => self.apply_interrupt(session_id).await,
            RepairCommand::CancelTool { tool_call_id, .. } => {
                Ok(self.apply_cancel_tool(tool_call_id))
            }
            RepairCommand::RestartSubagent {
                session_id, note, ..
            } => self.apply_restart_subagent(session_id, note).await,
            RepairCommand::Compact { session_id, .. } => self.apply_compact(session_id).await,
            RepairCommand::SwitchModel {
                session_id, model, ..
            } => self.apply_switch_model(session_id, model).await,
            RepairCommand::Repost {
                session_id, text, ..
            } => self.apply_repost(session_id, text).await,
            RepairCommand::RequestBudgetExtension {
                session_id, steps, ..
            } => self.apply_budget_extension(session_id, *steps).await,
        }
    }

    async fn apply_interrupt(&self, session_id: &str) -> Result<RepairOutcome, RuntimeError> {
        if self
            .interrupt_with_reason(session_id, InterruptReason::Superseded)
            .await?
        {
            Ok(applied(format!(
                "interrupted the execution in {session_id}"
            )))
        } else {
            Ok(refused(format!(
                "interrupt found no running execution or queued input for {session_id}"
            )))
        }
    }

    fn apply_cancel_tool(&self, tool_call_id: &str) -> RepairOutcome {
        let cancelled = self.cancel_tool_call(tool_call_id);
        if cancelled == 0 {
            tracing::debug!(
                tool_call_id = %tool_call_id,
                "observer cancel_tool found no running tool call"
            );
            refused(format!(
                "cancel_tool found no running tool call {tool_call_id}"
            ))
        } else {
            applied(format!(
                "cancelled {cancelled} tool call(s) of {tool_call_id}"
            ))
        }
    }

    async fn apply_restart_subagent(
        &self,
        session_id: &str,
        note: &str,
    ) -> Result<RepairOutcome, RuntimeError> {
        if !self
            .interrupt_with_reason(session_id, InterruptReason::Superseded)
            .await?
        {
            tracing::debug!(session = %session_id, "observer restart found no running execution");
        }
        let Some(prompt) = self.session_text(session_id, "user", Order::Asc).await? else {
            tracing::warn!(
                session = %session_id,
                "restart_subagent skipped: the child has no user prompt to replay"
            );
            return Ok(refused(format!(
                "restart_subagent skipped: {session_id} has no user prompt to replay"
            )));
        };
        let text = format!("{prompt}\n\n[observer restart] {note}");
        let root = self.session_root(session_id).await?;
        if self.observer_synthetic(session_id, &text, &root).await? {
            Ok(applied(format!("replayed the prompt of {session_id}")))
        } else {
            Ok(refused(format!(
                "restart_subagent skipped: an observer synthetic is already pending for {session_id}"
            )))
        }
    }

    async fn apply_compact(&self, session_id: &str) -> Result<RepairOutcome, RuntimeError> {
        if self.pending_compaction(session_id).await? {
            return Ok(refused(format!(
                "compact_context skipped: a compaction is already queued for {session_id}"
            )));
        }
        self.observer_compaction(session_id, None).await?;
        // The receipt covers what the runtime owns: the request is durable and
        // the actor is woken. Awaited completion would block this worker for a
        // whole model call, so the detail says "queued", not "compacted".
        Ok(applied(format!(
            "queued a compaction for {session_id}; it runs at the next step boundary"
        )))
    }

    async fn apply_switch_model(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<RepairOutcome, RuntimeError> {
        let requested = switch_model_target(
            model.to_owned(),
            self.configured_escalation_model(),
            session_id,
        )?;
        match self.observer_switch_model(session_id, &requested).await {
            Ok(Some(switched)) => Ok(applied(format!("switched {session_id} to {switched}"))),
            Ok(None) => Ok(refused(format!(
                "switch_model skipped: {session_id} already uses {requested}"
            ))),
            // The runtime reached a decision and refused it: a model the catalog
            // cannot run, a malformed reference or an unconfigured escalation
            // model. Reporting that as `applied: false` charges the observer's
            // repair budget, so a permanently impossible switch runs out of
            // attempts instead of being re-proposed on every tick.
            Err(error @ (RuntimeError::Config(_) | RuntimeError::SessionNotFound(_))) => {
                tracing::warn!(
                    session = %session_id,
                    error = %error,
                    "observer switch_model refused"
                );
                Ok(refused(format!("switch_model refused: {error}")))
            }
            // An infrastructure failure is not a verdict about the repair.
            Err(error) => Err(error),
        }
    }

    async fn apply_repost(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<RepairOutcome, RuntimeError> {
        let text = if text.is_empty() {
            let Some(text) = self.session_text(session_id, "user", Order::Desc).await? else {
                tracing::warn!(
                    session = %session_id,
                    "repost skipped: the session has no user prompt to re-deliver"
                );
                return Ok(refused(format!(
                    "repost skipped: {session_id} has no user prompt to re-deliver"
                )));
            };
            text
        } else {
            text.to_owned()
        };
        if self.has_pending_deliverable(session_id).await? {
            tracing::info!(
                session = %session_id,
                "repost skipped: the session already has pending input"
            );
            return Ok(refused(format!(
                "repost skipped: {session_id} already has pending input"
            )));
        }
        // The running prompt has already left the inbox, so the guard above
        // cannot see it. Re-injecting it mid-turn would execute the same
        // instructions twice.
        if !self.is_idle(session_id).await? {
            tracing::info!(
                session = %session_id,
                "repost skipped: a turn is already running"
            );
            return Ok(refused(format!(
                "repost skipped: a turn is already running in {session_id}"
            )));
        }
        let root = self.session_root(session_id).await?;
        if self.observer_synthetic(session_id, &text, &root).await? {
            Ok(applied(format!("re-delivered the prompt to {session_id}")))
        } else {
            Ok(refused(format!(
                "repost skipped: an observer synthetic is already pending for {session_id}"
            )))
        }
    }

    /// Grant extra steps and answer with the grant's real result.
    ///
    /// [`Self::grant_step_budget`] is the decision, so the receipt is derived
    /// from it: a session with no running turn is a refusal, never a success.
    async fn apply_budget_extension(
        &self,
        session_id: &str,
        steps: Option<u32>,
    ) -> Result<RepairOutcome, RuntimeError> {
        let requested = steps.unwrap_or_else(|| self.configured_extension_steps());
        if !self.grant_step_budget(session_id, requested) {
            return Ok(refused(format!(
                "request_budget_extension refused: {session_id} has no running turn to extend"
            )));
        }
        let root = self.session_root(session_id).await?;
        let notice = format!(
            "Step budget extended (+{requested} steps requested by observer). \
             Continue deepening your investigation or verification methodically."
        );
        if let Err(error) = self.observer_synthetic(session_id, &notice, &root).await {
            // The grant already happened; only its notice failed. Say so
            // instead of pretending the whole repair was refused.
            tracing::warn!(
                session = %session_id,
                error = %error,
                "observer budget grant notice failed"
            );
            return Ok(applied(format!(
                "granted {requested} extra steps to {session_id}; the notice could not be queued: {error}"
            )));
        }
        Ok(applied(format!(
            "granted {requested} extra steps to {session_id}"
        )))
    }

    /// `step_budget.extension_steps` from the merged config.
    fn configured_extension_steps(&self) -> u32 {
        let policy =
            crate::turn::budget::BudgetPolicy::from_merged(self.inner.config.config.merged());
        u32::try_from(policy.extension_steps).unwrap_or(15)
    }

    /// Whether a `compaction` inbox row is already waiting for the actor.
    async fn pending_compaction(&self, session: &str) -> Result<bool, RuntimeError> {
        let items = self.inner.config.store.list_inbox(session).await?;
        Ok(items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction")))
    }

    /// Text of the first/last message of `kind` in a session's transcript.
    async fn session_text(
        &self,
        session: &str,
        kind: &str,
        order: Order,
    ) -> Result<Option<String>, RuntimeError> {
        let page = self
            .inner
            .config
            .store
            .list_messages(MessageQuery {
                session_id: session.to_owned(),
                limit: 1,
                order,
                cursor: None,
                kind: Some(kind.to_owned()),
            })
            .await?;
        Ok(page
            .items
            .first()
            .and_then(|message| message.get("text"))
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    /// Root session that owns `session` (the session itself when unknown).
    async fn session_root(&self, session: &str) -> Result<String, RuntimeError> {
        Ok(match self.inner.config.store.session_node(session).await? {
            Some(node) => node.root_id,
            None => session.to_owned(),
        })
    }

    /// Whether the session has input the runner would deliver.
    async fn has_pending_deliverable(&self, session: &str) -> Result<bool, RuntimeError> {
        let items = self.inner.config.store.list_inbox(session).await?;
        Ok(items.iter().any(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("user" | "synthetic" | "peer" | "observer" | "compaction")
            )
        }))
    }

    /// `observer.escalation_model` from the merged config, when set.
    fn configured_escalation_model(&self) -> Option<String> {
        self.inner
            .config
            .config
            .merged()
            .pointer("/observer/escalation_model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned)
    }
}
