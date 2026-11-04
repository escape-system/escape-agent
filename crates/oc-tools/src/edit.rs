//! `edit` tool: exact replacement with fallback matching tiers.
//!
//! Tiers, in order: raw bytes, LF-normalized text, Unicode-normalized text
//! (typographic quotes/dashes/spaces), and line-based `trimEnd` matching.
//! Offsets are remapped to the source and replacements reuse the matched
//! region's line endings.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::read::MAX_FILE_BYTES;
use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

#[derive(Debug, Default, Clone, Copy)]
pub struct EditTool;

/// Refuse to load a target larger than [`MAX_FILE_BYTES`]: reading it whole
/// would blow the memory budget, and editing a prefix is never correct.
fn reject_oversized(path: &std::path::Path, display: &str) -> Result<(), ToolError> {
    let size = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        // Let `read_file` produce the canonical not-found/permission message.
        Err(_) => return Ok(()),
    };
    if size > MAX_FILE_BYTES {
        return Err(ToolError::new(format!(
            "File too large to edit: {display} is {size} bytes, which exceeds the {MAX_FILE_BYTES} byte edit limit; use grep and a bounded rewrite instead"
        ))
        .with_metadata(json!({"code": "file_too_large", "size": size, "limit": MAX_FILE_BYTES})));
    }
    Ok(())
}

const UNAMBIGUOUS_HINT: &str = "Add more surrounding context to make oldString unique, or set replaceAll to true to replace every occurrence.";

#[derive(Debug, Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit the contents of a file by finding and replacing exact text. When editing text from Read output, preserve the exact indentation (tabs or spaces) and omit the line-number prefix, such as `1: `. Never include the prefix in oldString or newString. Write line breaks as \\n; the file's existing CRLF or LF line endings are matched and preserved. If there is no exact match, the edit retries while ignoring typographic quote, dash, and space variants, then trailing whitespace on each line. The edit fails if oldString is not found. By default, oldString must identify a UNIQUE location. Multiple matches FAIL unless replaceAll is true."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to edit"},
                "oldString": {"type": "string", "description": "Exact text to find and replace"},
                "newString": {"type": "string", "description": "Text to replace oldString with (must differ from oldString)"},
                "replaceAll": {"type": "boolean", "description": "Whether to replace every occurrence of oldString. When false, oldString must match exactly once. Defaults to false."}
            },
            "required": ["path", "oldString", "newString"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let path = util::required_string(&input, "path")?;
        let old_string = util::required_string(&input, "oldString")?;
        let new_string = util::required_string(&input, "newString")?;
        let replace_all = util::optional_bool(&input, "replaceAll")?.unwrap_or(false);

        if old_string == new_string {
            return Err(ToolError::new(
                "No changes to apply: oldString and newString are identical.",
            ));
        }
        if old_string.is_empty() {
            return Err(ToolError::new(
                "oldString must not be empty. Use write to create or overwrite a file.",
            ));
        }

        let absolute = util::resolve(ctx, &path);
        reject_oversized(&absolute, &path)?;
        let bytes = util::read_file(&absolute, &path)?;
        let (source_bom, source) = util::decode_bytes(&bytes);

        // Strings that differ only in \r change line endings themselves and
        // therefore apply to the exact bytes instead of the LF view.
        let endings_only = old_string.replace('\r', "") == new_string.replace('\r', "");

        let (spans, replacement) = if endings_only {
            (find_occurrences(&source, &old_string), new_string.clone())
        } else {
            let normalized_new = new_string.replace("\r\n", "\n");
            let (view, map) = lf_view(&source);
            let search = old_string.replace("\r\n", "\n");

            let mut spans = find_occurrences(&source, &old_string);
            if spans.is_empty() {
                spans = find_occurrences(&view, &search)
                    .into_iter()
                    .map(|span| to_source(span, &map))
                    .collect();
            }
            if spans.is_empty() {
                spans = find_unicode(&view, &search)
                    .into_iter()
                    .map(|span| to_source(span, &map))
                    .collect();
            }
            if spans.is_empty() {
                spans = find_line_occurrences(&view, &search)
                    .into_iter()
                    .map(|span| to_source(span, &map))
                    .collect();
            }
            (spans, normalized_new)
        };

        let matches = spans.len();
        if matches == 0 {
            return Err(ToolError::new(format!(
                "Could not find oldString in {path}. It must match exactly, including whitespace and indentation."
            )));
        }
        if matches > 1 && !replace_all {
            return Err(ToolError::new(format!(
                "Found {matches} matches for oldString, but expected exactly one. {UNAMBIGUOUS_HINT}"
            )));
        }

        let file_ending = util::line_ending(&source, "\n");
        let selected: Vec<Span> = if replace_all { spans } else { vec![spans[0]] };

        let mut result = source.clone();
        for span in selected.iter().rev() {
            let region = &source[span.start..span.end];
            let text = if endings_only {
                replacement.clone()
            } else {
                let fallback = util::line_ending(region, &file_ending);
                util::with_region_endings(region, &replacement, &fallback)
            };
            result.replace_range(span.start..span.end, &text);
        }

        if result == source {
            return Err(ToolError::new(if endings_only {
                format!("No changes to apply: {path} already has those line endings.")
            } else {
                format!(
                    "No changes to apply: the edit leaves {path} unchanged. Line endings follow the file; to change them, write \\r explicitly in oldString or newString."
                )
            }));
        }

        let replacement_bom = result.starts_with(util::BOM);
        let (_, result_text) = util::split_bom(&result);
        let bom = source_bom || replacement_bom;
        let output_bytes = util::join_bom(result_text, bom).into_bytes();
        util::atomic_write(&absolute, &output_bytes)
            .map_err(|error| ToolError::new(format!("Unable to edit {path}: {error}")))?;

        let diff = crate::diff::file_diff(&path, &source, result_text, "modified");
        let content = format!(
            "Edited {path} ({matches} replacement{})",
            if matches == 1 { "" } else { "s" }
        );
        Ok(ToolResult::new(
            Some(content),
            None,
            Some(json!({"files": [diff]})),
        ))
    }
}

fn find_occurrences(haystack: &str, needle: &str) -> Vec<Span> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut offset = 0;
    while offset + needle.len() <= haystack.len() {
        match haystack[offset..].find(needle) {
            Some(found) => {
                let start = offset + found;
                let end = start + needle.len();
                spans.push(Span { start, end });
                offset = end;
            }
            None => break,
        }
    }
    spans
}

/// LF view of `source` plus a map from view byte offsets back to source byte
/// offsets (CRLF collapses to LF).
fn lf_view(source: &str) -> (String, Vec<usize>) {
    let bytes = source.as_bytes();
    let mut view = String::with_capacity(source.len());
    let mut map = Vec::with_capacity(source.len() + 1);
    let mut index = 0;
    while index < bytes.len() {
        map.push(index);
        if bytes[index] == b'\r' && index + 1 < bytes.len() && bytes[index + 1] == b'\n' {
            view.push('\n');
            index += 2;
            continue;
        }
        let character = source[index..].chars().next().expect("valid char boundary");
        let length = character.len_utf8();
        for _ in 1..length {
            map.push(index);
        }
        view.push(character);
        index += length;
    }
    map.push(source.len());
    (view, map)
}

fn to_source(span: Span, map: &[usize]) -> Span {
    Span {
        start: map[span.start],
        end: map[span.end],
    }
}

fn find_unicode(view: &str, needle: &str) -> Vec<Span> {
    let needle: Vec<char> = needle.chars().map(util::normalize_char).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = view.chars().collect();
    let normalized: Vec<char> = chars.iter().copied().map(util::normalize_char).collect();
    let mut offsets: Vec<usize> = view.char_indices().map(|(offset, _)| offset).collect();
    offsets.push(view.len());

    let mut spans = Vec::new();
    let length = needle.len();
    let mut index = 0;
    while index + length <= normalized.len() {
        if normalized[index..index + length] == needle[..] {
            spans.push(Span {
                start: offsets[index],
                end: offsets[index + length],
            });
            index += length;
        } else {
            index += 1;
        }
    }
    spans
}

#[derive(Debug, Clone, Copy)]
struct Line<'a> {
    start: usize,
    end: usize,
    content_end: usize,
    text: &'a str,
    newline: bool,
}

fn split_view_lines(view: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = view.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(Line {
                start,
                end: index + 1,
                content_end: index,
                text: &view[start..index],
                newline: true,
            });
            start = index + 1;
        }
    }
    if start < view.len() {
        lines.push(Line {
            start,
            end: view.len(),
            content_end: view.len(),
            text: &view[start..],
            newline: false,
        });
    }
    lines
}

fn find_line_occurrences(view: &str, search: &str) -> Vec<Span> {
    let trailing_newline = search.ends_with('\n');
    let mut expected: Vec<&str> = search.split('\n').collect();
    if trailing_newline {
        expected.pop();
    }
    let lines = split_view_lines(view);
    let mut spans: Vec<Span> = Vec::new();

    for (index, _) in lines.iter().enumerate() {
        if index + expected.len() > lines.len() {
            break;
        }
        let actual = &lines[index..index + expected.len()];
        let matches = actual.iter().zip(expected.iter()).all(|(line, wanted)| {
            util::normalize_text(line.text.trim_end()) == util::normalize_text(wanted.trim_end())
        });
        if !matches {
            continue;
        }
        let last = actual.last().expect("non-empty expected");
        if trailing_newline && !last.newline {
            continue;
        }
        let span = Span {
            start: lines[index].start,
            end: if trailing_newline {
                last.end
            } else {
                last.content_end
            },
        };
        if spans
            .iter()
            .any(|existing| existing.end > span.start && existing.start < span.end)
        {
            continue;
        }
        spans.push(span);
    }
    spans
}
