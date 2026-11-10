//! Durable idle reason: the observer must be able to tell a user stop from any
//! other interrupt without changing the existing `mark_session_idle` API.

use crate::support::{new_session, seed_session};
use oc_store::{NewSession, NewSessionTree, Store};
use tempfile::TempDir;

async fn seed_child(store: &Store, root: &str, child: &str) {
    store
        .create_session_with_tree(
            NewSession {
                parent_id: Some(root.to_owned()),
                ..new_session(child)
            },
            NewSessionTree {
                root_id: root.to_owned(),
                depth: 1,
                role: "subagent".to_owned(),
                spawn_allowed: true,
            },
        )
        .await
        .expect("child session");
}

#[tokio::test]
async fn interrupted_reason_is_durable_and_visible() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_root").await;

    store
        .mark_session_idle_with_reason("ses_root", 2_000, "interrupted", Some("user"))
        .await
        .expect("mark idle");

    let node = store
        .session_node("ses_root")
        .await
        .expect("node")
        .expect("row");
    assert_eq!(node.status, "interrupted");
    assert_eq!(node.idle_reason.as_deref(), Some("user"));
}

#[tokio::test]
async fn a_step_limited_success_keeps_its_reason_but_any_other_reason_does_not() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_root").await;

    // A step-limited turn still ends `succeeded`; the strict step-limit
    // grammar is the one reason that survives next to it.
    let limited = oc_types::step_limit_reason(7);
    store
        .mark_session_idle_with_reason("ses_root", 2_000, "succeeded", Some(&limited))
        .await
        .expect("step-limited idle");
    let node = store
        .session_node("ses_root")
        .await
        .expect("node")
        .expect("row");
    assert_eq!(node.status, "completed", "a step limit is not a failure");
    assert_eq!(
        node.idle_reason.as_deref(),
        Some(limited.as_str()),
        "the step-limit reason round-trips through the read filter"
    );
    assert_eq!(oc_types::parse_step_limit(&limited), Some(7));

    // Any other reason on a succeeded row is still dropped: the
    // interrupted-only rule stays intact for everything else.
    store
        .mark_session_idle_with_reason("ses_root", 3_000, "succeeded", Some("user"))
        .await
        .expect("unrelated reason");
    let node = store
        .session_node("ses_root")
        .await
        .expect("node")
        .expect("row");
    assert_eq!(node.status, "completed");
    assert_eq!(
        node.idle_reason, None,
        "a succeeded row with an unrelated reason still stores NULL"
    );

    // A near-miss of the grammar is not a step limit either.
    store
        .mark_session_idle_with_reason(
            "ses_root",
            4_000,
            "succeeded",
            Some("step limit reached after 7 steps and more"),
        )
        .await
        .expect("near miss");
    let node = store
        .session_node("ses_root")
        .await
        .expect("node")
        .expect("row");
    assert_eq!(
        node.idle_reason, None,
        "the grammar is strict; a trailing sentence is not a step limit"
    );
}

/// The runtime's internal `step_limit` outcome is durable next to its reason:
/// both columns round-trip, and the derived node status is `completed` because
/// the model did answer — the budget ended the turn, not a failure.
#[tokio::test]
async fn a_step_limited_outcome_keeps_its_reason() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_root").await;

    let reason = oc_types::step_limit_reason(3);
    store
        .mark_session_idle_with_reason("ses_root", 2_000, "step_limit", Some(&reason))
        .await
        .expect("step-limited idle");

    let session = store
        .get_session("ses_root")
        .await
        .expect("session")
        .expect("row");
    assert_eq!(
        session["outcome"], "step_limit",
        "the internal outcome literal is stored verbatim"
    );

    let node = store
        .session_node("ses_root")
        .await
        .expect("node")
        .expect("row");
    assert_eq!(
        node.status, "completed",
        "a step-limited turn is a completed turn"
    );
    assert_eq!(
        node.idle_reason.as_deref(),
        Some(reason.as_str()),
        "the reason survives next to the step_limit outcome"
    );

    // A succeeded row with an unrelated reason still stores NULL.
    seed_session(&store, "ses_plain").await;
    store
        .mark_session_idle_with_reason("ses_plain", 2_000, "succeeded", Some("user"))
        .await
        .expect("plain success");
    let node = store
        .session_node("ses_plain")
        .await
        .expect("node")
        .expect("row");
    assert_eq!(node.status, "completed");
    assert_eq!(node.idle_reason, None);

    // Any interrupted reason is kept, not just the `user` stop.
    seed_session(&store, "ses_stopped").await;
    store
        .mark_session_idle_with_reason("ses_stopped", 2_000, "interrupted", Some("budget"))
        .await
        .expect("interrupted");
    let node = store
        .session_node("ses_stopped")
        .await
        .expect("node")
        .expect("row");
    assert_eq!(node.status, "interrupted");
    assert_eq!(node.idle_reason.as_deref(), Some("budget"));
}

#[tokio::test]
async fn a_non_interrupted_outcome_clears_the_reason() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_root").await;

    store
        .mark_session_idle_with_reason("ses_root", 2_000, "interrupted", Some("user"))
        .await
        .expect("first idle");
    store
        .mark_session_idle("ses_root", 3_000, "succeeded")
        .await
        .expect("second idle");

    let node = store
        .session_node("ses_root")
        .await
        .expect("node")
        .expect("row");
    assert_eq!(node.status, "completed");
    assert_eq!(node.idle_reason, None);

    // A reason supplied for a non-interrupted outcome is not persisted.
    store
        .mark_session_idle_with_reason("ses_root", 4_000, "failed", Some("user"))
        .await
        .expect("third idle");
    let node = store
        .session_node("ses_root")
        .await
        .expect("node")
        .expect("row");
    assert_eq!(node.status, "failed");
    assert_eq!(node.idle_reason, None);
}

#[tokio::test]
async fn a_child_reason_does_not_leak_to_another_root() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_root").await;
    seed_session(&store, "ses_other").await;
    seed_child(&store, "ses_root", "ses_child").await;
    seed_child(&store, "ses_other", "ses_other_child").await;

    store
        .mark_session_idle_with_reason("ses_child", 2_000, "interrupted", Some("user"))
        .await
        .expect("mark child");

    let child = store
        .session_node("ses_child")
        .await
        .expect("node")
        .expect("row");
    assert_eq!(child.status, "interrupted");
    assert_eq!(child.idle_reason.as_deref(), Some("user"));

    for untouched in ["ses_root", "ses_other", "ses_other_child"] {
        let node = store
            .session_node(untouched)
            .await
            .expect("node")
            .expect("row");
        assert_ne!(node.status, "interrupted", "{untouched} must stay idle");
        assert_eq!(node.idle_reason, None, "{untouched} must not inherit");
    }
}

/// SHA-256 of the `WIRE` SQL exactly as it stood before `0012` was added,
/// computed from `git show HEAD:crates/oc-store/src/schema.rs`. `0001_wire` is
/// frozen: every existing database records this checksum, and a mismatch makes
/// `Store::open` refuse to start.
const WIRE_0001_SHA256: &str = "a237bc9c6ddb5fbd2e6dd768ba1bec35af9588aa8a70292202066151f6f451bd";

fn stored_checksum(path: &std::path::Path, id: &str) -> String {
    let connection = rusqlite::Connection::open(path).expect("raw connection");
    connection
        .query_row(
            "SELECT checksum FROM migration WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .expect("recorded migration")
}

#[tokio::test]
async fn the_frozen_wire_checksum_matches_the_pre_change_schema() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("checksum.sqlite");
    {
        let store = Store::open(&path).await.expect("open store");
        drop(store);
    }
    assert_eq!(
        stored_checksum(&path, "0001_wire"),
        WIRE_0001_SHA256,
        "0001_wire must stay byte-identical or existing databases refuse to open"
    );
}

#[tokio::test]
async fn the_migration_adds_the_column_to_an_existing_database() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("old.sqlite");
    {
        let store = Store::open(&path).await.expect("open store");
        store
            .create_session(new_session("ses_root"))
            .await
            .expect("session written before the upgrade");
        drop(store);
    }

    // Roll the database back to the pre-change shape: no `idle_reason` column
    // and no `0012` journal row, while `0001_wire` keeps the checksum computed
    // from the frozen original SQL.
    {
        let connection = rusqlite::Connection::open(&path).expect("raw connection");
        connection
            .execute_batch(
                "ALTER TABLE session_v2 DROP COLUMN idle_reason;
                 DELETE FROM migration WHERE id = '0012_session_idle_reason';",
            )
            .expect("roll back the migration");
        let present: i64 = connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('session_v2')
                 WHERE name = 'idle_reason'",
                [],
                |row| row.get(0),
            )
            .expect("column probe");
        assert_eq!(
            present, 0,
            "the simulated old database must lack the column"
        );
    }

    // Opening recomputes checksums: `0001_wire` matches, so the store starts,
    // and the pending `0012` runs the guarded ALTER.
    let store = Store::open(&path).await.expect("reopen migrates");
    store
        .mark_session_idle_with_reason("ses_root", 2_000, "interrupted", Some("user"))
        .await
        .expect("mark idle");
    let node = store
        .session_node("ses_root")
        .await
        .expect("node")
        .expect("row");
    assert_eq!(node.status, "interrupted");
    assert_eq!(node.idle_reason.as_deref(), Some("user"));
    drop(store);

    assert_eq!(
        stored_checksum(&path, "0001_wire"),
        WIRE_0001_SHA256,
        "the upgrade must not rewrite the frozen 0001 checksum"
    );
    let connection = rusqlite::Connection::open(&path).expect("raw connection");
    let journaled: i64 = connection
        .query_row(
            "SELECT count(*) FROM migration WHERE id = '0012_session_idle_reason'",
            [],
            |row| row.get(0),
        )
        .expect("count 0012 rows");
    assert_eq!(journaled, 1, "0012 must be journaled after it runs");
}
