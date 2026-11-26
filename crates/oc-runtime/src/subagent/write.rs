use std::time::Duration;

use oc_store::{Delivery, NewInbox};
use oc_subagents::{BudgetError, Policy};
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use oc_types::MessageId;
use serde_json::{Value, json};

use crate::runtime::Runtime;

use super::protocol::SubagentPeerMessage;
use super::settings::settings_for_session;
use super::tree::{
    budget_error, load_info, load_node, load_tree, optional_bool, optional_str, required_str,
    runtime_error, write_error,
};

#[derive(Debug, Clone)]
pub(super) struct WriteSessionTool {
    pub(super) runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for WriteSessionTool {
    fn name(&self) -> &'static str {
        "session_write"
    }

    fn description(&self) -> &'static str {
        "Write text into a descendant session (or self). Siblings and parents are rejected by policy: use `talk` for them. `delivery:\"steer\"` (default) wakes an idle target or joins an active turn at its next step boundary; `delivery:\"queue\"` wakes an idle target or queues for the next turn after an active turn completes (never injects mid-turn). Returns a confirmation naming the target and delivery mode."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Target session id." },
                "text": { "type": "string" },
                "delivery": { "type": "string", "enum": ["steer", "queue"], "description": "steer (default) wakes the target or joins its active turn at the next step boundary; queue wakes an idle target or runs as the next turn after a busy turn finishes." },
                "interrupt": { "type": "boolean", "description": "Interrupt the target's current execution first." },
                "kind": { "type": "string", "enum": ["note", "question", "answer"], "description": "Peer message kind (default `note`), matching `talk`'s vocabulary." },
                "reply_to": { "type": "string", "description": "Message id of the peer message this one answers. Echoed into the row's metadata as `metadata.replyTo`, which the timeline uses to pair an answer with its question." }
            },
            "required": ["session_id", "text"],
            "additionalProperties": false
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let to = match optional_str(&input, "session_id")
            .or_else(|| optional_str(&input, "sessionID"))
            .or_else(|| optional_str(&input, "to"))
        {
            Some(target) => target,
            None => required_str(&input, "session_id")?,
        };
        let text = required_str(&input, "text")?;
        let kind = match optional_str(&input, "kind").as_deref() {
            None | Some("note") => "note".to_owned(),
            Some("question") => "question".to_owned(),
            Some("answer") => "answer".to_owned(),
            Some(other) => {
                return Err(ToolError::new(format!(
                    "InvalidToolInput: kind must be note, question or answer, got {other:?}"
                )));
            }
        };
        let reply_to = optional_str(&input, "reply_to");
        let settings = settings_for_session(runtime, &caller).await;
        let delivery = match optional_str(&input, "delivery").as_deref() {
            None | Some("steer") => Delivery::Steer,
            Some("queue") => Delivery::Queue,
            Some(other) => {
                return Err(ToolError::new(format!(
                    "InvalidToolInput: unknown delivery {other:?}"
                )));
            }
        };
        let from_node = load_node(runtime, &caller).await?;
        let to_node = load_node(runtime, &to).await?;
        let tree = load_tree(runtime, &[caller.clone(), to.clone()]).await?;
        Policy::new(settings.limits.clone())
            .can_write(&from_node, &to_node, &tree)
            .map_err(|error| write_error(&error))?;

        loop {
            let result = {
                let mut state = runtime.inner.subagents.lock().expect("subagent state");
                state
                    .write_budget(&caller, &settings.limits)
                    .try_write(oc_types::now_millis())
            };
            match result {
                Ok(()) => break,
                Err(
                    ref error @ BudgetError::RateLimited {
                        retry_after_ms,
                        max_per_minute,
                        ..
                    },
                ) if max_per_minute > 0 && retry_after_ms > 0 && retry_after_ms <= 15_000 => {
                    let wait_ms = u64::try_from(retry_after_ms.saturating_add(25)).unwrap_or(25);
                    let wait = Duration::from_millis(wait_ms);
                    if let Some(cancel) = &ctx.cancel {
                        tokio::select! {
                            () = cancel.cancelled() => return Err(budget_error(error)),
                            () = tokio::time::sleep(wait) => {}
                        }
                    } else {
                        tokio::time::sleep(wait).await;
                    }
                }
                Err(error) => return Err(budget_error(&error)),
            }
        }

        if optional_bool(&input, "interrupt").unwrap_or(false) {
            runtime
                .interrupt(&to)
                .await
                .map_err(|error| runtime_error(&error))?;
        }
        let item_id = MessageId::new().into_inner();
        let mut metadata = json!({
            "source": "peer",
            "from": caller,
            "agent": from_node.agent,
            "kind": kind,
        });
        if let Some(reply_to) = &reply_to {
            // Same caller-supplied correlation `talk` records; omitted, never
            // `null`, when the caller names no reply target.
            metadata["replyTo"] = json!(reply_to);
        }
        // Materialised at admission (same as `talk`), so a queued write is
        // readable in the target's history before it is delivered.
        runtime
            .enqueue_materialized(NewInbox {
                id: item_id.clone(),
                session_id: to.clone(),
                kind: "synthetic".to_owned(),
                payload: json!({
                    "text": text,
                    "kind": kind,
                    "from": caller,
                    "agent": from_node.agent,
                    "metadata": metadata,
                }),
                delivery,
            })
            .await
            .map_err(|error| runtime_error(&error))?;
        // Same ephemeral notification `talk` publishes, so a write is not
        // second-class in the drawer.
        let peer_info = load_info(runtime, &to).await?;
        runtime
            .emit(
                &to,
                &peer_info.location.directory,
                SubagentPeerMessage {
                    session_id: to.clone(),
                    from: caller.clone(),
                    agent: from_node.agent.clone(),
                    kind: kind.clone(),
                    text: text.clone(),
                    request_reply: false,
                    message_id: item_id.clone(),
                },
            )
            .await
            .map_err(|error| runtime_error(&error))?;
        // Always wake the target actor after enqueueing so `queue` deliveries reach
        // idle subagents instead of sitting inert in SQLite; `promote_steers`
        // keeps `queue` rows out of an in-flight turn until the next turn.
        runtime
            .notify(&to)
            .await
            .map_err(|error| runtime_error(&error))?;
        if caller != to {
            let to_info = load_info(runtime, &to).await.ok();
            let description = to_info
                .and_then(|info| info.title)
                .unwrap_or_else(|| to.clone());
            super::wait::spawn_watcher(
                runtime.clone(),
                caller.clone(),
                to.clone(),
                to_node.agent,
                description,
                item_id,
                settings.timeout,
            );
        }
        let notice = match delivery {
            Delivery::Steer => format!("wrote to {to} (delivery: steer)"),
            Delivery::Queue => format!(
                "queued for {to} (delivery: queue); runs immediately if idle or after the current turn finishes"
            ),
        };
        Ok(ToolResult::text(notice))
    }
}
