use oc_store::SessionNodeRow;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Map, Value, json};

use crate::runtime::Runtime;

use super::tree::{load_node, store_error};

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
        "List the parent, siblings and children of this session. Returns ids, agents, statuses and titles only."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
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

        let mut parent_value = Value::Null;
        if let Some(parent) = &parent {
            parent_value = roster_entry(runtime, parent).await?;
        }
        let mut sibling_values = Vec::new();
        for sibling in &siblings {
            sibling_values.push(roster_entry(runtime, sibling).await?);
        }
        let mut child_values = Vec::new();
        for child in &children {
            child_values.push(roster_entry(runtime, child).await?);
        }
        let body = json!({
            "parent": parent_value,
            "siblings": sibling_values,
            "children": child_values,
        });
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
