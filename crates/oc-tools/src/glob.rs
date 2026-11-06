//! `glob` tool: ripgrep `--files` semantics via the `ignore` crate.

use std::fs;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

pub const DEFAULT_LIMIT: usize = 100;
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Clone, Copy)]
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Search file paths using a glob pattern (examples: \"**/*.ts\", \"src/**/*.tsx\")."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files against"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search. Defaults to the current working directory."
                },
                "hidden": {
                    "type": "boolean",
                    "description": "Include hidden files and directories (default: false)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of matching files to return (default: 100)"
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let pattern = util::required_string(&input, "pattern")?;
        let search = util::optional_string(&input, "path")?;
        let hidden = util::optional_bool(&input, "hidden")?.unwrap_or(false);
        let limit = util::optional_u64(&input, "limit")?
            .map_or(DEFAULT_LIMIT, util::to_usize)
            .max(1);

        let display = search.clone().unwrap_or_else(|| ".".to_string());
        let root = util::resolve(ctx, search.as_deref().unwrap_or("."));
        let metadata = fs::metadata(&root)
            .map_err(|_| ToolError::new(format!("Search path does not exist: {display}")))?;
        if !metadata.is_dir() {
            return Err(ToolError::new(format!(
                "Search path is not a directory: {display}"
            )));
        }

        util::PathGlob::new(&pattern)
            .map_err(|error| ToolError::new(format!("Invalid glob pattern: {error}")))?;

        let directory = ctx.directory.clone();
        let root_for_task = root.clone();
        let pattern_for_task = pattern.clone();
        let task = tokio::task::spawn_blocking(move || {
            walk(&root_for_task, &directory, &pattern_for_task, hidden, limit)
        });

        let result = tokio::time::timeout(SEARCH_TIMEOUT, task)
            .await
            .map_err(|_| {
                ToolError::new(format!(
                    "Search timed out after {} seconds. Consider using a more specific path or pattern.",
                    SEARCH_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|error| ToolError::new(format!("Unable to find files matching {pattern}: {error}")))?
            .map_err(|error| ToolError::new(format!("Unable to find files matching {pattern}: {error}")))?;

        let (entries, truncated) = result;
        let count = entries.len();
        let mut lines = if entries.is_empty() {
            vec!["No files found".to_string()]
        } else {
            entries
        };
        if truncated {
            lines.push(String::new());
            lines.push(format!(
                "(Results are truncated: showing first {count} results. Consider using a more specific path or pattern.)"
            ));
        }

        Ok(ToolResult::new(
            Some(lines.join("\n")),
            None,
            Some(json!({"count": count, "truncated": truncated})),
        ))
    }
}

fn walk(
    root: &Path,
    directory: &Path,
    pattern: &str,
    hidden: bool,
    limit: usize,
) -> Result<(Vec<String>, bool), String> {
    let matcher = util::PathGlob::new(pattern)?;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .hidden(!hidden)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != std::ffi::OsStr::new(".git"));

    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in builder.build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let relative_to_root = path.strip_prefix(root).unwrap_or(path);
        let relative = util::slash(relative_to_root);
        if !matcher.is_match(&relative) {
            continue;
        }
        if entries.len() >= limit {
            truncated = true;
            break;
        }
        entries.push(util::relative_display(directory, path));
    }
    entries.sort();
    Ok((entries, truncated))
}
