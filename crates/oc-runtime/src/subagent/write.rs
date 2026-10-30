use oc_store::{Delivery, NewInbox};
use oc_subagents::Policy;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use oc_types::MessageId;
use serde_json::{Value, json};

use crate::runtime::Runtime;

use super::settings::settings;
use super::tree::{
    load_node, load_tree, optional_bool, optional_str, required_str, runtime_error, store_error,
    write_error,
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

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let to = required_str(&input, "session_id")?;
        let text = required_str(&input, "text")?;
        let settings = settings(runtime);
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

        if optional_bool(&input, "interrupt").unwrap_or(false) {
            runtime
                .interrupt(&to)
                .await
                .map_err(|error| runtime_error(&error))?;
        }
        runtime
            .inner
            .config
            .store
            .enqueue(NewInbox {
                id: MessageId::new().into_inner(),
                session_id: to.clone(),
                kind: "synthetic".to_owned(),
                payload: json!({
                    "text": text,
                    "metadata": { "source": "peer", "from": caller },
                }),
                delivery,
            })
            .await
            .map_err(|error| store_error(&error))?;
        runtime
            .notify(&to)
            .await
            .map_err(|error| runtime_error(&error))?;
        Ok(ToolResult::text(format!("wrote to {to}")))
    }
}
