//! Integration tests for the project routes.

use oc_store::{NewSession, ProjectPatch, Store};
use tempfile::TempDir;

fn session(id: &str, project: &str, directory: &str, created: i64) -> NewSession {
    NewSession {
        id: id.to_owned(),
        parent_id: None,
        project_id: project.to_owned(),
        directory: directory.to_owned(),
        title: None,
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        time_created: created,
    }
}

fn ids_of(values: &[serde_json::Value]) -> Vec<String> {
    values
        .iter()
        .map(|value| value["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn project_list_get_update_and_archive() {
    let store = Store::open_memory().await.expect("store");
    store
        .create_session(session("ses_a", "prj_one", "/tmp/one", 1_000))
        .await
        .expect("project one");
    store
        .create_session(session("ses_b", "prj_two", "/tmp/two", 2_000))
        .await
        .expect("project two");

    let all = store.list_projects(None).await.expect("list");
    assert_eq!(ids_of(&all), vec!["prj_two", "prj_one"]);

    let project = store
        .get_project("prj_one")
        .await
        .expect("get")
        .expect("project exists");
    assert_eq!(project["directory"], "/tmp/one");
    assert_eq!(project["canonical"], "/tmp/one");
    assert_eq!(project["time"]["created"], 1_000);
    assert!(project.get("name").is_none());
    assert!(project.get("archived").is_none());

    let patched = store
        .update_project(
            "prj_one",
            ProjectPatch {
                name: Some("One".to_owned()),
                icon: Some("https://example.test/one.svg".to_owned()),
                archived: None,
            },
        )
        .await
        .expect("update")
        .expect("project exists");
    assert_eq!(patched["name"], "One");
    assert_eq!(patched["icon"], "https://example.test/one.svg");
    assert!(patched.get("archived").is_none());

    let archived = store
        .update_project(
            "prj_one",
            ProjectPatch {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("archive")
        .expect("project exists");
    assert_eq!(archived["archived"], true);
    assert_eq!(archived["name"], "One", "unprovided fields are untouched");

    let archived_list = store.list_projects(Some(true)).await.expect("archived");
    assert_eq!(ids_of(&archived_list), vec!["prj_one"]);
    let live_list = store.list_projects(Some(false)).await.expect("live");
    assert_eq!(ids_of(&live_list), vec!["prj_two"]);

    let unarchived = store
        .update_project(
            "prj_one",
            ProjectPatch {
                archived: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("unarchive")
        .expect("project exists");
    assert!(unarchived.get("archived").is_none());
    assert_eq!(store.list_projects(None).await.expect("all").len(), 2);
    assert!(
        store
            .list_projects(Some(true))
            .await
            .expect("archived after unarchive")
            .is_empty()
    );
}

#[tokio::test]
async fn project_update_only_sets_provided_fields() {
    let store = Store::open_memory().await.expect("store");
    store
        .create_session(session("ses_b", "prj_two", "/tmp/two", 2_000))
        .await
        .expect("project two");

    let renamed = store
        .update_project(
            "prj_two",
            ProjectPatch {
                name: Some("Two".to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("rename")
        .expect("project exists");
    assert_eq!(renamed["name"], "Two");
    assert!(renamed.get("icon").is_none(), "icon stays unset");

    let touched = store
        .update_project("prj_two", ProjectPatch::default())
        .await
        .expect("touch")
        .expect("empty patch still updates the timestamp");
    assert_eq!(touched["name"], "Two");
    assert_eq!(touched["id"], "prj_two");

    assert!(
        store
            .get_project("prj_missing")
            .await
            .expect("get missing")
            .is_none()
    );
    assert!(
        store
            .update_project(
                "prj_missing",
                ProjectPatch {
                    name: Some("ghost".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .expect("update missing")
            .is_none()
    );
}

#[tokio::test]
async fn project_reopen_persists_patch_and_archive() {
    let directory = TempDir::new().expect("temp dir");
    let path = directory.path().join("projects.db");
    let store = Store::open(&path).await.expect("open");
    store
        .create_session(session("ses_p", "prj_persist", "/tmp/persist", 5_000))
        .await
        .expect("project");
    store
        .update_project(
            "prj_persist",
            ProjectPatch {
                name: Some("Persisted".to_owned()),
                icon: Some("icon-url".to_owned()),
                archived: Some(true),
            },
        )
        .await
        .expect("update")
        .expect("project exists");
    drop(store);

    let reopened = Store::open(&path).await.expect("reopen");
    let project = reopened
        .get_project("prj_persist")
        .await
        .expect("get")
        .expect("project survived reopen");
    assert_eq!(project["name"], "Persisted");
    assert_eq!(project["icon"], "icon-url");
    assert_eq!(project["archived"], true);
    assert_eq!(project["time"]["created"], 5_000);
    assert_eq!(
        ids_of(&reopened.list_projects(Some(true)).await.expect("archived")),
        vec!["prj_persist"]
    );
    assert!(
        reopened
            .list_projects(Some(false))
            .await
            .expect("live")
            .is_empty()
    );

    let renamed = reopened
        .update_project(
            "prj_persist",
            ProjectPatch {
                name: Some("Renamed".to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("rename")
        .expect("project exists");
    assert_eq!(renamed["name"], "Renamed");
    assert_eq!(renamed["archived"], true, "archive survives a rename");
}
