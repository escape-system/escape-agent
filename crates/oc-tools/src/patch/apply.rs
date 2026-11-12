//! Prepare-then-apply pipeline: resolve every target and derive every result before the first write.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::hunks::{Hunk, derive};
use super::parser::parse;
use super::targets::patch_targets;
use crate::util;
use crate::{ToolContext, ToolError, ToolResult};

#[allow(clippy::too_many_lines)] // Single linear patch prepare/apply pipeline.
pub(super) async fn execute(ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
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
                        let display = util::relative_display(&ctx.directory, &absolute);
                        (absolute, display)
                    })
                    .filter(|(absolute, _)| !same_file(&target, absolute));

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
    let applied = apply_prepared(prepared)?;

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

fn same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn apply_prepared(prepared: Vec<Prepared>) -> Result<Vec<(char, String)>, ToolError> {
    let originals = snapshot_prepared(&prepared);
    let mut applied: Vec<(char, String)> = Vec::new();
    let mut touched: Vec<PathBuf> = Vec::new();

    for change in prepared {
        match change {
            Prepared::Add {
                target,
                resource,
                after,
            } => {
                if let Err(error) = util::atomic_write(&target, after.as_bytes()) {
                    let rollback_errors = rollback_touched(&touched, &originals);
                    return Err(failure(
                        format!("Failed to write {resource}: {error}"),
                        &applied,
                        &rollback_errors,
                    ));
                }
                record_touched(&mut touched, &target);
                applied.push(('A', resource));
            }
            Prepared::Delete {
                target, resource, ..
            } => {
                if let Err(error) = fs::remove_file(&target) {
                    let rollback_errors = rollback_touched(&touched, &originals);
                    return Err(failure(
                        format!("Failed to delete {resource}: {error}"),
                        &applied,
                        &rollback_errors,
                    ));
                }
                record_touched(&mut touched, &target);
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
                    if let Err(error) = util::atomic_write(&absolute, content.as_bytes()) {
                        let rollback_errors = rollback_touched(&touched, &originals);
                        return Err(failure(
                            format!("Failed to write {move_resource}: {error}"),
                            &applied,
                            &rollback_errors,
                        ));
                    }
                    record_touched(&mut touched, &absolute);
                    if let Err(error) = fs::remove_file(&target) {
                        let rollback_errors = rollback_touched(&touched, &originals);
                        return Err(failure(
                            format!(
                                "Wrote {move_resource} but failed to remove {resource}: {error}"
                            ),
                            &applied,
                            &rollback_errors,
                        ));
                    }
                    record_touched(&mut touched, &target);
                    applied.push(('M', move_resource));
                } else {
                    if let Err(error) = util::atomic_write(&target, content.as_bytes()) {
                        let rollback_errors = rollback_touched(&touched, &originals);
                        return Err(failure(
                            format!("Failed to write {resource}: {error}"),
                            &applied,
                            &rollback_errors,
                        ));
                    }
                    record_touched(&mut touched, &target);
                    applied.push(('M', resource));
                }
            }
        }
    }

    Ok(applied)
}

#[derive(Clone)]
enum OriginalState {
    File(Vec<u8>),
    Absent,
    Unreadable,
}

fn snapshot_prepared(prepared: &[Prepared]) -> HashMap<PathBuf, OriginalState> {
    let mut originals = HashMap::new();
    for change in prepared {
        match change {
            Prepared::Add { target, .. } | Prepared::Delete { target, .. } => {
                snapshot_original(&mut originals, target);
            }
            Prepared::Update {
                target,
                move_target,
                ..
            } => {
                snapshot_original(&mut originals, target);
                if let Some((absolute, _)) = move_target {
                    snapshot_original(&mut originals, absolute);
                }
            }
        }
    }
    originals
}

fn snapshot_original(originals: &mut HashMap<PathBuf, OriginalState>, path: &Path) {
    if originals.contains_key(path) {
        return;
    }
    let state = match fs::metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => OriginalState::Absent,
        Ok(meta) if meta.is_file() => {
            fs::read(path).map_or(OriginalState::Unreadable, OriginalState::File)
        }
        Err(_) | Ok(_) => OriginalState::Unreadable,
    };
    originals.insert(path.to_path_buf(), state);
}

fn record_touched(touched: &mut Vec<PathBuf>, path: &Path) {
    if !touched.iter().any(|existing| existing == path) {
        touched.push(path.to_path_buf());
    }
}

fn rollback_touched(
    touched: &[PathBuf],
    originals: &HashMap<PathBuf, OriginalState>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for path in touched.iter().rev() {
        let Some(state) = originals.get(path) else {
            continue;
        };
        match state {
            OriginalState::File(bytes) => {
                if let Err(error) = util::atomic_write(path, bytes) {
                    errors.push(format!("{}: {error}", path.display()));
                }
            }
            OriginalState::Absent => match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => errors.push(format!("{}: {error}", path.display())),
            },
            OriginalState::Unreadable => {}
        }
    }
    errors
}

fn failure(operation: String, applied: &[(char, String)], rollback_errors: &[String]) -> ToolError {
    if applied.is_empty() && rollback_errors.is_empty() {
        return ToolError::new(operation);
    }
    if rollback_errors.is_empty() {
        return ToolError::new(format!(
            "{operation}. Rolled back {} earlier change(s).",
            applied.len()
        ));
    }
    let completed = applied
        .iter()
        .map(|(_, resource)| resource.clone())
        .collect::<Vec<_>>()
        .join(", ");
    ToolError::new(format!(
        "{operation}. Rollback partially failed ({}); completed before failure: {completed}",
        rollback_errors.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            session_id: "ses_patch".to_string(),
            directory: dir.to_path_buf(),
            cwd: dir.to_path_buf(),
            cancel: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn update_moving_to_symlink_alias_of_same_file_does_not_delete_it() {
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("note.txt"), "hello\n").unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("alias")).unwrap();

        let patch = "\
*** Begin Patch
*** Update File: real/note.txt
*** Move to: alias/note.txt
@@
-hello
+world
*** End Patch";
        execute(&ctx(dir.path()), json!({ "patchText": patch }))
            .await
            .expect("move to symlink alias should preserve the file");

        assert_eq!(
            fs::read_to_string(real.join("note.txt")).unwrap(),
            "world\n"
        );
    }

    #[tokio::test]
    async fn multi_file_patch_rolls_back_earlier_changes_on_failure() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("keep.txt"), "before keep\n").unwrap();
        fs::write(dir.path().join("restore_me.txt"), "before delete\n").unwrap();
        fs::create_dir(dir.path().join("blocker")).unwrap();
        fs::write(dir.path().join("blocker/inside.txt"), "occupied\n").unwrap();

        let patch = "\
*** Begin Patch
*** Add File: created.txt
+transient
*** Update File: keep.txt
@@
-before keep
+after keep
*** Delete File: restore_me.txt
*** Add File: blocker
+cannot overwrite directory
*** End Patch";

        let err = execute(&ctx(dir.path()), json!({ "patchText": patch }))
            .await
            .expect_err("overwriting directory must fail apply and roll back");
        assert!(
            err.message.contains("Rolled back 3 earlier change(s)"),
            "{}",
            err.message
        );
        assert!(!dir.path().join("created.txt").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
            "before keep\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("restore_me.txt")).unwrap(),
            "before delete\n"
        );
    }
}
