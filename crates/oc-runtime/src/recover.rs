//! Boot recovery for executions interrupted by a process crash.
//!
//! Claims live in the actor map, so a new runtime starts with none. What can
//! survive is durable state: assistant messages with no completion stamp and
//! tool parts still `streaming`/`running`. Both are repaired before any event is
//! published, then sessions with deliverable inbox rows are woken with at most
//! eight concurrent notifications.

use oc_types::message::ToolContent;
use oc_types::payload::SessionExecutionInterrupted;
use oc_types::{InterruptReason, StructuredError};
use serde_json::{Map, Value, json};

use crate::runtime::{Runtime, RuntimeError};

/// Event kinds that close a session's execution. A second one for the same
/// session contradicts the first.
const TERMINAL_EXECUTIONS: [&str; 3] = [
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
];

impl Runtime {
    pub(crate) async fn recover(&self) -> Result<(), RuntimeError> {
        let sessions = self.list_all_sessions().await?;
        let mut repaired = Vec::new();
        for session in &sessions {
            let Some(id) = session.get("id").and_then(Value::as_str) else {
                continue;
            };
            let directory = session
                .get("location")
                .and_then(|location| location.get("directory"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let mut changed = false;
            for message in self.list_all_messages(id).await? {
                let repaired = match message.get("type").and_then(Value::as_str) {
                    Some("assistant") => self.repair_assistant(&message).await?,
                    Some("compaction") => repair_compaction(&message),
                    _ => None,
                };
                if let Some(body) = repaired {
                    self.inner
                        .config
                        .store
                        .update_message(id, message_id(&message), body)
                        .await?;
                    changed = true;
                }
            }
            if changed {
                // A terminal event may already be durable for this session: the
                // crashed process published it before dying, or an earlier boot
                // already recovered it. Publishing `interrupted` on top would
                // give the session two contradicting terminals.
                if !self.has_terminal_execution(id).await? {
                    self.emit(
                        id,
                        &directory,
                        SessionExecutionInterrupted {
                            session_id: Self::session_id(id),
                            reason: InterruptReason::Shutdown,
                        },
                    )
                    .await?;
                }
                repaired.push(id.to_owned());
            }
        }

        // A child repaired by boot recovery must settle to its parent: the
        // process-local watcher died with the old process, so without this the
        // parent keeps a `status:"running"` subagent tool forever and the child
        // failure is never delivered. Deliver exactly once (deduped against the
        // parent transcript and pending inbox).
        for child in &repaired {
            if let Err(error) = self.settle_recovered_child(child).await {
                tracing::warn!(
                    session = %child,
                    error = %error,
                    "recovered child could not be settled to its parent"
                );
            }
        }

        let mut wake = Vec::new();
        for session in &sessions {
            let Some(id) = session.get("id").and_then(Value::as_str) else {
                continue;
            };
            if self.next_deliverable(id).await?.is_some() {
                wake.push(id.to_owned());
            }
        }
        for chunk in wake.chunks(8) {
            let mut set = tokio::task::JoinSet::new();
            for id in chunk {
                let runtime = self.clone();
                let id = id.clone();
                set.spawn(async move { runtime.notify(&id).await });
            }
            while let Some(result) = set.join_next().await {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(error = %error, "recovery notify failed");
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "recovery notify task failed");
                    }
                }
            }
        }
        Ok(())
    }

    /// Delivers one failed `<subagent>` envelope to the parent of a child whose
    /// turn was repaired by boot recovery.
    ///
    /// The old process owned the completion watcher; after a crash nothing
    /// would ever settle the child. The delivery is keyed by the child id in
    /// the parent transcript and pending inbox, so repeated restarts cannot
    /// duplicate it.
    async fn settle_recovered_child(&self, child: &str) -> Result<(), RuntimeError> {
        let Some(node) = self.inner.config.store.session_node(child).await? else {
            return Ok(());
        };
        let Some(parent) = node.parent_id.clone() else {
            return Ok(());
        };
        let parent_messages = self.list_all_messages(&parent).await?;
        let mut offered = false;
        let mut agent = node.agent.clone();
        let mut description = "recovered subagent".to_owned();
        for message in &parent_messages {
            if message.get("type").and_then(Value::as_str) == Some("synthetic") {
                let text = message
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.contains(&format!("sessionID=\"{child}\"")) {
                    return Ok(());
                }
                if message.pointer("/metadata/childID").and_then(Value::as_str) == Some(child) {
                    return Ok(());
                }
            }
            for part in message
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if part.get("type").and_then(Value::as_str) != Some("tool")
                    || part.get("name").and_then(Value::as_str) != Some("subagent")
                {
                    continue;
                }
                let input = part.pointer("/state/input");
                let target = input
                    .and_then(|value| value.get("sessionID"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        part.pointer("/state/metadata/sessionID")
                            .and_then(Value::as_str)
                    });
                if target != Some(child) {
                    continue;
                }
                offered = true;
                if let Some(value) = input
                    .and_then(|value| value.get("agent"))
                    .and_then(Value::as_str)
                {
                    agent = value.to_owned();
                }
                if let Some(value) = input
                    .and_then(|value| value.get("description"))
                    .and_then(Value::as_str)
                    && !value.trim().is_empty()
                {
                    description = value.to_owned();
                }
            }
        }
        if !offered {
            return Ok(());
        }
        for item in self.inner.config.store.list_inbox(&parent).await? {
            if item
                .pointer("/payload/metadata/childID")
                .and_then(Value::as_str)
                == Some(child)
                || item
                    .pointer("/payload/text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains(&format!("sessionID=\"{child}\"")))
            {
                return Ok(());
            }
        }
        let reason = "server restarted before this subagent completed";
        crate::subagent::deliver_subagent_result(
            self,
            &parent,
            child,
            &agent,
            &description,
            (
                "failed".to_owned(),
                "The server restarted before this subagent completed.".to_owned(),
                Some(reason.to_owned()),
            ),
        )
        .await;
        Ok(())
    }
}

fn message_id(message: &Value) -> &str {
    message
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Child session id recorded on a `subagent` tool part.
///
/// The live executor writes `sessionID` into the running state's metadata
/// (`subagent/spawn.rs` progress report and `subagent/wait.rs`
/// `finish_foreground`); continuing an existing child also carries it in the
/// input.
fn subagent_child(part: &Value) -> Option<&str> {
    part.pointer("/state/metadata/sessionID")
        .and_then(Value::as_str)
        .or_else(|| {
            part.pointer("/state/input/sessionID")
                .and_then(Value::as_str)
        })
}

fn body(mut message: Value) -> Value {
    if let Some(object) = message.as_object_mut() {
        object.remove("id");
        object.remove("sessionID");
        object.remove("type");
    }
    message
}

impl Runtime {
    /// `true` when the session's event log already holds a durable terminal
    /// execution event.
    async fn has_terminal_execution(&self, session: &str) -> Result<bool, RuntimeError> {
        let mut after = 0;
        loop {
            let events = self
                .inner
                .config
                .store
                .events_after(session, after, 1_000)
                .await?;
            let Some(last) = events.last() else {
                return Ok(false);
            };
            if events
                .iter()
                .any(|event| TERMINAL_EXECUTIONS.contains(&event.kind.as_str()))
            {
                return Ok(true);
            }
            after = last.seq;
        }
    }

    /// Repairs one assistant message, reporting a settled child's real outcome
    /// for an in-flight `subagent` tool part instead of a blanket abort error.
    async fn repair_assistant(&self, message: &Value) -> Result<Option<Value>, RuntimeError> {
        let mut body = body(message.clone());
        let Some(object) = body.as_object_mut() else {
            return Ok(None);
        };
        let now = oc_types::now_millis();
        let incomplete = object
            .get("time")
            .and_then(|time| time.get("completed"))
            .is_none();
        let mut repaired = false;

        if let Some(content) = object.get_mut("content").and_then(Value::as_array_mut) {
            for part in content {
                if part.get("type").and_then(Value::as_str) != Some("tool") {
                    continue;
                }
                let status = part.pointer("/state/status").and_then(Value::as_str);
                if !matches!(status, Some("streaming" | "running")) {
                    continue;
                }
                let input = part
                    .pointer("/state/input")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_else(Map::new);
                let child = (part.get("name").and_then(Value::as_str) == Some("subagent"))
                    .then(|| subagent_child(part).map(str::to_owned))
                    .flatten();
                let settled = match child.as_deref() {
                    Some(child) => self.recovered_child_outcome(child).await?,
                    None => None,
                };
                let repaired_state = match settled {
                    Some(settled) => {
                        settled.into_state(child.as_deref().unwrap_or_default(), &input)
                    }
                    None => tool_error_state(&input),
                };
                if let Some(state) = part.get_mut("state") {
                    *state = repaired_state;
                }
                if let Some(time) = part.get_mut("time").and_then(Value::as_object_mut) {
                    time.insert("completed".to_owned(), Value::from(now));
                }
                repaired = true;
            }
        }

        if incomplete {
            let time = object
                .entry("time")
                .or_insert_with(|| json!({ "created": now }));
            if let Some(time) = time.as_object_mut() {
                time.insert("completed".to_owned(), Value::from(now));
            }
            object.insert("finish".to_owned(), Value::String("error".to_owned()));
            object.insert(
                "error".to_owned(),
                serde_json::to_value(StructuredError::new(
                    "MessageAbortedError",
                    "The server restarted before this step completed (STEP_INTERRUPTED).",
                ))
                .expect("structured error is serializable"),
            );
            repaired = true;
        }
        Ok(repaired.then_some(body))
    }

    /// The child's durable terminal state, when it has one.
    ///
    /// The session row is authoritative, exactly as in
    /// `subagent/protocol.rs::child_state` (A3): no row outcome means the child
    /// never settled, and recovery must not invent one.
    async fn recovered_child_outcome(
        &self,
        child: &str,
    ) -> Result<Option<RecoveredChild>, RuntimeError> {
        let Some(session) = self.inner.config.store.get_session(child).await? else {
            return Ok(None);
        };
        let state = match session.get("outcome").and_then(Value::as_str) {
            Some("succeeded") => "completed",
            Some("failed") => "failed",
            Some("interrupted") => "cancelled",
            _ => return Ok(None),
        };
        let messages = self.list_all_messages(child).await?;
        Ok(Some(RecoveredChild {
            state,
            text: final_assistant_text(&messages),
        }))
    }
}

/// A child whose last turn settled, as recovered from durable rows.
struct RecoveredChild {
    state: &'static str,
    text: String,
}

impl RecoveredChild {
    fn into_state(self, child: &str, input: &Map<String, Value>) -> Value {
        let text = if self.text.trim().is_empty() {
            format!("subagent {}", self.state)
        } else {
            self.text
        };
        let envelope = envelope(self.state, child, &text);
        if self.state == "completed" {
            let content = ToolContent::Text { text: envelope };
            json!({
                "status": "completed",
                "input": input,
                "content": [content],
                "metadata": { "sessionID": child, "status": self.state }
            })
        } else {
            json!({
                "status": "error",
                "input": input,
                "error": { "type": "ToolError", "message": envelope },
                "metadata": { "sessionID": child, "status": self.state }
            })
        }
    }
}

/// `<subagent>` envelope, matching `subagent/protocol.rs::envelope`.
fn envelope(state: &str, child: &str, text: &str) -> String {
    format!("<subagent sessionID=\"{child}\" state=\"{state}\">{text}</subagent>")
}

fn final_assistant_text(messages: &[Value]) -> String {
    for message in messages.iter().rev() {
        if message.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let text = message
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                        Some("text") => part.get("text").and_then(Value::as_str),
                        _ => None,
                    })
                    .filter(|text| !text.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if !text.trim().is_empty() {
            return text;
        }
    }
    String::new()
}

fn tool_error_state(input: &Map<String, Value>) -> Value {
    json!({
        "status": "error",
        "input": input,
        "error": {
            "type": "ToolAbortedError",
            "message": "Tool execution was cancelled by the restart."
        }
    })
}

fn repair_compaction(message: &Value) -> Option<Value> {
    if message.get("status").and_then(Value::as_str) != Some("running") {
        return None;
    }
    let mut body = body(message.clone());
    let object = body.as_object_mut()?;
    object.insert("status".to_owned(), Value::String("failed".to_owned()));
    object.insert(
        "error".to_owned(),
        serde_json::to_value(StructuredError::new(
            "CompactionAbortedError",
            "The server restarted while compaction was running.",
        ))
        .expect("structured error is serializable"),
    );
    Some(body)
}
