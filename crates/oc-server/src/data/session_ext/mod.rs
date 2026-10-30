//! Remaining session surface: fork, revert, context, diff, environment, view,
//! forms, saved permissions, export/import, statistics, skill activation and
//! instruction entries.
//!
//! Every handler is a thin projection over the store. Revert consumes the
//! file snapshots the runtime captures before mutating tools run: staging
//! diffs the earliest snapshot of every path against the current file, clearing
//! restores those snapshots, and committing truncates the tail. Form lifecycle
//! changes are published as ephemeral events, while view stamps and forks
//! append durable events so the session log stays authoritative.
//!
//! Handlers live in one submodule per resource; this facade owns the router and
//! re-exports every public name so `data::session_ext::*` paths stay stable.

use axum::Router;
use axum::routing::{delete, get, patch as patch_route, post, put};

use crate::AppState;

mod common;
mod context;
mod diff;
mod export;
mod fork;
mod forms;
mod import;
mod instructions;
mod revert;
mod saved_permissions;
mod skill;
mod stats;
mod view;

pub use context::context;
pub use diff::{DiffParams, diff};
pub use export::{ExportParams, export};
pub use fork::{ForkBody, fork};
pub use forms::{
    FormCancelParams, FormCreateBody, FormReplyBody, form_cancel, form_create, form_get, form_list,
    form_reply,
};
pub use import::{ImportBody, ImportLocation, import};
pub use instructions::{InstructionBody, instruction_delete, instruction_list, instruction_put};
pub use revert::{RevertStageBody, revert_clear, revert_commit, revert_stage};
pub use saved_permissions::{
    SavedPermissionParams, SavedPermissionPatch, saved_permission_delete, saved_permission_update,
    saved_permissions,
};
pub use skill::{SkillBody, skill};
pub use stats::{StatsParams, stats};
pub use view::{EnvironmentBody, ViewBody, environment, view};

/// Routes owned by this module.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/session/{sid}/fork", post(fork))
        .route("/api/session/{sid}/revert/stage", post(revert_stage))
        .route("/api/session/{sid}/revert", delete(revert_clear))
        .route("/api/session/{sid}/revert/commit", post(revert_commit))
        .route("/api/session/{sid}/context", get(context))
        .route("/api/session/{sid}/diff", get(diff))
        .route("/api/session/{sid}/environment", put(environment))
        .route("/api/session/{sid}/view", post(view))
        .route("/api/session/{sid}/form", get(form_list).post(form_create))
        .route(
            "/api/session/{sid}/form/{form_id}",
            get(form_get).delete(form_cancel),
        )
        .route("/api/session/{sid}/form/{form_id}/reply", post(form_reply))
        .route("/api/permission/saved", get(saved_permissions))
        .route(
            "/api/permission/saved/{id}",
            patch_route(saved_permission_update).delete(saved_permission_delete),
        )
        .route("/api/experimental/session/{sid}/export", get(export))
        .route("/api/experimental/session/import", post(import))
        .route("/api/experimental/session/stats", get(stats))
        .route("/api/experimental/session/{sid}/skill", post(skill))
        .route(
            "/api/experimental/session/{sid}/instructions/entries",
            get(instruction_list),
        )
        .route(
            "/api/experimental/session/{sid}/instructions/entries/{key}",
            put(instruction_put).delete(instruction_delete),
        )
}
