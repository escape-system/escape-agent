use crate::support::{new_session, seed_session};
use oc_store::{NewWorktree, Store, StoreError};
use serde_json::{Value, json};

fn new_worktree(id: &str, project: &str, directory: &str, created: i64) -> NewWorktree {
    NewWorktree {
        id: Some(id.to_owned()),
        project_id: project.to_owned(),
        directory: directory.to_owned(),
        canonical: Some(directory.to_owned()),
        branch: Some(format!("branch-{id}")),
        name: Some(id.to_owned()),
        time_created: Some(created),
        time_updated: None,
    }
}

#[tokio::test]
async fn worktrees_create_list_filter_and_remove() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_one").await;
    let mut second = new_session("ses_two");
    second.project_id = "prj_2".to_owned();
    second.directory = "/tmp/two".to_owned();
    store.create_session(second).await.expect("second project");

    let older = store
        .create_worktree(new_worktree("wkt_a", "prj_1", "/tmp/wt-a", 100))
        .await
        .expect("create oldest");
    assert_eq!(older, json!({ "directory": "/tmp/wt-a" }));
    let newer = store
        .create_worktree(new_worktree("wkt_b", "prj_1", "/tmp/wt-b", 200))
        .await
        .expect("create newest");
    assert_eq!(newer, json!({ "directory": "/tmp/wt-b" }));
    let other = store
        .create_worktree(new_worktree("wkt_c", "prj_2", "/tmp/wt-a", 150))
        .await
        .expect("same directory under another project");
    assert_eq!(other, json!({ "directory": "/tmp/wt-a" }));

    let directories = |values: &[Value]| -> Vec<String> {
        values
            .iter()
            .map(|value| value["directory"].as_str().unwrap_or_default().to_owned())
            .collect()
    };

    assert_eq!(
        directories(&store.list_worktrees(None).await.expect("list all")),
        vec!["/tmp/wt-b", "/tmp/wt-a", "/tmp/wt-a"]
    );
    assert_eq!(
        directories(
            &store
                .list_worktrees(Some("prj_1"))
                .await
                .expect("list project")
        ),
        vec!["/tmp/wt-b", "/tmp/wt-a"]
    );
    assert_eq!(
        store.get_worktree("wkt_a").await.expect("get worktree"),
        Some(json!({ "directory": "/tmp/wt-a" }))
    );
    assert_eq!(
        store
            .get_worktree("wkt_missing")
            .await
            .expect("get missing"),
        None
    );

    assert!(matches!(
        store
            .create_worktree(new_worktree("wkt_a", "prj_1", "/tmp/elsewhere", 300))
            .await,
        Err(StoreError::Conflict(_))
    ));
    assert!(matches!(
        store
            .create_worktree(new_worktree("wkt_d", "prj_1", "/tmp/wt-a", 300))
            .await,
        Err(StoreError::Conflict(_))
    ));
    assert!(matches!(
        store
            .create_worktree(new_worktree("wkt_e", "prj_missing", "/tmp/wt-e", 300))
            .await,
        Err(StoreError::NotFound(_))
    ));

    assert!(store.delete_worktree("wkt_a").await.expect("delete"));
    assert!(!store.delete_worktree("wkt_a").await.expect("re-delete"));
    assert!(
        store
            .delete_worktree_by_directory("prj_2", "/tmp/wt-a")
            .await
            .expect("delete by directory")
    );
    assert!(
        !store
            .delete_worktree_by_directory("prj_2", "/tmp/wt-a")
            .await
            .expect("re-delete by directory")
    );
    assert_eq!(
        directories(
            &store
                .list_worktrees(Some("prj_1"))
                .await
                .expect("list after delete")
        ),
        vec!["/tmp/wt-b"]
    );
}
