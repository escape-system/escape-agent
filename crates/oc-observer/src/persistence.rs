//! Host persistence contracts.
//!
//! The observer never touches SQLite. The host persists goal snapshots and the
//! repair audit trail behind these traits; a DB-backed `GoalStore` and
//! `observer_repair` table implement them. Everything here is async so a host
//! can reuse its store's executor.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::BoxFuture;
use crate::error::ObserverError;
use crate::goal::GoalSnapshot;
use crate::verdict::RepairKind;

/// One row of the repair audit trail (`observer_repair`).
///
/// The host writes this in the same transaction as the
/// `observer.repaired` event and the synthetic message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepairRecord {
    /// Root session.
    pub root_id: String,
    /// Session id or tool-call id.
    pub target: Option<String>,
    /// Repair kind.
    pub kind: Option<RepairKind>,
    /// Observer reason.
    pub reason: String,
    /// Kind-specific arguments.
    pub args: Value,
    /// 1-based attempt within the budget window.
    pub attempt: u32,
    /// When the repair ran (epoch ms).
    pub created_ms: i64,
}

impl RepairRecord {
    /// Build a record from a validated repair verdict.
    #[must_use]
    pub fn from_verdict(
        root_id: &str,
        verdict: &crate::verdict::ObserverVerdict,
        attempt: u32,
        now_ms: i64,
    ) -> Self {
        let (kind, args) = verdict
            .repair
            .as_ref()
            .map_or((None, Value::Object(serde_json::Map::new())), |repair| {
                (repair.kind, repair.args.clone())
            });
        Self {
            root_id: root_id.to_owned(),
            target: verdict.target.clone(),
            kind,
            reason: verdict.reason.clone(),
            args,
            attempt,
            created_ms: now_ms,
        }
    }
}

/// Goal persistence (one active goal per root).
pub trait GoalStore: Send + Sync {
    /// Load the goal for a root, if any.
    fn load<'a>(
        &'a self,
        root_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<GoalSnapshot>, ObserverError>>;

    /// Insert or update a goal snapshot.
    fn save<'a>(&'a self, goal: &'a GoalSnapshot) -> BoxFuture<'a, Result<(), ObserverError>>;
}

/// Repair audit persistence.
pub trait RepairLog: Send + Sync {
    /// Append one repair row.
    fn record<'a>(&'a self, record: &'a RepairRecord) -> BoxFuture<'a, Result<(), ObserverError>>;
}
