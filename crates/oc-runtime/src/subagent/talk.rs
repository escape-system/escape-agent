use oc_store::{Delivery, NewInbox};
use oc_subagents::Policy;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use oc_types::MessageId;
use serde_json::{Value, json};

use crate::runtime::Runtime;

use super::protocol::SubagentPeerMessage;
use super::settings::settings;
use super::tree::{
    budget_error, load_info, load_node, load_tree, message_error, optional_bool, required_str,
    runtime_error, store_error,
};

#[derive(Debug, Clone)]
pub(super) struct TalkTool {
    pub(super) runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for TalkTool {
    fn name(&self) -> &'static str {
        "talk"
    }

    fn description(&self) -> &'static str {
        "Send a peer note, question or answer to a sibling, parent or child session. The target receives a <peer> block."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "Target session id." },
                "kind": { "type": "string", "enum": ["note", "question", "answer"] },
                "text": { "type": "string" },
                "request_reply": { "type": "boolean", "description": "Ask the target to answer when it replies." }
            },
            "required": ["to", "kind", "text"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let to = required_str(&input, "to")?;
        let kind = required_str(&input, "kind")?;
        let text = required_str(&input, "text")?;
        if !matches!(kind.as_str(), "note" | "question" | "answer") {
            return Err(ToolError::new(
                "InvalidToolInput: kind must be note, question or answer",
            ));
        }
        let request_reply = optional_bool(&input, "request_reply").unwrap_or(false);
        let settings = settings(runtime);
        let from_node = load_node(runtime, &caller).await?;
        let to_node = load_node(runtime, &to).await?;
        let to_info = load_info(runtime, &to).await?;
        let tree = load_tree(runtime, &[caller.clone(), to.clone()]).await?;
        Policy::new(settings.limits.clone())
            .can_message(&from_node, &to_node, &tree)
            .map_err(|error| message_error(&error))?;

        {
            let mut state = runtime.inner.subagents.lock().expect("subagent state");
            state
                .sibling_budget(&caller, &settings.limits)
                .try_message(oc_types::now_millis())
                .map_err(|error| budget_error(&error))?;
        }

        let message_id = MessageId::new().into_inner();
        runtime
            .inner
            .config
            .store
            .enqueue(NewInbox {
                id: message_id.clone(),
                session_id: to.clone(),
                kind: "peer".to_owned(),
                payload: json!({
                    "text": text,
                    "kind": kind,
                    "from": caller,
                    "agent": from_node.agent,
                    "sender_session_id": caller,
                    "sender_agent": from_node.agent,
                    "request_reply": request_reply,
                    "metadata": {
                        "source": "peer",
                        "from": caller,
                        "agent": from_node.agent,
                        "kind": kind,
                    },
                }),
                delivery: Delivery::Steer,
            })
            .await
            .map_err(|error| store_error(&error))?;
        // The inbox row is durable; publish the ephemeral UI event before the
        // target is woken so a client sees the message before output starts.
        runtime
            .emit(
                &to,
                &to_info.location.directory,
                SubagentPeerMessage {
                    session_id: to.clone(),
                    from: caller.clone(),
                    agent: from_node.agent.clone(),
                    kind: kind.clone(),
                    text: text.clone(),
                    request_reply,
                    message_id,
                },
            )
            .await
            .map_err(|error| runtime_error(&error))?;
        if settings.siblings_wake {
            runtime
                .notify(&to)
                .await
                .map_err(|error| runtime_error(&error))?;
        }
        Ok(ToolResult::text(format!("{kind} delivered to {to}")))
    }
}
