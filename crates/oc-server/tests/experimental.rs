//! Experimental and debug route tests: confined writes, the polling watch
//! registry, and the truthful failure tags for services that do not exist.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use oc_config::{ConfigService, LoadOptions};
use oc_server::data::experimental;
use oc_server::{AppState, router};
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    base: tempfile::TempDir,
    root: PathBuf,
    state: AppState,
}

fn harness() -> Harness {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("watched.txt"), "initial").unwrap();

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
        root.clone(),
    );

    Harness { base, root, state }
}

async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, headers, bytes.to_vec())
}

async fn json_call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let (status, _, bytes) = request(
        app,
        method,
        uri,
        &[(CONTENT_TYPE.as_str(), "application/json")],
        body.to_string().into_bytes(),
    )
    .await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

#[tokio::test]
async fn write_creates_parents_and_reports_the_location() {
    let harness = harness();
    let app = router(harness.state);

    let (status, _, bytes) = request(
        &app,
        Method::POST,
        "/api/experimental/fs/write?path=sub/dir/new.txt",
        &[(CONTENT_TYPE.as_str(), "application/octet-stream")],
        b"hello write".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bytes:?}");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let reported = body["data"]["path"].as_str().unwrap();
    // `FileSystem.Write` is exactly `{path}` (additionalProperties: false).
    let keys: Vec<&str> = body["data"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["path"]);
    assert!(
        reported.replace('\\', "/").ends_with("sub/dir/new.txt"),
        "{reported}"
    );
    assert_eq!(
        std::fs::read(harness.root.join("sub/dir/new.txt")).unwrap(),
        b"hello write"
    );

    // A second write replaces the file atomically.
    let (status, _, bytes) = request(
        &app,
        Method::POST,
        "/api/experimental/fs/write?path=sub/dir/new.txt",
        &[],
        b"replaced".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bytes:?}");
    assert_eq!(
        std::fs::read(harness.root.join("sub/dir/new.txt")).unwrap(),
        b"replaced"
    );
}

#[tokio::test]
async fn write_rejects_paths_outside_the_location() {
    let harness = harness();
    let app = router(harness.state);

    for uri in [
        "/api/experimental/fs/write?path=../escape.txt",
        "/api/experimental/fs/write?path=%2e%2e%2fescape.txt",
        "/api/experimental/fs/write?path=sub/../../escape.txt",
    ] {
        let (status, _, bytes) = request(&app, Method::POST, uri, &[], b"nope".to_vec()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["_tag"], "FileNotFoundError", "{uri}");
    }
    assert!(!harness.root.parent().unwrap().join("escape.txt").exists());

    // An absolute path outside the location is rejected too.
    let outside = harness.base.path().join("outside.txt");
    let encoded = outside
        .to_string_lossy()
        .replace('%', "%25")
        .replace('\\', "%5C")
        .replace('/', "%2F");
    let uri = format!("/api/experimental/fs/write?path={encoded}");
    let (status, _, bytes) = request(&app, Method::POST, &uri, &[], b"nope".to_vec()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["_tag"],
        "FileNotFoundError"
    );
    assert!(!outside.exists());

    // A missing path parameter is a schema error, not a write to the cwd.
    let (status, _, bytes) = request(
        &app,
        Method::POST,
        "/api/experimental/fs/write",
        &[],
        b"x".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bytes:?}");
}

#[tokio::test]
async fn watch_registration_publishes_only_real_changes() {
    let harness = harness();
    let mut events = harness.state.bus().subscribe();
    // Use a short interval instead of the production one-second tick.
    let app = experimental::routes_with_watch_interval(Duration::from_millis(20))
        .with_state(harness.state);

    let (status, value) = json_call(
        &app,
        Method::POST,
        "/api/experimental/fs/watch",
        &json!({"path": "watched.txt"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{value:?}");

    std::fs::write(harness.root.join("watched.txt"), "changed length").unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.expect("bus open");
            if event.kind == "filesystem.changed" {
                break event;
            }
        }
    })
    .await
    .expect("filesystem.changed was not published");
    assert_eq!(event.data["event"], "change");
    assert!(
        event.data["file"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("watched.txt")
    );
    assert_eq!(
        event.location.as_ref().unwrap().directory,
        harness.root.to_string_lossy().as_ref()
    );

    // Re-registering is idempotent.
    let (status, _) = json_call(
        &app,
        Method::POST,
        "/api/experimental/fs/watch",
        &json!({"path": "watched.txt"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, value) = json_call(
        &app,
        Method::DELETE,
        "/api/experimental/fs/watch",
        &json!({"path": "watched.txt"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{value:?}");

    // Let the aborted task settle, drain anything already queued, then prove
    // that a further change is not published.
    tokio::time::sleep(Duration::from_millis(80)).await;
    while events.try_recv().is_ok() {}
    std::fs::write(harness.root.join("watched.txt"), "after unwatch").unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        events.try_recv().is_err(),
        "a cancelled watch must not publish"
    );
}

#[tokio::test]
async fn watch_rejects_paths_outside_the_location() {
    let harness = harness();
    let app = experimental::routes_with_watch_interval(Duration::from_millis(20))
        .with_state(harness.state);

    let (status, value) = json_call(
        &app,
        Method::POST,
        "/api/experimental/fs/watch",
        &json!({"path": "../secret.txt"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{value:?}");
    assert_eq!(value["_tag"], "FileNotFoundError");
}

#[tokio::test]
async fn media_is_not_faked() {
    let harness = harness();
    let app = router(harness.state);
    let hash = "a".repeat(64);

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/experimental/media/{hash}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["_tag"], "FileNotFoundError");
    assert_eq!(
        body["path"],
        format!("/api/experimental/media/{hash}").as_str()
    );
}

#[tokio::test]
async fn generate_routes_validate_then_report_the_missing_service() {
    let harness = harness();
    let app = router(harness.state);

    let (status, body) = json_call(
        &app,
        Method::POST,
        "/api/experimental/generate/text",
        &json!({"prompt": "hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body:?}");
    assert_eq!(body["_tag"], "ServiceUnavailableError");

    let (status, body) = json_call(
        &app,
        Method::POST,
        "/api/experimental/generate/text",
        &json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");

    let model = json!({"id": "test-model", "providerID": "test"});
    let (status, _) = json_call(
        &app,
        Method::POST,
        "/api/experimental/generate/image",
        &json!({"model": model.clone(), "prompt": "draw"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, body) = json_call(
        &app,
        Method::POST,
        "/api/experimental/generate/image",
        &json!({"model": model.clone(), "prompt": "draw", "n": 0}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");

    let (status, body) = json_call(
        &app,
        Method::POST,
        "/api/experimental/generate/decisions",
        &json!({
            "model": model.clone(),
            "input": [{"type": "text", "text": "evidence"}],
            "questions": []
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");

    let (status, _) = json_call(
        &app,
        Method::POST,
        "/api/experimental/generate/decisions",
        &json!({
            "model": model,
            "input": [{"type": "text", "text": "evidence"}],
            "questions": [{"type": "predicate", "name": "p", "instructions": "is it true?"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, _) = json_call(
        &app,
        Method::POST,
        "/api/session/ses_test/generate",
        &json!({"prompt": "continue"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn location_reload_requires_a_config_service() {
    let harness = harness();
    let app = router(harness.state.clone());
    let (status, _, bytes) =
        request(&app, Method::POST, "/api/location/reload", &[], Vec::new()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["_tag"], "ServiceUnavailableError");

    let config = config_service(harness.base.path());
    let configured = harness.state.with_config(Arc::new(config));
    let app = router(configured);
    let (status, _, bytes) =
        request(&app, Method::POST, "/api/location/reload", &[], Vec::new()).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{bytes:?}");
}

fn config_service(base: &Path) -> ConfigService {
    let config_dir = base.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let home = config_dir.to_string_lossy().into_owned();
    let mut env = HashMap::new();
    env.insert("HOME".to_owned(), home.clone());
    env.insert("USERPROFILE".to_owned(), home.clone());
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        config_dir.join("xdg").to_string_lossy().into_owned(),
    );
    env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());
    ConfigService::load(LoadOptions {
        directory: config_dir,
        env,
    })
    .unwrap()
}

#[tokio::test]
async fn debug_and_unimplemented_routes_report_truthful_statuses() {
    let harness = harness();
    let app = router(harness.state);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/debug/location")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["project"]["id"].as_str().unwrap().starts_with("prj_"));
    assert!(body.get("location").is_none(), "bare LocationRef: {body}");

    for (method, uri) in [
        (Method::DELETE, "/api/debug/location"),
        (Method::POST, "/api/debug/heap-dump"),
        (Method::GET, "/api/experimental/migration/v1"),
    ] {
        let (status, _, bytes) = request(&app, method, uri, &[], Vec::new()).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{uri}");
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["_tag"], "NotImplementedError", "{uri}");
    }

    let (status, body) = json_call(
        &app,
        Method::POST,
        "/api/experimental/client-mcp/token?name=client",
        &json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["_tag"], "ServiceUnavailableError");
}
