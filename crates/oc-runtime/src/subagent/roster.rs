use std::collections::HashSet;
use std::time::Duration;

use oc_store::SessionNodeRow;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::runtime::Runtime;

use super::tree::{load_node, runtime_error, store_error};

const MAX_ROSTER_WAIT_SECONDS: u64 = 300;
const ROSTER_SAFETY_POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WakeReason {
    Immediate,
    NoActiveChildren,
    Inbox,
    ChildSettled,
    Timeout,
}

impl WakeReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::NoActiveChildren => "no_active_children",
            Self::Inbox => "inbox",
            Self::ChildSettled => "child_settled",
            Self::Timeout => "timeout",
        }
    }
}

struct WaitOutcome {
    reason: WakeReason,
    settled_ids: Vec<String>,
}

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
                    "description": "Optional seconds to wait until at least one running child settles or a peer/subagent message arrives in this session's inbox (steer or queue delivery) before returning."
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
        let wait_outcome = if wait_seconds > 0 {
            wait_for_roster_event(
                runtime,
                &caller,
                Duration::from_secs(wait_seconds),
                ctx.cancel.as_ref(),
            )
            .await?
        } else {
            WaitOutcome {
                reason: WakeReason::Immediate,
                settled_ids: Vec::new(),
            }
        };
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
        body.insert("counts".to_owned(), children_counts(&children));
        body.insert(
            "wakeReason".to_owned(),
            Value::String(wait_outcome.reason.as_str().to_owned()),
        );
        if !wait_outcome.settled_ids.is_empty() {
            body.insert(
                "settledIDs".to_owned(),
                Value::Array(
                    wait_outcome
                        .settled_ids
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        if let Ok(caller_inbox) = store.list_inbox(&caller).await {
            let summary = caller_inbox_summary(&caller_inbox);
            if !summary.is_empty() {
                body.insert("callerInbox".to_owned(), Value::Array(summary));
            }
        }
        let body = Value::Object(body);
        Ok(ToolResult::text(body.to_string()).with_metadata(body))
    }
}

async fn roster_entry(runtime: &Runtime, node: &SessionNodeRow) -> Result<Value, ToolError> {
    let mut value = Map::new();
    value.insert("id".to_owned(), Value::String(node.id.clone()));
    if let Some(parent_id) = &node.parent_id {
        value.insert("parentID".to_owned(), Value::String(parent_id.clone()));
    }
    value.insert("depth".to_owned(), Value::from(node.depth));
    value.insert("role".to_owned(), Value::String(node.role.clone()));
    value.insert("agent".to_owned(), Value::String(node.agent.clone()));
    value.insert("status".to_owned(), Value::String(node.status.clone()));
    value.insert("childCount".to_owned(), Value::from(node.child_count));
    if let Some(model) = &node.model {
        value.insert("model".to_owned(), Value::String(model.clone()));
    }
    if let Some(idle_reason) = &node.idle_reason {
        value.insert("idleReason".to_owned(), Value::String(idle_reason.clone()));
    }
    let store = &runtime.inner.config.store;
    let session = store
        .get_session(&node.id)
        .await
        .map_err(|error| store_error(&error))?;
    if let Some(session_obj) = session.as_ref() {
        if let Some(title) = session_obj
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
        {
            value.insert("title".to_owned(), Value::String(title.to_owned()));
        }
        if !value.contains_key("model")
            && let Some(model_obj) = session_obj.get("model")
            && let (Some(pid), Some(mid)) = (
                model_obj.get("providerID").and_then(Value::as_str),
                model_obj.get("id").and_then(Value::as_str),
            )
        {
            value.insert("model".to_owned(), Value::String(format!("{pid}/{mid}")));
        }
        if let Some(outcome) = session_obj
            .get("outcome")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            value.insert("idleOutcome".to_owned(), Value::String(outcome.to_owned()));
        }
        if let Some(time) = session_obj.get("time").filter(|v| v.is_object()) {
            value.insert("time".to_owned(), time.clone());
        }
    }
    if let Ok(inbox) = store.list_inbox(&node.id).await
        && !inbox.is_empty()
    {
        value.insert("pendingInbox".to_owned(), Value::from(inbox.len()));
    }
    if node.status == "failed"
        && let Ok(page) = store
            .list_messages(oc_store::MessageQuery {
                session_id: node.id.clone(),
                limit: 3,
                order: oc_store::Order::Desc,
                cursor: None,
                kind: Some("assistant".to_owned()),
            })
            .await
        && let Some(err_msg) = page.items.into_iter().find_map(|msg| {
            msg.pointer("/error/data/message")
                .or_else(|| msg.pointer("/error/message"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
    {
        value.insert("lastError".to_owned(), Value::String(err_msg));
    }
    Ok(Value::Object(value))
}

fn children_counts(children: &[SessionNodeRow]) -> Value {
    let mut busy = 0_u64;
    let mut completed = 0_u64;
    let mut failed = 0_u64;
    let mut interrupted = 0_u64;
    let mut idle = 0_u64;
    let mut suspended = 0_u64;
    let mut archived = 0_u64;
    for child in children {
        match child.status.as_str() {
            "busy" => busy += 1,
            "completed" => completed += 1,
            "failed" => failed += 1,
            "interrupted" => interrupted += 1,
            "suspended" => suspended += 1,
            "archived" => archived += 1,
            _ => idle += 1,
        }
    }
    json!({
        "total": children.len(),
        "busy": busy,
        "completed": completed,
        "failed": failed,
        "interrupted": interrupted,
        "idle": idle,
        "suspended": suspended,
        "archived": archived,
    })
}

fn is_actionable_inbox_item(item: &Value) -> bool {
    matches!(
        item.get("delivery").and_then(Value::as_str),
        Some("steer" | "queue") | None
    ) && matches!(
        item.get("type").and_then(Value::as_str),
        Some("user" | "synthetic" | "peer" | "observer")
    )
}

fn caller_inbox_summary(inbox: &[Value]) -> Vec<Value> {
    inbox
        .iter()
        .filter(|item| is_actionable_inbox_item(item))
        .map(|item| {
            let mut row = Map::new();
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                row.insert("id".to_owned(), Value::String(id.to_owned()));
            }
            if let Some(delivery) = item.get("delivery").and_then(Value::as_str) {
                row.insert("delivery".to_owned(), Value::String(delivery.to_owned()));
            }
            if let Some(kind) = item.get("type").and_then(Value::as_str) {
                row.insert("type".to_owned(), Value::String(kind.to_owned()));
            }
            if let Some(from) = item
                .pointer("/payload/metadata/from")
                .or_else(|| item.pointer("/payload/from"))
                .and_then(Value::as_str)
            {
                row.insert("from".to_owned(), Value::String(from.to_owned()));
            }
            if let Some(agent) = item
                .pointer("/payload/metadata/agent")
                .or_else(|| item.pointer("/payload/agent"))
                .and_then(Value::as_str)
            {
                row.insert("agent".to_owned(), Value::String(agent.to_owned()));
            }
            Value::Object(row)
        })
        .collect()
}

fn check_inbox_wake(runtime: &Runtime, caller: &str, inbox: &[Value]) -> bool {
    let mut has_steer = false;
    let mut queue_ids: Vec<String> = Vec::new();
    for item in inbox {
        if !is_actionable_inbox_item(item) {
            continue;
        }
        if item.get("delivery").and_then(Value::as_str) == Some("queue") {
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                queue_ids.push(id.to_owned());
            } else {
                has_steer = true;
            }
        } else {
            has_steer = true;
        }
    }
    if has_steer {
        return true;
    }
    if queue_ids.is_empty() {
        return false;
    }
    let mut state = runtime.inner.subagents.lock().expect("subagent state");
    if state.has_unseen_queue_ids(caller, &queue_ids) {
        state.mark_queue_ids_seen(caller, &queue_ids);
        return true;
    }
    false
}

async fn wait_for_roster_event(
    runtime: &Runtime,
    caller: &str,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> Result<WaitOutcome, ToolError> {
    let store = &runtime.inner.config.store;
    let initial_children = store
        .children(caller)
        .await
        .map_err(|error| store_error(&error))?;
    let busy_ids: HashSet<String> = initial_children
        .into_iter()
        .filter(|child| child.status == "busy")
        .map(|child| child.id)
        .collect();
    if let Ok(inbox) = store.list_inbox(caller).await
        && check_inbox_wake(runtime, caller, &inbox)
    {
        return Ok(WaitOutcome {
            reason: WakeReason::Inbox,
            settled_ids: Vec::new(),
        });
    }
    if busy_ids.is_empty() {
        return Ok(WaitOutcome {
            reason: WakeReason::NoActiveChildren,
            settled_ids: Vec::new(),
        });
    }
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if runtime.inner.shutdown.is_cancelled() {
            return Err(runtime_error(&crate::RuntimeError::Shutdown));
        }
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(runtime_error(&crate::RuntimeError::Cancelled));
        }
        // Register notification before reading SQLite so concurrent transitions cannot be missed.
        let notified = runtime.inner.roster_wake.notified();
        tokio::pin!(notified);

        let inbox = store
            .list_inbox(caller)
            .await
            .map_err(|error| store_error(&error))?;
        if check_inbox_wake(runtime, caller, &inbox) {
            return Ok(WaitOutcome {
                reason: WakeReason::Inbox,
                settled_ids: Vec::new(),
            });
        }
        let current_children = store
            .children(caller)
            .await
            .map_err(|error| store_error(&error))?;
        let settled_ids: Vec<String> = current_children
            .iter()
            .filter(|child| busy_ids.contains(&child.id) && child.status != "busy")
            .map(|child| child.id.clone())
            .collect();
        if !settled_ids.is_empty() {
            return Ok(WaitOutcome {
                reason: WakeReason::ChildSettled,
                settled_ids,
            });
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(WaitOutcome {
                reason: WakeReason::Timeout,
                settled_ids: Vec::new(),
            });
        }
        let sleep_dur = deadline
            .saturating_duration_since(now)
            .min(ROSTER_SAFETY_POLL);
        tokio::select! {
            biased;
            () = runtime.inner.shutdown.cancelled() => {
                return Err(runtime_error(&crate::RuntimeError::Shutdown));
            }
            () = async {
                if let Some(token) = cancel {
                    token.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                return Err(runtime_error(&crate::RuntimeError::Cancelled));
            }
            () = &mut notified => {}
            () = tokio::time::sleep(sleep_dur) => {}
        }
    }
}
