//! Boot recovery for executions interrupted by a process crash.
//!
//! Claims live in the actor map, so a new runtime starts with none. What can
//! survive is durable state: assistant messages with no completion stamp and
//! tool parts still `streaming`/`running`. Both are repaired before any event is
//! published, then sessions with deliverable inbox rows are woken with at most
//! eight concurrent notifications.

use oc_types::payload::SessionExecutionInterrupted;
use oc_types::{InterruptReason, StructuredError};
use serde_json::{Map, Value, json};

use crate::runtime::{Runtime, RuntimeError};

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
                    Some("assistant") => repair_assistant(&message),
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
                self.emit(
                    id,
                    &directory,
                    SessionExecutionInterrupted {
                        session_id: Self::session_id(id),
                        reason: InterruptReason::Shutdown,
                    },
                )
                .await?;
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

fn body(mut message: Value) -> Value {
    if let Some(object) = message.as_object_mut() {
        object.remove("id");
        object.remove("sessionID");
        object.remove("type");
    }
    message
}

fn repair_assistant(message: &Value) -> Option<Value> {
    let mut body = body(message.clone());
    let object = body.as_object_mut()?;
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
            let Some(state) = part.get_mut("state") else {
                continue;
            };
            let status = state.get("status").and_then(Value::as_str);
            if matches!(status, Some("streaming" | "running")) {
                let input = state
                    .get("input")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_else(Map::new);
                *state = tool_error_state(&input);
                if let Some(time) = part.get_mut("time").and_then(Value::as_object_mut) {
                    time.insert("completed".to_owned(), Value::from(now));
                }
                repaired = true;
            }
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
    repaired.then_some(body)
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
