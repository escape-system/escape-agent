//! Scenario tests for the extended session surface: fork, revert, forms,
//! export/import, statistics, instructions and saved permissions.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use oc_config::{ConfigService, LoadOptions};
use oc_server::{AppState, router};
use oc_store::{NewMessage, NewSnapshot, Store};
use oc_types::{EventEnvelope, ServerInfo, ServerPaths, now_millis};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    _db: tempfile::TempDir,
    _config: tempfile::TempDir,
    state: AppState,
}

async fn harness() -> Harness {
    let db = tempfile::tempdir().unwrap();
    let store = Store::open(db.path().join("session_ext.sqlite"))
        .await
        .unwrap();

    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        config_dir.path().join("opencode.json"),
        r#"{"model":"test/model","agent":{"build":{"model":"test/model"}}}"#,
    )
    .unwrap();
    let skill = config_dir.path().join(".opencode/skill/demo");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: Demo\ndescription: demo skill\n---\nDo the demo.\n",
    )
    .unwrap();
    let home = config_dir.path().to_string_lossy().into_owned();
    let mut env = HashMap::new();
    env.insert("HOME".to_owned(), home.clone());
    env.insert("USERPROFILE".to_owned(), home.clone());
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        config_dir.path().join("xdg").to_string_lossy().into_owned(),
    );
    env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());
    let config = ConfigService::load(LoadOptions {
        directory: config_dir.path().to_path_buf(),
        env,
    })
    .unwrap();

    let state = AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: Some("127.0.0.1".to_owned()),
            urls: vec!["http://127.0.0.1:4096".to_owned()],
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        config_dir.path().to_path_buf(),
    )
    .with_store(Arc::new(store))
    .with_config(Arc::new(config));

    Harness {
        _db: db,
        _config: config_dir,
        state,
    }
}

async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Option<Value>) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 << 20).await.unwrap();
    let value = if bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&bytes).unwrap())
    };
    (status, value)
}

async fn create_session(app: &axum::Router, body: Value) -> Value {
    let (status, value) = call(app, Method::POST, "/api/session", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "create failed: {value:?}");
    value.unwrap()["data"].clone()
}

fn session_id(session: &Value) -> String {
    session["id"].as_str().unwrap().to_owned()
}

async fn prompt(app: &axum::Router, session_id: &str, text: &str) -> String {
    let (status, value) = call(
        app,
        Method::POST,
        &format!("/api/session/{session_id}/prompt"),
        Some(json!({ "text": text })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "prompt failed: {value:?}");
    value.unwrap()["data"]["id"].as_str().unwrap().to_owned()
}

async fn message_ids(app: &axum::Router, session_id: &str) -> Vec<String> {
    let (status, value) = call(
        app,
        Method::GET,
        &format!("/api/session/{session_id}/message?order=asc"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "message list failed: {value:?}");
    value.unwrap()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Creates a session whose working directory is `directory`.
async fn create_session_in(app: &axum::Router, directory: &std::path::Path, title: &str) -> Value {
    create_session(
        app,
        json!({
            "title": title,
            "location": { "directory": directory.to_string_lossy() },
        }),
    )
    .await
}

/// Simulates the runtime capture of `path` before the write owned by `message_id`.
async fn capture(
    store: &Store,
    session_id: &str,
    message_id: &str,
    path: &std::path::Path,
    existed: bool,
    content: Option<&str>,
) {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let seq = store
        .message_seq(session_id, message_id)
        .await
        .unwrap()
        .unwrap();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    store
        .insert_snapshot(NewSnapshot {
            id: format!("snap_test_{sequence}"),
            session_id: session_id.to_owned(),
            message_id: message_id.to_owned(),
            seq: i64::try_from(seq).unwrap(),
            tool_call_id: format!("call_test_{sequence}"),
            path: path.to_string_lossy().into_owned(),
            existed,
            skipped: false,
            content: content.map(|text| text.as_bytes().to_vec()),
            time_created: now_millis(),
        })
        .await
        .unwrap();
}

/// Drains every queued event of `kind`, ignoring the rest of the bus traffic.
fn take_events(
    events: &mut tokio::sync::broadcast::Receiver<Arc<EventEnvelope>>,
    kind: &str,
) -> Vec<Arc<EventEnvelope>> {
    let mut found = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event.kind == kind {
            found.push(event);
        }
    }
    found
}

#[allow(clippy::too_many_lines)] // one linear lifecycle scenario; splitting hides the created-then-replied order
#[tokio::test]
async fn forms_report_lifecycle_and_conflicts() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "forms"})).await;
    let sid = session_id(&session);
    let mut events = harness.state.bus().subscribe();
    let form = json!({
        "id": "frm_demo",
        "title": "Pick one",
        "metadata": {"source": "test"},
        "fields": [{"key": "answer", "type": "string", "required": true}],
    });
    let (status, created) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form"),
        Some(form.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create form: {created:?}");
    let created = created.unwrap()["data"].clone();
    assert_eq!(created["id"], "frm_demo");
    assert_eq!(created["sessionID"], sid);
    assert!(created.get("state").is_none(), "Form.Info has no state");

    let published = take_events(&mut events, "form.created");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].data, json!({ "form": created }));
    assert!(published[0].durable.is_none(), "form events are ephemeral");
    assert_eq!(
        published[0].location.as_ref().unwrap().directory,
        session["location"]["directory"].as_str().unwrap()
    );

    let (status, duplicate) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form"),
        Some(form),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(duplicate.unwrap()["_tag"], "ConflictError");

    let (status, list) = call(&app, Method::GET, &format!("/api/session/{sid}/form"), None).await;
    assert_eq!(status, StatusCode::OK);
    let list = list.unwrap();
    assert_eq!(list["data"].as_array().unwrap().len(), 1);

    let (status, detail) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/form/frm_demo"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail.unwrap()["data"]["state"],
        json!({"status": "pending"})
    );

    let (status, invalid) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form/frm_demo/reply"),
        Some(json!({"answer": {"unknown": "x"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.unwrap()["_tag"], "FormInvalidAnswerError");
    assert!(
        take_events(&mut events, "form.replied").is_empty(),
        "a rejected reply must not publish"
    );

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form/frm_demo/reply"),
        Some(json!({"answer": {"answer": "yes"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "reply: {body:?}");

    let published = take_events(&mut events, "form.replied");
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].data,
        json!({ "id": "frm_demo", "sessionID": sid, "answer": {"answer": "yes"} })
    );
    assert!(published[0].durable.is_none(), "form events are ephemeral");

    let (_, detail) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/form/frm_demo"),
        None,
    )
    .await;
    let detail = detail.unwrap()["data"].clone();
    assert_eq!(detail["state"]["status"], "answered");
    assert_eq!(detail["state"]["answer"], json!({"answer": "yes"}));

    let (status, settled) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form/frm_demo/reply"),
        Some(json!({"answer": {"answer": "yes"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(settled.unwrap()["_tag"], "FormAlreadySettledError");
    assert!(
        take_events(&mut events, "form.replied").is_empty(),
        "a settled form must not publish a second reply"
    );

    let (status, missing) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/form/frm_absent"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing.unwrap()["_tag"], "FormNotFoundError");
}

#[tokio::test]
async fn form_cancel_stores_message() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "cancel"})).await;
    let sid = session_id(&session);
    let mut events = harness.state.bus().subscribe();
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form"),
        Some(json!({
            "title": "Cancel me",
            "fields": [{"key": "answer", "type": "string"}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, list) = call(&app, Method::GET, &format!("/api/session/{sid}/form"), None).await;
    let form_id = list.unwrap()["data"][0]["id"].as_str().unwrap().to_owned();

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/session/{sid}/form/{form_id}?message=not%20now"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let published = take_events(&mut events, "form.cancelled");
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].data,
        json!({ "id": form_id, "sessionID": sid })
    );
    assert!(published[0].durable.is_none(), "form events are ephemeral");
    assert_eq!(
        published[0].location.as_ref().unwrap().directory,
        session["location"]["directory"].as_str().unwrap()
    );

    let (status, detail) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/form/{form_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let state = detail.unwrap()["data"]["state"].clone();
    assert_eq!(state["status"], "cancelled");
    assert_eq!(state["message"], "not now");
}

#[tokio::test]
async fn fork_copies_history_before_boundary() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(
        &app,
        json!({"title": "parent", "agent": "build", "metadata": {"k": "v"}}),
    )
    .await;
    let sid = session_id(&session);
    prompt(&app, &sid, "first").await;
    let second = prompt(&app, &sid, "second").await;
    let mut events = harness.state.bus().subscribe();

    let (status, forked) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/fork"),
        Some(json!({"before": second})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fork: {forked:?}");
    let forked = forked.unwrap()["data"].clone();
    let child = session_id(&forked);
    assert_ne!(child, sid);
    assert!(forked.get("parentID").is_none() || forked["parentID"].is_null());
    assert_eq!(forked["title"], "parent");
    assert_eq!(forked["agent"], "build");
    assert_eq!(forked["metadata"], json!({"k": "v"}));
    assert_eq!(
        forked["location"]["directory"],
        session["location"]["directory"]
    );

    let messages = message_ids(&app, &child).await;
    assert_eq!(
        messages.len(),
        1,
        "only the boundary-excluded prefix copies"
    );
    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{child}/message?order=asc"),
        None,
    )
    .await;
    assert_eq!(body.unwrap()["data"][0]["text"], "first");

    let published = take_events(&mut events, "session.forked");
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].data,
        json!({
            "sessionID": child,
            "parentID": sid,
            "boundary": { "type": "before", "messageID": second },
        })
    );
    assert!(
        published[0].data.get("instructions").is_none(),
        "optional instructions are omitted, never null"
    );
    let durable = published[0].durable.as_ref().expect("fork is durable");
    assert_eq!(durable.version, 2);
    assert_eq!(durable.aggregate_id, child);
    assert!(durable.seq > 0, "the fork event is persisted with a seq");
    assert_eq!(
        published[0].location.as_ref().unwrap().directory,
        session["location"]["directory"].as_str().unwrap()
    );

    let (status, missing) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/fork"),
        Some(json!({"before": "msg_absent"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing.unwrap()["_tag"], "MessageNotFoundError");
    assert!(
        take_events(&mut events, "session.forked").is_empty(),
        "a rejected fork must not publish"
    );

    // Without `before` the whole history copies and the boundary is `through`.
    let (status, forked) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/fork"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "through fork: {forked:?}");
    let through_child = session_id(&forked.unwrap()["data"]);
    assert_eq!(message_ids(&app, &through_child).await.len(), 2);
    let published = take_events(&mut events, "session.forked");
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].data,
        json!({
            "sessionID": through_child,
            "parentID": sid,
            "boundary": { "type": "through", "messageID": second },
        })
    );
    assert_eq!(published[0].durable.as_ref().unwrap().version, 2);
}

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

#[tokio::test]
async fn context_environment_and_view() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "context"})).await;
    let sid = session_id(&session);
    prompt(&app, &sid, "hello").await;

    let (status, context) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/context"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(context.unwrap()["data"].as_array().unwrap().len(), 1);

    let (status, _) = call(
        &app,
        Method::PUT,
        &format!("/api/session/{sid}/environment"),
        Some(json!({"variables": {"FOO": "bar"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, session_body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert_eq!(
        session_body.unwrap()["data"]["metadata"]["environment"],
        json!({"FOO": "bar"})
    );

    let idle = now_millis();
    let mut events = harness.state.bus().subscribe();
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/view"),
        Some(json!({"idle": idle})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let published = take_events(&mut events, "session.viewed");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].data, json!({ "sessionID": sid, "idle": idle }));
    let durable = published[0].durable.as_ref().expect("viewed is durable");
    assert_eq!(durable.version, 1);
    assert_eq!(durable.aggregate_id, sid);
    assert!(durable.seq >= 0);
    assert_eq!(
        published[0].location.as_ref().unwrap().directory,
        session["location"]["directory"].as_str().unwrap()
    );

    let (_, session_body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert_eq!(session_body.unwrap()["data"]["time"]["viewed"], idle);

    let (status, _) = call(
        &app,
        Method::PUT,
        &format!("/api/session/{sid}/environment"),
        Some(json!({"variables": {"FOO": 1}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn export_import_round_trip_and_conflict() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(
        &app,
        json!({"title": "export", "metadata": {"apiKey": "sk-secret", "keep": "yes"}}),
    )
    .await;
    let sid = session_id(&session);
    prompt(&app, &sid, "one").await;
    prompt(&app, &sid, "two").await;

    let (status, exported) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/{sid}/export"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let exported = exported.unwrap()["data"].clone();
    assert_eq!(exported["messages"].as_array().unwrap().len(), 2);

    let (status, sanitized) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/{sid}/export?sanitize=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sanitized = sanitized.unwrap()["data"].clone();
    assert!(sanitized["info"]["metadata"].get("apiKey").is_none());
    assert_eq!(sanitized["info"]["metadata"]["keep"], "yes");

    let mut info = exported["info"].clone();
    info["id"] = json!("ses_imported_test");
    let body = json!({"info": info, "messages": exported["messages"].clone()});
    let (status, imported) = call(
        &app,
        Method::POST,
        "/api/experimental/session/import",
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "import: {imported:?}");
    let imported = imported.unwrap()["data"].clone();
    assert_eq!(imported["id"], "ses_imported_test");
    assert_eq!(imported["title"], "export");
    let imported_ids = message_ids(&app, "ses_imported_test").await;
    assert_eq!(imported_ids.len(), 2);
    // `session_message.id` is database-wide unique, so an import next to its
    // source must not reuse the exported ids.
    let source_ids: Vec<&str> = exported["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["id"].as_str())
        .collect();
    assert!(
        imported_ids
            .iter()
            .all(|id| !source_ids.contains(&id.as_str()))
    );

    let (status, conflict) = call(
        &app,
        Method::POST,
        "/api/experimental/session/import",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict.unwrap()["_tag"], "ConflictError");

    let (status, missing) = call(
        &app,
        Method::POST,
        "/api/experimental/session/import",
        Some(json!({
            "info": {"id": "ses_child_test", "parentID": "ses_absent"},
            "messages": [],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing.unwrap()["_tag"], "SessionNotFoundError");
}

#[tokio::test]
async fn stats_aggregate_messages() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "stats"})).await;
    let sid = session_id(&session);
    prompt(&app, &sid, "question").await;
    let now = now_millis();
    let store = harness.state.store().unwrap();
    store
        .append_message(NewMessage {
            id: "msg_stats_assistant".to_owned(),
            session_id: sid.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "model": {"providerID": "test", "id": "model"},
                "tokens": {
                    "input": 10,
                    "output": 5,
                    "reasoning": 1,
                    "cache": {"read": 2, "write": 3}
                },
                "cost": 0.5,
                "time": {"created": now},
                "content": [{
                    "type": "tool",
                    "id": "call_1",
                    "name": "read",
                    "state": {"status": "completed"},
                    "time": {"created": now, "ran": now, "completed": now + 4}
                }],
            }),
            time_created: now,
        })
        .await
        .unwrap();

    let until = now_millis() + 1_000_000;
    let (code, stats) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/stats?from=0&to={until}&tools=detail"),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "stats: {stats:?}");
    let stats = stats.unwrap()["data"].clone();
    assert_eq!(stats["prompts"], 1);
    assert_eq!(stats["steps"], 1);
    assert_eq!(stats["sessions"], 1);
    assert_eq!(stats["tokens"]["input"], 10);
    assert_eq!(stats["tokens"]["output"], 5);
    assert_eq!(stats["tokens"]["cache"]["read"], 2);
    assert_eq!(stats["cost"], 0.5);
    assert_eq!(stats["tools"]["totals"]["calls"], 1);
    assert_eq!(stats["tools"]["totals"]["succeeded"], 1);
    assert_eq!(stats["tools"]["usage"][0]["name"], "read");
    assert_eq!(stats["tools"]["usage"][0]["durationP50"], 4.0);
    assert_eq!(stats["models"][0]["model"]["providerID"], "test");
    assert_eq!(stats["activeDays"], 1);
    assert_eq!(stats["streak"], 1);
    assert_eq!(stats["range"]["from"], 0);

    let (code, _) = call(
        &app,
        Method::GET,
        "/api/experimental/session/stats?from=10&to=5",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn skill_activation_enqueues_skill_content() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "skill"})).await;
    let sid = session_id(&session);

    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/experimental/session/{sid}/skill"),
        Some(json!({"id": "demo", "resume": false})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, inbox) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let item = inbox.unwrap()["data"][0].clone();
    assert_eq!(item["type"], "synthetic");
    let text = item["payload"]["text"].as_str().unwrap();
    assert!(text.contains("<skill_content name=\"Demo\">"));
    assert!(text.contains("Do the demo."));

    let (status, missing) = call(
        &app,
        Method::POST,
        &format!("/api/experimental/session/{sid}/skill"),
        Some(json!({"id": "absent"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing.unwrap()["_tag"], "SkillNotFoundError");
}

#[tokio::test]
async fn instruction_entries_round_trip_and_limit() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "instructions"})).await;
    let sid = session_id(&session);

    let (status, _) = call(
        &app,
        Method::PUT,
        &format!("/api/experimental/session/{sid}/instructions/entries/notes"),
        Some(json!({"value": {"a": 1}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, entries) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/{sid}/instructions/entries"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = entries.unwrap();
    assert_eq!(entries["data"][0]["key"], "notes");
    assert_eq!(entries["data"][0]["value"], json!({"a": 1}));

    let (status, _) = call(
        &app,
        Method::PUT,
        &format!("/api/experimental/session/{sid}/instructions/entries/Bad%20Key"),
        Some(json!({"value": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, too_large) = call(
        &app,
        Method::PUT,
        &format!("/api/experimental/session/{sid}/instructions/entries/huge"),
        Some(json!({"value": "x".repeat(1024 * 1024 + 16)})),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let too_large = too_large.unwrap();
    assert_eq!(too_large["_tag"], "InstructionEntryValueTooLargeError");
    assert_eq!(too_large["maxBytes"], 1_048_576);
    assert!(too_large["actualBytes"].as_u64().unwrap() > 1_048_576);

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/experimental/session/{sid}/instructions/entries/notes"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, entries) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/{sid}/instructions/entries"),
        None,
    )
    .await;
    assert_eq!(entries.unwrap()["data"], json!([]));
}

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

#[tokio::test]
async fn missing_sessions_are_404() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    for uri in [
        "/api/session/ses_absent/context",
        "/api/session/ses_absent/diff",
        "/api/session/ses_absent/form",
        "/api/experimental/session/ses_absent/instructions/entries",
    ] {
        let (status, body) = call(&app, Method::GET, uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body:?}");
        assert_eq!(body.unwrap()["_tag"], "SessionNotFoundError");
    }
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/session/ses_absent/view",
        Some(json!({"idle": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
