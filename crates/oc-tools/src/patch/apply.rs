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
