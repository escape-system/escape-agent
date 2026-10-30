//! [`oc_observer::GoalStore`] over the SQLite store.
//!
//! The runtime ships this adapter so a host can attach the observer without
//! writing its own goal persistence. Goal rows keep criteria and evidence;
//! [`oc_observer::GoalSnapshot`] derives `total` from criteria and `done` from
//! the evidence list. Other snapshot fields (wait clocks, `known_ids`) are
//! ephemeral engine bookkeeping and are not stored.
//!
//! [`load`](oc_observer::GoalStore::load) also records which root owns each
//! goal id. [`save`](oc_observer::GoalStore::save) needs that mapping because a
//! snapshot does not carry its root; a snapshot that was never loaded from this
//! adapter cannot be persisted and is reported as an executor error.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oc_observer::{BoxFuture, GoalPolicy, GoalSnapshot, GoalStatus, GoalStore, ObserverError};
use oc_store::{GoalPatch, NewGoal, Store, StoreError};
use serde_json::Value;

/// SQLite-backed goal store used by [`crate::Runtime`].
pub struct StoreGoalStore {
    store: Arc<Store>,
    policy: GoalPolicy,
    roots: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for StoreGoalStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoreGoalStore")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl StoreGoalStore {
    /// Build an adapter using [`GoalPolicy::default`].
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            policy: GoalPolicy::default(),
            roots: Mutex::new(HashMap::new()),
        }
    }

    /// Override the policy copied into every loaded snapshot.
    #[must_use]
    pub fn with_policy(mut self, policy: GoalPolicy) -> Self {
        self.policy = policy;
        self
    }
}

fn map_error(error: &StoreError) -> ObserverError {
    ObserverError::Executor(error.to_string())
}

fn parse_status(raw: &str) -> GoalStatus {
    match raw {
        "paused" => GoalStatus::Paused,
        "blocked" => GoalStatus::Blocked,
        "done" => GoalStatus::Done,
        "failed" => GoalStatus::Failed,
        "cancelled" => GoalStatus::Cancelled,
        _ => GoalStatus::Active,
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn snapshot_from_value(value: &Value, policy: GoalPolicy) -> GoalSnapshot {
    let criteria = string_list(value.get("criteria"));
    let evidence = string_list(value.get("evidence"));
    let total = u32::try_from(criteria.len()).unwrap_or(u32::MAX);
    let done = u32::try_from(evidence.len()).unwrap_or(u32::MAX).min(total);
    GoalSnapshot {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        objective: value
            .get("objective")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        criteria,
        done,
        total,
        status: parse_status(
            value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("active"),
        ),
        rounds: value
            .get("rounds")
            .and_then(Value::as_u64)
            .map_or(0, |rounds| u32::try_from(rounds).unwrap_or(u32::MAX)),
        stalls: value
            .get("stalls")
            .and_then(Value::as_u64)
            .map_or(0, |stalls| u32::try_from(stalls).unwrap_or(u32::MAX)),
        evidence,
        wait_until_ms: None,
        wait_started_ms: None,
        last_progress_ms: value
            .get("lastProgress")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        known_ids: Vec::new(),
        policy,
    }
}

/// Convert a stored goal row into the observer's snapshot view.
///
/// Prompt assembly only needs the display fields; the engine bookkeeping
/// (`known_ids`, wait clocks) stays empty and the default policy is used.
pub(crate) fn snapshot(value: &Value) -> GoalSnapshot {
    snapshot_from_value(value, GoalPolicy::default())
}

impl GoalStore for StoreGoalStore {
    fn load<'a>(
        &'a self,
        root_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<GoalSnapshot>, ObserverError>> {
        Box::pin(async move {
            let value = self
                .store
                .get_goal(root_id)
                .await
                .map_err(|error| map_error(&error))?;
            let Some(value) = value else {
                return Ok(None);
            };
            let snapshot = snapshot_from_value(&value, self.policy);
            if !snapshot.id.is_empty() {
                self.roots
                    .lock()
                    .expect("goal roots")
                    .insert(snapshot.id.clone(), root_id.to_owned());
            }
            Ok(Some(snapshot))
        })
    }

    fn save<'a>(&'a self, goal: &'a GoalSnapshot) -> BoxFuture<'a, Result<(), ObserverError>> {
        Box::pin(async move {
            let root = self
                .roots
                .lock()
                .expect("goal roots")
                .get(&goal.id)
                .cloned()
                .ok_or_else(|| {
                    ObserverError::Executor(format!(
                        "goal {} was never loaded; its root is unknown",
                        goal.id
                    ))
                })?;
            let patch = GoalPatch {
                status: Some(goal.status.as_str().to_owned()),
                rounds: Some(goal.rounds),
                stalls: Some(goal.stalls),
                last_progress: Some(goal.last_progress_ms),
                evidence: Some(goal.evidence.clone()),
            };
            let updated = self
                .store
                .update_goal(&goal.id, patch)
                .await
                .map_err(|error| map_error(&error))?;
            if updated.is_some() {
                return Ok(());
            }

            self.store
                .set_goal(NewGoal {
                    id: goal.id.clone(),
                    root_id: root,
                    objective: goal.objective.clone(),
                    criteria: goal.criteria.clone(),
                    model: None,
                    time_created: oc_types::now_millis(),
                })
                .await
                .map_err(|error| map_error(&error))?;
            if goal.status != GoalStatus::Active {
                self.store
                    .update_goal(
                        &goal.id,
                        GoalPatch {
                            status: Some(goal.status.as_str().to_owned()),
                            ..GoalPatch::default()
                        },
                    )
                    .await
                    .map_err(|error| map_error(&error))?;
            }
            Ok(())
        })
    }
}
