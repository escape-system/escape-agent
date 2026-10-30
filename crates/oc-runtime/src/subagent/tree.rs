use std::collections::BTreeMap;

use oc_store::SessionNodeRow;
use oc_subagents::{MessageError, NodeState, Role, SpawnError, WriteError};
use oc_tools::ToolError;
use oc_types::SessionInfo;
use serde_json::Value;

use crate::runtime::Runtime;

use super::digest::{DIGEST_ANCESTOR_STEPS, MAX_ANCESTOR_STEPS};

fn node_from_row(row: &SessionNodeRow) -> oc_subagents::SessionNode {
    oc_subagents::SessionNode {
        id: row.id.clone(),
        parent_id: row.parent_id.clone(),
        root_id: row.root_id.clone(),
        depth: row.depth,
        role: Role::from_column(&row.role).unwrap_or(Role::Subagent),
        spawn_allowed: row.spawn_allowed,
        child_count: row.child_count,
        agent: row.agent.clone(),
        model: row.model.clone(),
        state: NodeState::from_status(&row.status).unwrap_or(NodeState::Idle),
    }
}

pub(super) async fn load_node(
    runtime: &Runtime,
    id: &str,
) -> Result<oc_subagents::SessionNode, ToolError> {
    runtime
        .inner
        .config
        .store
        .session_node(id)
        .await
        .map_err(|error| store_error(&error))?
        .as_ref()
        .map(node_from_row)
        .ok_or_else(|| ToolError::new(format!("SessionNotFoundError: session {id} does not exist")))
}

pub(super) async fn load_info(runtime: &Runtime, id: &str) -> Result<SessionInfo, ToolError> {
    let value = runtime
        .inner
        .config
        .store
        .get_session(id)
        .await
        .map_err(|error| store_error(&error))?
        .ok_or_else(|| {
            ToolError::new(format!("SessionNotFoundError: session {id} does not exist"))
        })?;
    Runtime::parse_session(&value).map_err(|error| ToolError::new(error.to_string()))
}

pub(super) fn store_error(error: &oc_store::StoreError) -> ToolError {
    ToolError::new(format!("StoreError: {error}"))
}

/// Only the caller itself or one of its descendants may be summarized, so a
/// child can never read a sibling, an ancestor, or an unrelated transcript.
pub(super) async fn authorize_digest(
    runtime: &Runtime,
    caller: &str,
    target: &str,
) -> Result<(), ToolError> {
    if target == caller {
        return Ok(());
    }
    let store = &runtime.inner.config.store;
    let mut cursor = store
        .session_node(target)
        .await
        .map_err(|error| store_error(&error))?
        .and_then(|row| row.parent_id);
    for _ in 0..DIGEST_ANCESTOR_STEPS {
        let Some(current) = cursor.take() else {
            break;
        };
        if current == caller {
            return Ok(());
        }
        cursor = store
            .session_node(&current)
            .await
            .map_err(|error| store_error(&error))?
            .and_then(|row| row.parent_id);
    }
    Err(ToolError::new(format!(
        "DigestScopeError: session {target} is outside your subtree"
    )))
}

/// Load `ids` plus every ancestor into a tree usable by the policy checks.
pub(super) async fn load_tree(
    runtime: &Runtime,
    ids: &[String],
) -> Result<BTreeMap<String, oc_subagents::SessionNode>, ToolError> {
    let store = &runtime.inner.config.store;
    let mut tree = BTreeMap::new();
    for id in ids {
        let mut cursor = Some(id.clone());
        for _ in 0..MAX_ANCESTOR_STEPS {
            let Some(current) = cursor.take() else {
                break;
            };
            if tree.contains_key(&current) {
                break;
            }
            let Some(row) = store
                .session_node(&current)
                .await
                .map_err(|error| store_error(&error))?
            else {
                break;
            };
            cursor.clone_from(&row.parent_id);
            tree.insert(current, node_from_row(&row));
        }
    }
    Ok(tree)
}

pub(super) fn spawn_error(error: &SpawnError) -> ToolError {
    ToolError::new(format!("SpawnForbiddenError: {error}"))
}

pub(super) fn message_error(error: &MessageError) -> ToolError {
    ToolError::new(format!("SiblingScopeError: {error}"))
}

pub(super) fn write_error(error: &WriteError) -> ToolError {
    ToolError::new(format!("WriteScopeError: {error}"))
}

pub(super) fn budget_error(error: &oc_subagents::BudgetError) -> ToolError {
    ToolError::new(format!("BudgetError: {error}"))
}

pub(super) fn json_error(error: &oc_llm::LlmError) -> ToolError {
    ToolError::new(format!("ModelUnavailableError: {error}"))
}

pub(super) fn required_str(input: &Value, key: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::new(format!("InvalidToolInput: {key} is required")))
}

pub(super) fn optional_str(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

pub(super) fn optional_bool(input: &Value, key: &str) -> Option<bool> {
    input.get(key).and_then(Value::as_bool)
}

pub(super) fn runtime_error(error: &crate::RuntimeError) -> ToolError {
    ToolError::new(format!("RuntimeError: {error}"))
}
