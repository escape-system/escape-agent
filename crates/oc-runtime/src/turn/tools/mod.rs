use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use oc_types::InterruptReason;
use oc_types::MessageId;
use oc_types::message::StructuredError;
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::expectation::{self, Expectation};
use crate::runtime::{CancelToken, Runtime, RuntimeError};

use super::assistant::AssistantState;
use super::permissions::is_auto_allowed;
use super::types::{PendingToolCall, ResolvedTurn, ToolOutcome};

mod outcome;
mod run;
mod spill;

use outcome::{settle_flight_strip, settle_open};
use run::run_tool;

#[cfg(test)]
use oc_types::message::ToolContent;
#[cfg(test)]
use outcome::tool_content;

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

const MAX_TOOL_PARALLELISM: usize = 16;
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

/// Manages tool calls eagerly spawned mid-stream before `execute_tools` begins.
pub(in crate::turn) struct EagerToolBatch {
    observer: Option<Arc<oc_observer::ObserverEngine>>,
    root_id: String,
    depth: u32,
    open: Arc<std::sync::Mutex<Vec<String>>>,
    set: tokio::task::JoinSet<(usize, ToolOutcome)>,
    progress_tx: mpsc::UnboundedSender<(usize, String, Map<String, Value>)>,
    progress_rx: mpsc::UnboundedReceiver<(usize, String, Map<String, Value>)>,
    spawned: HashSet<usize>,
    eager_cancel: Arc<CancelToken>,
}

impl fmt::Debug for EagerToolBatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EagerToolBatch")
            .field("root_id", &self.root_id)
            .field("depth", &self.depth)
            .field("spawned", &self.spawned)
            .finish_non_exhaustive()
    }
}

impl EagerToolBatch {
    pub(in crate::turn) async fn new(
        runtime: &Runtime,
        session: &str,
        parent_cancel: &Arc<CancelToken>,
    ) -> Result<Self, RuntimeError> {
        let observer = runtime.observer();
        let (root_id, depth) = if observer.is_some() {
            match runtime.inner.config.store.session_node(session).await {
                Ok(Some(node)) => (node.root_id, node.depth),
                Ok(None) => (session.to_owned(), 0),
                Err(error) => return Err(error.into()),
            }
        } else {
            (String::new(), 0)
        };
        let (progress_tx, progress_rx) =
            mpsc::unbounded_channel::<(usize, String, Map<String, Value>)>();
        Ok(Self {
            observer,
            root_id,
            depth,
            open: Arc::new(std::sync::Mutex::new(Vec::new())),
            set: tokio::task::JoinSet::new(),
            progress_tx,
            progress_rx,
            spawned: HashSet::new(),
            eager_cancel: Arc::new(parent_cancel.child()),
        })
    }

    pub(in crate::turn) fn try_spawn_subagent(
        &mut self,
        runtime: &Runtime,
        turn: &ResolvedTurn,
        message_id: &MessageId,
        call: &PendingToolCall,
    ) -> bool {
        if call.name != "subagent" || call.invalid.is_some() {
            return false;
        }
        if crate::turn::session::is_complex_root_restricted_tool(&call.name)
            && !turn.tool_names.iter().any(|name| name == &call.name)
        {
            return false;
        }
        if !is_auto_allowed(runtime, turn, call) {
            return false;
        }
        if self.set.len() >= MAX_TOOL_PARALLELISM || self.spawned.contains(&call.index) {
            return false;
        }
        self.spawned.insert(call.index);
        spawn_tool_into_set(
            &mut self.set,
            runtime,
            turn,
            message_id,
            call,
            &self.eager_cancel,
            &self.root_id,
            self.depth,
            &self.open,
            &self.progress_tx,
        );
        true
    }

    pub(in crate::turn) async fn recv_progress(
        &mut self,
    ) -> Option<(usize, String, Map<String, Value>)> {
        self.progress_rx.recv().await
    }

    pub(in crate::turn) fn drain_progress(&mut self) -> Vec<(usize, String, Map<String, Value>)> {
        let mut drained = Vec::new();
        while let Ok(item) = self.progress_rx.try_recv() {
            drained.push(item);
        }
        drained
    }

    pub(in crate::turn) async fn abort_only(&mut self, reason: InterruptReason) {
        self.eager_cancel.cancel(reason);
        self.set.abort_all();
        while self.set.join_next().await.is_some() {}
        settle_open(self.observer.as_deref(), &self.open, "cancelled");
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_tool_into_set(
    set: &mut tokio::task::JoinSet<(usize, ToolOutcome)>,
    runtime: &Runtime,
    turn: &ResolvedTurn,
    message_id: &MessageId,
    call: &PendingToolCall,
    cancel: &Arc<CancelToken>,
    root_id: &str,
    depth: u32,
    open: &Arc<std::sync::Mutex<Vec<String>>>,
    progress_tx: &mpsc::UnboundedSender<(usize, String, Map<String, Value>)>,
) {
    let call = call.clone();
    let index = call.index;
    let runtime = runtime.clone();
    let turn = turn.clone();
    let message_id = message_id.clone();
    let token = Arc::clone(cancel);
    let strip_ctx = StripStartContext {
        root_id: root_id.to_owned(),
        depth,
        open: Arc::clone(open),
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
}

impl Runtime {
    pub(super) async fn execute_tools(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
        calls: &[PendingToolCall],
        eager: Option<EagerToolBatch>,
        cancel: &Arc<CancelToken>,
    ) -> Result<(), RuntimeError> {
        let mut batch = match eager {
            Some(batch) => batch,
            None => match EagerToolBatch::new(self, session, cancel).await {
                Ok(batch) => batch,
                Err(error) => {
                    self.clear_subagent_prewarm(session);
                    return Err(error);
                }
            },
        };
        let remaining: Vec<&PendingToolCall> = calls
            .iter()
            .filter(|call| !batch.spawned.contains(&call.index))
            .collect();
        let mut next = 0_usize;
        while next < remaining.len() || !batch.set.is_empty() {
            while batch.set.len() < MAX_TOOL_PARALLELISM && next < remaining.len() {
                let call = remaining[next];
                spawn_tool_into_set(
                    &mut batch.set,
                    self,
                    turn,
                    &state.id,
                    call,
                    &batch.eager_cancel,
                    &batch.root_id,
                    batch.depth,
                    &batch.open,
                    &batch.progress_tx,
                );
                next += 1;
            }
            let wait = tokio::select! {
                biased;
                () = cancel.cancelled() => ToolStepWait::Cancelled,
                Some(progress) = batch.progress_rx.recv() => ToolStepWait::Progress(progress),
                joined = batch.set.join_next() => ToolStepWait::Joined(joined),
            };
            let joined = match wait {
                ToolStepWait::Cancelled => {
                    batch.eager_cancel.cancel(cancel.reason());
                    self.clear_subagent_prewarm(session);
                    self.abort_cancelled_tools(session, turn, state, &batch.open, &mut batch.set)
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
            while let Ok((p_index, p_call_id, p_meta)) = batch.progress_rx.try_recv() {
                self.apply_tool_progress(session, turn, state, p_index, p_call_id, p_meta)
                    .await?;
            }
            match joined {
                Some(Ok((_index, ToolOutcome::Cancelled))) => {
                    batch.eager_cancel.cancel(cancel.reason());
                    self.clear_subagent_prewarm(session);
                    self.abort_cancelled_tools(session, turn, state, &batch.open, &mut batch.set)
                        .await;
                    return Err(RuntimeError::Cancelled);
                }
                Some(Ok((index, outcome))) => {
                    settle_flight_strip(
                        batch.observer.as_deref(),
                        &batch.root_id,
                        session,
                        &turn.agent_id,
                        batch.depth,
                        calls,
                        &batch.open,
                        index,
                        &outcome,
                    );
                    self.apply_tool_outcome(session, turn, state, outcome)
                        .await?;
                }
                Some(Err(error)) => {
                    batch.eager_cancel.cancel(InterruptReason::Shutdown);
                    self.clear_subagent_prewarm(session);
                    settle_open(batch.observer.as_deref(), &batch.open, "task failed");
                    self.clear_background_request(session);
                    return Err(RuntimeError::Task(error.to_string()));
                }
                None => break,
            }
        }
        self.clear_subagent_prewarm(session);
        self.clear_background_request(session);
        Ok(())
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
