//! `patch` tool: the `*** Begin Patch` envelope with add/delete/update/move.
//!
//! The facade holds [`PatchTool`] and its [`Tool`] contract; the envelope
//! grammar lives in [`parser`], the hunk model and context matcher in
//! [`hunks`], path extraction in [`targets`], and the prepare/apply pipeline in
//! [`apply`].

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolError, ToolResult};

mod apply;
mod hunks;
mod parser;
mod targets;

pub use hunks::{Chunk, Hunk, derive};
pub use parser::parse;
pub use targets::patch_targets;

#[derive(Debug, Default, Clone, Copy)]
pub struct PatchTool;

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &str {
        "patch"
    }

    fn description(&self) -> &str {
        "Use the `patch` tool to edit files. Your patch language is a stripped-down, file-oriented diff format designed to be easy to parse and safe to apply. You can think of it as a high-level envelope:\n\n*** Begin Patch\n[ one or more file sections ]\n*** End Patch\n\nEach operation starts with one of three headers:\n*** Add File: <path> - create a new file. Every following line is a + line.\n*** Delete File: <path> - remove an existing file. Nothing follows.\n*** Update File: <path> - patch an existing file in place (optionally with `*** Move to: <path>`).\nUpdate hunks use `@@` context markers; context lines start with a space, removed lines with `-`, added lines with `+`."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "patchText": {
                    "type": "string",
                    "description": "The full patch text describing add, update, and delete operations"
                }
            },
            "required": ["patchText"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        apply::execute(ctx, input).await
    }
}
