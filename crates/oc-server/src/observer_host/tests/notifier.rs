//! Notifier publishing: repair audits, rate limits, escalations and goal transitions.

use std::sync::Arc;
use std::time::Duration;

use oc_observer::{GoalSnapshot, GoalStatus, ObserverNotice, ObserverNotifier, RepairKind};
use oc_store::Store;
use oc_types::EventEnvelope;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::broadcast;

use super::super::BusObserverNotifier;
use super::{insert_goal, store_at};

async fn notifier_fixture() -> (
    BusObserverNotifier,
    broadcast::Receiver<Arc<EventEnvelope>>,
    Arc<Store>,
    TempDir,
) {
    let root = TempDir::new().expect("tempdir");
    let store = store_at(root.path()).await;
    let (tx, rx) = broadcast::channel(16);
    let notifier =
        BusObserverNotifier::new(tx, Arc::clone(&store), Arc::from("/tmp/observer-work"));
    (notifier, rx, store, root)
}

async fn next_event(rx: &mut broadcast::Receiver<Arc<EventEnvelope>>) -> Arc<EventEnvelope> {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("event within timeout")
        .expect("event")
}

#[tokio::test]
async fn repaired_persists_then_publishes() {
    let (notifier, mut rx, store, _root) = notifier_fixture().await;
    notifier.notify(ObserverNotice::Repaired {
        root_id: "ses_root".to_owned(),
        target: "ses_child".to_owned(),
        kind: RepairKind::CompactContext,
        reason: "child is looping".to_owned(),
        attempt: 1,
        budget_left: 4,
    });
    let event = next_event(&mut rx).await;
    assert_eq!(event.kind, "observer.repaired");
    assert_eq!(event.data["rootID"], "ses_root");
    assert_eq!(event.data["target"], "ses_child");
    assert_eq!(event.data["kind"], "compact_context");
    assert_eq!(event.data["attempt"], 1);
    assert_eq!(event.data["budgetLeft"], 4);
    assert_eq!(
        event
            .location
            .as_ref()
            .map(|location| location.directory.as_str()),
        Some("/tmp/observer-work")
    );
    assert!(event.durable.is_none());

    // The event is published after the audit row, so it must exist now.
    let rows = store.list_repairs("ses_root", 10).await.expect("repairs");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["kind"], "compact_context");
    assert_eq!(rows[0]["targetID"], "ses_child");
    assert_eq!(rows[0]["attempt"], 1);
    assert!(
        rows[0]["argsHash"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty())
    );
}

#[tokio::test]
async fn unavailable_is_rate_limited() {
    let (notifier, mut rx, _store, _root) = notifier_fixture().await;
    notifier.notify(ObserverNotice::Unavailable {
        root_id: "ses_root".to_owned(),
        reason: "timeout".to_owned(),
    });
    let event = next_event(&mut rx).await;
    assert_eq!(event.kind, "observer.unavailable");
    assert_eq!(event.data["reason"], "timeout");

    notifier.notify(ObserverNotice::Unavailable {
        root_id: "ses_root".to_owned(),
        reason: "timeout again".to_owned(),
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err(),
        "second unavailable notice must be rate-limited"
    );

    // A different root has its own window.
    notifier.notify(ObserverNotice::Unavailable {
        root_id: "ses_other".to_owned(),
        reason: "timeout".to_owned(),
    });
    let event = next_event(&mut rx).await;
    assert_eq!(event.kind, "observer.unavailable");
    assert_eq!(event.data["rootID"], "ses_other");
}

#[tokio::test]
async fn escalated_publishes_budget() {
    let (notifier, mut rx, _store, _root) = notifier_fixture().await;
    notifier.notify(ObserverNotice::Escalated {
        root_id: "ses_root".to_owned(),
        reason: "budget exhausted".to_owned(),
        budget: oc_observer::BudgetState {
            root_used: 5,
            root_limit: 5,
            targets: Vec::new(),
            exhausted: true,
        },
    });
    let event = next_event(&mut rx).await;
    assert_eq!(event.kind, "observer.escalated");
    assert_eq!(event.data["budget"]["exhausted"], true);
    assert_eq!(event.data["budget"]["root_used"], 5);
}

#[tokio::test]
async fn goal_publishes_snapshot() {
    let (notifier, mut rx, store, _root) = notifier_fixture().await;
    insert_goal(&store, "gol_1", "ses_root").await;
    let goal = GoalSnapshot {
        id: "gol_1".to_owned(),
        objective: "ship it".to_owned(),
        status: GoalStatus::Done,
        done: 1,
        total: 1,
        ..GoalSnapshot::default()
    };
    notifier.notify(ObserverNotice::Goal {
        root_id: "ses_root".to_owned(),
        goal_id: "gol_1".to_owned(),
        status: GoalStatus::Done,
        reason: "criteria met".to_owned(),
        goal,
    });
    let event = next_event(&mut rx).await;
    assert_eq!(event.kind, "goal.updated");
    assert_eq!(event.data["rootID"], "ses_root");
    assert_eq!(event.data["goalID"], "gol_1");
    assert_eq!(event.data["status"], "done");
    assert_eq!(event.data["reason"], "criteria met");
    assert_eq!(event.data["goal"]["objective"], "ship it");
    assert_eq!(
        event
            .location
            .as_ref()
            .map(|location| location.directory.as_str()),
        Some("/tmp/observer-work")
    );
}

#[tokio::test]
async fn goal_transition_persists_then_publishes() {
    let (notifier, mut rx, store, _root) = notifier_fixture().await;
    insert_goal(&store, "gol_1", "ses_root").await;

    let goal = GoalSnapshot {
        id: "gol_1".to_owned(),
        objective: "ship it".to_owned(),
        criteria: vec!["tests pass".to_owned()],
        done: 1,
        total: 1,
        status: GoalStatus::Done,
        rounds: 4,
        stalls: 2,
        evidence: vec!["cal_1".to_owned()],
        last_progress_ms: 1_700_000_000_000,
        ..GoalSnapshot::default()
    };
    notifier.notify(ObserverNotice::Goal {
        root_id: "ses_root".to_owned(),
        goal_id: "gol_1".to_owned(),
        status: GoalStatus::Done,
        reason: "criteria met".to_owned(),
        goal,
    });

    // The event is published after the update, so the row must already
    // carry the transitioned state when the event arrives.
    let event = next_event(&mut rx).await;
    assert_eq!(event.kind, "goal.updated");
    assert_eq!(event.data["goalID"], "gol_1");
    assert_eq!(event.data["status"], "done");
    assert_eq!(event.data["goal"]["rounds"], 4);
    assert_eq!(event.data["goal"]["stalls"], 2);

    let stored = store
        .get_goal("ses_root")
        .await
        .expect("goal read")
        .expect("goal row");
    assert_eq!(stored["id"], "gol_1");
    assert_eq!(stored["status"], "done");
    assert_eq!(stored["rounds"], 4);
    assert_eq!(stored["stalls"], 2);
    assert_eq!(stored["lastProgress"], 1_700_000_000_000_i64);
    assert_eq!(stored["evidence"], json!(["cal_1"]));

    assert!(
        tokio::time::timeout(Duration::from_millis(150), rx.recv())
            .await
            .is_err(),
        "exactly one goal.updated event per transition"
    );
}

#[tokio::test]
async fn goal_transition_without_row_publishes_nothing() {
    let (notifier, mut rx, _store, _root) = notifier_fixture().await;
    let goal = GoalSnapshot {
        id: "gol_missing".to_owned(),
        objective: "ship it".to_owned(),
        status: GoalStatus::Done,
        done: 1,
        total: 1,
        rounds: 3,
        ..GoalSnapshot::default()
    };
    notifier.notify(ObserverNotice::Goal {
        root_id: "ses_root".to_owned(),
        goal_id: "gol_missing".to_owned(),
        status: GoalStatus::Done,
        reason: "criteria met".to_owned(),
        goal,
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(250), rx.recv())
            .await
            .is_err(),
        "a missing goal row must not publish goal.updated"
    );
}
