use axum::http::{Method, StatusCode};
use oc_server::router;
use serde_json::json;

use crate::support::{call, create_session, harness};

#[tokio::test]
async fn saved_permissions_list_update_delete() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "permissions"})).await;
    let project = session["projectID"].as_str().unwrap().to_owned();
    let store = harness.state.store().unwrap();
    let rule = store
        .save_permission_rule(&project, "read", "/tmp/example")
        .await
        .unwrap();
    let id = rule["id"].as_str().unwrap().to_owned();

    let (status, list) = call(
        &app,
        Method::GET,
        &format!("/api/permission/saved?projectID={project}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.unwrap()["data"][0]["id"], id);

    let (status, updated) = call(
        &app,
        Method::PATCH,
        &format!("/api/permission/saved/{id}"),
        Some(json!({"action": "write"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch: {updated:?}");
    assert_eq!(updated.unwrap()["data"]["action"], "write");

    let (status, missing) = call(
        &app,
        Method::PATCH,
        "/api/permission/saved/psv_absent",
        Some(json!({"action": "write"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing.unwrap()["_tag"], "PermissionSavedNotFoundError");

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/permission/saved/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = call(
        &app,
        Method::GET,
        &format!("/api/permission/saved?projectID={project}"),
        None,
    )
    .await;
    assert_eq!(list.unwrap()["data"], json!([]));
}
