//! `patch` tool: the `*** Begin Patch` envelope with add/delete/update/move.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

#[derive(Debug, Default, Clone, Copy)]
pub struct PatchTool;

#[derive(Debug, Clone)]
pub struct Chunk {
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub change_context: Option<String>,
    pub end_of_file: bool,
}

#[derive(Debug, Clone)]
pub enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<Chunk>,
    },
}

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

    #[allow(clippy::too_many_lines)] // Single linear patch prepare/apply pipeline.
    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let patch_text = util::required_string(&input, "patchText")?;
        if patch_text.is_empty() {
            return Err(ToolError::new("patchText is required"));
        }
        if patch_text.len() as u64 > crate::read::MAX_FILE_BYTES {
            return Err(ToolError::new(format!(
                "Patch text too large: patch is {} bytes, which exceeds the {} byte limit",
                patch_text.len(),
                crate::read::MAX_FILE_BYTES
            )));
        }
        let hunks = parse(&patch_text)
            .map_err(|error| ToolError::new(format!("patch verification failed: {error}")))?;
        if hunks.is_empty() {
            return Err(ToolError::new("patch rejected: empty patch"));
        }

        let lock_paths: Vec<PathBuf> = patch_targets(&patch_text)
            .into_iter()
            .map(|target| util::resolve(ctx, &target))
            .collect();
        let _file_guards = util::lock_files(&lock_paths).await;

        let mut state: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut prepared: Vec<Prepared> = Vec::new();

        for hunk in hunks {
            match hunk {
                Hunk::Add { path, contents } => {
                    let target = util::resolve(ctx, &path);
                    let resource = util::relative_display(&ctx.directory, &target);
                    let content = if contents.is_empty() || contents.ends_with('\n') {
                        contents
                    } else {
                        format!("{contents}\n")
                    };
                    let (_, after) = util::split_bom(&content);
                    let after = after.to_string();
                    state.insert(target.clone(), Some(content));
                    prepared.push(Prepared::Add {
                        target,
                        resource,
                        after,
                    });
                }
                Hunk::Delete { path } => {
                    let target = util::resolve(ctx, &path);
                    let resource = util::relative_display(&ctx.directory, &target);
                    reject_oversized(&target, &resource)?;
                    let before = match state.get(&target) {
                        Some(None) => {
                            return Err(verification(format!(
                                "Failed to delete {resource}: file does not exist"
                            )));
                        }
                        Some(Some(content)) => util::split_bom(content).1.to_string(),
                        None => {
                            let bytes = fs::read(&target).map_err(|error| {
                                verification(format!(
                                    "Failed to delete {resource}: {}",
                                    util::io_message(&error)
                                ))
                            })?;
                            util::decode_bytes(&bytes).1
                        }
                    };
                    state.insert(target.clone(), None);
                    prepared.push(Prepared::Delete {
                        target,
                        resource,
                        before,
                    });
                }
                Hunk::Update {
                    path,
                    move_path,
                    chunks,
                } => {
                    let target = util::resolve(ctx, &path);
                    let resource = util::relative_display(&ctx.directory, &target);
                    reject_oversized(&target, &resource)?;
                    let original = match state.get(&target) {
                        Some(None) => {
                            return Err(verification(format!(
                                "Failed to read file to update {}: file does not exist",
                                target.display()
                            )));
                        }
                        Some(Some(content)) => content.clone(),
                        None => {
                            let bytes = fs::read(&target).map_err(|error| {
                                verification(format!(
                                    "Failed to read file to update {}: {}",
                                    target.display(),
                                    util::io_message(&error)
                                ))
                            })?;
                            let (bom, text) = util::decode_bytes(&bytes);
                            util::join_bom(&text, bom)
                        }
                    };
                    let (content, bom) = derive(&path, &chunks, &original).map_err(verification)?;
                    let next = util::join_bom(&content, bom);
                    let before = util::split_bom(&original).1.to_string();
                    let after = content;

                    let move_target = move_path
                        .map(|destination| {
                            let absolute = util::resolve(ctx, &destination);
                            (
                                absolute,
                                util::relative_display(
                                    &ctx.directory,
                                    &util::resolve(ctx, &destination),
                                ),
                            )
                        })
                        .filter(|(absolute, _)| absolute != &target);

                    if let Some((absolute, _)) = &move_target {
                        state.insert(target.clone(), None);
                        state.insert(absolute.clone(), Some(next.clone()));
                    } else {
                        state.insert(target.clone(), Some(next.clone()));
                    }

                    prepared.push(Prepared::Update {
                        target,
                        resource,
                        before,
                        after,
                        content: next,
                        move_target,
                    });
                }
            }
        }

        let diffs: Vec<Value> = prepared.iter().map(Prepared::diff).collect();
        let mut applied: Vec<(char, String)> = Vec::new();

        for change in prepared {
            match change {
                Prepared::Add {
                    target,
                    resource,
                    after,
                } => {
                    util::atomic_write(&target, after.as_bytes()).map_err(|error| {
                        failure(format!("Failed to write {resource}: {error}"), &applied)
                    })?;
                    applied.push(('A', resource));
                }
                Prepared::Delete {
                    target, resource, ..
                } => {
                    fs::remove_file(&target).map_err(|error| {
                        failure(format!("Failed to delete {resource}: {error}"), &applied)
                    })?;
                    applied.push(('D', resource));
                }
                Prepared::Update {
                    target,
                    resource,
                    content,
                    move_target,
                    ..
                } => {
                    if let Some((absolute, move_resource)) = move_target {
                        util::atomic_write(&absolute, content.as_bytes()).map_err(|error| {
                            failure(
                                format!("Failed to write {move_resource}: {error}"),
                                &applied,
                            )
                        })?;
                        fs::remove_file(&target).map_err(|error| {
                            failure(
                                format!(
                                    "Wrote {move_resource} but failed to remove {resource}: {error}"
                                ),
                                &applied,
                            )
                        })?;
                        applied.push(('M', move_resource));
                    } else {
                        util::atomic_write(&target, content.as_bytes()).map_err(|error| {
                            failure(format!("Failed to write {resource}: {error}"), &applied)
                        })?;
                        applied.push(('M', resource));
                    }
                }
            }
        }

        let mut output = String::from("Success. Updated the following files:");
        for (kind, resource) in &applied {
            output.push('\n');
            output.push(*kind);
            output.push(' ');
            output.push_str(resource);
        }

        Ok(ToolResult::new(
            Some(output),
            None,
            Some(json!({"files": diffs})),
        ))
    }
}

enum Prepared {
    Add {
        target: PathBuf,
        resource: String,
        after: String,
    },
    Delete {
        target: PathBuf,
        resource: String,
        before: String,
    },
    Update {
        target: PathBuf,
        resource: String,
        before: String,
        after: String,
        content: String,
        move_target: Option<(PathBuf, String)>,
    },
}

impl Prepared {
    fn diff(&self) -> Value {
        match self {
            Self::Add {
                resource, after, ..
            } => crate::diff::file_diff(resource, "", after, "added"),
            Self::Delete {
                resource, before, ..
            } => crate::diff::file_diff(resource, before, "", "deleted"),
            Self::Update {
                resource,
                before,
                after,
                move_target,
                ..
            } => {
                let file = move_target
                    .as_ref()
                    .map_or(resource.as_str(), |(_, resource)| resource.as_str());
                crate::diff::file_diff(file, before, after, "modified")
            }
        }
    }
}

fn reject_oversized(path: &Path, display: &str) -> Result<(), ToolError> {
    let size = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(_) => return Ok(()),
    };
    if size > crate::read::MAX_FILE_BYTES {
        return Err(verification(format!(
            "File too large to patch: {display} is {size} bytes, which exceeds the {} byte limit",
            crate::read::MAX_FILE_BYTES
        )));
    }
    Ok(())
}

fn verification(message: impl Into<String>) -> ToolError {
    ToolError::new(format!("patch verification failed: {}", message.into()))
}

fn failure(operation: String, applied: &[(char, String)]) -> ToolError {
    if applied.is_empty() {
        return ToolError::new(operation);
    }
    let completed = applied
        .iter()
        .map(|(_, resource)| resource.clone())
        .collect::<Vec<_>>()
        .join(", ");
    ToolError::new(format!(
        "{operation}. Completed before failure: {completed}"
    ))
}

fn strip_heredoc(input: &str) -> String {
    let trimmed = input.trim();
    let Some(newline) = trimmed.find('\n') else {
        return trimmed.to_string();
    };
    let first = trimmed[..newline].trim();
    let token = first.strip_prefix("cat").map_or(first, str::trim);
    let Some(rest) = token.strip_prefix("<<") else {
        return trimmed.to_string();
    };
    let delimiter = rest.trim_matches(|c| c == '\'' || c == '"');
    if delimiter.is_empty() {
        return trimmed.to_string();
    }
    let body = &trimmed[newline + 1..];
    let Some(last_newline) = body.rfind('\n') else {
        return trimmed.to_string();
    };
    if body[last_newline + 1..].trim() != delimiter {
        return trimmed.to_string();
    }
    body[..last_newline].trim().to_string()
}

pub fn parse(patch_text: &str) -> Result<Vec<Hunk>, String> {
    let text = strip_heredoc(patch_text);
    let lines: Vec<String> = text.trim().lines().map(str::to_string).collect();
    let begin = lines
        .first()
        .is_some_and(|line| line.trim() == "*** Begin Patch");
    let end = lines
        .last()
        .is_some_and(|line| line.trim() == "*** End Patch");
    if !begin {
        return Err("The first line of the patch must be '*** Begin Patch'".to_string());
    }
    if !end || lines.len() < 2 {
        return Err("The last line of the patch must be '*** End Patch'".to_string());
    }
    let last = lines.len() - 1;

    let mut hunks = Vec::new();
    let mut index = 1;
    while index < last {
        let line = &lines[index];
        let header = line.trim();
        if index == 1
            && header.starts_with("*** Environment ID:")
            && !header["*** Environment ID:".len()..].trim().is_empty()
        {
            index += 1;
            continue;
        }
        if let Some(path) = header.strip_prefix("*** Add File: ") {
            let path = path.trim().to_string();
            let (contents, next) = parse_add(&lines, index + 1, last, &path)?;
            hunks.push(Hunk::Add { path, contents });
            index = next;
            continue;
        }
        if let Some(path) = header.strip_prefix("*** Delete File: ") {
            let path = path.trim().to_string();
            if index + 1 < last {
                let next = lines[index + 1].trim();
                if !is_boundary(next) {
                    if next.starts_with("*** ") {
                        return Err(invalid_hunk(next, index + 2, None));
                    }
                    return Err(invalid_hunk(
                        next,
                        index + 2,
                        Some(format!(
                            "Unexpected line after Delete File '{path}': '{next}'. Delete hunks do not contain body lines"
                        )),
                    ));
                }
            }
            hunks.push(Hunk::Delete { path });
            index += 1;
            continue;
        }
        if let Some(path) = header.strip_prefix("*** Update File: ") {
            let path = path.trim().to_string();
            let mut next = index + 1;
            while next < last && lines[next].trim_end() == "*** End of File" {
                next += 1;
            }
            let mut move_path = None;
            if let Some(candidate) = lines.get(next) {
                let trimmed = candidate.trim_end();
                if trimmed == "*** Move to:" || trimmed.starts_with("*** Move to: ") {
                    let destination = trimmed
                        .strip_prefix("*** Move to:")
                        .map_or("", str::trim)
                        .to_string();
                    if destination.is_empty() {
                        return Err(invalid_hunk(
                            lines[next].trim(),
                            next + 1,
                            Some(format!("Move destination for '{path}' must not be empty")),
                        ));
                    }
                    move_path = Some(destination);
                    next += 1;
                }
            }
            let (chunks, next) = parse_update(&lines, next, last, &path, index)?;
            hunks.push(Hunk::Update {
                path,
                move_path,
                chunks,
            });
            index = next;
            continue;
        }
        return Err(invalid_hunk(header, index + 1, None));
    }
    Ok(hunks)
}

/// Every file path a patch touches, in document order and deduplicated.
///
/// Both sides of an update-and-move are included (`*** Update File:` then
/// `*** Move to:`). This is an authorization helper: it reads only the patch
/// text and never touches the filesystem, and it tolerates malformed hunks by
/// extracting whatever headers are present.
#[must_use]
pub fn patch_targets(patch_text: &str) -> Vec<String> {
    let text = strip_heredoc(patch_text);
    let lines: Vec<&str> = text.lines().collect();
    let mut targets: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut index = 0;
    while index < lines.len() {
        let header = lines[index].trim();
        if let Some(path) = header.strip_prefix("*** Add File: ") {
            push_target(&mut targets, &mut seen, path);
        } else if let Some(path) = header.strip_prefix("*** Delete File: ") {
            push_target(&mut targets, &mut seen, path);
        } else if let Some(path) = header.strip_prefix("*** Update File: ") {
            push_target(&mut targets, &mut seen, path);
            let mut next = index + 1;
            while next < lines.len() && lines[next].trim_end() == "*** End of File" {
                next += 1;
            }
            if let Some(candidate) = lines.get(next) {
                let candidate = candidate.trim_end();
                if let Some(destination) = candidate.strip_prefix("*** Move to:")
                    && (destination.is_empty() || destination.starts_with(' '))
                {
                    push_target(&mut targets, &mut seen, destination);
                    index = next;
                }
            }
        }
        index += 1;
    }
    targets
}

fn push_target(targets: &mut Vec<String>, seen: &mut HashSet<String>, path: &str) {
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    if seen.insert(path.to_string()) {
        targets.push(path.to_string());
    }
}

fn invalid_hunk(line: &str, line_number: usize, reason: Option<String>) -> String {
    match reason {
        Some(reason) => format!("Invalid hunk at line {line_number}: {reason}"),
        None => format!(
            "Invalid hunk at line {line_number}: '{line}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
        ),
    }
}

fn is_boundary(line: &str) -> bool {
    line == "*** End Patch"
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}

fn parse_add(
    lines: &[String],
    start: usize,
    end: usize,
    path: &str,
) -> Result<(String, usize), String> {
    let mut contents = Vec::new();
    let mut index = start;
    while index < end && !is_boundary(lines[index].trim()) {
        if !lines[index].starts_with('+') {
            let line = lines[index].trim();
            return Err(invalid_hunk(
                line,
                index + 1,
                Some(format!(
                    "Invalid Add File line for '{path}': expected a line starting with '+', got '{line}'"
                )),
            ));
        }
        contents.push(lines[index][1..].to_string());
        index += 1;
    }
    Ok((contents.join("\n"), index))
}

#[allow(clippy::too_many_lines)] // One hunk parser, kept in one place for grammar fidelity.
fn parse_update(
    lines: &[String],
    start: usize,
    end: usize,
    path: &str,
    hunk: usize,
) -> Result<(Vec<Chunk>, usize), String> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut index = start;
    let mut after_end_of_file = false;

    while index < end {
        let line = &lines[index];
        let update_line = line.trim_end();

        if after_end_of_file {
            if update_line.is_empty() {
                index += 1;
                continue;
            }
            if update_line == "@@" || update_line.starts_with("@@ ") {
                after_end_of_file = false;
            } else if is_boundary(update_line) {
                break;
            } else {
                return Err(invalid_hunk(
                    line,
                    index + 1,
                    Some(format!(
                        "Expected update hunk to start with a @@ context marker, got: '{line}'"
                    )),
                ));
            }
        }

        if update_line == "*** End of File" {
            if let Some(chunk) = chunks.last_mut() {
                if chunk.old_lines.is_empty() && chunk.new_lines.is_empty() {
                    return Err(invalid_hunk(
                        update_line,
                        index + 1,
                        Some("Update hunk does not contain any lines".to_string()),
                    ));
                }
                chunk.end_of_file = true;
            }
            after_end_of_file = true;
            index += 1;
            continue;
        }
        if is_boundary(update_line) {
            break;
        }
        if update_line == "@@" || update_line.starts_with("@@ ") {
            if let Some(previous) = chunks.last()
                && previous.old_lines.is_empty()
                && previous.new_lines.is_empty()
            {
                return Err(invalid_hunk(
                    line,
                    index + 1,
                    Some(format!(
                        "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                    )),
                ));
            }
            chunks.push(Chunk {
                old_lines: Vec::new(),
                new_lines: Vec::new(),
                change_context: if update_line == "@@" {
                    None
                } else {
                    Some(update_line["@@ ".len()..].to_string())
                },
                end_of_file: false,
            });
            index += 1;
            continue;
        }
        if chunks.is_empty() {
            chunks.push(Chunk {
                old_lines: Vec::new(),
                new_lines: Vec::new(),
                change_context: None,
                end_of_file: false,
            });
        }
        let chunk = chunks.last_mut().expect("chunk exists");
        if line.is_empty() {
            chunk.old_lines.push(String::new());
            chunk.new_lines.push(String::new());
            index += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix(' ') {
            chunk.old_lines.push(rest.to_string());
            chunk.new_lines.push(rest.to_string());
            index += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix('-') {
            chunk.old_lines.push(rest.to_string());
            index += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            chunk.new_lines.push(rest.to_string());
            index += 1;
            continue;
        }
        let populated = !chunk.old_lines.is_empty() || !chunk.new_lines.is_empty();
        let reason = if populated {
            format!("Expected update hunk to start with a @@ context marker, got: '{line}'")
        } else {
            format!(
                "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
            )
        };
        return Err(invalid_hunk(line, index + 1, Some(reason)));
    }

    if chunks.is_empty() {
        let line = lines[hunk].trim();
        return Err(invalid_hunk(
            line,
            hunk + 1,
            Some(format!("Update file hunk for path '{path}' is empty")),
        ));
    }
    let last = chunks.last().expect("chunk exists");
    if last.old_lines.is_empty() && last.new_lines.is_empty() {
        let line = lines.get(index).map_or("", |line| line.trim());
        let reason = if line == "*** End Patch" {
            "Update hunk does not contain any lines".to_string()
        } else {
            format!(
                "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
            )
        };
        return Err(invalid_hunk(line, index + 1, Some(reason)));
    }
    Ok((chunks, index))
}

/// Apply `chunks` to `original`, returning BOM-free content and the resulting
/// BOM flag.
pub fn derive(path: &str, chunks: &[Chunk], original: &str) -> Result<(String, bool), String> {
    let (source_bom, source) = util::split_bom(original);
    let mut lines: Vec<String> = source.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let replacements = compute_replacements(&lines, path, chunks)?;
    for (start, remove, insert) in replacements.into_iter().rev() {
        lines.splice(start..start + remove, insert);
    }
    if !lines.last().is_some_and(String::is_empty) {
        lines.push(String::new());
    }
    let joined = lines.join("\n");
    let (next_bom, next) = util::split_bom(&joined);
    Ok((next.to_string(), source_bom || next_bom))
}

fn compute_replacements(
    lines: &[String],
    path: &str,
    chunks: &[Chunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, String> {
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index = 0usize;

    for chunk in chunks {
        if let Some(context) = &chunk.change_context {
            let found = seek(lines, std::slice::from_ref(context), line_index, false)
                .ok_or_else(|| format!("Failed to find context '{context}' in {path}"))?;
            line_index = found + 1;
        }
        if chunk.old_lines.is_empty() {
            replacements.push((lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }

        let mut old: &[String] = &chunk.old_lines;
        let mut new: &[String] = &chunk.new_lines;
        let mut found = seek(lines, old, line_index, chunk.end_of_file);
        if found.is_none() && old.last().is_some_and(String::is_empty) {
            old = &old[..old.len() - 1];
            if new.last().is_some_and(String::is_empty) {
                new = &new[..new.len() - 1];
            }
            found = seek(lines, old, line_index, chunk.end_of_file);
        }
        let Some(found) = found else {
            if chunk.old_lines.iter().all(String::is_empty) {
                let expected = if chunk.old_lines.len() == 1 {
                    "an expected blank line".to_string()
                } else {
                    format!("{} consecutive blank lines", chunk.old_lines.len())
                };
                return Err(format!("Failed to find {expected} in {path}"));
            }
            return Err(format!(
                "Failed to find expected lines in {path}:\n{}",
                chunk.old_lines.join("\n")
            ));
        };
        replacements.push((found, old.len(), new.to_vec()));
        line_index = found + old.len();
    }

    replacements.sort_by_key(|replacement| replacement.0);
    Ok(replacements)
}

fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }
    let comparators: [fn(&str, &str) -> bool; 4] = [exact, rstrip, trim, normalized];

    if eof {
        if pattern.len() > lines.len() {
            return None;
        }
        let offset = lines.len() - pattern.len();
        if offset < start {
            return None;
        }
        for compare in comparators {
            if matches_at(lines, pattern, offset, compare) {
                return Some(offset);
            }
        }
        return None;
    }

    if pattern.len() > lines.len() {
        return None;
    }
    for compare in comparators {
        for offset in start..=lines.len() - pattern.len() {
            if matches_at(lines, pattern, offset, compare) {
                return Some(offset);
            }
        }
    }
    None
}

fn matches_at(
    lines: &[String],
    pattern: &[String],
    offset: usize,
    compare: fn(&str, &str) -> bool,
) -> bool {
    pattern
        .iter()
        .enumerate()
        .all(|(index, line)| compare(lines[offset + index].as_str(), line.as_str()))
}

fn exact(left: &str, right: &str) -> bool {
    left == right
}

fn rstrip(left: &str, right: &str) -> bool {
    left.trim_end() == right.trim_end()
}

fn trim(left: &str, right: &str) -> bool {
    left.trim() == right.trim()
}

fn normalized(left: &str, right: &str) -> bool {
    util::normalize_text(left.trim()) == util::normalize_text(right.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_targets_extracts_every_hunk_kind() {
        let patch = "*** Begin Patch\n\
                     *** Add File: added.txt\n\
                     +hello\n\
                     *** Delete File: removed.txt\n\
                     *** Update File: updated.txt\n\
                     @@\n\
                     -old\n\
                     +new\n\
                     *** End Patch\n";
        assert_eq!(
            patch_targets(patch),
            vec!["added.txt", "removed.txt", "updated.txt"]
        );
    }

    #[test]
    fn patch_targets_includes_both_sides_of_a_move() {
        let patch = "*** Begin Patch\n\
                     *** Update File: old/name.txt\n\
                     *** Move to: new/name.txt\n\
                     @@\n\
                     -a\n\
                     +b\n\
                     *** Update File: later.txt\n\
                     @@\n\
                     -c\n\
                     +d\n\
                     *** End Patch\n";
        assert_eq!(
            patch_targets(patch),
            vec!["old/name.txt", "new/name.txt", "later.txt"]
        );
    }

    #[test]
    fn patch_targets_dedupes_and_preserves_document_order() {
        let patch = "*** Begin Patch\n\
                     *** Update File: a.txt\n\
                     @@\n\
                     -1\n\
                     +2\n\
                     *** Update File: b.txt\n\
                     @@\n\
                     -1\n\
                     +2\n\
                     *** Update File: a.txt\n\
                     @@\n\
                     -2\n\
                     +3\n\
                     *** Delete File: b.txt\n\
                     *** End Patch\n";
        assert_eq!(patch_targets(patch), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn patch_targets_handles_environment_header_and_heredoc() {
        let patch = "cat <<'EOF'\n*** Begin Patch\n*** Environment ID: env-1\n*** Add File: only.txt\n+x\n*** End Patch\nEOF";
        assert_eq!(patch_targets(patch), vec!["only.txt"]);
    }

    #[test]
    fn patch_targets_skips_empty_and_space_less_move_headers() {
        let patch = "*** Begin Patch\n*** Update File: \n*** Move to:\n*** End Patch\n";
        assert!(patch_targets(patch).is_empty());

        let no_space =
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to:b.txt\n*** End Patch\n";
        assert_eq!(patch_targets(no_space), vec!["a.txt"]);
    }
}
