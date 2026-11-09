//! Context boxes: tree sharing, create-or-update, append, kind validation,
//! expiry and soft deletion (`docs/25-context-boxes.md`).

use crate::support::{new_session, seed_session};
use oc_store::{BoxQuery, MAX_BOX_CHARS, NewBox, NewSession, NewSessionTree, Store, StoreError};

fn new_box(root: &str, key: &str, kind: &str, content: &str) -> NewBox {
    NewBox {
        root_id: root.to_owned(),
        owner_session: root.to_owned(),
        key: key.to_owned(),
        title: Some(format!("title {key}")),
        description: Some(format!("description {key}")),
        kind: kind.to_owned(),
        tags: Vec::new(),
        content: content.to_owned(),
    }
}

async fn seed_tree(store: &Store) -> String {
    seed_session(store, "ses_root").await;
    store
        .create_session_with_tree(
            NewSession {
                parent_id: Some("ses_root".to_owned()),
                ..new_session("ses_child")
            },
            NewSessionTree {
                root_id: "ses_root".to_owned(),
                depth: 1,
                role: "subagent".to_owned(),
                spawn_allowed: true,
            },
        )
        .await
        .expect("child session");
    "ses_root".to_owned()
}

#[tokio::test]
async fn boxes_are_shared_across_the_session_tree() {
    let store = Store::open_memory().await.expect("memory store");
    let root = seed_tree(&store).await;

    let created = store
        .write_box(new_box(&root, "decisions", "stable", "use rusqlite"), false)
        .await
        .expect("root writes");
    assert_eq!(created["key"], "decisions");
    assert_eq!(created["kind"], "stable");
    assert_eq!(created["size"], 12);
    assert_eq!(created["revision"], 1);
    assert!(created["id"].as_str().unwrap().starts_with("box_"));

    // A subagent writes into the same tree and the root sees it.
    store
        .write_box(
            NewBox {
                owner_session: "ses_child".to_owned(),
                ..new_box(&root, "findings", "stable", "chart-limits.js caps pages")
            },
            false,
        )
        .await
        .expect("child writes");
    let listed = store
        .list_boxes(BoxQuery {
            root_id: root.clone(),
            ..BoxQuery::default()
        })
        .await
        .expect("list");
    let keys: Vec<&str> = listed
        .items
        .iter()
        .filter_map(|item| item["key"].as_str())
        .collect();
    assert_eq!(keys, vec!["decisions", "findings"]);
    assert_eq!(listed.items[1]["ownerSession"], "ses_child");

    // A different root never sees these rows.
    seed_session(&store, "ses_other").await;
    let stranger = store
        .list_boxes(BoxQuery {
            root_id: "ses_other".to_owned(),
            ..BoxQuery::default()
        })
        .await
        .expect("list stranger");
    assert!(stranger.items.is_empty());
}

#[tokio::test]
async fn write_is_create_or_update_by_key_and_bumps_revision() {
    let store = Store::open_memory().await.expect("memory store");
    let root = seed_tree(&store).await;

    let first = store
        .write_box(new_box(&root, "state", "stable", "phase 1"), false)
        .await
        .expect("create");
    let second = store
        .write_box(new_box(&root, "state", "stable", "phase 2"), false)
        .await
        .expect("update");
    assert_eq!(first["id"], second["id"], "the id is the box identity");
    assert_eq!(second["revision"], 2);
    assert_eq!(second["content"], "phase 2");

    let appended = store
        .write_box(new_box(&root, "state", "stable", "phase 3"), true)
        .await
        .expect("append");
    assert_eq!(appended["content"], "phase 2\nphase 3");
    assert_eq!(appended["revision"], 3);
    assert_eq!(appended["size"], 15);

    let listed = store
        .list_boxes(BoxQuery {
            root_id: root,
            ..BoxQuery::default()
        })
        .await
        .expect("list");
    assert_eq!(listed.items.len(), 1, "one row per key");
}

#[tokio::test]
async fn invalid_keys_kinds_and_oversized_content_are_rejected() {
    let store = Store::open_memory().await.expect("memory store");
    let root = seed_tree(&store).await;

    for (key, kind) in [
        ("", "stable"),
        ("Upper", "stable"),
        ("with space", "stable"),
        ("ok", "permanent"),
    ] {
        let error = store
            .write_box(new_box(&root, key, kind, "x"), false)
            .await
            .expect_err("invalid write");
        assert!(
            matches!(error, StoreError::Invalid(_)),
            "{key}/{kind} must be invalid, got {error}"
        );
    }

    let huge = "a".repeat(MAX_BOX_CHARS + 1);
    let error = store
        .write_box(new_box(&root, "huge", "stable", &huge), false)
        .await
        .expect_err("oversized");
    assert!(matches!(error, StoreError::Invalid(_)));

    // A missing tree root is reported as not found, never as a dangling row.
    let error = store
        .write_box(new_box("ses_missing", "a", "stable", "x"), false)
        .await
        .expect_err("missing root");
    assert!(matches!(error, StoreError::NotFound(_)));
}

#[tokio::test]
async fn expiration_hides_temporary_boxes_and_keeps_stable_ones() {
    let store = Store::open_memory().await.expect("memory store");
    let root = seed_tree(&store).await;
    store
        .write_box(new_box(&root, "durable", "stable", "crates"), false)
        .await
        .expect("stable");
    store
        .write_box(new_box(&root, "scratch", "temporary", "notes"), false)
        .await
        .expect("temporary");

    let expired = store.expire_temporary_boxes(&root).await.expect("expire");
    assert_eq!(expired, 1, "only the temporary box expires");
    assert_eq!(
        store
            .expire_temporary_boxes(&root)
            .await
            .expect("second pass"),
        0,
        "expiry is idempotent"
    );

    let live = store
        .list_boxes(BoxQuery {
            root_id: root.clone(),
            ..BoxQuery::default()
        })
        .await
        .expect("live");
    assert_eq!(live.items.len(), 1);
    assert_eq!(live.items[0]["key"], "durable");

    let all = store
        .list_boxes(BoxQuery {
            root_id: root.clone(),
            include_expired: true,
            ..BoxQuery::default()
        })
        .await
        .expect("all");
    assert_eq!(all.items.len(), 2);
    let scratch = all
        .items
        .iter()
        .find(|item| item["key"] == "scratch")
        .expect("scratch");
    assert!(scratch["timeExpired"].as_i64().is_some());

    // Reads honour the flag, and a rewrite revives an expired box.
    assert!(
        store
            .get_box(&root, "scratch", false)
            .await
            .expect("read hidden")
            .is_none()
    );
    assert!(
        store
            .get_box(&root, "scratch", true)
            .await
            .expect("read expired")
            .is_some()
    );
    let revived = store
        .write_box(new_box(&root, "scratch", "temporary", "fresh"), false)
        .await
        .expect("revive");
    assert!(revived.get("timeExpired").is_none());
    assert!(revived["revision"].as_i64().unwrap() >= 2);

    let keys = store.box_keys(&root).await.expect("keys");
    assert_eq!(keys.len(), 2, "the revived box is live again");
}

#[tokio::test]
async fn delete_soft_deletes_and_frees_the_key() {
    let store = Store::open_memory().await.expect("memory store");
    let root = seed_tree(&store).await;
    store
        .write_box(new_box(&root, "gone", "stable", "x"), false)
        .await
        .expect("create");
    assert!(store.delete_box(&root, "gone").await.expect("delete"));
    assert!(
        !store
            .delete_box(&root, "gone")
            .await
            .expect("second delete")
    );
    assert!(
        store
            .get_box(&root, "gone", true)
            .await
            .expect("read")
            .is_none()
    );

    // The key is reusable; the deleted row stays for the audit trail.
    let recreated = store
        .write_box(new_box(&root, "gone", "stable", "y"), false)
        .await
        .expect("recreate");
    assert_eq!(recreated["content"], "y");
    assert_eq!(recreated["revision"], 1, "recreation starts a new history");
}

#[tokio::test]
async fn list_reports_has_more_and_orders_by_key() {
    let store = Store::open_memory().await.expect("memory store");
    let root = seed_tree(&store).await;
    for key in ["b", "a", "c"] {
        store
            .write_box(new_box(&root, key, "stable", key), false)
            .await
            .expect("write");
    }
    let page = store
        .list_boxes(BoxQuery {
            root_id: root.clone(),
            limit: 2,
            ..BoxQuery::default()
        })
        .await
        .expect("page");
    assert!(page.has_more);
    let keys: Vec<&str> = page
        .items
        .iter()
        .filter_map(|item| item["key"].as_str())
        .collect();
    assert_eq!(keys, vec!["a", "b"]);

    let filtered = store
        .list_boxes(BoxQuery {
            root_id: root,
            kind: Some("temporary".to_owned()),
            ..BoxQuery::default()
        })
        .await
        .expect("filtered");
    assert!(filtered.items.is_empty());
}
