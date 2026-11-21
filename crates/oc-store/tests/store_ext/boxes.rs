//! Context boxes: tree sharing, create-or-update, append, kind validation,
//! expiry and soft deletion (`docs/25-context-boxes.md`).

use crate::support::{new_session, seed_session};
use oc_store::{
    BoxMetaPatch, BoxQuery, BoxSearchQuery, EditBox, MAX_BOX_CHARS, NewBox, NewSession,
    NewSessionTree, Store, StoreError, WriteBox,
};

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
async fn expiry_with_owners_touches_only_that_subtree() {
    let store = Store::open_memory().await.expect("memory store");
    let root = seed_tree(&store).await;
    // A second subtree under the same root: a sibling of the compacting child.
    store
        .create_session_with_tree(
            NewSession {
                parent_id: Some(root.clone()),
                ..new_session("ses_sibling")
            },
            NewSessionTree {
                root_id: root.clone(),
                depth: 1,
                role: "subagent".to_owned(),
                spawn_allowed: false,
            },
        )
        .await
        .expect("sibling session");

    for (owner, key) in [
        (root.as_str(), "root-scratch"),
        ("ses_sibling", "sibling-scratch"),
        ("ses_child", "child-scratch"),
    ] {
        store
            .write_box(
                NewBox {
                    owner_session: owner.to_owned(),
                    ..new_box(&root, key, "temporary", "notes")
                },
                false,
            )
            .await
            .expect("temporary write");
    }

    let scoped = store
        .expire_temporary_boxes_for(&root, &["ses_child".to_owned()])
        .await
        .expect("scoped expire");
    assert_eq!(scoped, 1, "only the compacting subtree's box expires");

    let live: Vec<String> = store
        .box_keys(&root)
        .await
        .expect("live keys")
        .into_iter()
        .collect();
    assert_eq!(
        live,
        vec!["root-scratch".to_owned(), "sibling-scratch".to_owned()],
        "the root and sibling boxes survive the child's compaction"
    );
    assert!(
        store
            .get_box(&root, "child-scratch", false)
            .await
            .expect("child read")
            .is_none(),
        "the compacting subtree's temporary box is expired"
    );

    // Idempotent: the same owners expire nothing new.
    assert_eq!(
        store
            .expire_temporary_boxes_for(&root, &["ses_child".to_owned()])
            .await
            .expect("scoped expire again"),
        0
    );
    // An empty owner list is a no-op, never "the whole tree".
    assert_eq!(
        store
            .expire_temporary_boxes_for(&root, &[])
            .await
            .expect("empty owners"),
        0
    );
    assert_eq!(store.box_keys(&root).await.expect("still live").len(), 2);

    // The unfiltered form still expires the whole tree.
    assert_eq!(
        store
            .expire_temporary_boxes(&root)
            .await
            .expect("whole tree"),
        2
    );
    assert!(
        store
            .box_keys(&root)
            .await
            .expect("empty live set")
            .is_empty()
    );
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
            root_id: root.clone(),
            kind: Some("temporary".to_owned()),
            ..BoxQuery::default()
        })
        .await
        .expect("filtered");
    assert!(filtered.items.is_empty());
}

#[tokio::test]
async fn session_scoped_boxes_isolate_siblings_while_preserving_tree_boxes() {
    let store = Store::open_memory().await.expect("memory store");
    let root = seed_tree(&store).await;
    store
        .create_session_with_tree(
            NewSession {
                parent_id: Some("ses_root".to_owned()),
                ..new_session("ses_sibling")
            },
            NewSessionTree {
                root_id: "ses_root".to_owned(),
                depth: 1,
                role: "subagent".to_owned(),
                spawn_allowed: true,
            },
        )
        .await
        .expect("sibling session");

    store
        .write_box(
            new_box(&root, "scratch", "stable", "shared tree scratch"),
            false,
        )
        .await
        .expect("tree box");
    store
        .write_box_scoped(
            WriteBox {
                root_id: root.clone(),
                owner_session: "ses_child".to_owned(),
                scope: Some("session".to_owned()),
                key: "scratch".to_owned(),
                kind: Some("temporary".to_owned()),
                tags: Some(vec!["worker-a".to_owned()]),
                content: "child private scratch".to_owned(),
                ..WriteBox::default()
            },
            false,
        )
        .await
        .expect("child private");
    store
        .write_box_scoped(
            WriteBox {
                root_id: root.clone(),
                owner_session: "ses_sibling".to_owned(),
                scope: Some("session".to_owned()),
                key: "scratch".to_owned(),
                kind: Some("stable".to_owned()),
                tags: Some(vec!["worker-b".to_owned()]),
                content: "sibling private scratch".to_owned(),
                ..WriteBox::default()
            },
            false,
        )
        .await
        .expect("sibling private");

    // Child's default get_box_scoped sees its own private session box; root sees tree box.
    let child_read = store
        .get_box_scoped(&root, None, Some("ses_child"), "scratch", false)
        .await
        .expect("read")
        .expect("child box");
    assert_eq!(child_read["scope"], "session");
    assert_eq!(child_read["content"], "child private scratch");

    let tree_read = store
        .get_box_scoped(&root, Some("tree"), Some("ses_child"), "scratch", false)
        .await
        .expect("read")
        .expect("tree box");
    assert_eq!(tree_read["scope"], "tree");
    assert_eq!(tree_read["content"], "shared tree scratch");

    let root_list = store
        .list_boxes(BoxQuery {
            root_id: root.clone(),
            session_id: Some("ses_root".to_owned()),
            ..BoxQuery::default()
        })
        .await
        .expect("root list");
    assert_eq!(root_list.items.len(), 1);
    assert_eq!(root_list.items[0]["content"], "shared tree scratch");

    let child_list = store
        .list_boxes(BoxQuery {
            root_id: root.clone(),
            session_id: Some("ses_child".to_owned()),
            ..BoxQuery::default()
        })
        .await
        .expect("child list");
    assert_eq!(child_list.items.len(), 2);
    assert!(
        child_list
            .items
            .iter()
            .all(|item| item["content"] != "sibling private scratch")
    );
}

#[tokio::test]
async fn write_box_scoped_preserves_kind_and_tags_and_clears_deleted_metadata() {
    let store = Store::open_memory().await.expect("memory store");
    let root = seed_tree(&store).await;

    let created = store
        .write_box_scoped(
            WriteBox {
                root_id: root.clone(),
                owner_session: root.clone(),
                key: "notes".to_owned(),
                title: Some("Old Title".to_owned()),
                description: Some("Old Description".to_owned()),
                kind: Some("temporary".to_owned()),
                tags: Some(vec!["alpha".to_owned(), "beta".to_owned()]),
                content: "line 1".to_owned(),
                ..WriteBox::default()
            },
            false,
        )
        .await
        .expect("create");
    assert_eq!(created["kind"], "temporary");

    // Append with omitted kind & tags must keep temporary & existing tags.
    let appended = store
        .write_box_scoped(
            WriteBox {
                root_id: root.clone(),
                owner_session: root.clone(),
                key: "notes".to_owned(),
                content: "line 2".to_owned(),
                expected_revision: Some(1),
                ..WriteBox::default()
            },
            true,
        )
        .await
        .expect("append");
    assert_eq!(appended["kind"], "temporary");
    assert_eq!(appended["tags"], serde_json::json!(["alpha", "beta"]));
    assert_eq!(appended["revision"], 2);

    // Revision conflict rejects stale writes.
    let err = store
        .write_box_scoped(
            WriteBox {
                root_id: root.clone(),
                owner_session: root.clone(),
                key: "notes".to_owned(),
                content: "stale".to_owned(),
                expected_revision: Some(1),
                ..WriteBox::default()
            },
            false,
        )
        .await
        .expect_err("stale revision");
    assert!(matches!(err, StoreError::Invalid(_)));

    // Recreating a deleted box without title/description clears stale metadata.
    assert!(store.delete_box(&root, "notes").await.expect("delete"));
    let recreated = store
        .write_box_scoped(
            WriteBox {
                root_id: root.clone(),
                owner_session: root.clone(),
                key: "notes".to_owned(),
                content: "fresh".to_owned(),
                ..WriteBox::default()
            },
            false,
        )
        .await
        .expect("recreate");
    assert_eq!(recreated["revision"], 1);
    assert_eq!(recreated["kind"], "stable");
    assert!(recreated.get("title").is_none());
    assert!(recreated.get("description").is_none());
}

#[tokio::test]
async fn edit_meta_search_and_cursor_pagination_work() {
    let store = Store::open_memory().await.expect("memory store");
    let root = seed_tree(&store).await;

    let draft = store
        .write_box_scoped(
            WriteBox {
                root_id: root.clone(),
                owner_session: "ses_child".to_owned(),
                scope: Some("session".to_owned()),
                key: "plan-draft".to_owned(),
                description: Some("Draft plan".to_owned()),
                kind: Some("temporary".to_owned()),
                tags: Some(vec!["plan".to_owned(), "backend".to_owned()]),
                content: "step 1: check config\nstep 2: TODO later\nstep 3: verify".to_owned(),
                ..WriteBox::default()
            },
            false,
        )
        .await
        .expect("draft");
    assert_eq!(draft["revision"], 1);

    let edited = store
        .edit_box(EditBox {
            root_id: root.clone(),
            owner_session: "ses_child".to_owned(),
            scope: Some("session".to_owned()),
            key: "plan-draft".to_owned(),
            find: "TODO later".to_owned(),
            replace: "update migration".to_owned(),
            replace_all: false,
            expected_revision: Some(1),
        })
        .await
        .expect("edit");
    assert_eq!(edited["revision"], 2);
    assert!(
        edited["content"]
            .as_str()
            .unwrap()
            .contains("update migration")
    );

    // Promote from session temporary -> tree stable and rename key.
    let promoted = store
        .update_box_meta(BoxMetaPatch {
            root_id: root.clone(),
            owner_session: "ses_child".to_owned(),
            scope: Some("session".to_owned()),
            key: "plan-draft".to_owned(),
            new_key: Some("plan-final".to_owned()),
            new_scope: Some("tree".to_owned()),
            kind: Some("stable".to_owned()),
            expected_revision: Some(2),
            ..BoxMetaPatch::default()
        })
        .await
        .expect("meta");
    assert_eq!(promoted["scope"], "tree");
    assert_eq!(promoted["key"], "plan-final");
    assert_eq!(promoted["kind"], "stable");
    assert_eq!(promoted["revision"], 3);

    // Search matches case-insensitively with line and charOffset.
    let hits = store
        .search_boxes(BoxSearchQuery {
            root_id: root.clone(),
            session_id: Some("ses_root".to_owned()),
            tag: Some("backend".to_owned()),
            query: "MIGRATION".to_owned(),
            ..BoxSearchQuery::default()
        })
        .await
        .expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["key"], "plan-final");
    assert_eq!(hits[0]["line"], 2);
    assert!(hits[0]["charOffset"].as_u64().is_some());

    // Prefix + cursor pagination across multiple boxes.
    for key in ["mod-a", "mod-b", "mod-c"] {
        store
            .write_box(new_box(&root, key, "stable", key), false)
            .await
            .expect("write");
    }
    let page1 = store
        .list_boxes(BoxQuery {
            root_id: root.clone(),
            prefix: Some("mod-".to_owned()),
            limit: 2,
            ..BoxQuery::default()
        })
        .await
        .expect("page1");
    assert_eq!(page1.items.len(), 2);
    assert!(page1.has_more);
    let page2 = store
        .list_boxes(BoxQuery {
            root_id: root,
            prefix: Some("mod-".to_owned()),
            cursor: page1.next_cursor,
            limit: 2,
            ..BoxQuery::default()
        })
        .await
        .expect("page2");
    assert_eq!(page2.items.len(), 1);
    assert!(!page2.has_more);
    assert_eq!(page2.items[0]["key"], "mod-c");
}
