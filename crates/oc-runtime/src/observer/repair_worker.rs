use oc_store::{Delivery, MessageQuery, NewInbox, Order, SessionPatch};
use oc_types::payload::SessionModelSelected;
use oc_types::{MessageId, SessionId, now_millis};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::runtime::{Runtime, RuntimeError};

use super::repair::{RepairCommand, switch_model_target};

impl Runtime {
    /// Enqueue an observer synthetic message and notify its session.
    pub(crate) async fn observer_synthetic(
        &self,
        session: &str,
        text: &str,
        root: &str,
    ) -> Result<(), RuntimeError> {
        let payload = json!({
            "text": text,
            "metadata": { "observer": true, "target": root },
            "time": { "created": now_millis() },
        });
        self.inner
            .config
            .store
            .enqueue(NewInbox {
                id: MessageId::new().into_inner(),
                session_id: session.to_owned(),
                kind: "synthetic".to_owned(),
                payload,
                delivery: Delivery::Steer,
            })
            .await?;
        self.notify_local(session).await
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
        self.inner
            .config
            .store
            .enqueue(NewInbox {
                id: MessageId::new().into_inner(),
                session_id: session.to_owned(),
                kind: "compaction".to_owned(),
                payload: Value::Object(payload),
                delivery: Delivery::Queue,
            })
            .await?;
        self.notify_local(session).await
    }

    async fn observer_switch_model(&self, session: &str, model: &str) -> Result<(), RuntimeError> {
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
        if let Err(error) = self.inner.config.catalog.resolve(model) {
            return Err(RuntimeError::Config(format!(
                "repair model {model:?} is not in the catalog: {error}"
            )));
        }
        let previous = info.model.clone();
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
        .await
    }

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
            let Some(command) = command else {
                return;
            };
            if let Err(error) = self.apply_repair_command(command).await {
                tracing::warn!(error = %error, "observer repair failed");
            }
        }
    }

    async fn apply_repair_command(&self, command: RepairCommand) -> Result<(), RuntimeError> {
        match command {
            RepairCommand::Interrupt { session_id } => {
                if !self.interrupt(&session_id).await? {
                    tracing::debug!(session = %session_id, "observer interrupt found no running execution");
                }
            }
            RepairCommand::CancelTool { tool_call_id } => {
                tracing::warn!(
                    tool_call_id = %tool_call_id,
                    "cancel_tool repair is not supported: the runtime has no per-tool token; use interrupt"
                );
            }
            RepairCommand::RestartSubagent { session_id, note } => {
                if !self.interrupt(&session_id).await? {
                    tracing::debug!(session = %session_id, "observer restart found no running execution");
                }
                let Some(prompt) = self.session_text(&session_id, "user", Order::Asc).await? else {
                    tracing::warn!(
                        session = %session_id,
                        "restart_subagent skipped: the child has no user prompt to replay"
                    );
                    return Ok(());
                };
                let text = format!("{prompt}\n\n[observer restart] {note}");
                let root = self.session_root(&session_id).await?;
                self.observer_synthetic(&session_id, &text, &root).await?;
            }
            RepairCommand::Compact { session_id } => {
                self.observer_compaction(&session_id, None).await?;
            }
            RepairCommand::SwitchModel { session_id, model } => {
                let model =
                    switch_model_target(model, self.configured_escalation_model(), &session_id)?;
                self.observer_switch_model(&session_id, &model).await?;
            }
            RepairCommand::Repost { session_id, text } => {
                let text = if text.is_empty() {
                    let Some(text) = self.session_text(&session_id, "user", Order::Desc).await?
                    else {
                        tracing::warn!(
                            session = %session_id,
                            "repost skipped: the session has no user prompt to re-deliver"
                        );
                        return Ok(());
                    };
                    text
                } else {
                    text
                };
                if self.has_pending_deliverable(&session_id).await? {
                    tracing::info!(
                        session = %session_id,
                        "repost skipped: the session already has pending input"
                    );
                    return Ok(());
                }
                // The running prompt has already left the inbox, so the guard
                // above cannot see it. Re-injecting it mid-turn would execute
                // the same instructions twice.
                if !self.is_idle(&session_id).await? {
                    tracing::info!(
                        session = %session_id,
                        "repost skipped: a turn is already running"
                    );
                    return Ok(());
                }
                let root = self.session_root(&session_id).await?;
                self.observer_synthetic(&session_id, &text, &root).await?;
            }
        }
        Ok(())
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
                Some("user" | "synthetic" | "peer" | "observer")
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
