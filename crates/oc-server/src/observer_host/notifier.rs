//! The publishing half of the observer: bus events, repair audits and goal transitions.
//!
//! Compaction *advice* does not travel through [`ObserverNotice`]: the observer
//! engine has no token usage (`ObserverInput` carries none), so the deterministic
//! adviser lives in `oc-runtime` (`observer/advice.rs`) and publishes its own
//! durable `observer.advised` event.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use oc_observer::{GoalSnapshot, ObserverNotice, ObserverNotifier};
use oc_store::{GoalPatch, NewRepair, Store};
use oc_types::{EventEnvelope, EventId, EventLocation};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use super::UNAVAILABLE_NOTICE_INTERVAL_MS;

/// Publishes observer events and persists repair audit rows and goal
/// transitions.
pub struct BusObserverNotifier {
    tx: broadcast::Sender<Arc<EventEnvelope>>,
    store: Arc<Store>,
    directory: Arc<str>,
    last_unavailable: Mutex<HashMap<String, i64>>,
}

impl BusObserverNotifier {
    /// Build a notifier over the server bus and the shared store.
    #[must_use]
    pub fn new(
        tx: broadcast::Sender<Arc<EventEnvelope>>,
        store: Arc<Store>,
        directory: Arc<str>,
    ) -> Self {
        Self {
            tx,
            store,
            directory,
            last_unavailable: Mutex::new(HashMap::new()),
        }
    }

    /// Persist the audit row, then publish `observer.repaired`.
    fn report_repair(
        &self,
        root_id: &str,
        target: &str,
        kind: &'static str,
        reason: &str,
        attempt: u32,
        budget_left: u32,
    ) {
        let record = NewRepair {
            id: repair_id(),
            root_id: root_id.to_owned(),
            target_id: target.to_owned(),
            kind: kind.to_owned(),
            reason: reason.to_owned(),
            args_hash: repair_args_hash(kind, target, reason),
            attempt,
            time_created: oc_types::now_millis(),
        };
        let payload = json!({
            "rootID": root_id,
            "target": target,
            "kind": kind,
            "reason": reason,
            "attempt": attempt,
            "budgetLeft": budget_left,
        });
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                root = %record.root_id,
                "observer.repaired dropped: no tokio runtime for the audit write"
            );
            return;
        };
        let tx = self.tx.clone();
        let store = Arc::clone(&self.store);
        let directory = Arc::clone(&self.directory);
        let log_root = record.root_id.clone();
        handle.spawn(async move {
            // Audit first, event second: a received event implies the repair
            // row exists.
            if let Err(error) = store.record_repair(record).await {
                tracing::warn!(
                    root = %log_root,
                    error = %error,
                    "observer repair audit write failed"
                );
                return;
            }
            publish(&tx, &directory, "observer.repaired", payload);
        });
    }

    /// Persist the goal transition, then publish `goal.updated`.
    ///
    /// A received event therefore implies the goal row carries the
    /// transitioned status, rounds, stalls, progress clock and evidence.
    fn report_goal(
        &self,
        root_id: &str,
        goal_id: &str,
        status: &'static str,
        reason: &str,
        goal: &GoalSnapshot,
    ) {
        // `GoalPatch` has no columns for `wait_until_ms` / `wait_started_ms`;
        // those clocks stay engine-only (see `StoreGoalStore::save`).
        let patch = GoalPatch {
            status: Some(status.to_owned()),
            rounds: Some(goal.rounds),
            stalls: Some(goal.stalls),
            last_progress: Some(goal.last_progress_ms),
            evidence: Some(goal.evidence.clone()),
        };
        let payload = json!({
            "rootID": root_id,
            "goalID": goal_id,
            "status": status,
            "reason": reason,
            "goal": goal,
        });
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                root = %root_id,
                goal = %goal_id,
                "goal.updated dropped: no tokio runtime for the goal write"
            );
            return;
        };
        let tx = self.tx.clone();
        let store = Arc::clone(&self.store);
        let directory = Arc::clone(&self.directory);
        let goal_id = goal_id.to_owned();
        let log_root = root_id.to_owned();
        let log_goal = goal_id.clone();
        handle.spawn(async move {
            // Write first, event second: a received event implies the row was
            // committed.
            match store.update_goal(&goal_id, patch).await {
                Ok(Some(_)) => publish(&tx, &directory, "goal.updated", payload),
                Ok(None) => tracing::warn!(
                    root = %log_root,
                    goal = %log_goal,
                    "goal.updated dropped: goal row no longer exists"
                ),
                Err(error) => tracing::warn!(
                    root = %log_root,
                    goal = %log_goal,
                    %error,
                    "goal update write failed"
                ),
            }
        });
    }
}

impl fmt::Debug for BusObserverNotifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BusObserverNotifier")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

impl ObserverNotifier for BusObserverNotifier {
    fn notify(&self, event: ObserverNotice) {
        match event {
            ObserverNotice::Repaired {
                root_id,
                target,
                kind,
                reason,
                attempt,
                budget_left,
            } => self.report_repair(
                &root_id,
                &target,
                kind.as_str(),
                &reason,
                attempt,
                budget_left,
            ),
            ObserverNotice::Escalated {
                root_id,
                reason,
                budget,
            } => publish(
                &self.tx,
                &self.directory,
                "observer.escalated",
                json!({"rootID": root_id, "reason": reason, "budget": budget}),
            ),
            ObserverNotice::Unavailable { root_id, reason } => {
                let now = oc_types::now_millis();
                let mut seen = self
                    .last_unavailable
                    .lock()
                    .expect("observer unavailable clock");
                if seen
                    .get(&root_id)
                    .is_some_and(|last| now.saturating_sub(*last) < UNAVAILABLE_NOTICE_INTERVAL_MS)
                {
                    return;
                }
                seen.insert(root_id.clone(), now);
                drop(seen);
                publish(
                    &self.tx,
                    &self.directory,
                    "observer.unavailable",
                    json!({"rootID": root_id, "reason": reason}),
                );
            }
            ObserverNotice::Goal {
                root_id,
                goal_id,
                status,
                reason,
                goal,
            } => self.report_goal(&root_id, &goal_id, status.as_str(), &reason, &goal),
            ObserverNotice::Nudged { .. } | ObserverNotice::Inspected { .. } => {
                // The runtime applies nudges/inspects; publishing here would
                // duplicate its effects. Compaction advice is host-side (see the
                // module doc), never an observer notice.
                tracing::debug!("observer nudge/inspect applied by the runtime");
            }
            ObserverNotice::RepairFailed {
                root_id,
                target,
                kind,
                reason,
            } => tracing::warn!(
                root = %root_id,
                target = %target,
                kind = kind.as_str(),
                %reason,
                "observer repair failed"
            ),
        }
    }
}

fn publish(tx: &broadcast::Sender<Arc<EventEnvelope>>, directory: &str, kind: &str, data: Value) {
    let mut envelope = EventEnvelope::now(kind, data);
    envelope.location = Some(EventLocation {
        directory: directory.to_owned(),
    });
    // A send error only means no subscriber is connected.
    let _ = tx.send(Arc::new(envelope));
}

/// Stable hash of the repair context. Verdict arguments are not available to
/// the notifier, so kind, target and reason stand in for them.
fn repair_args_hash(kind: &str, target: &str, reason: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(target.as_bytes());
    hasher.update(b"\0");
    hasher.update(reason.as_bytes());
    hex::encode(hasher.finalize())
}

fn repair_id() -> String {
    // Reuse the event-id generator; `obr_` marks an observer repair row.
    EventId::new().into_inner().replacen("evt_", "obr_", 1)
}
