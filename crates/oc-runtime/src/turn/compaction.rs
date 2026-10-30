use std::sync::Arc;

use futures_util::StreamExt;
use oc_llm::{LlmMessage, LlmOptions, LlmRequest};
use oc_store::NewMessage;
use oc_types::MessageId;
use serde_json::{Value, json};

use crate::runtime::{CancelToken, Runtime, RuntimeError};

use super::types::ResolvedTurn;

impl Runtime {
    /// Runs one `compaction` inbox row: resolve the session turn, load the
    /// context and call [`Runtime::compact`].
    ///
    /// The inbox row is consumed after the attempt succeeds, fails or is
    /// cancelled, so a broken compaction cannot spin the actor. Turn-level
    /// failures are already persisted as a failed compaction message; they are
    /// logged here and reported as a clean outcome because the actor must keep
    /// serving the session.
    pub(crate) async fn run_manual_compaction(
        &self,
        session: &str,
        item: &Value,
        cancel: &Arc<CancelToken>,
    ) -> Result<(), RuntimeError> {
        let outcome = self.compact_from_inbox(session, cancel).await;
        if let Some(inbox_id) = item.get("id").and_then(Value::as_str) {
            // Consume the row before reporting success. If the delete fails the
            // row would still be the next deliverable and the actor would run
            // the same compaction again without backoff, so surface the error
            // instead of pretending the row is gone.
            self.inner
                .config
                .store
                .cancel_inbox(session, inbox_id)
                .await?;
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
        let turn = self.resolve_turn(&info, &directory)?;
        let context = self.load_context(session).await?;
        self.compact(session, &turn, &context, cancel, "manual")
            .await
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn compact(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        context: &[LlmMessage],
        cancel: &Arc<CancelToken>,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        let created = oc_types::now_millis();
        let id = MessageId::new();
        let keep_tokens = self.compaction_keep_tokens();
        let keep_id = self.compaction_keep_id(session, keep_tokens).await?;
        let mut running = json!({
            "time": {"created": created},
            "status": "running",
            "reason": reason,
            "summary": ""
        });
        if let Some(keep_id) = &keep_id {
            running["keepID"] = json!(keep_id);
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

        let mut messages = context.to_vec();
        messages.push(LlmMessage::user(crate::compaction::summary_prompt(None)));
        let api_key = self.credential_key(&turn.model.provider.id).await;
        let request = LlmRequest {
            model: turn.model.clone(),
            system: vec![
                "You compact an agent conversation into a handoff summary. Follow the \
                 requested sections exactly and keep concrete facts, paths and failures."
                    .to_owned(),
            ],
            messages,
            tools: Vec::new(),
            options: LlmOptions {
                api_key,
                tool_choice: Some("none".to_owned()),
                ..LlmOptions::default()
            },
        };

        let stream_result = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = self.inner.model.stream(request) => Some(result),
        };
        let Some(stream_result) = stream_result else {
            self.fail_compaction(session, &id, created, reason, "compaction was cancelled")
                .await?;
            return Err(RuntimeError::Cancelled);
        };
        let mut stream = match stream_result {
            Ok(stream) => stream,
            Err(error) => {
                self.fail_compaction(session, &id, created, reason, &error.to_string())
                    .await?;
                return Err(RuntimeError::Llm(error));
            }
        };

        let mut summary = String::new();
        let mut failure = None;
        loop {
            let event = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.fail_compaction(session, &id, created, reason, "compaction was cancelled")
                        .await?;
                    return Err(RuntimeError::Cancelled);
                }
                event = stream.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            match event {
                oc_llm::LlmEvent::TextDelta { delta } => summary.push_str(&delta),
                oc_llm::LlmEvent::TextEnd { text } => summary = text,
                oc_llm::LlmEvent::ProviderError { message } => {
                    failure = Some(message);
                    break;
                }
                _ => {}
            }
        }
        if let Some(message) = failure {
            self.fail_compaction(session, &id, created, reason, &message)
                .await?;
            return Err(RuntimeError::Llm(oc_llm::LlmError::Provider { message }));
        }
        if summary.trim().is_empty() {
            self.fail_compaction(session, &id, created, reason, "the summary was empty")
                .await?;
            return Err(RuntimeError::Config(
                "compaction returned an empty summary".to_owned(),
            ));
        }
        let mut completed = json!({
            "time": {"created": created},
            "status": "completed",
            "reason": reason,
            "model": turn.model_ref.clone(),
            "summary": summary
        });
        if let Some(keep_id) = &keep_id {
            completed["keepID"] = json!(keep_id);
        }
        self.inner
            .config
            .store
            .update_message(session, id.as_str(), completed)
            .await?;
        Ok(())
    }

    /// Recent-context budget preserved across a compaction (`compaction.keep.tokens`).
    fn compaction_keep_tokens(&self) -> u64 {
        self.inner
            .config
            .config
            .merged()
            .get("compaction")
            .and_then(|value| value.get("keep"))
            .and_then(|value| value.get("tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(crate::compaction::CompactionConfig::default().keep_tokens)
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
                    Some("user" | "synthetic" | "system" | "skill" | "peer" | "assistant")
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
                        Some("user" | "synthetic" | "peer")
                    )
                })
                .unwrap_or(candidates.len().saturating_sub(1));
        } else {
            let mut accumulated = 0_u64;
            for index in (0..candidates.len()).rev() {
                let next = accumulated.saturating_add(costs[index]);
                if next > keep_tokens && accumulated > 0 {
                    break;
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
        id: &MessageId,
        created: i64,
        reason: &str,
        message: &str,
    ) -> Result<(), RuntimeError> {
        let failed = json!({
            "time": {"created": created},
            "status": "failed",
            "reason": reason,
            "error": {
                "type": "CompactionError",
                "message": message
            }
        });
        self.inner
            .config
            .store
            .update_message(session, id.as_str(), failed)
            .await?;
        Ok(())
    }
}
