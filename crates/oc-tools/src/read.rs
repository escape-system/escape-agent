//! `read` tool: numbered text pages, directory listings and media.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};

use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

pub const MAX_READ_LINES: usize = 2_000;
pub const MAX_READ_BYTES: usize = 50 * 1024;
pub const MAX_MEDIA_BYTES: u64 = 20 * 1024 * 1024;
/// Hard ceiling for a whole-file read. Larger files must be searched with
/// `grep` (or read in bounded sections by another tool).
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_LINE_LENGTH: usize = 2_000;
/// Bytes inspected to decide whether a file is image/PDF media.
const MEDIA_SNIFF_BYTES: usize = 16;

#[derive(Debug, Default, Clone, Copy)]
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file or directory. Supports text files, images, and PDFs. Images and PDFs are presented directly to the model. Each text line is prefixed by its 1-based line number as <line>: <content>. The prefix is for reference and is not part of the file content. Directory entries are returned one per line. Use offset and limit to read large files or directories in sections. Prefer one larger read over many small slices, and use grep to find specific content in large files."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File or directory to read"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "The line or directory entry to start reading from (1-based)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "The maximum number of lines or directory entries to read (defaults to and capped at 2000)"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    #[allow(clippy::too_many_lines)] // Read dispatch plus formatting stays in one place.
    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let path = util::required_string(&input, "path")?;
        let offset = util::optional_u64(&input, "offset")?;
        let limit = util::optional_u64(&input, "limit")?;
        let absolute = util::resolve(ctx, &path);

        let metadata = match fs::metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::new(format!("File not found: {path}")));
            }
            Err(error) => {
                return Err(ToolError::new(format!("Unable to read {path}: {error}")));
            }
        };

        if metadata.is_dir() {
            return list_directory(&path, &absolute, offset, limit);
        }
        if !metadata.is_file() {
            return Err(non_regular(&path));
        }

        let mut file = File::open(&absolute)
            .map_err(|error| ToolError::new(format!("Unable to read {path}: {error}")))?;
        // Re-check the opened handle: the path may have changed after stat, and
        // special files (devices, FIFOs, sockets) are never read.
        let metadata = file
            .metadata()
            .map_err(|error| ToolError::new(format!("Unable to read {path}: {error}")))?;
        if !metadata.is_file() {
            return Err(non_regular(&path));
        }

        let bytes = read_prefix(&mut file, &path)?;
        let media = media_mime(&bytes);
        let byte_limit = if media.is_some() {
            MAX_MEDIA_BYTES
        } else {
            MAX_FILE_BYTES
        };
        if metadata.len() > byte_limit {
            return Err(too_large(&path, metadata.len(), byte_limit));
        }
        // `take` bounds the read even when the reported size is wrong or the
        // file is growing (for example a procfs entry).
        let bytes = read_bounded(file, bytes, byte_limit)
            .map_err(|error| ToolError::new(format!("Unable to read {path}: {error}")))?;
        let observed = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if observed > byte_limit {
            return Err(too_large(&path, observed, byte_limit));
        }

        if let Some(mime) = media {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let uri = format!("data:{mime};base64,{encoded}");
            let label = if mime == "application/pdf" {
                "PDF read successfully"
            } else {
                "Image read successfully"
            };
            return Ok(ToolResult::new(
                Some(label.to_string()),
                Some(json!([
                    {"type": "text", "text": label},
                    {"type": "file", "uri": uri, "mime": mime}
                ])),
                Some(json!({"truncated": false})),
            ));
        }

        let text = String::from_utf8_lossy(&bytes);
        let lines = split_lines(&text);

        let paged = offset.is_some() || limit.is_some() || bytes.len() > MAX_READ_BYTES;
        if !paged {
            if bytes.contains(&0) {
                return Err(ToolError::new(format!("Cannot read binary file: {path}")));
            }
            let mut output = String::new();
            if lines.is_empty() {
                let _ = write!(output, "Read file {path}, 0 lines");
            } else {
                let _ = write!(output, "Read file {path}, lines 1-{}", lines.len());
                for (index, line) in lines.iter().enumerate() {
                    let _ = write!(output, "\n{}: {}", index + 1, clamp_line(line));
                }
            }
            return Ok(ToolResult::new(
                Some(output),
                None,
                Some(json!({"truncated": false})),
            ));
        }

        let start = util::to_usize(offset.unwrap_or(1).max(1));
        let requested = limit
            .map_or(MAX_READ_LINES, |value| {
                if value == 0 {
                    MAX_READ_LINES
                } else {
                    util::to_usize(value)
                }
            })
            .min(MAX_READ_LINES);

        let mut selected: Vec<String> = Vec::new();
        let mut size = 0usize;
        let mut next: Option<usize> = None;

        for (index, line) in lines.iter().enumerate().skip(start - 1) {
            let number = index + 1;
            if selected.len() >= requested || size >= MAX_READ_BYTES {
                next = Some(number);
                break;
            }
            if line.contains('\0') {
                return Err(ToolError::new(format!("Cannot read binary file: {path}")));
            }
            let clamped = clamp_line(line);
            let line_size = clamped.len() + usize::from(!selected.is_empty());
            if size + line_size > MAX_READ_BYTES {
                next = Some(number);
                break;
            }
            selected.push(clamped);
            size += line_size;
        }

        if selected.is_empty() && start != 1 {
            let total = lines.len();
            return Err(ToolError::new(format!(
                "Offset {start} is out of range for this file ({total} {})",
                if total == 1 { "line" } else { "lines" }
            )));
        }

        let mut output = String::new();
        if selected.is_empty() {
            let _ = write!(output, "Read file {path}, 0 lines");
        } else {
            let _ = write!(
                output,
                "Read file {path}, lines {start}-{}",
                start + selected.len() - 1
            );
            for (index, line) in selected.iter().enumerate() {
                let _ = write!(output, "\n{}: {}", start + index, line);
            }
        }
        if let Some(next) = next {
            let _ = write!(
                output,
                "\n[Output truncated. Continue reading with offset: {next}]"
            );
        }

        Ok(ToolResult::new(
            Some(output),
            None,
            Some(json!({"truncated": next.is_some()})),
        ))
    }
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn clamp_line(line: &str) -> String {
    if line.chars().count() > MAX_LINE_LENGTH {
        let head: String = line.chars().take(MAX_LINE_LENGTH).collect();
        format!("{head}... (line truncated to {MAX_LINE_LENGTH} chars)")
    } else {
        line.to_string()
    }
}

fn media_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else {
        None
    }
}

fn non_regular(display: &str) -> ToolError {
    ToolError::new(format!(
        "Cannot read non-regular file (device, FIFO, socket or directory): {display}"
    ))
}

/// Error surfaced when a file exceeds the bounded read limit (a tool-level
/// equivalent of HTTP 413).
fn too_large(display: &str, size: u64, limit: u64) -> ToolError {
    ToolError::new(format!(
        "File too large to read: {display} is {size} bytes, which exceeds the {limit} byte read limit; use grep to search it"
    ))
    .with_metadata(json!({"code": "file_too_large", "size": size, "limit": limit}))
}

/// Read up to [`MEDIA_SNIFF_BYTES`] bytes to classify media before committing to
/// the (larger) media or text limit.
fn read_prefix(file: &mut File, display: &str) -> Result<Vec<u8>, ToolError> {
    let mut prefix = vec![0_u8; MEDIA_SNIFF_BYTES];
    let mut filled = 0_usize;
    while filled < prefix.len() {
        match file.read(&mut prefix[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(ToolError::new(format!("Unable to read {display}: {error}")));
            }
        }
    }
    prefix.truncate(filled);
    Ok(prefix)
}

/// Append from `reader` to `bytes` while keeping at most one byte past
/// `limit`, so callers can detect overflow without unbounded allocation.
fn read_bounded<R: Read>(reader: R, mut bytes: Vec<u8>, limit: u64) -> io::Result<Vec<u8>> {
    let remaining = limit.saturating_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    let mut bounded = reader.take(remaining.saturating_add(1));
    bounded.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn list_directory(
    display: &str,
    directory: &Path,
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<ToolResult, ToolError> {
    let mut entries: Vec<(String, bool)> = Vec::new();
    let read = fs::read_dir(directory)
        .map_err(|error| ToolError::new(format!("Unable to read {display}: {error}")))?;
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = fs::metadata(entry.path());
        let is_dir = metadata.as_ref().is_ok_and(fs::Metadata::is_dir);
        let is_file = metadata.as_ref().is_ok_and(fs::Metadata::is_file);
        let is_symlink = entry.file_type().is_ok_and(|kind| kind.is_symlink());
        if !is_dir && !is_file && !is_symlink {
            continue;
        }
        entries.push((if is_dir { format!("{name}/") } else { name }, is_dir));
    }
    entries.sort_by(|left, right| {
        if left.1 == right.1 {
            left.0.cmp(&right.0)
        } else if left.1 {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    });

    let start = util::to_usize(offset.unwrap_or(1).max(1));
    let requested = limit
        .map_or(MAX_READ_LINES, |value| {
            if value == 0 {
                MAX_READ_LINES
            } else {
                util::to_usize(value)
            }
        })
        .min(MAX_READ_LINES);
    let selected: Vec<&(String, bool)> = entries.iter().skip(start - 1).take(requested).collect();
    let truncated = start - 1 + selected.len() < entries.len();

    let mut output = String::new();
    if selected.is_empty() {
        let _ = write!(output, "Read directory {display}, 0 entries");
    } else {
        let _ = write!(
            output,
            "Read directory {display}, entries {start}-{}",
            start + selected.len() - 1
        );
        for (name, _) in &selected {
            output.push('\n');
            output.push_str(name);
        }
    }
    if truncated {
        let _ = write!(
            output,
            "\n[Output truncated. Continue reading with offset: {}]",
            start + selected.len()
        );
    }

    Ok(ToolResult::new(
        Some(output),
        None,
        Some(json!({"truncated": truncated})),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::TempDir;

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            session_id: "ses_read_limits".to_string(),
            directory: dir.to_path_buf(),
            cwd: dir.to_path_buf(),
            cancel: None,
        }
    }

    #[test]
    fn bounded_tail_read_stops_one_past_limit() {
        let bytes = read_bounded(io::repeat(b'x'), b"prefix".to_vec(), 8).expect("bounded read");
        assert_eq!(bytes.len(), 9, "reads at most limit + 1 bytes in total");
        assert!(u64::try_from(bytes.len()).unwrap_or(0) > 8);

        let short = read_bounded(io::repeat(b'x'), Vec::new(), 0).expect("bounded read");
        assert_eq!(short.len(), 1);
    }

    #[test]
    fn media_detection_uses_prefix_only() {
        assert_eq!(media_mime(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(media_mime(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(media_mime(b"GIF89a"), Some("image/gif"));
        assert_eq!(media_mime(b"plain text"), None);
    }

    #[tokio::test]
    async fn rejects_text_files_over_the_read_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("huge.txt");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        drop(file);

        let error = ReadTool
            .execute(&ctx(dir.path()), json!({"path": "huge.txt"}))
            .await
            .unwrap_err();
        assert!(error.message.contains("too large"), "{}", error.message);
        assert_eq!(
            error.metadata.as_ref().expect("metadata")["code"],
            "file_too_large"
        );
        assert_eq!(
            error.metadata.as_ref().expect("metadata")["limit"],
            MAX_FILE_BYTES
        );
    }

    #[tokio::test]
    async fn rejects_media_over_the_media_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("huge.png");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
        file.set_len(MAX_MEDIA_BYTES + 1).unwrap();
        drop(file);

        let error = ReadTool
            .execute(&ctx(dir.path()), json!({"path": "huge.png"}))
            .await
            .unwrap_err();
        assert!(error.message.contains("too large"), "{}", error.message);
        assert_eq!(
            error.metadata.as_ref().expect("metadata")["limit"],
            MAX_MEDIA_BYTES
        );
    }

    #[tokio::test]
    async fn small_files_still_read() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("small.txt"), "a\nb\n").unwrap();
        let output = ReadTool
            .execute(&ctx(dir.path()), json!({"path": "small.txt"}))
            .await
            .unwrap()
            .output
            .unwrap();
        assert!(output.contains("1: a"), "{output}");
        assert!(output.contains("2: b"), "{output}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_device_nodes() {
        let dir = TempDir::new().unwrap();
        for path in ["/dev/zero", "/dev/null"] {
            let error = ReadTool
                .execute(&ctx(dir.path()), json!({"path": path}))
                .await
                .unwrap_err();
            assert!(
                error.message.contains("non-regular"),
                "{path}: {}",
                error.message
            );
        }
    }
}
