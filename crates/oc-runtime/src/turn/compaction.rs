use std::sync::Arc;

use futures_util::StreamExt;
use oc_llm::{LlmMessage, LlmOptions, LlmRequest};
use oc_store::NewMessage;
use oc_types::MessageId;
use oc_types::message::StructuredError;
use oc_types::payload::{
    SessionCompactionEnded, SessionCompactionFailed, SessionCompactionStarted,
};
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
        let mut turn = self.resolve_turn(&info, &directory)?;
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
            "reason": event_reason(reason),
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
        self.emit(
            session,
            &turn.directory,
            SessionCompactionStarted {
                session_id: Runtime::session_id(session),
                reason: event_reason(reason).to_owned(),
                input_id: Some(id.clone()),
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
        let mut messages = context.to_vec();
        messages.push(LlmMessage::user(crate::compaction::summary_prompt(None)));
        let api_key = self.credential_key(&turn.model.provider.id).await;
        let request = LlmRequest {
            model: turn.model.clone(),
            system: system.clone(),
            messages,
            tools: turn.tools.clone(),
            options: LlmOptions {
                api_key: api_key.clone(),
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
            self.fail_compaction(
                session,
                &turn.directory,
                &id,
                created,
                reason,
                "compaction was cancelled",
            )
            .await?;
            return Err(RuntimeError::Cancelled);
        };
        let mut stream = match stream_result {
            Ok(stream) => stream,
            Err(error) if is_context_overflow_message(&error.to_string()) => {
                let mut shortened = shorten_messages_for_compaction(context);
                shortened.push(LlmMessage::user(crate::compaction::summary_prompt(None)));
                let fallback = LlmRequest {
                    model: turn.model.clone(),
                    system,
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
                    result = self.inner.model.stream(fallback) => Some(result),
                };
                match retry_result {
                    Some(Ok(stream)) => stream,
                    Some(Err(retry_err)) => {
                        self.fail_compaction(
                            session,
                            &turn.directory,
                            &id,
                            created,
                            reason,
                            &retry_err.to_string(),
                        )
                        .await?;
                        return Err(RuntimeError::Llm(retry_err));
                    }
                    None => {
                        self.fail_compaction(
                            session,
                            &turn.directory,
                            &id,
                            created,
                            reason,
                            "compaction was cancelled",
                        )
                        .await?;
                        return Err(RuntimeError::Cancelled);
                    }
                }
            }
            Err(error) => {
                self.fail_compaction(
                    session,
                    &turn.directory,
                    &id,
                    created,
                    reason,
                    &error.to_string(),
                )
                .await?;
                return Err(RuntimeError::Llm(error));
            }
        };

        let mut summary = String::new();
        let mut usage = None;
        let mut failure = None;
        loop {
            let event = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.fail_compaction(
                        session,
                        &turn.directory,
                        &id,
                        created,
                        reason,
                        "compaction was cancelled",
                    )
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
                oc_llm::LlmEvent::Finish { usage: finish, .. } => usage = Some(finish),
                oc_llm::LlmEvent::ProviderError { message } => {
                    failure = Some(message);
                    break;
                }
                _ => {}
            }
        }
        if let Some(message) = failure {
            self.fail_compaction(session, &turn.directory, &id, created, reason, &message)
                .await?;
            return Err(RuntimeError::Llm(oc_llm::LlmError::Provider { message }));
        }
        if summary.trim().is_empty() {
            self.fail_compaction(
                session,
                &turn.directory,
                &id,
                created,
                reason,
                "the summary was empty",
            )
            .await?;
            return Err(RuntimeError::Config(
                "compaction returned an empty summary".to_owned(),
            ));
        }
        let mut completed = json!({
            "time": {"created": created},
            "status": "completed",
            "reason": event_reason(reason),
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
        let tokens = usage.as_ref().map(super::types::tokens_from_usage);
        self.emit(
            session,
            &turn.directory,
            SessionCompactionEnded {
                session_id: Runtime::session_id(session),
                reason: event_reason(reason).to_owned(),
                model: Some(turn.model_ref.clone()),
                native: None,
                text: summary,
                cost: usage.as_ref().and_then(|usage| usage.cost),
                tokens,
            },
        )
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
        directory: &str,
        id: &MessageId,
        created: i64,
        reason: &str,
        message: &str,
    ) -> Result<(), RuntimeError> {
        let failed = json!({
            "time": {"created": created},
            "status": "failed",
            "reason": event_reason(reason),
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
        self.emit(
            session,
            directory,
            SessionCompactionFailed {
                session_id: Runtime::session_id(session),
                reason: event_reason(reason).to_owned(),
                error: StructuredError::new("CompactionError", message),
                input_id: Some(id.clone()),
                cost: None,
                tokens: None,
            },
        )
        .await?;
        Ok(())
    }
}

/// Map the stored compaction `reason` to the wire literal. Upstream defines
/// only `auto` and `manual`; a context-overflow compaction is automatic.
fn event_reason(reason: &str) -> &str {
    match reason {
        "manual" => "manual",
        _ => "auto",
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

/// Mask oversized historical tool outputs when a compaction request itself
/// exceeds the provider's context window.
pub(super) fn shorten_messages_for_compaction(messages: &[LlmMessage]) -> Vec<LlmMessage> {
    const HEAD_CHARS: usize = 1_000;
    const TAIL_CHARS: usize = 500;
    const MAX_TOOL_CHARS: usize = 2_000;

    let last_tool_msg = messages
        .iter()
        .rposition(|message| message.role == oc_llm::Role::Tool);
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            if Some(index) == last_tool_msg {
                return message.clone();
            }
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
                    other => other.clone(),
                })
                .collect();
            LlmMessage::new(message.role, content)
        })
        .collect()
}
