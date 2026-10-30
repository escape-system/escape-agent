use axum::http::{Method, StatusCode};
use oc_server::router;
use serde_json::json;

use crate::support::{
    call, capture, create_session, create_session_in, harness, message_ids, prompt, session_id,
};

#[tokio::test]
async fn revert_stages_reports_and_commits() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "revert"})).await;
    let sid = session_id(&session);
    let first = prompt(&app, &sid, "keep").await;
    let second = prompt(&app, &sid, "drop").await;

    let (status, staged) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/revert/stage"),
        Some(json!({"messageID": second, "files": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "stage: {staged:?}");
    let staged = staged.unwrap();
    assert_eq!(staged["data"]["messageID"], second);
    assert_eq!(
        staged["data"]["files"],
        json!([]),
        "no snapshots means no invented files"
    );

    let (_, session_body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert_eq!(session_body.unwrap()["data"]["revert"]["messageID"], second);

    let (status, diff) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/diff?from={first}&to={second}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(diff.unwrap()["data"], json!([]));

    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/revert/commit"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let messages = message_ids(&app, &sid).await;
    assert_eq!(messages, vec![first], "commit drops the boundary and after");
    let (_, session_body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert!(session_body.unwrap()["data"].get("revert").is_none());
}

#[tokio::test]
async fn revert_clear_removes_staged_boundary() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "clear"})).await;
    let sid = session_id(&session);
    let message = prompt(&app, &sid, "text").await;
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/revert/stage"),
        Some(json!({"messageID": message})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/session/{sid}/revert"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, session_body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert!(session_body.unwrap()["data"].get("revert").is_none());
    assert_eq!(message_ids(&app, &sid).await.len(), 1);
}

#[tokio::test]
async fn revert_stage_diffs_snapshots_against_current_files() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let store = harness.state.store().unwrap().clone();

    let work = tempfile::tempdir().unwrap();
    let session = create_session_in(&app, work.path(), "snapshot diff").await;
    let sid = session_id(&session);
    prompt(&app, &sid, "first").await;
    let boundary = prompt(&app, &sid, "second").await;

    let modified = work.path().join("a.txt");
    let added = work.path().join("new.txt");
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("outside.txt");
    std::fs::write(&modified, "one\ntwo\n").unwrap();
    std::fs::write(&external, "keep\n").unwrap();

    capture(&store, &sid, &boundary, &modified, true, Some("one\ntwo\n")).await;
    capture(&store, &sid, &boundary, &added, false, None).await;
    capture(&store, &sid, &boundary, &external, true, Some("keep\n")).await;

    std::fs::write(&modified, "one\nthree\n").unwrap();
    std::fs::write(&added, "hello\n").unwrap();

    let (status, staged) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/revert/stage"),
        Some(json!({"messageID": boundary})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "stage: {staged:?}");
    let staged = staged.unwrap();
    assert_eq!(staged["data"]["messageID"], boundary);
    let files = staged["data"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "the external path is ignored: {files:?}");

    let modified_path = modified.to_string_lossy().into_owned();
    let modified_diff = files
        .iter()
        .find(|file| file["file"].as_str() == Some(modified_path.as_str()))
        .expect("modified file is reported");
    assert_eq!(modified_diff["status"], "modified");
    assert_eq!(modified_diff["additions"], 1);
    assert_eq!(modified_diff["deletions"], 1);
    let patch = modified_diff["patch"].as_str().unwrap();
    assert!(patch.contains("-two\n"), "{patch}");
    assert!(patch.contains("+three\n"), "{patch}");

    let added_path = added.to_string_lossy().into_owned();
    let added_diff = files
        .iter()
        .find(|file| file["file"].as_str() == Some(added_path.as_str()))
        .expect("added file is reported");
    assert_eq!(added_diff["status"], "added");
    assert_eq!(added_diff["additions"], 1);
    assert_eq!(added_diff["deletions"], 0);
    assert!(
        added_diff["patch"].as_str().unwrap().contains("+hello\n"),
        "{added_diff}"
    );

    let (_, session_body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert_eq!(
        session_body.unwrap()["data"]["revert"]["files"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // The diff route recomputes against the tree instead of replaying the
    // cached list captured while staging.
    std::fs::write(&modified, "one\nfour\n").unwrap();
    let (status, diff) = call(&app, Method::GET, &format!("/api/session/{sid}/diff"), None).await;
    assert_eq!(status, StatusCode::OK, "diff: {diff:?}");
    let diff = diff.unwrap();
    let files = diff["data"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let modified_diff = files
        .iter()
        .find(|file| file["file"].as_str() == Some(modified_path.as_str()))
        .expect("modified file is recomputed");
    assert_eq!(modified_diff["additions"], 1);
    assert_eq!(modified_diff["deletions"], 1);
    let patch = modified_diff["patch"].as_str().unwrap();
    assert!(patch.contains("+four\n"), "{patch}");
    assert!(!patch.contains("+three\n"), "{patch}");
}

#[tokio::test]
async fn revert_clear_restores_files_and_drops_snapshots() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let store = harness.state.store().unwrap().clone();

    let work = tempfile::tempdir().unwrap();
    let session = create_session_in(&app, work.path(), "snapshot clear").await;
    let sid = session_id(&session);
    let boundary = prompt(&app, &sid, "change").await;

    let modified = work.path().join("a.txt");
    let added = work.path().join("b.txt");
    std::fs::write(&modified, "before\n").unwrap();
    capture(&store, &sid, &boundary, &modified, true, Some("before\n")).await;
    capture(&store, &sid, &boundary, &added, false, None).await;

    std::fs::write(&modified, "after\n").unwrap();
    std::fs::write(&added, "created\n").unwrap();

    let (status, staged) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/revert/stage"),
        Some(json!({"messageID": boundary})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "stage: {staged:?}");
    assert_eq!(
        staged.unwrap()["data"]["files"].as_array().unwrap().len(),
        2
    );

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/session/{sid}/revert"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert_eq!(std::fs::read_to_string(&modified).unwrap(), "before\n");
    assert!(
        !added.exists(),
        "a file added after the boundary is removed"
    );
    assert!(
        store.snapshots_after(&sid, 0).await.unwrap().is_empty(),
        "clear drops the restored snapshots"
    );
    let (_, session_body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert!(session_body.unwrap()["data"].get("revert").is_none());
    assert_eq!(
        message_ids(&app, &sid).await,
        vec![boundary],
        "clear keeps the transcript"
    );
}

#[tokio::test]
async fn revert_commit_truncates_and_drops_snapshots() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let store = harness.state.store().unwrap().clone();

    let work = tempfile::tempdir().unwrap();
    let session = create_session_in(&app, work.path(), "snapshot commit").await;
    let sid = session_id(&session);
    let keep = prompt(&app, &sid, "keep").await;
    let boundary = prompt(&app, &sid, "drop").await;

    let file = work.path().join("a.txt");
    std::fs::write(&file, "before\n").unwrap();
    capture(&store, &sid, &boundary, &file, true, Some("before\n")).await;
    std::fs::write(&file, "after\n").unwrap();

    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/revert/stage"),
        Some(json!({"messageID": boundary})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(store.snapshots_after(&sid, 0).await.unwrap().len(), 1);

    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/revert/commit"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert_eq!(message_ids(&app, &sid).await, vec![keep]);
    assert!(store.snapshots_after(&sid, 0).await.unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "after\n",
        "commit keeps the current tree"
    );
    let (_, session_body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert!(session_body.unwrap()["data"].get("revert").is_none());
}

#[tokio::test]
async fn revert_ignores_paths_outside_the_session_directory() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let store = harness.state.store().unwrap().clone();

    let work = tempfile::tempdir().unwrap();
    let session = create_session_in(&app, work.path(), "snapshot escape").await;
    let sid = session_id(&session);
    let boundary = prompt(&app, &sid, "boundary").await;

    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("outside.txt");
    std::fs::write(&external, "original\n").unwrap();
    capture(&store, &sid, &boundary, &external, true, Some("original\n")).await;
    std::fs::write(&external, "changed\n").unwrap();

    let (status, staged) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/revert/stage"),
        Some(json!({"messageID": boundary})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "stage: {staged:?}");
    assert_eq!(
        staged.unwrap()["data"]["files"],
        json!([]),
        "an external path is not part of the session diff"
    );

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/session/{sid}/revert"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        std::fs::read_to_string(&external).unwrap(),
        "changed\n",
        "an external file is never restored"
    );
}

#[tokio::test]
async fn revert_stage_rejects_unknown_boundary() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "missing boundary"})).await;
    let sid = session_id(&session);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/revert/stage"),
        Some(json!({"messageID": "msg_absent"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "MessageNotFoundError");
}
