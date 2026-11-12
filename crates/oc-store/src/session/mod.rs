//! Session rows, patching and keyset pagination over `session_v2`.
//!
//! The facade owns the wire types, the create/policy/get paths and the policy
//! vocabulary; [list] holds the keyset query, [pagination] the cursor payload
//! and page bookkeeping, [patch] the update paths, [tree] the parent/child
//! helpers and [value] the row-to-wire mapping.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cursor::{Cursor, Order};
use crate::db::{DEFAULT_LIMIT, Store, in_tx};
use crate::error::StoreError;

mod list;
mod pagination;
mod patch;
mod tree;
mod value;

pub(crate) use patch::keeps_idle_reason;
pub(crate) use tree::{delete_tree, ensure_session, parent_position};
use value::session_value;

/// Input for creating a session.
#[derive(Debug, Clone)]
pub struct NewSession {
    /// Wire session id (`ses_…`).
    pub id: String,
    /// Parent session for a subagent session.
    pub parent_id: Option<String>,
    /// Owning project id (`prj_…`).
    pub project_id: String,
    /// Absolute working directory.
    pub directory: String,
    /// Display title.
    pub title: Option<String>,
    /// Selected agent id.
    pub agent: Option<String>,
    /// Selected model (`Session.Info` model object).
    pub model: Option<Value>,
    /// Host-supplied metadata object.
    pub metadata: Option<Value>,
    /// Permission ruleset.
    pub permissions: Option<Value>,
    /// Creation time in epoch milliseconds.
    pub time_created: i64,
}

/// Explicit tree/policy columns for [`Store::create_session_with_tree`].
#[derive(Debug, Clone)]
pub struct NewSessionTree {
    /// Root session of the tree the new session belongs to.
    pub root_id: String,
    /// Distance from the root; must be the parent's depth plus one.
    pub depth: u32,
    /// `primary`, `subagent` or `coordinator`.
    pub role: String,
    /// Whether the new session may spawn children.
    pub spawn_allowed: bool,
}

/// Filters and pagination for [`Store::list_sessions`].
#[derive(Debug, Clone)]
pub struct SessionQuery {
    /// Page size; `0` means the default (50) and values above 200 are clamped.
    pub limit: usize,
    /// Sort order on `(time_updated, id)`.
    pub order: Order,
    /// Case-insensitive substring match on the title (documented table scan).
    pub search: Option<String>,
    /// `Some(None)` selects root sessions, `Some(Some(id))` selects children.
    pub parent_id: Option<Option<String>>,
    /// `Some(true)` selects archived sessions, `Some(false)` unarchived ones.
    pub archived: Option<bool>,
    /// Exact working-directory match.
    pub directory: Option<String>,
    /// Exact project match.
    pub project: Option<String>,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
    /// Exact subpath match against `session_v2.path`.
    pub subpath: Option<String>,
}

impl Default for SessionQuery {
    fn default() -> Self {
        Self {
            limit: DEFAULT_LIMIT,
            order: Order::Desc,
            search: None,
            parent_id: None,
            archived: None,
            directory: None,
            project: None,
            cursor: None,
            subpath: None,
        }
    }
}

/// Fields to change on a session. Absent fields are left untouched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionPatch {
    /// New title.
    pub title: Option<String>,
    /// New selected agent.
    pub agent: Option<String>,
    /// New selected model; JSON `null` clears the column.
    pub model: Option<Value>,
    /// New metadata object; JSON `null` clears the column.
    pub metadata: Option<Value>,
    /// New permission ruleset; JSON `null` clears the column.
    pub permissions: Option<Value>,
    /// New working directory.
    pub directory: Option<String>,
    /// New subpath.
    pub path: Option<String>,
    /// New revert state; JSON `null` clears the column.
    pub revert: Option<Value>,
    /// Archive or unarchive the session.
    pub archived: Option<bool>,
}

/// One page of sessions.
#[derive(Debug, Clone)]
pub struct SessionPage {
    /// Wire `Session.Info` objects in the requested order.
    pub items: Vec<Value>,
    /// Keyset cursors for the neighbouring pages.
    pub cursor: Cursor,
}

/// `session_v2.directory` is nullable (frozen `0001_wire`), but
/// `Session.Info.location` is **required** by the vendored wire contract
/// (`Session.Info.required` includes `location`, `Location.PublicRef.required`
/// is `[directory]`). A session row with no directory therefore falls back to
/// its owning project's directory, which the schema guarantees (`project.directory`
/// is `NOT NULL` and `session_v2.project_id` is a foreign key). That is a stored,
/// schema-valid `AbsolutePath` — not a fabricated placeholder — so the wire key
/// is always present and never `null`.
pub(crate) const SESSION_COLUMNS: &str = "id, project_id, parent_id, root_id, depth, role, \
spawn_allowed, child_count, fork_session_id, fork_boundary, \
coalesce(directory, (SELECT p.directory FROM project p WHERE p.id = session_v2.project_id)), \
path, title, agent, \
metadata, permission, model, revert, summary_diffs, cost, tokens_input, tokens_output, \
tokens_reasoning, tokens_cache_read, tokens_cache_write, time_created, time_updated, time_busy, \
time_idle, time_viewed, idle_outcome, time_compacting, time_archived, time_suspended, \
resume_attempts";

impl Store {
    /// Creates a session and returns its wire `Session.Info`.
    ///
    /// The owning project row is inserted when missing so the foreign key holds.
    /// `root_id`/`depth` are derived from the parent and the policy columns keep
    /// their defaults (`primary`, spawning allowed).
    pub async fn create_session(&self, new: NewSession) -> Result<Value, StoreError> {
        self.create_session_inner(new, None).await
    }

    /// Creates a session with explicit tree columns in one transaction,
    /// incrementing the parent's `child_count` when `parent_id` is set.
    ///
    /// `tree.role` must be `primary`, `subagent` or `coordinator`;
    /// `tree.root_id` must not be empty; and when a parent is given,
    /// `tree.depth` must equal the parent's depth + 1. Duplicate ids are a
    /// `Conflict` and leave the parent's count untouched.
    pub async fn create_session_with_tree(
        &self,
        new: NewSession,
        tree: NewSessionTree,
    ) -> Result<Value, StoreError> {
        validate_tree(&tree)?;
        self.create_session_inner(new, Some(tree)).await
    }

    /// Updates the policy columns of an existing session.
    ///
    /// Unknown ids are a `NotFound`; unknown roles are `Invalid`.
    pub async fn set_session_policy(
        &self,
        id: &str,
        role: &str,
        spawn_allowed: bool,
    ) -> Result<(), StoreError> {
        validate_role(role)?;
        let id = id.to_owned();
        let role = role.to_owned();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let changed = transaction.execute(
                    "UPDATE session_v2 SET role = ?2, spawn_allowed = ?3 WHERE id = ?1",
                    params![&id, &role, i64::from(spawn_allowed)],
                )?;
                if changed == 0 {
                    return Err(StoreError::NotFound(format!("session {id}")));
                }
                Ok(())
            })
        })
        .await
    }
    /// Loads a session as a wire `Session.Info`.
    pub async fn get_session(&self, id: &str) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        self.read(move |connection| {
            let sql = format!("SELECT {SESSION_COLUMNS} FROM session_v2 WHERE id = ?1");
            let row = connection
                .query_row(&sql, [&id], |row| Ok(session_value(row)))
                .optional()?;
            row.transpose()
        })
        .await
    }
}

const ROLES: [&str; 3] = ["primary", "subagent", "coordinator"];

fn validate_role(role: &str) -> Result<(), StoreError> {
    if ROLES.contains(&role) {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "unknown session role {role:?}"
        )))
    }
}
fn validate_tree(tree: &NewSessionTree) -> Result<(), StoreError> {
    if tree.root_id.trim().is_empty() {
        return Err(StoreError::Invalid(
            "tree root_id must not be empty".to_owned(),
        ));
    }
    validate_role(&tree.role)
}
