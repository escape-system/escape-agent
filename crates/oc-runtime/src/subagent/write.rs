use std::time::Duration;

use oc_store::{Delivery, NewInbox};
use oc_subagents::{BudgetError, Policy};
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use oc_types::MessageId;
use serde_json::{Value, json};

use crate::runtime::Runtime;

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
        "Write text into a descendant session (or self). Siblings and parents are rejected by policy."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Target session id." },
                "text": { "type": "string" },
                "delivery": { "type": "string", "enum": ["steer", "queue"] },
                "interrupt": { "type": "boolean", "description": "Interrupt the target's current execution first." }
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
        runtime
            .enqueue_and_emit(NewInbox {
                id: item_id.clone(),
                session_id: to.clone(),
                kind: "synthetic".to_owned(),
                payload: json!({
                    "text": text,
                    "metadata": { "source": "peer", "from": caller },
                }),
                delivery,
            })
            .await
            .map_err(|error| runtime_error(&error))?;
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
        Ok(ToolResult::text(format!("wrote to {to}")))
    }
}
