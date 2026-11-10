//! Session-tree policy for subagents: spawn limits, sibling/coordinator
//! messaging, write authorization, model resolution and pure rate budgets.
//!
//! This crate has no store, runtime, server or network dependency. Every
//! decision is a typed `Ok`/`Err`; the runtime maps errors onto tool errors
//! (`SpawnForbiddenError`, `SiblingScopeError`, …) without guessing.
//!
//! # Runtime mapping
//!
//! [`SessionNode`] combines the wire `Session.Info` with the policy columns in
//! `session_v2` (`docs/03-storage.md`):
//!
//! | `SessionNode` | source |
//! |---|---|
//! | `id`, `parent_id` | `Session.Info.id`, `Session.Info.parentID` |
//! | `agent`, `model` | `Session.Info.agent`, `ModelRef::to_reference()` |
//! | `root_id`, `depth`, `role`, `spawn_allowed`, `child_count` | `session_v2` columns written at create time |
//! | `state` | `SessionStatus` (`busy`→`Running`, `idle`→`Idle`) or the durable outcome (`completed`/`failed`/`cancelled`) |
//!
//! Role/depth are read from the row, never recomputed by ancestor walks on the
//! spawn path. `oc-store` already selects the five columns in
//! `SESSION_COLUMNS`, but `session_value` drops them when producing the wire
//! `Session.Info`; the runtime adapter needs a row accessor (or a small
//! `Store::session_node` query) before it can call [`Policy::spawn`].
//!
//! # Tree lookup
//!
//! [`TreeLookup`] needs parent links only. The runtime implements it over a
//! per-request map of `Store::get_session` rows (id + `parentID`); the
//! roster/tree API uses the existing child query
//! `Store::list_sessions(SessionQuery { parent_id: Some(Some(id)), .. })`
//! (`session_v2.parent_id` index), which also supplies the `siblings` list.
//! Coordinator scope is derived from the parent chain here, bounded at 1024
//! steps so a corrupt cycle cannot hang the request.
//!
//! ```ignore
//! let policy = Policy::new(limits);
//! let plan = policy.spawn(&parent, "explore", None, &available_agents)?;
//! // create session with plan.depth / plan.role / plan.spawn_allowed
//! let model = resolve_model(call_model, Some(&plan.agent), defaults, &parent_model, &available_models)?;
//! ```

pub mod budget;
pub mod error;
pub mod limits;
pub mod message;
pub mod model;
pub mod node;
pub mod policy;
pub mod spawn;

pub use budget::{DEFAULT_SPAWNS_PER_MIN, SiblingBudget, SpawnBudget, WINDOW_MS, WriteBudget};
pub use error::{BudgetError, MessageError, ModelError, SpawnError, WriteError};
pub use limits::Limits;
pub use message::{SessionTree, TreeLookup};
pub use model::{ModelCandidate, ModelChoice, ModelDefaults, ModelSource, resolve_model};
pub use node::{NodeState, Role, SessionNode};
pub use policy::Policy;
pub use spawn::{ChildPlan, PRIMARY_ONLY_AGENTS};
