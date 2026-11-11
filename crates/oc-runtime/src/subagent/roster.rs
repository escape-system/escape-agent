use std::collections::HashSet;
use std::time::Duration;

use oc_store::SessionNodeRow;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::runtime::Runtime;

use super::tree::{load_node, runtime_error, store_error};

const MAX_ROSTER_WAIT_SECONDS: u64 = 300;
const ROSTER_WAIT_POLL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub(super) struct RosterTool {
    pub(super) runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for RosterTool {
    fn name(&self) -> &'static str {
        "roster"
    }

    fn description(&self) -> &'static str {
        "List the parent, siblings and children of this session. Returns ids, agents, statuses and titles only. Pass `wait_seconds` to wait until a running child settles or a peer/subagent message arrives instead of polling or spawning sleep subagents."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "wait_seconds": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_ROSTER_WAIT_SECONDS,
                    "description": "Optional seconds to wait until at least one running child settles or a peer/subagent message arrives in this session's inbox before returning."
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let wait_seconds = input
            .get("wait_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(MAX_ROSTER_WAIT_SECONDS);
        if wait_seconds > 0 {
            wait_for_roster_event(
                runtime,
                &caller,
                Duration::from_secs(wait_seconds),
                ctx.cancel.as_ref(),
            )
            .await?;
        }
        let store = &runtime.inner.config.store;
        let node = load_node(runtime, &caller).await?;
        let mut children = store
            .children(&caller)
            .await
            .map_err(|error| store_error(&error))?;
        let parent = match &node.parent_id {
            Some(parent) => store
                .session_node(parent)
                .await
                .map_err(|error| store_error(&error))?,
            None => None,
        };
        let siblings = match &node.parent_id {
            Some(parent) => store
                .children(parent)
                .await
                .map_err(|error| store_error(&error))?
                .into_iter()
                .filter(|child| child.id != caller)
                .collect(),
            None => Vec::new(),
        };
        children.sort_by(|left, right| left.id.cmp(&right.id));
        let mut siblings: Vec<SessionNodeRow> = siblings;
        siblings.sort_by(|left, right| left.id.cmp(&right.id));

        // Optional JSON fields are omitted, never null (AGENTS.md). A root
        // caller has no parent, so the key is absent rather than `null`;
        // `docs/05-subagents.md` documents `parent` as an object.
        let mut body = Map::new();
        if let Some(parent) = &parent {
            body.insert("parent".to_owned(), roster_entry(runtime, parent).await?);
        }
        let mut sibling_values = Vec::new();
        for sibling in &siblings {
            sibling_values.push(roster_entry(runtime, sibling).await?);
        }
        let mut child_values = Vec::new();
        for child in &children {
            child_values.push(roster_entry(runtime, child).await?);
        }
        body.insert("siblings".to_owned(), Value::Array(sibling_values));
        body.insert("children".to_owned(), Value::Array(child_values));
        let body = Value::Object(body);
        Ok(ToolResult::text(body.to_string()).with_metadata(body))
    }
}

async fn roster_entry(runtime: &Runtime, node: &SessionNodeRow) -> Result<Value, ToolError> {
    let mut value = Map::new();
    value.insert("id".to_owned(), Value::String(node.id.clone()));
    value.insert("agent".to_owned(), Value::String(node.agent.clone()));
    value.insert("status".to_owned(), Value::String(node.status.clone()));
    let session = runtime
        .inner
        .config
        .store
        .get_session(&node.id)
        .await
        .map_err(|error| store_error(&error))?;
    if let Some(title) = session
        .as_ref()
        .and_then(|session| session.get("title"))
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
    {
        value.insert("title".to_owned(), Value::String(title.to_owned()));
    }
    Ok(Value::Object(value))
}

async fn wait_for_roster_event(
    runtime: &Runtime,
    caller: &str,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> Result<(), ToolError> {
    let store = &runtime.inner.config.store;
    let initial_children = store
        .children(caller)
        .await
        .map_err(|error| store_error(&error))?;
    let busy_ids: HashSet<String> = initial_children
        .into_iter()
        .filter(|child| child.status != "idle")
        .map(|child| child.id)
        .collect();
    if busy_ids.is_empty() {
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if runtime.inner.shutdown.is_cancelled() {
            return Err(runtime_error(&crate::RuntimeError::Shutdown));
        }
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(runtime_error(&crate::RuntimeError::Cancelled));
        }
        let inbox = store
            .list_inbox(caller)
            .await
            .map_err(|error| store_error(&error))?;
        let has_steer = inbox.iter().any(|item| {
            item.get("delivery").and_then(Value::as_str) == Some("steer")
                && matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("user" | "synthetic" | "peer" | "observer")
                )
        });
        if has_steer {
            return Ok(());
        }
        let current_children = store
            .children(caller)
            .await
            .map_err(|error| store_error(&error))?;
        let any_settled = current_children
            .iter()
            .any(|child| busy_ids.contains(&child.id) && child.status == "idle");
        if any_settled || tokio::time::Instant::now() >= deadline {
            return Ok(());
        }
        tokio::time::sleep(ROSTER_WAIT_POLL).await;
    }
}
