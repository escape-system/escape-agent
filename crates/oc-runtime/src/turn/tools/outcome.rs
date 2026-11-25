use std::sync::Arc;

use oc_types::message::{AssistantPart, ToolContent, ToolState};
use oc_types::payload::{SessionToolFailed, SessionToolProgress, SessionToolSuccess};
use serde_json::{Map, Value};

use crate::runtime::{Runtime, RuntimeError};
use crate::turn::assistant::{AssistantState, tool_id};
use crate::turn::types::{AuthFailure, PendingToolCall, ResolvedTurn, ToolOutcome};

use super::spill::{char_boundary_end, char_boundary_tail, spill_tool_output};
use super::{MAX_INLINE_TOOL_OUTPUT_CHARS, SPILL_HEAD_CHARS, SPILL_TAIL_CHARS};

impl Runtime {
    /// Abort every in-flight tool task and settle the turn as cancelled.
    ///
    /// Both cancellation paths (a cancelled turn and a tool that reports
    /// `Cancelled`) need the identical sequence: abort the live tasks, settle
    /// their flight strips, drop any pending background request and schedule
    /// the finalizer once. Extracted so the two paths cannot drift apart.
    pub(super) async fn abort_cancelled_tools(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
        open: &Arc<std::sync::Mutex<Vec<String>>>,
        set: &mut tokio::task::JoinSet<(usize, ToolOutcome)>,
    ) {
        set.abort_all();
        while set.join_next().await.is_some() {}
        let observer = self.observer();
        settle_open(observer.as_deref(), open, "cancelled");
        self.clear_background_request(session);
        self.schedule_finalizer(session.to_owned(), turn.directory.clone(), state.clone())
            .await;
    }

    pub(super) async fn apply_tool_progress(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
        index: usize,
        call_id: String,
        metadata: Map<String, Value>,
    ) -> Result<(), RuntimeError> {
        let mut updated = false;
        if let Some(AssistantPart::Tool(tool)) = state.content.get_mut(index)
            && let ToolState::Running { metadata: slot, .. } = &mut tool.state
        {
            *slot = Some(metadata.clone());
            updated = true;
        }
        if updated {
            self.persist_assistant(session, state).await?;
            self.emit(
                session,
                &turn.directory,
                SessionToolProgress {
                    session_id: Runtime::session_id(session),
                    assistant_message_id: state.id.clone(),
                    id: call_id,
                    metadata,
                },
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn apply_tool_outcome(
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

#[allow(clippy::too_many_arguments)]
pub(super) fn settle_flight_strip(
    engine: Option<&oc_observer::ObserverEngine>,
    root_id: &str,
    session: &str,
    agent_id: &str,
    depth: u32,
    calls: &[PendingToolCall],
    open: &Arc<std::sync::Mutex<Vec<String>>>,
    index: usize,
    outcome: &ToolOutcome,
) {
    let Some(engine) = engine else {
        return;
    };
    let Some(call) = calls.iter().find(|call| call.index == index) else {
        return;
    };
    let was_open = {
        let mut guard = open.lock().expect("open strips");
        let before = guard.len();
        guard.retain(|id| id != &call.id);
        guard.len() != before
    };
    if !was_open {
        if matches!(outcome, ToolOutcome::Cancelled) {
            return;
        }
        if let Some(strip) = crate::observer::flight_strip(root_id, session, agent_id, depth, call)
        {
            engine.observe_start(strip);
        } else {
            return;
        }
    }
    let (strip_state, strip_outcome) = crate::observer::strip_result(outcome);
    engine.observe_settle(&call.id, strip_state, strip_outcome.as_deref());
}

pub(super) fn settle_open(
    engine: Option<&oc_observer::ObserverEngine>,
    open: &Arc<std::sync::Mutex<Vec<String>>>,
    outcome: &str,
) {
    let ids: Vec<String> = open.lock().expect("open strips").drain(..).collect();
    let Some(engine) = engine else {
        return;
    };
    for id in ids {
        engine.observe_settle(&id, oc_observer::StripState::Cancelled, Some(outcome));
    }
}

pub(super) fn auth_outcome(failure: AuthFailure, call: PendingToolCall) -> ToolOutcome {
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

#[cfg(test)]
pub(super) fn tool_content(
    result: &oc_tools::ToolResult,
    directory: &str,
    call_id: &str,
) -> (Vec<ToolContent>, Option<Map<String, Value>>) {
    tool_content_with_limit(result, directory, call_id, None)
}

pub(super) fn tool_content_with_limit(
    result: &oc_tools::ToolResult,
    directory: &str,
    call_id: &str,
    max_bytes: Option<usize>,
) -> (Vec<ToolContent>, Option<Map<String, Value>>) {
    let max_inline_chars = max_bytes
        .filter(|v| *v > 0)
        .unwrap_or(MAX_INLINE_TOOL_OUTPUT_CHARS);
    let (head_chars, tail_chars) = if max_inline_chars < SPILL_HEAD_CHARS + SPILL_TAIL_CHARS {
        (
            ((max_inline_chars * 2) / 5).max(1),
            (max_inline_chars / 5).max(1),
        )
    } else {
        (SPILL_HEAD_CHARS, SPILL_TAIL_CHARS)
    };
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
    let mut metadata = metadata_map(result.metadata.as_ref());
    for (part_index, part) in content.iter_mut().enumerate() {
        let ToolContent::Text { text } = part else {
            continue;
        };
        if text.len() <= max_inline_chars {
            continue;
        }
        let total_chars = text.chars().count();
        if total_chars <= max_inline_chars {
            continue;
        }
        let spill_id = if part_index == 0 {
            call_id.to_owned()
        } else {
            format!("{call_id}-{part_index}")
        };
        let spill_path = spill_tool_output(directory, &spill_id, text);
        let head_end = char_boundary_end(text, head_chars);
        let tail_start = char_boundary_tail(text, tail_chars);
        let omitted = total_chars.saturating_sub(head_chars + tail_chars);
        let location_note = match &spill_path {
            Some(path) => format!(
                "Full untruncated output saved to `{path}` — use `read` with `offset`/`limit` or `grep` on that file to inspect the middle section."
            ),
            None => {
                "Use a narrower query or `offset`/`limit` to inspect specific sections.".to_owned()
            }
        };
        let truncated_text = format!(
            "{}\n\n... [truncated {omitted} chars ({total_chars} chars total). {location_note}] ...\n\n{}",
            &text[..head_end],
            &text[tail_start..],
        );
        *text = truncated_text;
        let meta = metadata.get_or_insert_with(Map::new);
        meta.insert("truncated".to_owned(), Value::Bool(true));
        meta.insert("originalChars".to_owned(), Value::from(total_chars));
        if let Some(path) = spill_path {
            meta.insert("spillPath".to_owned(), Value::String(path));
        }
    }
    (content, metadata)
}

pub(super) fn metadata_map(value: Option<&Value>) -> Option<Map<String, Value>> {
    value
        .and_then(Value::as_object)
        .filter(|map| !map.is_empty())
        .cloned()
}
