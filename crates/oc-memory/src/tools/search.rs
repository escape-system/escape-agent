//! `memory_search`: retrieve a rendered `<memory>` block across visible scopes.

use std::sync::Arc;

use async_trait::async_trait;
use oc_tools::util;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

use crate::service::{MAX_SEARCH_LIMIT, RetrieveRequest, SEARCH_DEFAULT};

use super::common::string_array;
use super::{SCOPES, ToolScope};

const SEARCH_DESCRIPTION: &str = "Search durable memories visible to this session (global, project, agent and session \
     scopes). Results are rendered as a <memory> block with ids and kinds.";

/// `memory_search {query, scope?, kinds?, tags?, limit?}`
#[derive(Debug)]
pub struct MemorySearchTool {
    pub(super) scope: Arc<ToolScope>,
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &'static str {
        "memory_search"
    }

    fn description(&self) -> &'static str {
        SEARCH_DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to search for in memory text"
                },
                "scope": {
                    "type": "string",
                    "enum": SCOPES,
                    "description": "Limit to one scope; defaults to every scope visible to this session"
                },
                "kinds": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["fact", "decision", "preference", "pattern", "gotcha"]
                    },
                    "description": "Restrict to these kinds"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Require at least one of these tags"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "Maximum number of memories in the block (default: 8)"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let query = util::required_string(&input, "query")?;
        let requested = util::optional_string(&input, "scope")?;
        let kinds = string_array(&input, "kinds")?;
        let tags = string_array(&input, "tags")?;
        let limit = util::optional_u64(&input, "limit")?
            .map_or(SEARCH_DEFAULT, util::to_usize)
            .clamp(1, MAX_SEARCH_LIMIT);
        let scopes = self
            .scope
            .scopes(ctx, requested.as_deref())
            .map_err(ToolError::from)?;

        let block = self
            .scope
            .service
            .retrieve(RetrieveRequest {
                scopes,
                query,
                kinds,
                tags,
                limit,
                token_budget: 0,
            })
            .await
            .map_err(ToolError::from)?;

        let output = if block.text.is_empty() {
            "No memories found.".to_owned()
        } else {
            block.text.clone()
        };
        Ok(ToolResult::new(
            Some(output),
            None,
            Some(json!({"count": block.ids.len(), "tokens": block.tokens})),
        ))
    }
}
