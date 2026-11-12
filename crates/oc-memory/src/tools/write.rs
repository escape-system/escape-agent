//! `memory_write`: store one durable memory through the memory service.

use std::sync::Arc;

use async_trait::async_trait;
use oc_tools::util;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

use crate::service::NewMemoryInput;

use super::common::{optional_confidence, string_array};
use super::{SCOPES, ToolScope};

const WRITE_DESCRIPTION: &str = "Store one durable memory that survives sessions: a project fact, decision, preference, \
     pattern or gotcha. Prefer memories a future session cannot derive from the repository. \
     Text is capped at 2000 characters; credentials are rejected. Write at most one memory per \
     turn unless the user asks for more.";

/// `memory_write {text, kind?, scope?, tags?, importance?, confidence?}`
#[derive(Debug)]
pub struct MemoryWriteTool {
    pub(super) scope: Arc<ToolScope>,
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn name(&self) -> &'static str {
        "memory_write"
    }

    fn description(&self) -> &'static str {
        WRITE_DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Memory text: one self-contained statement, at most 2000 characters"
                },
                "kind": {
                    "type": "string",
                    "enum": ["fact", "decision", "preference", "pattern", "gotcha"],
                    "description": "Memory kind (default: fact)"
                },
                "scope": {
                    "type": "string",
                    "enum": SCOPES,
                    "description": "Where the memory applies (default: project)"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional tags used by search"
                },
                "importance": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 5,
                    "description": "1-5 (default: 3); importance 5 is pinned into every retrieval"
                },
                "confidence": {
                    "type": "number",
                    "minimum": 0,
                    "maximum": 1,
                    "description": "0-1 (default: 0.7)"
                }
            },
            "required": ["text"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let text = util::required_string(&input, "text")?;
        let kind = util::optional_string(&input, "kind")?
            .map(|kind| kind.trim().to_owned())
            .filter(|kind| !kind.is_empty());
        let scope = util::optional_string(&input, "scope")?
            .map(|scope| scope.trim().to_owned())
            .filter(|scope| !scope.is_empty())
            .unwrap_or_else(|| "project".to_owned());
        if !SCOPES.contains(&scope.as_str()) {
            return Err(ToolError::new(format!(
                "Invalid arguments: `scope` must be one of {}",
                SCOPES.join(", ")
            )));
        }
        let scope_id = self.scope.scope_id(ctx, &scope).map_err(ToolError::from)?;
        let tags = string_array(&input, "tags")?;
        let importance = util::optional_u64(&input, "importance")?
            .map(|value| u8::try_from(value.clamp(1, 5)).unwrap_or(5));
        let confidence = optional_confidence(&input, "confidence")?;

        let stored = self
            .scope
            .service
            .write(NewMemoryInput {
                text,
                kind,
                scope: Some(scope.clone()),
                scope_id,
                tags,
                importance,
                confidence,
                source: Some("agent".to_owned()),
                source_session: Some(ctx.session_id.clone()),
                source_message: None,
            })
            .await
            .map_err(ToolError::from)?;

        let id = stored["id"].as_str().unwrap_or_default().to_owned();
        let stored_kind = stored["kind"].as_str().unwrap_or("fact").to_owned();
        Ok(ToolResult::new(
            Some(format!("Stored memory {id} [{stored_kind}] scope={scope}")),
            None,
            Some(json!({"id": id, "kind": stored_kind, "scope": scope})),
        ))
    }
}
