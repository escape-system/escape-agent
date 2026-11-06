use std::path::Path;
use std::sync::Arc;

use oc_tools::ToolContext;
use oc_types::MessageId;
use oc_types::message::{AssistantPart, StructuredError, ToolContent, ToolState};
use oc_types::payload::{SessionToolFailed, SessionToolProgress, SessionToolSuccess};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::expectation::{self, Expectation};
use crate::runtime::{CancelToken, Runtime, RuntimeError};

use super::assistant::{AssistantState, tool_id};
use super::permissions::authorize_tool_call;
use super::types::{AuthFailure, PendingToolCall, ResolvedTurn, ToolOutcome};

#[derive(Clone)]
struct ToolProgressSender {
    index: usize,
    call_id: String,
    tx: mpsc::UnboundedSender<(usize, String, Map<String, Value>)>,
}

tokio::task_local! {
    static TOOL_PROGRESS: ToolProgressSender;
}

/// Emit intermediate metadata for the currently executing tool call in this task.
pub(crate) fn report_tool_progress(metadata: Map<String, Value>) {
    let _ = TOOL_PROGRESS.try_with(|sender| {
        let _ = sender
            .tx
            .send((sender.index, sender.call_id.clone(), metadata));
    });
}

enum ToolStepWait {
    Cancelled,
    Progress((usize, String, Map<String, Value>)),
    Joined(Option<Result<(usize, ToolOutcome), tokio::task::JoinError>>),
}

const MAX_TOOL_PARALLELISM: usize = 8;
/// Maximum characters kept inline inside a single tool text block (~12k tokens).
/// Larger outputs are spilled to `.opencode/tool-output/<call-id>.txt` and
/// replaced inline with head + spillover path notice + tail.
const MAX_INLINE_TOOL_OUTPUT_CHARS: usize = 48_000;
const SPILL_HEAD_CHARS: usize = 20_000;
const SPILL_TAIL_CHARS: usize = 10_000;

#[derive(Clone)]
struct StripStartContext {
    root_id: String,
    depth: u32,
    open: Arc<std::sync::Mutex<Vec<String>>>,
}

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
        let open: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut set = tokio::task::JoinSet::new();
        let (progress_tx, mut progress_rx) =
            mpsc::unbounded_channel::<(usize, String, Map<String, Value>)>();
        let mut next = 0_usize;
        while next < calls.len() || !set.is_empty() {
            while set.len() < MAX_TOOL_PARALLELISM && next < calls.len() {
                let call = calls[next].clone();
                let index = call.index;
                let runtime = self.clone();
                let turn = turn.clone();
                let message_id = state.id.clone();
                let token = Arc::clone(cancel);
                let strip_ctx = StripStartContext {
                    root_id: root_id.clone(),
                    depth,
                    open: Arc::clone(&open),
                };
                let sender = ToolProgressSender {
                    index,
                    call_id: call.id.clone(),
                    tx: progress_tx.clone(),
                };
                set.spawn(TOOL_PROGRESS.scope(sender, async move {
                    (
                        index,
                        run_tool(runtime, turn, message_id, call, token, strip_ctx).await,
                    )
                }));
                next += 1;
            }
            let wait = tokio::select! {
                biased;
                () = cancel.cancelled() => ToolStepWait::Cancelled,
                Some(progress) = progress_rx.recv() => ToolStepWait::Progress(progress),
                joined = set.join_next() => ToolStepWait::Joined(joined),
            };
            let joined = match wait {
                ToolStepWait::Cancelled => {
                    self.abort_cancelled_tools(session, turn, state, &open, &mut set)
                        .await;
                    return Err(RuntimeError::Cancelled);
                }
                ToolStepWait::Progress((index, call_id, metadata)) => {
                    self.apply_tool_progress(session, turn, state, index, call_id, metadata)
                        .await?;
                    continue;
                }
                ToolStepWait::Joined(joined) => joined,
            };
            while let Ok((p_index, p_call_id, p_meta)) = progress_rx.try_recv() {
                self.apply_tool_progress(session, turn, state, p_index, p_call_id, p_meta)
                    .await?;
            }
            match joined {
                Some(Ok((_index, ToolOutcome::Cancelled))) => {
                    self.abort_cancelled_tools(session, turn, state, &open, &mut set)
                        .await;
                    return Err(RuntimeError::Cancelled);
                }
                Some(Ok((index, outcome))) => {
                    settle_flight_strip(
                        observer.as_deref(),
                        &root_id,
                        session,
                        &turn.agent_id,
                        depth,
                        calls,
                        &open,
                        index,
                        &outcome,
                    );
                    self.apply_tool_outcome(session, turn, state, outcome)
                        .await?;
                }
                Some(Err(error)) => {
                    settle_open(observer.as_deref(), &open, "task failed");
                    self.clear_background_request(session);
                    return Err(RuntimeError::Task(error.to_string()));
                }
                None => break,
            }
        }
        self.clear_background_request(session);
        Ok(())
    }

    /// Abort every in-flight tool task and settle the turn as cancelled.
    ///
    /// Both cancellation paths (a cancelled turn and a tool that reports
    /// `Cancelled`) need the identical sequence: abort the live tasks, settle
    /// their flight strips, drop any pending background request and schedule
    /// the finalizer once. Extracted so the two paths cannot drift apart.
    async fn abort_cancelled_tools(
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

    async fn apply_tool_progress(
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

#[allow(clippy::too_many_lines)]
async fn run_tool(
    runtime: Runtime,
    turn: ResolvedTurn,
    message_id: MessageId,
    call: PendingToolCall,
    cancel: Arc<CancelToken>,
    strip_ctx: StripStartContext,
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

    if super::session::is_complex_root_restricted_tool(&call.name)
        && !turn.tool_names.iter().any(|name| name == &call.name)
    {
        return ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error: StructuredError::new(
                "ComplexModePolicyError",
                format!(
                    "Tool {:?} is disabled for the root orchestrator (depth 0) in /complex mode. \
                     Delegate file exploration, edits, and shell execution by spawning a `subagent` \
                     or a `role: \"coordinator\"` subagent.",
                    call.name
                ),
            ),
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

    if let Some(engine) = runtime.observer().as_deref()
        && let Some(strip) = crate::observer::flight_strip(
            &strip_ctx.root_id,
            turn.info.id.as_ref(),
            &turn.agent_id,
            strip_ctx.depth,
            &call,
        )
    {
        strip_ctx
            .open
            .lock()
            .expect("open strips")
            .push(call.id.clone());
        engine.observe_start(strip);
    }

    let tool_token = cancel.token.child_token();
    let _tool_cancel = runtime.register_tool_cancel(
        &call.id,
        turn.info.id.as_ref(),
        &call.name,
        tool_token.clone(),
    );

    let context = ToolContext {
        session_id: turn.info.id.to_string(),
        directory: Path::new(&turn.directory).to_path_buf(),
        cwd: Path::new(&turn.directory).to_path_buf(),
        cancel: Some(tool_token.clone()),
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
        () = tool_token.cancelled() => {
            if cancel.is_cancelled() {
                return ToolOutcome::Cancelled;
            }
            return ToolOutcome::Failed {
                index: call.index,
                input: call.input,
                error: StructuredError::new(
                    "ToolCancelledError",
                    format!(
                        "Tool call {:?} ({}) was cancelled by observer repair.",
                        call.id, call.name
                    ),
                ),
                content: None,
                metadata: None,
                executed: true,
            };
        }
        result = tool.execute(&context, input) => result,
    };
    if tool_token.is_cancelled() && !cancel.is_cancelled() {
        return ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error: StructuredError::new(
                "ToolCancelledError",
                format!(
                    "Tool call {:?} ({}) was cancelled by observer repair.",
                    call.id, call.name
                ),
            ),
            content: None,
            metadata: None,
            executed: true,
        };
    }
    match result {
        Ok(result) => {
            let (tool_parts, tool_metadata) = tool_content(&result, &turn.directory, &call.id);
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

#[allow(clippy::too_many_arguments)]
fn settle_flight_strip(
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

fn settle_open(
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
    let mut value = match input {
        Value::String(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(parsed @ Value::Object(_)) => parsed,
            Ok(_) => {
                return (
                    Map::new(),
                    Some(Expectation::derived(tool)),
                    Some(StructuredError::new(
                        "InvalidToolArgumentsError",
                        format!(
                            "Tool arguments for {tool:?} must be a JSON object; re-invoke {tool:?} with valid JSON arguments."
                        ),
                    )),
                );
            }
            Err(error) => {
                return (
                    Map::new(),
                    Some(Expectation::derived(tool)),
                    Some(StructuredError::new(
                        "InvalidToolArgumentsError",
                        format!(
                            "Tool arguments for {tool:?} are not valid JSON ({error}); re-invoke {tool:?} with valid JSON arguments."
                        ),
                    )),
                );
            }
        },
        other => other,
    };
    match expectation::extract(&mut value, tool) {
        Ok(expectation) => (
            value.as_object().cloned().unwrap_or_default(),
            Some(expectation),
            None,
        ),
        Err(error) => {
            tracing::warn!(tool = %tool, error = %error, "invalid expectation; falling back to derived");
            (
                value.as_object().cloned().unwrap_or_default(),
                Some(Expectation::derived(tool)),
                None,
            )
        }
    }
}

fn tool_content(
    result: &oc_tools::ToolResult,
    directory: &str,
    call_id: &str,
) -> (Vec<ToolContent>, Option<Map<String, Value>>) {
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
        if text.len() <= MAX_INLINE_TOOL_OUTPUT_CHARS {
            continue;
        }
        let total_chars = text.chars().count();
        if total_chars <= MAX_INLINE_TOOL_OUTPUT_CHARS {
            continue;
        }
        let spill_id = if part_index == 0 {
            call_id.to_owned()
        } else {
            format!("{call_id}-{part_index}")
        };
        let spill_path = spill_tool_output(directory, &spill_id, text);
        let head_end = char_boundary_end(text, SPILL_HEAD_CHARS);
        let tail_start = char_boundary_tail(text, SPILL_TAIL_CHARS);
        let omitted = total_chars.saturating_sub(SPILL_HEAD_CHARS + SPILL_TAIL_CHARS);
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

fn spill_tool_output(directory: &str, call_id: &str, text: &str) -> Option<String> {
    let sanitized: String = call_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let filename = if sanitized.is_empty() {
        format!("tool-{}.txt", oc_types::now_millis())
    } else {
        format!("{sanitized}.txt")
    };
    let primary_dir = Path::new(directory).join(".opencode").join("tool-output");
    if std::fs::create_dir_all(&primary_dir).is_ok() {
        let target = primary_dir.join(&filename);
        if std::fs::write(&target, text).is_ok() {
            return Some(target.to_string_lossy().into_owned());
        }
    }
    let fallback_dir = std::env::temp_dir().join("opencode-tool-output");
    std::fs::create_dir_all(&fallback_dir).ok()?;
    let target = fallback_dir.join(&filename);
    std::fs::write(&target, text).ok()?;
    Some(target.to_string_lossy().into_owned())
}

fn char_boundary_end(text: &str, max_chars: usize) -> usize {
    text.char_indices()
        .nth(max_chars)
        .map_or(text.len(), |(idx, _)| idx)
}

fn char_boundary_tail(text: &str, tail_chars: usize) -> usize {
    let total = text.chars().count();
    let skip = total.saturating_sub(tail_chars);
    text.char_indices().nth(skip).map_or(0, |(idx, _)| idx)
}

fn metadata_map(value: Option<&Value>) -> Option<Map<String, Value>> {
    value
        .and_then(Value::as_object)
        .filter(|map| !map.is_empty())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_tool_output_stays_inline_without_spilling() {
        let result = oc_tools::ToolResult::text("hello world");
        let (parts, metadata) = tool_content(&result, ".", "call_small");
        assert_eq!(
            parts,
            vec![ToolContent::Text {
                text: "hello world".to_owned()
            }]
        );
        assert!(metadata.is_none());
    }

    #[test]
    fn oversized_tool_output_spills_to_disk_and_truncates_inline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_string_lossy().into_owned();
        let big = format!(
            "HEAD_MARKER_{}_{}_TAIL_MARKER",
            "a".repeat(30_000),
            "b".repeat(30_000)
        );
        let result = oc_tools::ToolResult::text(big.clone());
        let (parts, metadata) = tool_content(&result, &dir_str, "call_oversized_1");
        assert_eq!(parts.len(), 1);
        let ToolContent::Text { text } = &parts[0] else {
            panic!("expected text part");
        };
        assert!(text.starts_with("HEAD_MARKER_"));
        assert!(text.ends_with("_TAIL_MARKER"));
        assert!(text.contains("Full untruncated output saved to"));
        assert!(text.chars().count() < MAX_INLINE_TOOL_OUTPUT_CHARS);

        let meta = metadata.expect("metadata");
        assert_eq!(meta.get("truncated").and_then(Value::as_bool), Some(true));
        let spill_path = meta
            .get("spillPath")
            .and_then(Value::as_str)
            .expect("spillPath");
        let saved = std::fs::read_to_string(spill_path).expect("read spilled file");
        assert_eq!(saved, big);
    }
}
