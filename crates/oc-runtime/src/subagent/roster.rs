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
    AllSettled,
    Timeout,
}

impl WakeReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::NoActiveChildren => "no_active_children",
            Self::Inbox => "inbox",
            Self::ChildSettled => "child_settled",
            Self::AllSettled => "all_settled",
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
        "List the parent, siblings and children of this session. Returns ids, agents, statuses and titles only. Pass `wait_seconds` to wait until a running child settles or a message arrives (and `wait_all: true` to wait until all watched busy children finish in one call instead of looping)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "wait_seconds": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_ROSTER_WAIT_SECONDS,
                    "description": "Optional seconds to wait until running children settle or a peer/subagent message arrives in this session's inbox before returning."
                },
                "wait_all": {
                    "type": "boolean",
                    "description": "When true alongside `wait_seconds`, wait until ALL currently busy (or listed `session_ids`) children have settled instead of waking as soon as the first one finishes."
                },
                "session_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of child session ids to watch when waiting."
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let (wait_seconds, wait_all, target_ids) = parse_wait_args(&input);
        let wait_outcome = if wait_seconds > 0 {
            wait_for_roster_event(
                runtime,
                &caller,
                Duration::from_secs(wait_seconds),
                wait_all,
                target_ids.as_ref(),
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
        body.insert("siblings".to_owned(), Value::Array(sibling_values.clone()));
        body.insert("children".to_owned(), Value::Array(child_values));
        body.insert(
            "counts".to_owned(),
            roster_summary_counts(
                runtime,
                &caller,
                node.role,
                &children,
                &siblings,
                &sibling_values,
            ),
        );
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

fn roster_summary_counts(
    runtime: &Runtime,
    caller: &str,
    role: oc_subagents::Role,
    children: &[SessionNodeRow],
    siblings: &[SessionNodeRow],
    sibling_values: &[Value],
) -> Value {
    if role != oc_subagents::Role::Coordinator {
        return children_counts(children);
    }
    let watched = active_watcher_children(runtime, caller);
    let managed_siblings: Vec<&SessionNodeRow> = siblings
        .iter()
        .zip(sibling_values.iter())
        .filter(|(row, entry)| {
            watched.contains(&row.id)
                || entry.get("coordinatorID").and_then(Value::as_str) == Some(caller)
        })
        .map(|(row, _)| row)
        .collect();
    let mut combined: Vec<&SessionNodeRow> = children.iter().collect();
    combined.extend(managed_siblings);
    row_refs_counts(&combined)
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
        if let Some(coord) = session_obj
            .pointer("/metadata/coordinatorID")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            value.insert("coordinatorID".to_owned(), Value::String(coord.to_owned()));
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
    let refs: Vec<&SessionNodeRow> = children.iter().collect();
    row_refs_counts(&refs)
}

fn row_refs_counts(children: &[&SessionNodeRow]) -> Value {
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

fn is_watched_subagent_completion(item: &Value, watched_busy: &HashSet<String>) -> bool {
    let source = item
        .pointer("/payload/metadata/source")
        .and_then(Value::as_str);
    let child_id = item
        .pointer("/payload/metadata/childID")
        .and_then(Value::as_str);
    source == Some("subagent") && child_id.is_some_and(|id| watched_busy.contains(id))
}

fn check_inbox_wake(
    runtime: &Runtime,
    caller: &str,
    inbox: &[Value],
    suppress_watched_completions: Option<&HashSet<String>>,
) -> bool {
    let mut has_steer = false;
    let mut queue_ids: Vec<String> = Vec::new();
    for item in inbox {
        if !is_actionable_inbox_item(item) {
            continue;
        }
        if let Some(watched) = suppress_watched_completions
            && is_watched_subagent_completion(item, watched)
        {
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

fn parse_wait_args(input: &Value) -> (u64, bool, Option<HashSet<String>>) {
    let wait_seconds = input
        .get("wait_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_ROSTER_WAIT_SECONDS);
    let wait_all = input
        .get("wait_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let target_ids = input
        .get("session_ids")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<HashSet<String>>()
        })
        .filter(|set| !set.is_empty());
    (wait_seconds, wait_all, target_ids)
}

fn active_watcher_children(runtime: &Runtime, caller: &str) -> HashSet<String> {
    let state = runtime.inner.subagents.lock().expect("subagent state");
    state
        .active_watchers
        .iter()
        .filter(|(parent, _)| parent == caller)
        .map(|(_, child)| child.clone())
        .collect()
}

async fn evaluate_roster_children(
    runtime: &Runtime,
    caller: &str,
    coordinator_parent: Option<&str>,
    busy_ids: &HashSet<String>,
    wait_all: bool,
) -> Result<(Vec<String>, Option<WakeReason>), ToolError> {
    let store = &runtime.inner.config.store;
    let mut tracked = store
        .children(caller)
        .await
        .map_err(|error| store_error(&error))?;
    if let Some(parent_id) = coordinator_parent {
        let siblings = store
            .children(parent_id)
            .await
            .map_err(|error| store_error(&error))?;
        tracked.extend(
            siblings
                .into_iter()
                .filter(|row| row.id != caller && busy_ids.contains(&row.id)),
        );
    }
    let watched = active_watcher_children(runtime, caller);
    let is_still_active = |id: &str| {
        watched.contains(id)
            || tracked
                .iter()
                .any(|child| child.id == id && child.status == "busy")
    };
    let mut settled_ids: Vec<String> = busy_ids
        .iter()
        .filter(|id| !is_still_active(id))
        .cloned()
        .collect();
    settled_ids.sort();
    let all_done = wait_all && !busy_ids.iter().any(|id| is_still_active(id));
    if all_done {
        return Ok((settled_ids, Some(WakeReason::AllSettled)));
    }
    if !wait_all && !settled_ids.is_empty() {
        return Ok((settled_ids, Some(WakeReason::ChildSettled)));
    }
    Ok((settled_ids, None))
}

async fn snapshot_initial_busy_ids(
    runtime: &Runtime,
    caller: &str,
    coordinator_parent: Option<&str>,
    target_ids: Option<&HashSet<String>>,
) -> Result<HashSet<String>, ToolError> {
    let store = &runtime.inner.config.store;
    let initial_children = store
        .children(caller)
        .await
        .map_err(|error| store_error(&error))?;
    let watched = active_watcher_children(runtime, caller);
    let mut busy_ids: HashSet<String> = initial_children
        .into_iter()
        .filter(|child| {
            (child.status == "busy" || watched.contains(&child.id))
                && target_ids.is_none_or(|wanted| wanted.contains(&child.id))
        })
        .map(|child| child.id)
        .collect();
    let Some(parent_id) = coordinator_parent else {
        return Ok(busy_ids);
    };
    let siblings = store
        .children(parent_id)
        .await
        .map_err(|error| store_error(&error))?;
    for sibling in siblings {
        if sibling.id == caller || !target_ids.is_none_or(|wanted| wanted.contains(&sibling.id)) {
            continue;
        }
        let is_busy_or_watched = sibling.status == "busy" || watched.contains(&sibling.id);
        if !is_busy_or_watched {
            continue;
        }
        let owned_by_coordinator = watched.contains(&sibling.id)
            || target_ids.is_some_and(|wanted| wanted.contains(&sibling.id))
            || store
                .get_session(&sibling.id)
                .await
                .ok()
                .flatten()
                .and_then(|obj| {
                    obj.pointer("/metadata/coordinatorID")
                        .and_then(Value::as_str)
                        .map(|coord| coord == caller)
                })
                == Some(true);
        if owned_by_coordinator {
            busy_ids.insert(sibling.id);
        }
    }
    Ok(busy_ids)
}

async fn wait_for_roster_event(
    runtime: &Runtime,
    caller: &str,
    timeout: Duration,
    wait_all: bool,
    target_ids: Option<&HashSet<String>>,
    cancel: Option<&CancellationToken>,
) -> Result<WaitOutcome, ToolError> {
    let store = &runtime.inner.config.store;
    let caller_node = load_node(runtime, caller).await?;
    let coordinator_parent = (caller_node.role == oc_subagents::Role::Coordinator)
        .then(|| caller_node.parent_id.clone())
        .flatten();
    let busy_ids =
        snapshot_initial_busy_ids(runtime, caller, coordinator_parent.as_deref(), target_ids)
            .await?;
    let suppress_completions = (!busy_ids.is_empty()).then_some(&busy_ids);
    if let Ok(inbox) = store.list_inbox(caller).await
        && check_inbox_wake(runtime, caller, &inbox, suppress_completions)
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

        let (settled_ids, reason) = evaluate_roster_children(
            runtime,
            caller,
            coordinator_parent.as_deref(),
            &busy_ids,
            wait_all,
        )
        .await?;
        if let Some(reason) = reason {
            return Ok(WaitOutcome {
                reason,
                settled_ids,
            });
        }

        let inbox = store
            .list_inbox(caller)
            .await
            .map_err(|error| store_error(&error))?;
        if check_inbox_wake(runtime, caller, &inbox, suppress_completions) {
            return Ok(WaitOutcome {
                reason: WakeReason::Inbox,
                settled_ids,
            });
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(WaitOutcome {
                reason: WakeReason::Timeout,
                settled_ids,
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
