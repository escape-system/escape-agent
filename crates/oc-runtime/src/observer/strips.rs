use std::collections::{HashMap, VecDeque};

use oc_observer::{FlightStrip, StripState, TreeRow};
use oc_types::now_millis;

use super::MAX_TREE_ROWS;

/// Breadth-first tree rows, root first, capped at [`MAX_TREE_ROWS`].
pub(super) async fn build_tree(
    runtime: &crate::runtime::Runtime,
    root: &str,
) -> Result<Vec<TreeRow>, oc_store::StoreError> {
    let store = &runtime.inner.config.store;
    // Fetch a generous recursive window in a single read and preserve exact BFS
    // order/truncation in memory.
    let nodes = store.session_tree(root, MAX_TREE_ROWS * 4).await?;
    let mut root_node = None;
    let mut by_parent: HashMap<String, Vec<oc_store::SessionNodeRow>> = HashMap::new();
    for node in nodes {
        if node.id == root {
            root_node = Some(node);
        } else if let Some(parent) = node.parent_id.clone() {
            by_parent.entry(parent).or_default().push(node);
        }
    }
    let Some(root_node) = root_node else {
        return Ok(Vec::new());
    };

    let row_of = |node: oc_store::SessionNodeRow, parent: Option<String>| {
        let state = if node.status == "busy" && runtime.is_waiting_permission(&node.id) {
            "waiting_permission".to_owned()
        } else {
            node.status
        };
        TreeRow {
            sid: node.id,
            parent,
            agent: node.agent,
            state,
            idle_reason: node.idle_reason,
            depth: node.depth,
        }
    };

    let mut rows = Vec::new();
    let mut queue = VecDeque::new();
    let root_id = root_node.id.clone();
    rows.push(row_of(root_node, None));
    queue.push_back(root_id);

    while let Some(parent) = queue.pop_front() {
        if rows.len() >= MAX_TREE_ROWS {
            break;
        }
        let Some(children) = by_parent.remove(&parent) else {
            continue;
        };
        for child in children {
            if rows.len() >= MAX_TREE_ROWS {
                break;
            }
            let id = child.id.clone();
            let parent_id = child.parent_id.clone();
            rows.push(row_of(child, parent_id));
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
        started_mono_ms: Some(oc_observer::clock::process_mono_ms()),
        settled_mono_ms: None,
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
