use crate::support::seed_session;
use oc_store::{NewSnapshot, Store, StoreError};

fn new_snapshot(id: &str, session: &str, message: &str, seq: i64, path: &str) -> NewSnapshot {
    NewSnapshot {
        id: format!("snp_{id}"),
        session_id: session.to_owned(),
        message_id: message.to_owned(),
        seq,
        tool_call_id: format!("call_{id}"),
        path: path.to_owned(),
        existed: true,
        skipped: false,
        content: None,
        time_created: 1_000 + seq,
    }
}

#[tokio::test]
async fn snapshots_insert_list_ordering_and_blob_roundtrip() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;
    seed_session(&store, "ses_b").await;

    store
        .insert_snapshot(NewSnapshot {
            content: Some(b"alpha".to_vec()),
            ..new_snapshot("1", "ses_a", "msg_1", 10, "/tmp/one/a.txt")
        })
        .await
        .expect("insert first");
    store
        .insert_snapshot(NewSnapshot {
            content: Some(b"beta".to_vec()),
            ..new_snapshot("2", "ses_a", "msg_1", 10, "/tmp/one/b.txt")
        })
        .await
        .expect("insert same-seq second");
    store
        .insert_snapshot(NewSnapshot {
            existed: false,
            ..new_snapshot("3", "ses_a", "msg_2", 12, "/tmp/one/new.txt")
        })
        .await
        .expect("insert new file");
    store
        .insert_snapshot(NewSnapshot {
            skipped: true,
            ..new_snapshot("4", "ses_a", "msg_2", 12, "/tmp/one/locked.bin")
        })
        .await
        .expect("insert skipped");
    store
        .insert_snapshot(NewSnapshot {
            content: Some(Vec::new()),
            existed: false,
            ..new_snapshot("6", "ses_a", "msg_2", 12, "/tmp/one/empty.txt")
        })
        .await
        .expect("insert empty blob");
    store
        .insert_snapshot(NewSnapshot {
            content: Some(b"other".to_vec()),
            ..new_snapshot("5", "ses_b", "msg_x", 11, "/tmp/two/a.txt")
        })
        .await
        .expect("insert other session");

    let rows = store.snapshots_after("ses_a", 0).await.expect("list");
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        ids,
        ["snp_1", "snp_2", "snp_3", "snp_4", "snp_6"],
        "seq ascending, then id ascending"
    );

    assert_eq!(rows[0].session_id, "ses_a");
    assert_eq!(rows[0].message_id, "msg_1");
    assert_eq!(rows[0].seq, 10);
    assert_eq!(rows[0].tool_call_id, "call_1");
    assert_eq!(rows[0].path, "/tmp/one/a.txt");
    assert!(rows[0].existed);
    assert!(!rows[0].skipped);
    assert_eq!(rows[0].content.as_deref(), Some(b"alpha".as_slice()));
    assert_eq!(rows[0].time_created, 1_010);

    assert_eq!(
        rows[1].content.as_deref(),
        Some(b"beta".as_slice()),
        "second row at the same seq keeps its own bytes"
    );

    let new_file = &rows[2];
    assert!(!new_file.existed);
    assert!(new_file.content.is_none());

    let skipped = &rows[3];
    assert!(skipped.existed);
    assert!(skipped.skipped);
    assert!(skipped.content.is_none(), "skipped rows keep metadata only");
    assert_eq!(skipped.path, "/tmp/one/locked.bin");
    assert_eq!(skipped.tool_call_id, "call_4");

    let empty = &rows[4];
    assert!(!empty.existed);
    assert_eq!(
        empty.content.as_deref(),
        Some(b"".as_slice()),
        "an empty BLOB stays distinct from NULL"
    );

    let boundary = store
        .snapshots_after("ses_a", 12)
        .await
        .expect("inclusive boundary");
    let tail_ids: Vec<&str> = boundary.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        tail_ids,
        ["snp_3", "snp_4", "snp_6"],
        "the boundary seq itself is included"
    );
    assert!(
        store
            .snapshots_after("ses_a", 13)
            .await
            .expect("past the tail")
            .is_empty()
    );
    assert_eq!(
        store
            .snapshots_after("ses_a", 10)
            .await
            .expect("first boundary")
            .len(),
        5
    );

    let other = store
        .snapshots_after("ses_b", 0)
        .await
        .expect("other session");
    assert_eq!(ids_of_rows(&other), ["snp_5"]);
}

fn ids_of_rows(rows: &[oc_store::SnapshotRow]) -> Vec<&str> {
    rows.iter().map(|row| row.id.as_str()).collect()
}

#[tokio::test]
async fn earliest_snapshots_after_dedupes_by_path() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;

    // Two captures of `a` and `b`, two of `c` at the same seq for the id tie.
    for (id, message, seq, path) in [
        ("a1", "msg_1", 5, "/tmp/one/a.txt"),
        ("a2", "msg_2", 7, "/tmp/one/a.txt"),
        ("b1", "msg_1", 6, "/tmp/one/b.txt"),
        ("b2", "msg_2", 8, "/tmp/one/b.txt"),
        ("c2", "msg_2", 7, "/tmp/one/c.txt"),
        ("c1", "msg_2", 7, "/tmp/one/c.txt"),
    ] {
        store
            .insert_snapshot(new_snapshot(id, "ses_a", message, seq, path))
            .await
            .expect("insert snapshot");
    }

    let earliest = store
        .earliest_snapshots_after("ses_a", 0)
        .await
        .expect("earliest");
    assert_eq!(
        ids_of_rows(&earliest),
        ["snp_a1", "snp_b1", "snp_c1"],
        "one row per path: min seq, then min id"
    );
    assert_eq!(earliest[0].path, "/tmp/one/a.txt");
    assert_eq!(earliest[0].seq, 5);
    assert_eq!(earliest[2].path, "/tmp/one/c.txt");
    assert_eq!(
        earliest[2].seq, 7,
        "same-seq captures collapse to the smallest id"
    );

    let after_six = store
        .earliest_snapshots_after("ses_a", 6)
        .await
        .expect("earliest from six");
    assert_eq!(
        ids_of_rows(&after_six),
        ["snp_b1", "snp_a2", "snp_c1"],
        "boundary is inclusive and earlier per-path rows are excluded"
    );

    let after_eight = store
        .earliest_snapshots_after("ses_a", 8)
        .await
        .expect("earliest from eight");
    assert_eq!(
        ids_of_rows(&after_eight),
        ["snp_b2"],
        "paths with no capture at or after the boundary disappear"
    );

    assert_eq!(
        store
            .earliest_snapshots_after("ses_a", 0)
            .await
            .expect("unchanged")
            .len(),
        3,
        "listing is a read and never consumes rows"
    );
}

#[tokio::test]
async fn delete_snapshots_after_range() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;
    seed_session(&store, "ses_b").await;

    for (id, seq) in [("s3", 3), ("s5", 5), ("s7", 7), ("s9", 9)] {
        store
            .insert_snapshot(new_snapshot(id, "ses_a", "msg_1", seq, "/tmp/one/a.txt"))
            .await
            .expect("insert session row");
    }
    store
        .insert_snapshot(new_snapshot("other", "ses_b", "msg_1", 5, "/tmp/two/a.txt"))
        .await
        .expect("insert other row");

    assert_eq!(
        store
            .delete_snapshots_after("ses_a", 5)
            .await
            .expect("delete from five"),
        3,
        "seq >= boundary is removed"
    );
    assert_eq!(
        ids_of_rows(&store.snapshots_after("ses_a", 0).await.expect("remaining")),
        ["snp_s3"]
    );
    assert_eq!(
        store
            .delete_snapshots_after("ses_a", 100)
            .await
            .expect("delete past the tail"),
        0
    );
    assert_eq!(
        store
            .delete_snapshots_after("ses_a", 0)
            .await
            .expect("delete everything"),
        1
    );
    assert!(
        store
            .snapshots_after("ses_a", 0)
            .await
            .expect("empty")
            .is_empty()
    );
    assert_eq!(
        ids_of_rows(&store.snapshots_after("ses_b", 0).await.expect("other")),
        ["snp_other"],
        "another session's rows are untouched"
    );
}

#[tokio::test]
async fn snapshots_cascade_when_the_session_is_deleted() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;
    seed_session(&store, "ses_b").await;

    store
        .insert_snapshot(new_snapshot("a1", "ses_a", "msg_1", 1, "/tmp/one/a.txt"))
        .await
        .expect("insert a1");
    store
        .insert_snapshot(new_snapshot("a2", "ses_a", "msg_2", 2, "/tmp/one/b.txt"))
        .await
        .expect("insert a2");
    store
        .insert_snapshot(new_snapshot("b1", "ses_b", "msg_1", 1, "/tmp/two/a.txt"))
        .await
        .expect("insert b1");

    store.delete_session("ses_a").await.expect("delete session");

    assert!(
        store
            .snapshots_after("ses_a", 0)
            .await
            .expect("cascaded list")
            .is_empty()
    );
    assert!(
        store
            .earliest_snapshots_after("ses_a", 0)
            .await
            .expect("cascaded earliest")
            .is_empty()
    );
    assert_eq!(
        ids_of_rows(&store.snapshots_after("ses_b", 0).await.expect("survivor")),
        ["snp_b1"]
    );

    let missing = store
        .insert_snapshot(new_snapshot("x", "ses_missing", "msg_1", 1, "/tmp/x"))
        .await;
    assert!(
        matches!(missing, Err(StoreError::NotFound(_))),
        "an unknown session is reported, not silently orphaned: {missing:?}"
    );
}
