//! `memory_forget`: soft-delete one memory by id.

use std::sync::Arc;

use async_trait::async_trait;
use oc_tools::util;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

use super::ToolScope;

const FORGET_DESCRIPTION: &str =
    "Soft-delete a memory by id (mem_…). It stops appearing in searches immediately.";

/// `memory_forget {id}`
#[derive(Debug)]
pub struct MemoryForgetTool {
    pub(super) scope: Arc<ToolScope>,
}

#[async_trait]
impl Tool for MemoryForgetTool {
    fn name(&self) -> &'static str {
        "memory_forget"
    }

    fn description(&self) -> &'static str {
        FORGET_DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "pattern": "^mem_",
                    "description": "Memory id (mem_…)"
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let id = util::required_string(&input, "id")?;
        let forgotten = self
            .scope
            .service
            .forget(&id)
            .await
            .map_err(ToolError::from)?;
        let output = if forgotten {
            format!("Forgot memory {id}.")
        } else {
            format!("No live memory with id {id}.")
        };
        Ok(ToolResult::new(
            Some(output),
            None,
            Some(json!({"id": id, "forgotten": forgotten})),
        ))
    }
}
