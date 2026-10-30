use std::time::Duration;

use oc_types::message::{AssistantPart, FinishReason, StructuredError, ToolState};
use oc_types::payload::{SessionStepFailed, SessionTextEnded, SessionToolFailed};
use serde_json::Map;

use crate::runtime::{Runtime, RuntimeError};

use super::assistant::{AssistantState, ordinal_u32};

impl Runtime {
    pub(super) async fn schedule_finalizer(
        &self,
        session: String,
        directory: String,
        state: AssistantState,
    ) {
        let runtime = self.clone();
        let handle = tokio::spawn(async move {
            runtime
                .finalize_interrupted(&session, &directory, state)
                .await
        });
        match tokio::time::timeout(Duration::from_secs(5), handle).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                tracing::error!(error = %error, "interrupt finalizer failed");
            }
            Ok(Err(error)) => {
                tracing::error!(error = %error, "interrupt finalizer task failed");
            }
            Err(_) => {
                tracing::error!("interrupt finalizer exceeded its five second budget");
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn finalize_interrupted(
        &self,
        session: &str,
        directory: &str,
        mut state: AssistantState,
    ) -> Result<(), RuntimeError> {
        if let Some((index, text)) = state.close_text() {
            self.emit(
                session,
                directory,
                SessionTextEnded {
                    session_id: Runtime::session_id(session),
                    assistant_message_id: state.id.clone(),
                    ordinal: ordinal_u32(index),
                    text,
                    native: None,
                },
            )
            .await?;
        }
        if let Some((index, text)) = state.close_reasoning() {
            self.emit(
                session,
                directory,
                oc_types::payload::SessionReasoningEnded {
                    session_id: Runtime::session_id(session),
                    assistant_message_id: state.id.clone(),
                    ordinal: ordinal_u32(index),
                    text,
                    native: None,
                },
            )
            .await?;
        }

        let tool_error = StructuredError::new(
            "ToolAbortedError",
            "Tool execution was cancelled before it completed.",
        );
        let now = oc_types::now_millis();
        let mut aborted = Vec::new();
        for part in &mut state.content {
            let AssistantPart::Tool(tool) = part else {
                continue;
            };
            if matches!(
                tool.state,
                ToolState::Streaming { .. } | ToolState::Running { .. }
            ) {
                let input = match &tool.state {
                    ToolState::Running { input, .. } => input.clone(),
                    _ => Map::new(),
                };
                tool.state = ToolState::Error {
                    input,
                    error: tool_error.clone(),
                    content: None,
                    metadata: None,
                };
                tool.executed = Some(false);
                tool.time.completed = Some(now);
                aborted.push(tool.id.clone());
            }
        }

        let error = StructuredError::new(
            "MessageAbortedError",
            "The operation was interrupted before it completed (STEP_INTERRUPTED).",
        );
        state.error = Some(error.clone());
        state.finish = Some(FinishReason::Error);
        state.completed = Some(now);
        self.persist_assistant(session, &state).await?;
        for id in aborted {
            self.emit(
                session,
                directory,
                SessionToolFailed {
                    session_id: Runtime::session_id(session),
                    assistant_message_id: state.id.clone(),
                    id,
                    error: tool_error.clone(),
                    content: None,
                    metadata: None,
                    executed: false,
                    native: None,
                },
            )
            .await?;
        }
        self.emit(
            session,
            directory,
            SessionStepFailed {
                session_id: Runtime::session_id(session),
                assistant_message_id: state.id.clone(),
                error,
                finish: Some(FinishReason::Error),
                raw_finish: state.raw_finish.clone(),
                native: None,
                cost: state.cost,
                tokens: state.tokens,
                snapshot: None,
                files: None,
            },
        )
        .await?;
        Ok(())
    }
}
