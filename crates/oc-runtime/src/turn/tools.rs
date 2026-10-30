use std::path::Path;
use std::sync::Arc;

use oc_tools::ToolContext;
use oc_types::MessageId;
use oc_types::message::{AssistantPart, StructuredError, ToolContent, ToolState};
use oc_types::payload::{SessionToolFailed, SessionToolSuccess};
use serde_json::{Map, Value};

use crate::expectation::{self, Expectation};
use crate::runtime::{CancelToken, Runtime, RuntimeError};

use super::assistant::{AssistantState, tool_id};
use super::permissions::authorize_tool_call;
use super::types::{AuthFailure, PendingToolCall, ResolvedTurn, ToolOutcome};

const MAX_TOOL_PARALLELISM: usize = 8;

impl Runtime {
    pub(super) async fn execute_tools(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
        calls: &[PendingToolCall],
        cancel: &Arc<CancelToken>,
    ) -> Result<(), RuntimeError> {
        let observer = self.observer();
        let (root_id, depth) = if observer.is_some() {
            match self.inner.config.store.session_node(session).await {
                Ok(Some(node)) => (node.root_id, node.depth),
                Ok(None) => (session.to_owned(), 0),
                Err(error) => return Err(error.into()),
            }
        } else {
            (String::new(), 0)
        };
        let mut open: Vec<String> = Vec::new();
        let mut set = tokio::task::JoinSet::new();
        let mut next = 0_usize;
        while next < calls.len() || !set.is_empty() {
            while set.len() < MAX_TOOL_PARALLELISM && next < calls.len() {
                let call = calls[next].clone();
                let index = call.index;
                if let Some(engine) = observer.as_deref()
                    && let Some(strip) = crate::observer::flight_strip(
                        &root_id,
                        session,
                        &turn.agent_id,
                        depth,
                        &call,
                    )
                {
                    engine.observe_start(strip);
                    open.push(call.id.clone());
                }
                let runtime = self.clone();
                let turn = turn.clone();
                let message_id = state.id.clone();
                let token = Arc::clone(cancel);
                set.spawn(async move {
                    (
                        index,
                        run_tool(runtime, turn, message_id, call, token).await,
                    )
                });
                next += 1;
            }
            let joined = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    settle_open(observer.as_deref(), &mut open, "cancelled");
                    self.schedule_finalizer(
                        session.to_owned(),
                        turn.directory.clone(),
                        state.clone(),
                    )
                    .await;
                    return Err(RuntimeError::Cancelled);
                }
                joined = set.join_next() => joined,
            };
            match joined {
                Some(Ok((_index, ToolOutcome::Cancelled))) => {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    settle_open(observer.as_deref(), &mut open, "cancelled");
                    self.schedule_finalizer(
                        session.to_owned(),
                        turn.directory.clone(),
                        state.clone(),
                    )
                    .await;
                    return Err(RuntimeError::Cancelled);
                }
                Some(Ok((index, outcome))) => {
                    if let Some(engine) = observer.as_deref()
                        && let Some(call) = calls.iter().find(|call| call.index == index)
                    {
                        let (strip_state, strip_outcome) = crate::observer::strip_result(&outcome);
                        engine.observe_settle(&call.id, strip_state, strip_outcome.as_deref());
                        let call_id = call.id.clone();
                        open.retain(|id| id != &call_id);
                    }
                    self.apply_tool_outcome(session, turn, state, outcome)
                        .await?;
                }
                Some(Err(error)) => {
                    settle_open(observer.as_deref(), &mut open, "task failed");
                    return Err(RuntimeError::Task(error.to_string()));
                }
                None => break,
            }
        }
        Ok(())
    }

    async fn apply_tool_outcome(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
        outcome: ToolOutcome,
    ) -> Result<(), RuntimeError> {
        let now = oc_types::now_millis();
        match outcome {
            ToolOutcome::Completed {
                index,
                input,
                content,
                metadata,
                executed,
            } => {
                if let Some(AssistantPart::Tool(tool)) = state.content.get_mut(index) {
                    tool.state = ToolState::Completed {
                        input: input.clone(),
                        content: content.clone(),
                        metadata: metadata.clone(),
                    };
                    tool.executed = Some(executed);
                    tool.time.ran = Some(now);
                    tool.time.completed = Some(now);
                }
                self.persist_assistant(session, state).await?;
                let id = tool_id(state, index);
                self.emit(
                    session,
                    &turn.directory,
                    SessionToolSuccess {
                        session_id: Runtime::session_id(session),
                        assistant_message_id: state.id.clone(),
                        id,
                        content,
                        metadata,
                        executed,
                        native: None,
                    },
                )
                .await?;
            }
            ToolOutcome::Failed {
                index,
                input,
                error,
                content,
                metadata,
                executed,
            } => {
                if let Some(AssistantPart::Tool(tool)) = state.content.get_mut(index) {
                    tool.state = ToolState::Error {
                        input,
                        error: error.clone(),
                        content: content.clone(),
                        metadata: metadata.clone(),
                    };
                    tool.executed = Some(executed);
                    if executed {
                        tool.time.ran = Some(now);
                    }
                    tool.time.completed = Some(now);
                }
                self.persist_assistant(session, state).await?;
                let id = tool_id(state, index);
                self.emit(
                    session,
                    &turn.directory,
                    SessionToolFailed {
                        session_id: Runtime::session_id(session),
                        assistant_message_id: state.id.clone(),
                        id,
                        error,
                        content,
                        metadata,
                        executed,
                        native: None,
                    },
                )
                .await?;
            }
            ToolOutcome::Cancelled => {}
        }
        Ok(())
    }
}

async fn run_tool(
    runtime: Runtime,
    turn: ResolvedTurn,
    message_id: MessageId,
    call: PendingToolCall,
    cancel: Arc<CancelToken>,
) -> ToolOutcome {
    if let Some(expectation) = &call.expectation {
        tracing::debug!(
            tool = %call.name,
            why = %expectation.why,
            expect_seconds = expectation.expect_seconds,
            "tool expectation"
        );
    }
    if let Some(error) = call.invalid.clone() {
        return ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error,
            content: None,
            metadata: None,
            executed: false,
        };
    }

    if let Err(failure) = authorize_tool_call(&runtime, &turn, &message_id, &call, &cancel).await {
        return auth_outcome(failure, call);
    }

    let Some(tool) = turn.registry.get(&call.name).cloned() else {
        return ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error: StructuredError::new(
                "ToolNotFoundError",
                format!("Tool {:?} is not registered.", call.name),
            ),
            content: None,
            metadata: None,
            executed: false,
        };
    };

    let context = ToolContext {
        session_id: turn.info.id.to_string(),
        directory: Path::new(&turn.directory).to_path_buf(),
        cwd: Path::new(&turn.directory).to_path_buf(),
        cancel: Some(cancel.token.clone()),
    };
    let input = Value::Object(call.input.clone());
    crate::snapshot::capture_before_tool(
        &runtime,
        turn.info.id.as_ref(),
        message_id.as_ref(),
        &call.name,
        &call.id,
        &input,
        &turn.directory,
    )
    .await;
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => return ToolOutcome::Cancelled,
        result = tool.execute(&context, input) => result,
    };
    match result {
        Ok(result) => {
            let (tool_parts, tool_metadata) = tool_content(&result);
            ToolOutcome::Completed {
                index: call.index,
                input: call.input,
                content: tool_parts,
                metadata: tool_metadata,
                executed: true,
            }
        }
        Err(error) => ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error: StructuredError::new("ToolError", error.message),
            content: None,
            metadata: metadata_map(error.metadata.as_ref()),
            executed: true,
        },
    }
}

fn settle_open(
    engine: Option<&oc_observer::ObserverEngine>,
    open: &mut Vec<String>,
    outcome: &str,
) {
    let Some(engine) = engine else {
        open.clear();
        return;
    };
    for id in open.drain(..) {
        engine.observe_settle(&id, oc_observer::StripState::Cancelled, Some(outcome));
    }
}

fn auth_outcome(failure: AuthFailure, call: PendingToolCall) -> ToolOutcome {
    match failure {
        AuthFailure::Cancelled => ToolOutcome::Cancelled,
        AuthFailure::Denied(error) => ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error,
            content: None,
            metadata: None,
            executed: false,
        },
    }
}

pub(super) fn strip_expectation(
    tool: &str,
    input: Value,
) -> (
    Map<String, Value>,
    Option<Expectation>,
    Option<StructuredError>,
) {
    let mut value = input;
    match expectation::extract(&mut value, tool) {
        Ok(expectation) => (
            value.as_object().cloned().unwrap_or_default(),
            Some(expectation),
            None,
        ),
        Err(error) => (
            value.as_object().cloned().unwrap_or_default(),
            None,
            Some(StructuredError::new("InvalidToolInput", error.to_string())),
        ),
    }
}

fn tool_content(result: &oc_tools::ToolResult) -> (Vec<ToolContent>, Option<Map<String, Value>>) {
    let mut content = result
        .content
        .as_ref()
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value::<ToolContent>(item.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if content.is_empty() {
        let text = result.output.clone().unwrap_or_default();
        content.push(ToolContent::Text { text });
    }
    (content, metadata_map(result.metadata.as_ref()))
}

fn metadata_map(value: Option<&Value>) -> Option<Map<String, Value>> {
    value
        .and_then(Value::as_object)
        .filter(|map| !map.is_empty())
        .cloned()
}
