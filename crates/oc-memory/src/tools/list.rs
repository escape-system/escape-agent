//! `memory_list`: page stored memories by importance and recency.

use std::sync::Arc;

use async_trait::async_trait;
use oc_tools::util;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

use crate::render::normalize_line;
use crate::service::{LIST_DEFAULT, LIST_MAX, ListRequest};

use super::{SCOPES, ToolScope};

const LIST_DESCRIPTION: &str = "List stored memories by importance and recency, optionally filtered by scope, kind and \
     cursor.";

/// `memory_list {scope?, kind?, limit?, cursor?}`
#[derive(Debug)]
pub struct MemoryListTool {
    pub(super) scope: Arc<ToolScope>,
}

#[async_trait]
impl Tool for MemoryListTool {
    fn name(&self) -> &'static str {
        "memory_list"
    }

    fn description(&self) -> &'static str {
        LIST_DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": SCOPES,
                    "description": "Limit to one scope; defaults to every scope visible to this session"
                },
                "kind": {
                    "type": "string",
                    "enum": ["fact", "decision", "preference", "pattern", "gotcha"],
                    "description": "Restrict to this kind"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "description": "Page size (default: 20)"
                },
                "cursor": {
                    "type": "string",
                    "description": "Cursor from `more: cursor=<n>` in a previous page"
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let requested = util::optional_string(&input, "scope")?;
        let kind = util::optional_string(&input, "kind")?
            .map(|kind| kind.trim().to_owned())
            .filter(|kind| !kind.is_empty());
        let limit = util::optional_u64(&input, "limit")?
            .map_or(LIST_DEFAULT, util::to_usize)
            .clamp(1, LIST_MAX);
        let cursor = util::optional_string(&input, "cursor")?;
        let scopes = self
            .scope
            .scopes(ctx, requested.as_deref())
            .map_err(ToolError::from)?;

        let page = self
            .scope
            .service
            .list_page(ListRequest {
                scopes,
                kind,
                tags: Vec::new(),
                limit,
                cursor: cursor.clone(),
            })
            .await
            .map_err(ToolError::from)?;
        let rows = &page.items;

        let count = rows.len();
        let offset = cursor
            .as_deref()
            .and_then(|cursor| cursor.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let next = page.more.then(|| offset.saturating_add(count).to_string());

        let mut output = String::new();
        for row in rows {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(row["id"].as_str().unwrap_or_default());
            output.push_str(" [");
            output.push_str(row["kind"].as_str().unwrap_or("fact"));
            output.push_str("] (");
            output.push_str(row["scope"].as_str().unwrap_or("project"));
            output.push_str(") ");
            output.push_str(&normalize_line(row["text"].as_str().unwrap_or_default()));
        }
        if output.is_empty() {
            output.push_str("No memories stored.");
        } else if let Some(next) = &next {
            output.push_str("\n(more: cursor=");
            output.push_str(next);
            output.push(')');
        }
        if page.window_exhausted {
            // The store cannot report a total either, so "may exist" is the most
            // a truthful answer can claim here.
            output.push_str("\n(window: page ended at the pagination limit; more memories may exist — narrow the scope or kind)");
        }

        Ok(ToolResult::new(
            Some(output),
            None,
            Some(json!({
                "count": count,
                "cursor": next,
                "windowExhausted": page.window_exhausted,
            })),
        ))
    }
}
