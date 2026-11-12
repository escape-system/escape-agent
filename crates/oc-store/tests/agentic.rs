//! Integration tests for the subagent tree, goals, observations and repairs.

use oc_store::{
    GoalPatch, NewEvent, NewGoal, NewObservation, NewRepair, NewSession, NewSessionTree, Store,
    StoreError,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn session(
    id: &str,
    parent: Option<&str>,
    created: i64,
    agent: Option<&str>,
    model: Option<Value>,
) -> NewSession {
    NewSession {
        id: id.to_owned(),
        parent_id: parent.map(str::to_owned),
        project_id: "prj_1".to_owned(),
        directory: "/tmp/one".to_owned(),
        title: None,
        agent: agent.map(str::to_owned),
        model,
        metadata: None,
        permissions: None,
        time_created: created,
    }
}

fn goal(id: &str, created: i64) -> NewGoal {
    NewGoal {
        id: id.to_owned(),
        root_id: "ses_root".to_owned(),
        objective: "ship the store".to_owned(),
        criteria: vec!["tests pass".to_owned(), "docs updated".to_owned()],
        model: Some("anthropic/claude".to_owned()),
        time_created: created,
    }
}

fn observation(id: &str, started: i64) -> NewObservation {
    NewObservation {
        id: id.to_owned(),
        root_id: "ses_root".to_owned(),
        session_id: "ses_root".to_owned(),
        tool_call_id: format!("call_{id}"),
        tool: "shell".to_owned(),
        why: "run the focused test".to_owned(),
        expect_s: 90,
        intervene_s: 150,
        started,
        parent_call: None,
        synthetic: false,
    }
}

fn repair(id: &str, created: i64) -> NewRepair {
    NewRepair {
        id: id.to_owned(),
        root_id: "ses_root".to_owned(),
        target_id: "ses_root".to_owned(),
        kind: "retry_tool".to_owned(),
        reason: "timed out".to_owned(),
        args_hash: "abc123".to_owned(),
        attempt: 1,
        time_created: created,
    }
}

#[tokio::test]
async fn session_tree_rows_and_status() {
    let store = Store::open_memory().await.expect("store");
    store
        .create_session(session("ses_root", None, 1_000, None, None))
        .await
        .expect("root");
    store
        .create_session(session(
            "ses_child",
            Some("ses_root"),
            2_000,
            Some("general"),
            Some(json!({ "providerID": "anthropic", "id": "claude", "variant": "high" })),
        ))
        .await
        .expect("child");
    store
        .create_session(session("ses_grand", Some("ses_child"), 3_000, None, None))
        .await
        .expect("grand");

    let root = store
        .session_node("ses_root")
        .await
        .expect("node")
        .expect("root row");
    assert_eq!(root.id, "ses_root");
    assert_eq!(root.parent_id, None);
    assert_eq!(root.root_id, "ses_root");
    assert_eq!(root.depth, 0);
    assert_eq!(root.role, "primary");
    assert!(root.spawn_allowed);
    assert_eq!(root.child_count, 1);
    assert_eq!(root.agent, "build");
    assert_eq!(root.model, None);
    assert_eq!(root.status, "idle");

    let child = store
        .session_node("ses_child")
        .await
        .expect("node")
        .expect("child row");
    assert_eq!(child.parent_id.as_deref(), Some("ses_root"));
    assert_eq!(child.root_id, "ses_root");
    assert_eq!(child.depth, 1);
    assert_eq!(child.agent, "general");
    assert_eq!(child.model.as_deref(), Some("anthropic/claude#high"));
    assert_eq!(child.child_count, 1);

    let grand = store
        .session_node("ses_grand")
        .await
        .expect("node")
        .expect("grand row");
    assert_eq!(grand.depth, 2);
    assert_eq!(grand.root_id, "ses_root");
    assert!(
        store
            .session_node("ses_missing")
            .await
            .expect("node")
            .is_none()
    );

    let children = store.children("ses_root").await.expect("children");
    assert_eq!(
        children
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        vec!["ses_child"]
    );
    assert!(
        store
            .children("ses_grand")
            .await
            .expect("children")
            .is_empty()
    );

    store
        .append_event(NewEvent {
            id: "evt_archive".to_owned(),
            aggregate_id: "ses_root".to_owned(),
            kind: "session.archived".to_owned(),
            data: json!({ "sessionID": "ses_root" }),
            durable: true,
            version: 1,
            created: 4_000,
        })
        .await
        .expect("archive");
    assert_eq!(
        store
            .session_node("ses_root")
            .await
            .expect("node")
            .expect("root row")
            .status,
        "archived"
    );
}

#[tokio::test]
async fn child_count_increment_clamps_and_reports_missing() {
    let store = Store::open_memory().await.expect("store");
    store
        .create_session(session("ses_root", None, 1_000, None, None))
        .await
        .expect("root");

    store
        .increment_child_count("ses_root", 2)
        .await
        .expect("increment");
    assert_eq!(
        store
            .session_node("ses_root")
            .await
            .expect("node")
            .expect("row")
            .child_count,
        2
    );

    store
        .increment_child_count("ses_root", -10)
        .await
        .expect("clamped decrement");
    assert_eq!(
        store
            .session_node("ses_root")
            .await
            .expect("node")
            .expect("row")
            .child_count,
        0
    );

    let missing = store.increment_child_count("ses_missing", 1).await;
    assert!(matches!(missing, Err(StoreError::NotFound(_))));
}

#[tokio::test]
async fn goals_lifecycle_and_active_conflict() {
    let store = Store::open_memory().await.expect("store");
    let created = store
        .set_goal(goal("gol_1", 1_000))
        .await
        .expect("set goal");
    assert_eq!(created["id"], "gol_1");
    assert_eq!(created["rootID"], "ses_root");
    assert_eq!(created["status"], "active");
    assert_eq!(created["rounds"], 0);
    assert_eq!(created["stalls"], 0);
    assert_eq!(created["evidence"], json!([]));
    assert_eq!(created["criteria"], json!(["tests pass", "docs updated"]));
    // `done`/`total` are derived, emitted as integers: nothing is done while
    // the goal is active, and `total` counts the criteria.
    assert!(created["done"].is_u64());
    assert!(created["total"].is_u64());
    assert_eq!(created["done"], json!(0));
    assert_eq!(created["total"], json!(2));
    assert_eq!(created["model"], "anthropic/claude");
    assert_eq!(created["time"]["created"], 1_000);

    let conflict = store.set_goal(goal("gol_other", 1_500)).await;
    assert!(matches!(conflict, Err(StoreError::Conflict(_))));

    let updated = store
        .update_goal(
            "gol_1",
            GoalPatch {
                status: Some("paused".to_owned()),
                rounds: Some(2),
                stalls: Some(1),
                last_progress: Some(1_400),
                evidence: Some(vec!["call_1".to_owned()]),
            },
        )
        .await
        .expect("update")
        .expect("goal exists");
    assert_eq!(updated["status"], "paused");
    assert_eq!(updated["rounds"], 2);
    assert_eq!(updated["stalls"], 1);
    assert_eq!(updated["lastProgress"], 1_400);
    assert_eq!(updated["evidence"], json!(["call_1"]));

    let current = store
        .get_goal("ses_root")
        .await
        .expect("get")
        .expect("goal");
    assert_eq!(current["id"], "gol_1");

    let second = store.set_goal(goal("gol_2", 2_000)).await.expect("second");
    assert_eq!(second["status"], "active");
    let current = store
        .get_goal("ses_root")
        .await
        .expect("get")
        .expect("goal");
    assert_eq!(current["id"], "gol_2");

    let reactivate = store
        .update_goal(
            "gol_1",
            GoalPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(reactivate, Err(StoreError::Conflict(_))));

    let goals = store.list_goals("ses_root").await.expect("list");
    assert_eq!(goals.len(), 2);
    assert_eq!(goals[0]["id"], "gol_2");
    assert!(
        store
            .get_goal("ses_missing")
            .await
            .expect("get missing")
            .is_none()
    );
    assert!(
        store
            .update_goal("gol_missing", GoalPatch::default())
            .await
            .expect("update missing")
            .is_none()
    );
    assert!(
        store
            .update_goal("gol_2", GoalPatch::default())
            .await
            .expect("touch")
            .is_some()
    );
}

#[tokio::test]
async fn accepted_goal_reports_derived_done_and_total() {
    let store = Store::open_memory().await.expect("store");
    store.set_goal(goal("gol_1", 1_000)).await.expect("goal");
    let paused = store
        .update_goal(
            "gol_1",
            GoalPatch {
                status: Some("paused".to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("pause")
        .expect("goal exists");
    assert_eq!(paused["done"], json!(0), "a paused goal claims no progress");
    assert_eq!(paused["total"], json!(2));

    // Accepting the goal is the only way criteria count as done: the row is
    // still the same one and `total` does not move, but `done` becomes `total`.
    let accepted = store
        .update_goal(
            "gol_1",
            GoalPatch {
                status: Some("done".to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("accept")
        .expect("goal exists");
    assert_eq!(accepted["status"], "done");
    assert_eq!(accepted["done"], json!(2));
    assert_eq!(accepted["total"], json!(2));

    // `get_goal` and the `list_goals` projection derive the same fields.
    let current = store
        .get_goal("ses_root")
        .await
        .expect("get")
        .expect("goal");
    assert_eq!(current["done"], json!(2));
    assert_eq!(current["total"], json!(2));
    let goals = store.list_goals("ses_root").await.expect("list accepted");
    assert_eq!(goals[0]["done"], json!(2));
    assert_eq!(goals[0]["total"], json!(2));
}

#[tokio::test]
async fn observations_record_settle_and_list() {
    let store = Store::open_memory().await.expect("store");
    store
        .record_observation(observation("obs_1", 1_000))
        .await
        .expect("record");

    let listed = store.list_observations("ses_root", 10).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], "obs_1");
    assert_eq!(listed[0]["rootID"], "ses_root");
    assert_eq!(listed[0]["sessionID"], "ses_root");
    assert_eq!(listed[0]["toolCallID"], "call_obs_1");
    assert_eq!(listed[0]["expectSeconds"], 90);
    assert_eq!(listed[0]["interveneSeconds"], 150);
    assert_eq!(listed[0]["state"], "running");
    assert_eq!(listed[0]["synthetic"], false);
    assert!(listed[0].get("settled").is_none());

    assert!(
        store
            .settle_observation("obs_1", "ok", Some("exit 0"), 2_000)
            .await
            .expect("settle")
    );
    assert!(
        !store
            .settle_observation("obs_1", "error", None, 3_000)
            .await
            .expect("second settle")
    );
    let settled = store
        .list_observations("ses_root", 10)
        .await
        .expect("list settled");
    assert_eq!(settled[0]["state"], "ok");
    assert_eq!(settled[0]["outcome"], "exit 0");
    assert_eq!(settled[0]["settled"], 2_000);

    let mut synthetic = observation("obs_2", 3_000);
    synthetic.synthetic = true;
    synthetic.parent_call = Some("call_0".to_owned());
    store
        .record_observation(synthetic)
        .await
        .expect("synthetic");
    let latest = store
        .list_observations("ses_root", 1)
        .await
        .expect("limited");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0]["id"], "obs_2");
    assert_eq!(latest[0]["synthetic"], true);
    assert_eq!(latest[0]["parentCall"], "call_0");

    let duplicate = store.record_observation(observation("obs_1", 4_000)).await;
    assert!(matches!(duplicate, Err(StoreError::Conflict(_))));
    assert!(
        !store
            .settle_observation("obs_missing", "ok", None, 5_000)
            .await
            .expect("settle missing")
    );
}

#[tokio::test]
async fn repairs_record_and_list() {
    let store = Store::open_memory().await.expect("store");
    store
        .record_repair(repair("rep_1", 1_000))
        .await
        .expect("first");
    store
        .record_repair(repair("rep_2", 2_000))
        .await
        .expect("second");

    let listed = store.list_repairs("ses_root", 10).await.expect("list");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["id"], "rep_2");
    assert_eq!(listed[0]["rootID"], "ses_root");
    assert_eq!(listed[0]["targetID"], "ses_root");
    assert_eq!(listed[0]["kind"], "retry_tool");
    assert_eq!(listed[0]["reason"], "timed out");
    assert_eq!(listed[0]["argsHash"], "abc123");
    assert_eq!(listed[0]["attempt"], 1);
    assert_eq!(listed[0]["time"]["created"], 2_000);

    let limited = store.list_repairs("ses_root", 1).await.expect("limited");
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0]["id"], "rep_2");
    assert!(
        store
            .list_repairs("ses_other", 10)
            .await
            .expect("empty")
            .is_empty()
    );

    let duplicate = store.record_repair(repair("rep_1", 3_000)).await;
    assert!(matches!(duplicate, Err(StoreError::Conflict(_))));
}

#[tokio::test]
async fn agentic_rows_survive_reopen() {
    let directory = TempDir::new().expect("temp dir");
    let path = directory.path().join("agentic.db");
    let store = Store::open(&path).await.expect("open");
    store
        .create_session(session("ses_root", None, 1_000, None, None))
        .await
        .expect("root");
    store.set_goal(goal("gol_1", 1_000)).await.expect("goal");
    store
        .update_goal(
            "gol_1",
            GoalPatch {
                rounds: Some(3),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    store
        .record_observation(observation("obs_1", 1_000))
        .await
        .expect("observation");
    store
        .record_repair(repair("rep_1", 1_000))
        .await
        .expect("repair");
    drop(store);

    let reopened = Store::open(&path).await.expect("reopen");
    assert_eq!(
        reopened
            .session_node("ses_root")
            .await
            .expect("node")
            .expect("row")
            .id,
        "ses_root"
    );
    let goal = reopened
        .get_goal("ses_root")
        .await
        .expect("goal")
        .expect("goal row");
    assert_eq!(goal["rounds"], 3);
    let observations = reopened
        .list_observations("ses_root", 10)
        .await
        .expect("observations");
    assert_eq!(observations[0]["state"], "running");
    assert!(
        reopened
            .settle_observation("obs_1", "ok", Some("ok"), 2_000)
            .await
            .expect("settle after reopen")
    );
    assert_eq!(
        reopened
            .list_repairs("ses_root", 10)
            .await
            .expect("repairs")
            .len(),
        1
    );
    assert_eq!(
        reopened.list_goals("ses_root").await.expect("goals").len(),
        1
    );
}

#[tokio::test]
async fn create_session_with_tree_sets_columns_and_counts() {
    let store = Store::open_memory().await.expect("store");
    store
        .create_session(session("ses_root", None, 1_000, None, None))
        .await
        .expect("root");

    let child = store
        .create_session_with_tree(
            session("ses_child", Some("ses_root"), 2_000, Some("general"), None),
            NewSessionTree {
                root_id: "ses_root".to_owned(),
                depth: 1,
                role: "subagent".to_owned(),
                spawn_allowed: false,
            },
        )
        .await
        .expect("child");
    assert_eq!(child["id"], "ses_child");

    let node = store
        .session_node("ses_child")
        .await
        .expect("node")
        .expect("child row");
    assert_eq!(node.root_id, "ses_root");
    assert_eq!(node.depth, 1);
    assert_eq!(node.role, "subagent");
    assert!(!node.spawn_allowed);
    let root = store
        .session_node("ses_root")
        .await
        .expect("node")
        .expect("root row");
    assert_eq!(root.child_count, 1);

    store
        .create_session_with_tree(
            session("ses_grand", Some("ses_child"), 3_000, None, None),
            NewSessionTree {
                root_id: "ses_root".to_owned(),
                depth: 2,
                role: "coordinator".to_owned(),
                spawn_allowed: true,
            },
        )
        .await
        .expect("grandchild");
    let grand = store
        .session_node("ses_grand")
        .await
        .expect("node")
        .expect("grand row");
    assert_eq!(grand.depth, 2);
    assert_eq!(grand.role, "coordinator");
    assert_eq!(grand.root_id, "ses_root");
    assert_eq!(
        store
            .session_node("ses_child")
            .await
            .expect("node")
            .expect("child row")
            .child_count,
        1
    );
}

#[tokio::test]
async fn create_session_with_tree_rejects_invalid_input() {
    let store = Store::open_memory().await.expect("store");
    store
        .create_session(session("ses_root", None, 1_000, None, None))
        .await
        .expect("root");
    store
        .create_session(session("ses_child", Some("ses_root"), 2_000, None, None))
        .await
        .expect("child");

    let duplicate = store
        .create_session_with_tree(
            session("ses_child", Some("ses_root"), 4_000, None, None),
            NewSessionTree {
                root_id: "ses_root".to_owned(),
                depth: 1,
                role: "subagent".to_owned(),
                spawn_allowed: true,
            },
        )
        .await;
    assert!(matches!(duplicate, Err(StoreError::Conflict(_))));
    assert_eq!(
        store
            .session_node("ses_root")
            .await
            .expect("node")
            .expect("root row")
            .child_count,
        1,
        "a failed create leaves the parent count untouched"
    );

    let wrong_depth = store
        .create_session_with_tree(
            session("ses_bad_depth", Some("ses_child"), 5_000, None, None),
            NewSessionTree {
                root_id: "ses_root".to_owned(),
                depth: 5,
                role: "subagent".to_owned(),
                spawn_allowed: true,
            },
        )
        .await;
    assert!(matches!(wrong_depth, Err(StoreError::Invalid(_))));
    assert_eq!(
        store
            .session_node("ses_child")
            .await
            .expect("node")
            .expect("child row")
            .child_count,
        0
    );

    let empty_root = store
        .create_session_with_tree(
            session("ses_bad_root", None, 6_000, None, None),
            NewSessionTree {
                root_id: "   ".to_owned(),
                depth: 0,
                role: "primary".to_owned(),
                spawn_allowed: true,
            },
        )
        .await;
    assert!(matches!(empty_root, Err(StoreError::Invalid(_))));

    let bad_role = store
        .create_session_with_tree(
            session("ses_bad_role", None, 7_000, None, None),
            NewSessionTree {
                root_id: "ses_bad_role".to_owned(),
                depth: 0,
                role: "wizard".to_owned(),
                spawn_allowed: true,
            },
        )
        .await;
    assert!(matches!(bad_role, Err(StoreError::Invalid(_))));
}

#[tokio::test]
async fn set_session_policy_updates_and_reports_missing() {
    let store = Store::open_memory().await.expect("store");
    store
        .create_session(session("ses_root", None, 1_000, None, None))
        .await
        .expect("root");

    store
        .set_session_policy("ses_root", "coordinator", false)
        .await
        .expect("policy");
    let node = store
        .session_node("ses_root")
        .await
        .expect("node")
        .expect("root row");
    assert_eq!(node.role, "coordinator");
    assert!(!node.spawn_allowed);

    let missing = store
        .set_session_policy("ses_missing", "primary", true)
        .await;
    assert!(matches!(missing, Err(StoreError::NotFound(_))));
    let invalid = store.set_session_policy("ses_root", "wizard", true).await;
    assert!(matches!(invalid, Err(StoreError::Invalid(_))));
}
