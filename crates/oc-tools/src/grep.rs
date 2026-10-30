//! `grep` tool: line-oriented content search with `grep-searcher`/`grep-regex`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use serde_json::{Value, json};

use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

pub const DEFAULT_LIMIT: usize = 100;
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LINE_CHARS: usize = 2_000;

#[derive(Debug, Default, Clone, Copy)]
pub struct GrepTool;

struct Hit {
    path: String,
    line: u64,
    text: String,
}

enum SearchError {
    InvalidPattern(String),
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents using ripgrep's regular expression syntax or literal text matching. Use it to locate specific code, symbols, or text patterns, and narrow searches with `path` or `include`. Returns matching file paths, line numbers, and line previews."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Regular expression or literal text to match in file contents."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search. Defaults to the current working directory."
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter files (for example, \"*.js\" or \"*.{ts,tsx}\")"
                },
                "literal": {
                    "type": "boolean",
                    "description": "Treat `pattern` as exact text instead of a regular expression (default: false)."
                },
                "caseSensitive": {
                    "type": "boolean",
                    "description": "Use case-sensitive matching (default: true)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of matching lines to return (default: 100)"
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let pattern = util::required_string(&input, "pattern")?;
        if pattern.is_empty() {
            return Err(ToolError::new("Pattern must not be empty"));
        }
        let search = util::optional_string(&input, "path")?;
        let include = util::optional_string(&input, "include")?;
        let literal = util::optional_bool(&input, "literal")?.unwrap_or(false);
        let case_sensitive = util::optional_bool(&input, "caseSensitive")?.unwrap_or(true);
        let limit = util::optional_u64(&input, "limit")?
            .map_or(DEFAULT_LIMIT, util::to_usize)
            .max(1);

        let display = search.clone().unwrap_or_else(|| ".".to_string());
        let root = util::resolve(ctx, search.as_deref().unwrap_or("."));
        let metadata = fs::metadata(&root)
            .map_err(|_| ToolError::new(format!("Search path does not exist: {display}")))?;
        let is_file = metadata.is_file();

        let directory = ctx.directory.clone();
        let pattern_for_task = pattern.clone();
        let task = tokio::task::spawn_blocking(move || {
            let options = SearchOptions {
                pattern: &pattern_for_task,
                include: include.as_deref(),
                literal,
                case_sensitive,
                limit,
            };
            search_in(&root, is_file, &directory, &options)
        });

        let result = tokio::time::timeout(SEARCH_TIMEOUT, task)
            .await
            .map_err(|_| {
                ToolError::new(format!(
                    "Search timed out after {} seconds. Consider using a more specific path or pattern.",
                    SEARCH_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|error| ToolError::new(format!("Unable to grep for {pattern}: {error}")))?;

        let (hits, truncated) = match result {
            Ok(value) => value,
            Err(SearchError::InvalidPattern(message)) => {
                return Err(ToolError::new(format!("Invalid regex pattern: {message}")));
            }
        };

        let count = hits.len();
        let mut lines = if hits.is_empty() {
            vec!["No matches found".to_string()]
        } else {
            vec![format!("Found {count} matches")]
        };
        let mut current = String::new();
        for hit in &hits {
            if current != hit.path {
                if !current.is_empty() {
                    lines.push(String::new());
                }
                current.clone_from(&hit.path);
                lines.push(format!("{}:", hit.path));
            }
            lines.push(format!("  Line {}: {}", hit.line, hit.text));
        }
        if truncated {
            lines.push(String::new());
            lines.push(format!(
                "(Results are truncated: showing first {count} results. Consider using a more specific path or pattern.)"
            ));
        }

        Ok(ToolResult::new(
            Some(lines.join("\n")),
            None,
            Some(json!({"matches": count, "truncated": truncated})),
        ))
    }
}

struct SearchOptions<'a> {
    pattern: &'a str,
    include: Option<&'a str>,
    literal: bool,
    case_sensitive: bool,
    limit: usize,
}

fn search_in(
    root: &Path,
    is_file: bool,
    directory: &Path,
    options: &SearchOptions<'_>,
) -> Result<(Vec<Hit>, bool), SearchError> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!options.case_sensitive)
        .fixed_strings(options.literal)
        .build(options.pattern)
        .map_err(|error| SearchError::InvalidPattern(error.to_string()))?;
    let include = options
        .include
        .map(util::PathGlob::new)
        .transpose()
        .map_err(SearchError::InvalidPattern)?;

    let files = collect_files(root, is_file);

    let mut hits = Vec::new();
    let mut truncated = false;

    for (path, relative) in files {
        if let Some(include) = &include
            && !include.is_match(&relative)
        {
            continue;
        }
        let display = util::relative_display(directory, &path);
        let mut searcher = SearcherBuilder::new().line_number(true).build();
        let sink = UTF8(|line_number, line| {
            let text = line.trim_end_matches(['\n', '\r']);
            hits.push(Hit {
                path: display.clone(),
                line: line_number,
                text: clamp_line(text),
            });
            Ok(hits.len() <= options.limit)
        });
        // Per-file failures (binary files, permissions) are skipped, like rg.
        let _ = searcher.search_path(&matcher, &path, sink);
        if hits.len() > options.limit {
            hits.truncate(options.limit);
            truncated = true;
            break;
        }
    }

    Ok((hits, truncated))
}

fn collect_files(root: &Path, is_file: bool) -> Vec<(PathBuf, String)> {
    if is_file {
        let relative = util::slash(root.file_name().map_or(root, Path::new));
        return vec![(root.to_path_buf(), relative)];
    }
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .hidden(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != std::ffi::OsStr::new(".git"));
    let mut files = Vec::new();
    for entry in builder.build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = util::slash(entry.path().strip_prefix(root).unwrap_or(entry.path()));
        files.push((entry.path().to_path_buf(), relative));
    }
    files.sort_by(|left, right| left.1.cmp(&right.1));
    files
}

fn clamp_line(line: &str) -> String {
    if line.chars().count() > MAX_LINE_CHARS {
        let head: String = line.chars().take(MAX_LINE_CHARS).collect();
        format!("{head}...")
    } else {
        line.to_string()
    }
}
