//! Shared helpers: argument access, path resolution, BOM handling, atomic
//! writes, line-ending preservation, glob matching and identifiers.

use std::cmp::Ordering as CmpOrdering;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use globset::{GlobBuilder, GlobMatcher};
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::{ToolContext, ToolError};

pub const BOM: char = '\u{feff}';

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Process-unique monotonically increasing value used for temp/output names.
pub fn unique() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
        });
    nanos
        .wrapping_mul(1_000_003)
        .wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Saturating `u64` to `usize` conversion for model-provided limits.
pub fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Whether `shell` accepts `-o pipefail`, which makes a pipeline report the
/// last non-zero segment instead of the last segment's status.
///
/// bash, zsh and ksh support it. A POSIX `sh` may or may not: dash 0.5.12 does,
/// but POSIX does not require it and a shell built without it (for example a
/// stripped-down `ash`) may reject the option. The shell binary is therefore
/// probed once and the answer cached per path, so a shell that does not
/// understand the option keeps last-segment semantics instead of failing to
/// start. The probe runs a fixed string, never user input, and only inspects
/// whether the option was accepted.
pub fn shell_supports_pipefail(shell: &Path) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(known) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(shell)
    {
        return *known;
    }
    let supported = probe_pipefail(shell);
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(shell.to_path_buf(), supported);
    supported
}

fn probe_pipefail(shell: &Path) -> bool {
    let output = std::process::Command::new(shell)
        .args(["-o", "pipefail", "-c", "printf ok"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) => output.status.success() && output.stdout == b"ok",
        Err(_) => false,
    }
}

pub fn resolve(ctx: &ToolContext, input: &str) -> PathBuf {
    resolve_from(&ctx.directory, input)
}

pub fn resolve_from(base: &Path, input: &str) -> PathBuf {
    let path = Path::new(input);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn file_lock_key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| lexical_absolute(path))
}

/// Acquire per-file async locks in deterministic sorted order so concurrent
/// `edit`, `write`, and `patch` tool calls within the same turn (or across
/// sessions) serialize mutations to the same file without deadlocking or
/// silently overwriting earlier edits.
pub async fn lock_files(paths: &[PathBuf]) -> Vec<OwnedMutexGuard<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut keys: Vec<PathBuf> = paths.iter().map(|path| file_lock_key(path)).collect();
    keys.sort();
    keys.dedup();
    let mutexes: Vec<Arc<AsyncMutex<()>>> = {
        let mut guard = map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        keys.into_iter()
            .map(|key| {
                Arc::clone(
                    guard
                        .entry(key)
                        .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
                )
            })
            .collect()
    };
    let mut guards = Vec::with_capacity(mutexes.len());
    for mutex in mutexes {
        guards.push(mutex.lock_owned().await);
    }
    guards
}

/// Acquire the per-file async lock for a single `path`.
pub async fn lock_file(path: &Path) -> OwnedMutexGuard<()> {
    lock_files(&[path.to_path_buf()])
        .await
        .into_iter()
        .next()
        .expect("single file lock")
}

pub fn required_string(input: &Value, key: &str) -> Result<String, ToolError> {
    match input.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(ToolError::new(format!(
            "Invalid arguments: `{key}` must be a string"
        ))),
        None => Err(ToolError::new(format!(
            "Invalid arguments: missing required argument `{key}`"
        ))),
    }
}

pub fn optional_string(input: &Value, key: &str) -> Result<Option<String>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ToolError::new(format!(
            "Invalid arguments: `{key}` must be a string"
        ))),
    }
}

pub fn optional_bool(input: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolError::new(format!(
            "Invalid arguments: `{key}` must be a boolean"
        ))),
    }
}

pub fn optional_u64(input: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number.as_u64().map(Some).ok_or_else(|| {
            ToolError::new(format!(
                "Invalid arguments: `{key}` must be a non-negative integer"
            ))
        }),
        Some(_) => Err(ToolError::new(format!(
            "Invalid arguments: `{key}` must be a non-negative integer"
        ))),
    }
}

/// Split a leading UTF-8 BOM from decoded text.
pub fn split_bom(text: &str) -> (bool, &str) {
    let stripped = text.trim_start_matches(BOM);
    (stripped.len() != text.len(), stripped)
}

pub fn join_bom(text: &str, bom: bool) -> String {
    let (_, body) = split_bom(text);
    if bom {
        let mut out = String::with_capacity(body.len() + 3);
        out.push(BOM);
        out.push_str(body);
        out
    } else {
        body.to_string()
    }
}

pub fn has_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xEF, 0xBB, 0xBF])
}

/// Decode bytes lossily, returning the BOM flag and BOM-free text.
pub fn decode_bytes(bytes: &[u8]) -> (bool, String) {
    let decoded = String::from_utf8_lossy(bytes).into_owned();
    let (bom, text) = split_bom(&decoded);
    (bom, text.to_string())
}

/// Write `bytes` via a temp file in the destination directory followed by a
/// rename. Parent directories are created. On Windows a rename over an
/// existing file falls back to remove-then-rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{name}.tmp-{}-{counter}", std::process::id()));
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        match fs::rename(&temp, path) {
            Ok(()) => Ok(()),
            Err(error) => {
                #[cfg(windows)]
                {
                    if path.exists() {
                        fs::remove_file(path)?;
                        return fs::rename(&temp, path);
                    }
                }
                Err(error)
            }
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Replace path separators with `/` for wire output.
pub fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Display `target` relative to `base`, using `/` separators. Leaves the
/// location (for example an authorised absolute search root) fall back to a
/// slashed absolute path.
pub fn relative_display(base: &Path, target: &Path) -> String {
    let base = lexical_absolute(base);
    let target = lexical_absolute(target);
    let base_components: Vec<Component<'_>> = base.components().collect();
    let target_components: Vec<Component<'_>> = target.components().collect();

    let mut common = 0;
    while common < base_components.len()
        && common < target_components.len()
        && base_components[common] == target_components[common]
    {
        common += 1;
    }

    // Different roots (for example two Windows drives) cannot be relativized.
    if common == 0 && target.is_absolute() {
        return slash(&target);
    }

    let mut out = PathBuf::new();
    for _ in common..base_components.len() {
        out.push("..");
    }
    for component in &target_components[common..] {
        out.push(component.as_os_str());
    }
    if out.as_os_str().is_empty() {
        ".".to_string()
    } else {
        slash(&out)
    }
}

fn lexical_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Map a character to its ASCII-ish equivalent for the Unicode matching tier.
pub fn normalize_char(value: char) -> char {
    match value {
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
        '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
        other => other,
    }
}

pub fn normalize_text(value: &str) -> String {
    value.chars().map(normalize_char).collect()
}

/// Dominant line ending in `text`, or `fallback` when unclear.
pub fn line_ending(text: &str, fallback: &str) -> String {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    match crlf.cmp(&lf) {
        CmpOrdering::Equal => fallback.to_string(),
        CmpOrdering::Greater => "\r\n".to_string(),
        CmpOrdering::Less => "\n".to_string(),
    }
}

/// Split like `String.split(/\r?\n/)`: the trailing `\r` of CRLF is removed
/// and no line is dropped.
fn split_optional_cr(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            let mut end = index;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            out.push(&text[start..end]);
            start = index + 1;
        }
    }
    out.push(&text[start..]);
    out
}

fn line_endings(text: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            if index > 0 && bytes[index - 1] == b'\r' {
                out.push("\r\n");
            } else {
                out.push("\n");
            }
        }
    }
    out
}

fn alignment_key(line: &str) -> String {
    normalize_text(line.trim_end())
}

/// Join `replacement`'s lines with the line endings of the matched region.
/// Lines kept from the region reuse their ending; changed lines reuse the
/// ending of the line they replace; extra lines use `fallback`.
pub fn with_region_endings(region: &str, replacement: &str, fallback: &str) -> String {
    let before: Vec<String> = split_optional_cr(region)
        .iter()
        .map(|line| alignment_key(line))
        .collect();
    let endings = line_endings(region);
    let after: Vec<&str> = replacement.split('\n').collect();
    let source = align_lines(&before, &after);
    let mut out = String::new();
    for (index, line) in after.iter().enumerate() {
        out.push_str(line);
        if index + 1 < after.len() {
            let ending = source
                .get(index)
                .copied()
                .flatten()
                .and_then(|at| endings.get(at).copied())
                .unwrap_or(fallback);
            out.push_str(ending);
        }
    }
    out
}

/// Longest-common-subsequence alignment. For each `after` line returns the
/// `before` line it keeps or, between kept lines, the line it replaces.
fn align_lines(before: &[String], after: &[&str]) -> Vec<Option<usize>> {
    let width = after.len() + 1;
    let mut table = vec![0u32; (before.len() + 1) * width];
    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            table[i * width + j] = if before[i] == alignment_key(after[j]) {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let mut result = vec![None; after.len()];
    let mut gap_before: Vec<usize> = Vec::new();
    let mut gap_after: Vec<usize> = Vec::new();
    let flush = |result: &mut Vec<Option<usize>>,
                 gap_before: &mut Vec<usize>,
                 gap_after: &mut Vec<usize>| {
        for (index, at) in gap_after.iter().enumerate() {
            result[*at] = gap_before.get(index).copied();
        }
        gap_before.clear();
        gap_after.clear();
    };
    let (mut i, mut j) = (0usize, 0usize);
    while i < before.len() && j < after.len() {
        if before[i] == alignment_key(after[j]) {
            flush(&mut result, &mut gap_before, &mut gap_after);
            result[j] = Some(i);
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            gap_before.push(i);
            i += 1;
        } else {
            gap_after.push(j);
            j += 1;
        }
    }
    while i < before.len() {
        gap_before.push(i);
        i += 1;
    }
    while j < after.len() {
        gap_after.push(j);
        j += 1;
    }
    flush(&mut result, &mut gap_before, &mut gap_after);
    result
}

/// Glob matcher with ripgrep's basename semantics: patterns without a `/`
/// match the file name at any depth, patterns with one match the whole
/// root-relative path.
#[derive(Debug, Clone)]
pub struct PathGlob {
    matcher: GlobMatcher,
    basename: bool,
}

impl PathGlob {
    pub fn new(pattern: &str) -> Result<Self, String> {
        let basename = !pattern.contains('/');
        let matcher = GlobBuilder::new(pattern)
            .literal_separator(!basename)
            .build()
            .map_err(|error| error.to_string())?
            .compile_matcher();
        Ok(Self { matcher, basename })
    }

    pub fn is_match(&self, relative: &str) -> bool {
        if self.basename {
            Path::new(relative)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| self.matcher.is_match(name))
        } else {
            self.matcher.is_match(relative)
        }
    }
}

/// Read a file's bytes, mapping the common failures to tool messages.
pub fn read_file(path: &Path, display: &str) -> Result<Vec<u8>, ToolError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(ToolError::new(format!(
                "Path is a directory, not a file: {display}"
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ToolError::new(format!("File not found: {display}")));
        }
        Err(error) => {
            return Err(ToolError::new(format!("Unable to read {display}: {error}")));
        }
    }
    fs::read(path).map_err(|error| ToolError::new(format!("Unable to read {display}: {error}")))
}

pub fn io_message(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => "file does not exist".to_string(),
        io::ErrorKind::PermissionDenied => "access denied".to_string(),
        _ => error.to_string(),
    }
}

/// Persist a captured output stream, returning the path used for spill files.
pub fn tool_output_dir() -> PathBuf {
    std::env::temp_dir().join("opencode").join("tool-output")
}

pub fn output_file(extension: &str) -> PathBuf {
    let dir = tool_output_dir();
    let _ = fs::create_dir_all(&dir);
    dir.join(format!(
        "tool_{:012x}{extension}",
        unique() & 0xffff_ffff_ffff
    ))
}

pub fn count_lines(path: &Path) -> io::Result<usize> {
    use std::io::Read;
    let mut file = File::open(path)?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut newlines = 0usize;
    let mut last = None;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        newlines += buffer[..read]
            .iter()
            .fold(0usize, |count, byte| count + usize::from(*byte == b'\n'));
        last = Some(buffer[read - 1]);
    }
    Ok(newlines + usize::from(last.is_some() && last != Some(b'\n')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_endings_preserve_crlf() {
        assert_eq!(with_region_endings("a\r\nb", "c\nd", "\n"), "c\r\nd");
        assert_eq!(with_region_endings("a\nb", "c\nd", "\r\n"), "c\nd");
    }

    #[test]
    fn glob_basename_semantics() {
        let matcher = PathGlob::new("*.ts").unwrap();
        assert!(matcher.is_match("src/app.ts"));
        assert!(!matcher.is_match("src/app.rs"));
        let matcher = PathGlob::new("src/*.ts").unwrap();
        assert!(matcher.is_match("src/app.ts"));
        assert!(!matcher.is_match("src/deep/app.ts"));
    }
}
