use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use oc_config::{ConfigService, LoadOptions};
use oc_server::AppState;
use oc_store::{NewSnapshot, Store};
use oc_types::{EventEnvelope, ServerInfo, ServerPaths, now_millis};
use serde_json::{Value, json};
use tower::ServiceExt;

pub(crate) struct Harness {
    _db: tempfile::TempDir,
    _config: tempfile::TempDir,
    pub(crate) state: AppState,
}

pub(crate) async fn harness() -> Harness {
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

pub(crate) async fn call(
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

pub(crate) async fn create_session(app: &axum::Router, body: Value) -> Value {
    let (status, value) = call(app, Method::POST, "/api/session", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "create failed: {value:?}");
    value.unwrap()["data"].clone()
}

pub(crate) fn session_id(session: &Value) -> String {
    session["id"].as_str().unwrap().to_owned()
}

pub(crate) async fn prompt(app: &axum::Router, session_id: &str, text: &str) -> String {
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

pub(crate) async fn message_ids(app: &axum::Router, session_id: &str) -> Vec<String> {
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
pub(crate) async fn create_session_in(
    app: &axum::Router,
    directory: &std::path::Path,
    title: &str,
) -> Value {
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
pub(crate) async fn capture(
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
pub(crate) fn take_events(
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

pub(crate) async fn message_list(app: &axum::Router, session_id: &str) -> Vec<Value> {
    let (status, value) = call(
        app,
        Method::GET,
        &format!("/api/session/{session_id}/message?order=asc"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "message list failed: {value:?}");
    value.unwrap()["data"].as_array().unwrap().clone()
}
