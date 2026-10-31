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
use oc_types::{MessageId, Tokens};

use crate::runtime::{CancelToken, Runtime, RuntimeError};

use super::assistant::{AssistantState, ordinal_u32};
use super::tools::strip_expectation;
use super::types::{PendingToolCall, ResolvedTurn, StepRun, finish_reason, tokens_from_usage};

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

    #[allow(clippy::too_many_lines)]
    pub(super) async fn stream_step(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        messages: &[LlmMessage],
        final_call: bool,
        cancel: &Arc<CancelToken>,
    ) -> Result<StepRun, RuntimeError> {
        let created = oc_types::now_millis();
        let id = MessageId::new();
        let mut state = AssistantState::new(
            id.clone(),
            turn.agent_id.clone(),
            turn.model_ref.clone(),
            created,
        );
        self.inner
            .config
            .store
            .append_message(NewMessage {
                id: id.as_str().to_owned(),
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
                assistant_message_id: id.clone(),
                agent: turn.agent_id.clone(),
                model: turn.model_ref.clone(),
                snapshot: None,
                started: created,
            },
        )
        .await?;

        let options = LlmOptions {
            api_key: self.credential_key(&turn.model.provider.id).await,
            tool_choice: final_call.then(|| "none".to_owned()),
            cache_key: Some(session.to_owned()),
            ..LlmOptions::default()
        };
        let request = LlmRequest {
            model: turn.model.clone(),
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
                    let index = state.ensure_tool(&call_id, &name);
                    let (clean, expectation, invalid) = strip_expectation(&name, input);
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
                    let cost = oc_llm::model_cost(&turn.model.model, &usage);
                    state.cost = Some(cost);
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
            let error = StructuredError::new("ProviderError", message.clone());
            let tokens = state.tokens.clone().unwrap_or_default();
            let cost = state.cost.unwrap_or(0.0);
            self.fail_step(session, turn, state, error, Some(tokens), cost)
                .await?;
            return Err(RuntimeError::Llm(oc_llm::LlmError::Provider { message }));
        }
        if cancel.is_cancelled() {
            self.schedule_finalizer(session.to_owned(), turn.directory.clone(), state)
                .await;
            return Err(RuntimeError::Cancelled);
        }
        state.finish.get_or_insert(FinishReason::Unknown);
        self.persist_assistant(session, &state).await?;
        Ok(StepRun {
            tokens: state.tokens.clone().unwrap_or_default(),
            cost: state.cost.unwrap_or(0.0),
            state,
            calls,
        })
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
