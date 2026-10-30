//! Session routes: CRUD, selection, move, prompts and the pending-input
//! admission endpoints.
//!
//! `/prompt`, `/synthetic`, `/compact` and `/command` durably admit input
//! (inbox row, user message, enqueued event) and then wake the runtime when it
//! is attached. Execution itself never happens in a handler. `/shell` lives in
//! [`crate::data::shells`].
//!
//! Handlers live in `bodies` (wire bodies), `crud`, `select`, `run`,
//! `prompt`, `wait` and `events`; every name is re-exported here.

mod bodies;
mod crud;
mod events;
mod prompt;
mod run;
mod select;
mod wait;

pub use bodies::{
    AgentBody, CommandBody, CompactBody, CreateBody, CreateLocation, InterruptParams, ListParams,
    ModelBody, MoveBody, PatchBody, PromptBody, SyntheticBody, WaitParams,
};
pub use crud::{active, create, delete, get, list, patch};
pub(crate) use events::{notify, publish_enqueued};
pub use prompt::{command, compact, prompt, synthetic};
pub use run::{background, interrupt};
pub use select::{move_session, select_agent, select_model};
pub use wait::wait;

use serde::Deserialize;

/// Deep-object `location[directory]` shared by location-scoped routes.
#[derive(Debug, Default, Deserialize)]
pub struct LocationQuery {
    /// Requested working directory.
    #[serde(rename = "location[directory]")]
    pub directory: Option<String>,
}
