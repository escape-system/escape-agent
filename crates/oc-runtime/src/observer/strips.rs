use std::collections::VecDeque;

use oc_observer::{FlightStrip, StripState, TreeRow};
use oc_types::now_millis;

use super::MAX_TREE_ROWS;

/// Breadth-first tree rows, root first, capped at [`MAX_TREE_ROWS`].
pub(super) async fn build_tree(
    runtime: &crate::runtime::Runtime,
    root: &str,
) -> Result<Vec<TreeRow>, oc_store::StoreError> {
    let store = &runtime.inner.config.store;
    let mut rows = Vec::new();
    let mut queue = VecDeque::new();
    if let Some(node) = store.session_node(root).await? {
        let state = if node.status == "busy" && runtime.is_waiting_permission(&node.id) {
            "waiting_permission".to_owned()
        } else {
            node.status
        };
        rows.push(TreeRow {
            sid: node.id.clone(),
            parent: None,
            agent: node.agent,
            state,
            idle_reason: node.idle_reason,
            depth: node.depth,
        });
        queue.push_back(node.id);
    }
    while let Some(parent) = queue.pop_front() {
        if rows.len() >= MAX_TREE_ROWS {
            break;
        }
        for child in store.children(&parent).await? {
            if rows.len() >= MAX_TREE_ROWS {
                break;
            }
            let id = child.id.clone();
            let state = if child.status == "busy" && runtime.is_waiting_permission(&id) {
                "waiting_permission".to_owned()
            } else {
                child.status
            };
            rows.push(TreeRow {
                sid: id.clone(),
                parent: child.parent_id,
                agent: child.agent,
                state,
                idle_reason: child.idle_reason,
                depth: child.depth,
            });
            queue.push_back(id);
        }
    }
    Ok(rows)
}

/// Map one tool outcome to the settle state and one-line outcome.
pub(crate) fn strip_result(outcome: &crate::turn::ToolOutcome) -> (StripState, Option<String>) {
    match outcome {
        crate::turn::ToolOutcome::Completed { .. } => (StripState::Ok, Some("ok".to_owned())),
        crate::turn::ToolOutcome::Failed { error, .. } => {
            (StripState::Error, Some(error.message.clone()))
        }
        crate::turn::ToolOutcome::Cancelled => {
            (StripState::Cancelled, Some("cancelled".to_owned()))
        }
    }
}

/// Build the flight strip for one dispatched tool call.
pub(crate) fn flight_strip(
    root_id: &str,
    session_id: &str,
    agent: &str,
    depth: u32,
    call: &crate::turn::PendingToolCall,
) -> Option<FlightStrip> {
    let expectation = call.expectation.as_ref()?;
    Some(FlightStrip {
        id: oc_types::ObservationId::new().into_inner(),
        root_id: root_id.to_owned(),
        session_id: session_id.to_owned(),
        agent: agent.to_owned(),
        tool_call_id: call.id.clone(),
        tool: call.name.clone(),
        why: expectation.why.clone(),
        expect_s: expectation.expect_seconds,
        intervene_s: expectation.intervene_after_seconds,
        started_ms: now_millis(),
        settled_ms: None,
        state: StripState::Running,
        outcome: None,
        parent_call: None,
        depth,
        retryable: retryable_tool(&call.name),
        retry_of: None,
    })
}

/// Tools the observer may re-run without side effects.
fn retryable_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read" | "glob" | "grep" | "lsp" | "digest" | "roster" | "memory_search"
    )
}
