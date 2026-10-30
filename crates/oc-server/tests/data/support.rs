use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use oc_config::{ConfigService, LoadOptions};
use oc_server::AppState;
use oc_store::Store;
use oc_types::{ServerInfo, ServerPaths};
use serde_json::Value;
use tower::ServiceExt;

pub(crate) struct Harness {
    _db: tempfile::TempDir,
    _config: tempfile::TempDir,
    pub(crate) state: AppState,
}

pub(crate) async fn harness() -> Harness {
    let db = tempfile::tempdir().unwrap();
    let store = Store::open(db.path().join("data.sqlite")).await.unwrap();

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
    let command = config_dir.path().join(".opencode/command");
    std::fs::create_dir_all(&command).unwrap();
    std::fs::write(
        command.join("deploy.md"),
        "---\ndescription: deploy things\n---\nDeploy $ARGUMENTS now.\n",
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
    // Shadow ambient inline config; whitespace is ignored by the loader.
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
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
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

pub(crate) struct SseReader {
    body: axum::body::Body,
    buffer: String,
}

impl SseReader {
    pub(crate) fn new(response: axum::response::Response) -> Self {
        Self {
            body: response.into_body(),
            buffer: String::new(),
        }
    }

    pub(crate) async fn next_event(&mut self) -> Option<Value> {
        loop {
            if let Some(index) = self.buffer.find("\n\n") {
                let chunk = self.buffer[..index].to_owned();
                self.buffer.drain(..index + 2);
                for line in chunk.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        return Some(serde_json::from_str(data).unwrap());
                    }
                }
                continue;
            }
            let frame = tokio::time::timeout(Duration::from_secs(3), self.body.frame())
                .await
                .ok()??
                .ok()?;
            let data = frame.into_data().ok()?;
            self.buffer.push_str(&String::from_utf8_lossy(&data));
        }
    }
}

pub(crate) async fn sse(app: &axum::Router, uri: &str) -> axum::response::Response {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    response
}
