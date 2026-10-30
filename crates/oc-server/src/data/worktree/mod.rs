//! Project worktree routes backed by the real `git` executable.
//!
//! Every git invocation is an argv array (never a shell) with a deadline and an
//! output cap. `POST /api/worktree` creates a detached worktree under a base
//! directory, `DELETE` removes one (or every linked one when the caller omits
//! `directory`, and `POST /api/worktree/refresh` prunes stale administrative
//! entries and reconciles the saved inventory with
//! `git worktree list --porcelain`.
//!
//! # Inventory scope
//!
//! The inventory is the project's *linked* worktrees. The repository's main
//! working tree is reported by `git worktree list` too, but it is not
//! removable with `git worktree remove` and is not part of the inventory, so
//! refresh excludes it. The main path comes from
//! `git rev-parse --show-toplevel` for the project directory (the same call
//! already required to locate the repository); paths are canonicalised, with
//! the Windows verbatim prefix stripped and case folded, before comparison.
//! Any saved row that resolves to the main working tree is deleted, so a
//! previously polluted inventory heals on the next refresh.
//!
//! # Path policy
//!
//! A worktree target must live inside one of the roots the server already
//! owns: the project's canonical directory, the configured `worktree.directory`
//! (`$OPENCODE_DATA_DIR/worktree/<project>` when unset), or the global data
//! `worktree` root. Parent components are rejected before touching the
//! filesystem, and the nearest existing ancestor is canonicalised, so a symlink
//! cannot move the target outside those roots. Names are a single path
//! component: separators, `.` and `..` are refused.
//!
//! Responses are the upstream bare wire objects the generated client parses:
//! `GET` answers `Worktree.Directory[]` (`[{directory}]`) and `POST` answers
//! `Worktree.Info` (`{directory}`); the 204 routes send no body.

pub mod git;
pub mod http;
pub mod paths;
pub mod plan;

pub use http::{CreateBody, ListParams, RefreshBody, RemoveBody, create, list, refresh, remove};

use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use oc_types::ApiError;
use serde_json::json;

use crate::AppState;

/// Deadline for one git invocation.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Captured stdout limit; worktree listings are small.
const MAX_STDOUT: usize = 1024 * 1024;
/// Captured stderr limit kept for diagnostics.
const MAX_STDERR: usize = 8 * 1024;
/// Highest `-N` suffix tried when a requested worktree name is taken.
const MAX_NAME_SUFFIX: u32 = 10;

/// Worktree routes, merged into the protected router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/worktree", get(list).post(create).delete(remove))
        .route("/api/worktree/refresh", post(refresh))
}

fn worktree_error(message: impl Into<String>, force_required: Option<bool>) -> ApiError {
    let message = message.into();
    let mut data = json!({ "message": message });
    if let Some(force) = force_required {
        data["forceRequired"] = json!(force);
    }
    ApiError::new(400, "WorktreeError", message)
        .with("name", json!("WorktreeError"))
        .with("data", data)
}
