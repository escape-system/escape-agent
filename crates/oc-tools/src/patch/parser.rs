//! `*** Begin Patch` envelope: header dispatch and the Add/Update hunk grammar.

use super::hunks::{Chunk, Hunk};

pub(super) fn strip_heredoc(input: &str) -> String {
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
