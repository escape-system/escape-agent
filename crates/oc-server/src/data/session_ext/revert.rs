//! Revert staging, clearing and committing over the runtime's file snapshots.

use std::path::PathBuf;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use oc_store::{SessionPatch, SnapshotRow, Store, StoreError};
use oc_tools::diff::file_diff;
use oc_tools::util::atomic_write;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::data::fs::confined_target;
use crate::data::{no_content, require_store, schema_error};
use crate::error::ErrorResponse;

use super::common::{message_not_found, require_session};

/// Canonical session directory; every snapshot path is confined to it.
fn session_root(state: &AppState, session: &Value) -> PathBuf {
    let directory = session
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .map_or_else(|| state.directory().to_path_buf(), PathBuf::from);
    std::fs::canonicalize(&directory).unwrap_or(directory)
}

/// Dense sequence of a message, if it still exists.
pub(super) async fn boundary_seq(
    store: &Store,
    session_id: &str,
    message_id: &str,
) -> Result<Option<i64>, StoreError> {
    Ok(store
        .message_seq(session_id, message_id)
        .await?
        .and_then(|seq| i64::try_from(seq).ok()))
}

/// The diff of one snapshot against the current file.
///
/// Only regular files inside the session directory take part. A skipped
/// snapshot has no comparable pre-image, and a path that leaves the directory
/// belongs to someone else, so both are ignored.
fn snapshot_diff(root: &std::path::Path, row: &SnapshotRow) -> Option<Value> {
    if row.skipped {
        return None;
    }
    let target = confined_target(root, &row.path).ok()?;
    let current = match std::fs::metadata(&target) {
        Ok(meta) if meta.is_file() => Some(std::fs::read(&target).ok()?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Ok(_) | Err(_) => return None,
    };
    let before = if row.existed {
        let content = row.content.as_deref()?;
        Some(String::from_utf8_lossy(content).into_owned())
    } else {
        None
    };
    let after = current.map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let (status, before, after) = match (before, after) {
        (None, None) => return None,
        (None, Some(after)) => ("added", String::new(), after),
        (Some(before), None) => ("deleted", before, String::new()),
        (Some(before), Some(after)) => {
            if before == after {
                return None;
            }
            ("modified", before, after)
        }
    };
    Some(file_diff(&row.path, &before, &after, status))
}

/// Recomputes the staged revert diffs from the snapshots and the current tree.
pub(super) async fn revert_diffs(
    store: &Store,
    state: &AppState,
    session: &Value,
    session_id: &str,
    boundary: i64,
) -> Result<Vec<Value>, StoreError> {
    let root = session_root(state, session);
    let rows = store.earliest_snapshots_after(session_id, boundary).await?;
    Ok(rows
        .iter()
        .filter_map(|row| snapshot_diff(&root, row))
        .collect())
}

/// Restores one earliest snapshot, leaving anything it cannot safely own.
fn restore_snapshot(root: &std::path::Path, row: &SnapshotRow) {
    if row.skipped {
        tracing::warn!(path = %row.path, "snapshot was skipped; leaving the path untouched");
        return;
    }
    let Ok(target) = confined_target(root, &row.path) else {
        tracing::warn!(
            path = %row.path,
            "snapshot path escapes the session directory; leaving it untouched"
        );
        return;
    };
    let existing = std::fs::metadata(&target);
    if row.existed {
        let Some(content) = row.content.as_deref() else {
            tracing::warn!(path = %row.path, "snapshot has no content; leaving the path untouched");
            return;
        };
        match &existing {
            Ok(meta) if !meta.is_file() => {
                tracing::warn!(
                    path = %row.path,
                    "snapshot target is not a regular file; leaving it untouched"
                );
                return;
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    path = %row.path,
                    "snapshot target is unreadable; leaving it untouched"
                );
                return;
            }
            _ => {}
        }
        if let Err(error) = atomic_write(&target, content) {
            tracing::warn!(error = %error, path = %row.path, "snapshot restore failed");
        }
    } else {
        match existing {
            Ok(meta) if meta.is_file() => {
                if let Err(error) = std::fs::remove_file(&target) {
                    tracing::warn!(error = %error, path = %row.path, "snapshot removal failed");
                }
            }
            Ok(_) => tracing::warn!(
                path = %row.path,
                "snapshot target is not a regular file; leaving it untouched"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => tracing::warn!(
                path = %row.path,
                "snapshot target is unreadable; leaving it untouched"
            ),
        }
    }
}

/// Restores the earliest snapshot of every path captured at or after `boundary`.
async fn restore_snapshots(
    store: &Store,
    state: &AppState,
    session: &Value,
    session_id: &str,
    boundary: i64,
) -> Result<(), StoreError> {
    let root = session_root(state, session);
    for row in store.earliest_snapshots_after(session_id, boundary).await? {
        restore_snapshot(&root, &row);
    }
    Ok(())
}

/// `POST /api/session/:sid/revert/stage`
#[derive(Debug, Deserialize)]
pub struct RevertStageBody {
    #[serde(rename = "messageID")]
    message_id: String,
    #[serde(default)]
    #[allow(dead_code)] // accepted for wire compatibility; the diff is always computed
    files: Option<bool>,
}

/// `POST /api/session/:sid/revert/stage`
pub async fn revert_stage(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<RevertStageBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let boundary = boundary_seq(store, &session_id, &body.message_id)
        .await?
        .ok_or_else(|| message_not_found(&session_id, &body.message_id))?;

    let files = revert_diffs(store, &state, &session, &session_id, boundary).await?;
    let revert = json!({ "messageID": body.message_id, "files": files });
    store
        .patch_session(
            &session_id,
            SessionPatch {
                revert: Some(revert.clone()),
                ..SessionPatch::default()
            },
        )
        .await?;
    Ok(Json(json!({ "data": revert })))
}

/// `DELETE /api/session/:sid/revert`
pub async fn revert_clear(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let boundary = session
        .pointer("/revert/messageID")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(message_id) = boundary {
        if let Some(seq) = boundary_seq(store, &session_id, &message_id).await? {
            restore_snapshots(store, &state, &session, &session_id, seq).await?;
            store.delete_snapshots_after(&session_id, seq).await?;
        } else {
            tracing::warn!(
                session_id = %session_id,
                message_id = %message_id,
                "revert boundary message is missing; snapshots were not restored"
            );
        }
        store
            .patch_session(
                &session_id,
                SessionPatch {
                    revert: Some(Value::Null),
                    ..SessionPatch::default()
                },
            )
            .await?;
    }
    Ok(no_content())
}

/// `POST /api/session/:sid/revert/commit`
pub async fn revert_commit(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let boundary = session
        .pointer("/revert/messageID")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(message_id) = boundary {
        // Commit deletes the boundary message and everything after it, matching
        // the documented revert boundary, and drops the snapshots that fed it.
        if let Some(seq) = boundary_seq(store, &session_id, &message_id).await? {
            store
                .truncate_session(&session_id, u64::try_from(seq).unwrap_or_default())
                .await?;
            store.delete_snapshots_after(&session_id, seq).await?;
        }
        store
            .patch_session(
                &session_id,
                SessionPatch {
                    revert: Some(Value::Null),
                    ..SessionPatch::default()
                },
            )
            .await?;
    }
    Ok(no_content())
}
