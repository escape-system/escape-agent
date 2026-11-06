//! Pre-write snapshots for the revert engine.
//!
//! Before `write`, `edit` or `patch` runs, the runtime copies the current bytes
//! of every file the call is about to touch. The rows are keyed by session and
//! the assistant message `seq` that produced them, so `revert` can restore the
//! tree to the state as of any message and `clear` can drop everything after
//! it.
//!
//! Capture is strictly best effort: an unreadable file, an oversized file or a
//! storage failure is logged and never fails the tool call.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use oc_store::NewSnapshot;
use oc_types::{EventId, now_millis};
use serde_json::Value;

use crate::runtime::Runtime;

/// Largest file body persisted in a snapshot row.
const CONTENT_CAP: u64 = 5 * 1024 * 1024;

/// Capture the pre-image of every path `tool_name` is about to modify.
///
/// Only mutating file tools are captured. Targets are resolved against
/// `directory` and anything that escapes it (lexically or through a symlink)
/// is skipped: external edits are not the session's to revert.
pub(crate) async fn capture_before_tool(
    runtime: &Runtime,
    session_id: &str,
    message_id: &str,
    tool_name: &str,
    tool_call_id: &str,
    input: &Value,
    directory: &str,
) {
    let targets = match tool_name {
        "write" | "edit" => match input.get("path").and_then(Value::as_str) {
            Some(raw) => vec![raw.to_owned()],
            None => Vec::new(),
        },
        "patch" => match input.get("patchText").and_then(Value::as_str) {
            Some(text) => oc_tools::patch::patch_targets(text),
            None => Vec::new(),
        },
        _ => return,
    };

    let mut paths: Vec<PathBuf> = Vec::with_capacity(targets.len());
    for raw in targets {
        let Some(resolved) = resolve(directory, &raw) else {
            continue;
        };
        if !paths.iter().any(|known| same_path(known, &resolved)) {
            paths.push(resolved);
        }
    }
    if paths.is_empty() {
        return;
    }

    let store = &runtime.inner.config.store;
    let seq = match store.message_seq(session_id, message_id).await {
        Ok(Some(seq)) => seq,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(error = %error, "snapshot seq lookup failed");
            return;
        }
    };
    let Ok(seq) = i64::try_from(seq) else {
        tracing::warn!(seq, "snapshot seq does not fit i64");
        return;
    };

    let created = now_millis();
    for path in paths {
        let row = NewSnapshot {
            id: snapshot_id(),
            session_id: session_id.to_owned(),
            message_id: message_id.to_owned(),
            seq,
            tool_call_id: tool_call_id.to_owned(),
            path: path.to_string_lossy().into_owned(),
            existed: true,
            skipped: false,
            content: None,
            time_created: created,
        };
        let row = sample(&path, row);
        if let Err(error) = store.insert_snapshot(row).await {
            tracing::warn!(error = %error, path = %path.display(), "snapshot insert failed");
        }
    }
}

/// Fill the sampled fields of `row` from the file at `path`.
fn sample(path: &Path, mut row: NewSnapshot) -> NewSnapshot {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            row.existed = false;
        }
        Err(_) => {
            row.skipped = true;
        }
        Ok(meta) if !meta.is_file() || meta.len() > CONTENT_CAP => {
            row.skipped = true;
        }
        Ok(meta) => match std::fs::File::open(path) {
            Ok(file) => {
                let cap = usize::try_from(CONTENT_CAP).unwrap_or(usize::MAX);
                let capacity = usize::try_from(meta.len()).unwrap_or(cap);
                let mut bytes = Vec::with_capacity(capacity.min(cap));
                let mut handle = file.take(CONTENT_CAP + 1);
                match handle.read_to_end(&mut bytes) {
                    Ok(_) if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > CONTENT_CAP => {
                        row.skipped = true;
                    }
                    Ok(_) => row.content = Some(bytes),
                    Err(_) => row.skipped = true,
                }
            }
            Err(_) => row.skipped = true,
        },
    }
    row
}

/// `snap_` id sharing the wire shape of event ids.
fn snapshot_id() -> String {
    let event = EventId::new();
    let raw = event
        .as_str()
        .strip_prefix("evt_")
        .unwrap_or(event.as_str());
    format!("snap_{raw}")
}

/// Resolve a tool target against the session directory, refusing escapes.
fn resolve(directory: &str, raw: &str) -> Option<PathBuf> {
    let base = lexical(Path::new(directory));
    let raw_path = Path::new(raw);
    let joined = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        base.join(raw_path)
    };
    let resolved = lexical(&joined);
    if !confined(&base, &resolved) {
        return None;
    }

    // Symlinks can point outside the directory even when the joined path looks
    // confined; when the filesystem can resolve it, re-check the real target.
    if let Ok(root) = std::fs::canonicalize(&base) {
        let real = std::fs::canonicalize(&resolved).ok().or_else(|| {
            let parent = resolved.parent()?;
            let name = resolved.file_name()?;
            Some(std::fs::canonicalize(parent).ok()?.join(name))
        });
        if let Some(real) = real
            && !confined(&root, &real)
        {
            return None;
        }
    }

    Some(resolved)
}

/// Remove `.` and fold `..` without touching the filesystem.
fn lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Windows-aware containment check on slash-normalized, lowercased text.
fn confined(directory: &Path, path: &Path) -> bool {
    let base = normalize_text(directory);
    let path = normalize_text(path);
    path == base || path.starts_with(&format!("{base}/"))
}

fn same_path(left: &Path, right: &Path) -> bool {
    normalize_text(left) == normalize_text(right)
}

fn normalize_text(path: &Path) -> String {
    #[cfg(windows)]
    let mut text = path.to_string_lossy().replace('\\', "/").to_lowercase();
    #[cfg(not(windows))]
    let mut text = path.to_string_lossy().replace('\\', "/");
    while text.len() > 1 && text.ends_with('/') && !text.ends_with(":/") {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_folds_dot_and_parent_components() {
        let base = if cfg!(windows) { r"C:\work" } else { "/work" };
        let path = Path::new(base).join("a/./b/../c.txt");
        assert_eq!(lexical(&path), Path::new(base).join("a/c.txt"));
    }

    #[test]
    fn resolve_rejects_escapes() {
        let directory = if cfg!(windows) { r"C:\work" } else { "/work" };
        assert!(resolve(directory, "../outside.txt").is_none());
        assert!(resolve(directory, "inside.txt").is_some());
    }

    #[test]
    fn snapshot_ids_use_snap_prefix() {
        let id = snapshot_id();
        assert!(id.starts_with("snap_"), "got {id}");
        assert_eq!(id.len(), 31);
    }
}
