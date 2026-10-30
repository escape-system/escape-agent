//! SQLite persistence for sessions, messages, pending inbox input and the
//! durable event log, plus forms, schedules, saved permission rules, instruction
//! entries, lane ticket history, session maintenance (revert/viewed, file
//! snapshots), stored credentials and registered worktrees.
//!
//! The schema follows `docs/03-storage.md`: one database file in WAL mode, wire
//! tables that stay byte-compatible with upstream, plus additive tables for our
//! own features. Bodies are wire JSON ([`serde_json::Value`]) end to end; the
//! store never re-shapes a message or session object.
//!
//! One writer connection lives on a dedicated OS thread and writes are
//! serialized through it; reads are spread over a small connection pool and run
//! on `spawn_blocking`, so no SQLite call blocks the async runtime.
//!
//! # Example
//!
//! ```
//! use oc_store::{NewSession, Store};
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), oc_store::StoreError> {
//! let store = Store::open_memory().await?;
//! let session = store
//!     .create_session(NewSession {
//!         id: "ses_example".to_owned(),
//!         parent_id: None,
//!         project_id: "prj_example".to_owned(),
//!         directory: "/tmp/example".to_owned(),
//!         title: Some("example".to_owned()),
//!         agent: None,
//!         model: None,
//!         metadata: None,
//!         permissions: None,
//!         time_created: 1_700_000_000_000,
//!     })
//!     .await?;
//! assert_eq!(session["id"], "ses_example");
//! # Ok(())
//! # }
//! ```
//!
//! [`Store::open`] shows the file-backed variant.

mod agentic;
mod credentials;
mod cursor;
mod db;
mod error;
mod event;
mod form;
mod inbox;
mod instructions;
mod lane;
mod memory;
mod message;
mod permissions;
mod project;
mod schedule;
mod schema;
mod session;
mod session_ext;
mod snapshot;
mod worktrees;

pub use agentic::{GoalPatch, NewGoal, NewObservation, NewRepair, SessionNodeRow};
pub use credentials::{CredentialPatch, NewCredential};
pub use cursor::{Cursor, Order};
pub use db::Store;
pub use error::StoreError;
pub use event::{NewEvent, StoredEvent};
pub use form::NewForm;
pub use inbox::{Delivery, NewInbox};
pub use memory::{MemoryHit, MemoryQuery, NewMemory};
pub use message::{MessagePage, MessageQuery, NewMessage};
pub use project::ProjectPatch;
pub use schedule::{
    NewSchedule, ScheduleAction, ScheduleEnds, ScheduleOutcome, SchedulePatch, ScheduleWhen,
    duration_ms,
};
pub use session::{NewSession, NewSessionTree, SessionPage, SessionPatch, SessionQuery};
pub use snapshot::{NewSnapshot, SnapshotRow};
pub use worktrees::NewWorktree;
