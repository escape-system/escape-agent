//! `write` tool: create or overwrite a file, preserving an existing BOM.

use std::fs;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

#[derive(Debug, Default, Clone, Copy)]
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Writes a file to the local filesystem, overwriting if one exists.\n\nMissing parent directories are created automatically.\n\nUse this tool to create new files or overwrite existing files. For partial changes, use the edit tool instead."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write to"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let path = util::required_string(&input, "path")?;
        let content = util::required_string(&input, "content")?;
        let absolute = util::resolve(ctx, &path);

        let existed = absolute.exists();
        let existing_bom = fs::read(&absolute).is_ok_and(|bytes| util::has_bom(&bytes));
        let (content_bom, body) = util::split_bom(&content);
        let bytes = util::join_bom(body, existing_bom || content_bom).into_bytes();

        util::atomic_write(&absolute, &bytes)
            .map_err(|error| ToolError::new(format!("Unable to write {path}: {error}")))?;

        Ok(ToolResult::text(format!(
            "{} file successfully: {path}",
            if existed { "Wrote" } else { "Created" }
        )))
    }
}
