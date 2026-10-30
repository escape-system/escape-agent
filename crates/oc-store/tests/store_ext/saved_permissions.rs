use oc_store::{Store, StoreError};

#[tokio::test]
async fn saved_permissions_are_idempotent_and_updatable() {
    let store = Store::open_memory().await.expect("memory store");

    let first = store
        .save_permission_rule("prj_1", "read", "/tmp/a")
        .await
        .expect("save");
    assert_eq!(first["projectID"], "prj_1");
    assert_eq!(first["action"], "read");
    assert_eq!(first["resource"], "/tmp/a");
    assert!(
        first["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("psv_"))
    );
    let created = first["time"]["created"].as_i64().expect("created");
    let id = first["id"].as_str().expect("id").to_owned();

    let again = store
        .save_permission_rule("prj_1", "read", "/tmp/a")
        .await
        .expect("idempotent save");
    assert_eq!(again["id"], id.as_str());
    assert_eq!(again["time"]["created"], created);
    assert_eq!(
        store
            .list_saved_permissions(Some("prj_1"))
            .await
            .expect("list")
            .len(),
        1
    );

    store
        .save_permission_rule("prj_1", "write", "/tmp/b")
        .await
        .expect("second rule");
    store
        .save_permission_rule("prj_2", "read", "/tmp/a")
        .await
        .expect("other project");
    assert_eq!(
        store
            .list_saved_permissions(Some("prj_1"))
            .await
            .expect("project list")
            .len(),
        2
    );
    assert_eq!(
        store
            .list_saved_permissions(None)
            .await
            .expect("all list")
            .len(),
        3
    );

    let updated = store
        .update_saved_permission(&id, Some("prj_1"), Some("edit"), None)
        .await
        .expect("update")
        .expect("present");
    assert_eq!(updated["id"], id.as_str());
    assert_eq!(updated["action"], "edit");
    assert_eq!(updated["resource"], "/tmp/a");
    assert!(
        store
            .update_saved_permission(&id, Some("prj_2"), Some("deny"), None)
            .await
            .expect("wrong project")
            .is_none()
    );
    assert!(matches!(
        store
            .update_saved_permission(&id, None, Some("write"), Some("/tmp/b"))
            .await,
        Err(StoreError::Conflict(_))
    ));
    assert!(
        store
            .update_saved_permission("psv_missing", None, Some("read"), None)
            .await
            .expect("missing")
            .is_none()
    );

    assert!(store.delete_saved_permission(&id).await.expect("delete"));
    assert!(
        !store
            .delete_saved_permission(&id)
            .await
            .expect("delete again")
    );
    assert_eq!(
        store
            .list_saved_permissions(Some("prj_1"))
            .await
            .expect("after delete")
            .len(),
        1
    );
}
