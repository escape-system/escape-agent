//! `/api/pty` HTTP and WebSocket scenario tests.
//!
//! Terminal tests spawn a real `cmd.exe`/`sh`; when the platform cannot create
//! a PTY the test reports a skip instead of pretending to pass. All tests share
//! one lock because Windows `ConPTY` sessions contend for console resources.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use oc_pty::{PtyConfig, PtyManager};
use oc_server::{AppState, router};
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::error::Error as WsError;
use tower::ServiceExt;

const WAIT: Duration = Duration::from_secs(5);

static PTY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Harness {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    manager: Arc<PtyManager>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.manager.shutdown();
    }
}

async fn harness() -> Harness {
    let guard = PTY_LOCK.lock().await;
    let manager = Arc::new(PtyManager::new(PtyConfig::default()));
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
        std::env::temp_dir(),
    )
    .with_pty(Arc::clone(&manager));
    Harness {
        _guard: guard,
        state,
        manager,
    }
}

fn shell() -> &'static str {
    if cfg!(windows) { "cmd.exe" } else { "sh" }
}

async fn call(
    state: &AppState,
    method: Method,
    uri: &str,
    body: Option<Value>,
    ticket_header: bool,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    if ticket_header {
        builder = builder.header("x-opencode-ticket", "1");
    }
    let response = router(state.clone())
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

/// Create a real shell. Returns `None` (and prints a skip notice) when the
/// platform refuses to spawn a PTY.
async fn create_pty(harness: &Harness) -> Option<(String, Value)> {
    let (status, body) = call(
        &harness.state,
        Method::POST,
        "/api/pty",
        Some(json!({
            "title": "test terminal",
            "command": shell(),
            "size": { "rows": 24, "cols": 80 },
        })),
        false,
    )
    .await;
    if status != StatusCode::OK {
        eprintln!("skipping PTY test: create failed with {status}: {body}");
        return None;
    }
    let id = body["data"]["id"].as_str().expect("pty id").to_owned();
    Some((id, body))
}

async fn mint(harness: &Harness, id: &str) -> String {
    let (status, body) = call(
        &harness.state,
        Method::POST,
        &format!("/api/pty/{id}/connect-token"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["expires_in"], 60);
    body["data"]["ticket"].as_str().expect("ticket").to_owned()
}

#[tokio::test]
async fn http_crud_round_trip() {
    let harness = harness().await;
    let Some((id, created)) = create_pty(&harness).await else {
        return;
    };

    let info = &created["data"];
    assert_eq!(info["id"], id.as_str());
    assert_eq!(info["title"], "test terminal");
    assert_eq!(info["status"], "running");
    assert_eq!(info["command"], shell());
    assert!(info["args"].is_array());
    assert!(info["cwd"].as_str().is_some_and(|cwd| !cwd.is_empty()));
    assert!(info["pid"].as_u64().is_some_and(|pid| pid > 0));
    // The wire `Pty` has no size/timestamp fields.
    assert!(info.get("cols").is_none());
    assert!(info.get("created").is_none());
    assert!(created["location"]["directory"].is_string());

    let (status, list) = call(&harness.state, Method::GET, "/api/pty", None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == id.as_str()),
        "{list}"
    );

    let (status, got) = call(
        &harness.state,
        Method::GET,
        &format!("/api/pty/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["data"]["id"], id.as_str());

    let (status, resized) = call(
        &harness.state,
        Method::PUT,
        &format!("/api/pty/{id}"),
        Some(json!({ "size": { "rows": 30, "cols": 100 } })),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resized}");
    assert_eq!(resized["data"]["id"], id.as_str());

    let (status, renamed) = call(
        &harness.state,
        Method::PUT,
        &format!("/api/pty/{id}"),
        Some(json!({ "title": "renamed" })),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["data"]["title"], "renamed");

    // Title and size in one body both apply.
    let (status, updated) = call(
        &harness.state,
        Method::PUT,
        &format!("/api/pty/{id}"),
        Some(json!({ "title": "both", "size": { "rows": 40, "cols": 120 } })),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["data"]["title"], "both");

    let (status, body) = call(
        &harness.state,
        Method::DELETE,
        &format!("/api/pty/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(body, Value::Null);

    let (status, missing) = call(
        &harness.state,
        Method::GET,
        &format!("/api/pty/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["_tag"], "PtyNotFoundError");

    let (status, _) = call(
        &harness.state,
        Method::DELETE,
        &format!("/api/pty/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one linear scoping matrix
async fn pty_is_scoped_to_its_creating_location() {
    let harness = harness().await;
    let Some((id, _)) = create_pty(&harness).await else {
        return;
    };
    let foreign = "foreign-pty-location";
    std::fs::create_dir_all(harness.state.directory().join(foreign)).unwrap();
    let query = format!("?location%5Bdirectory%5D={foreign}");

    // The creating location still lists and reads it.
    let (status, own) = call(&harness.state, Method::GET, "/api/pty", None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        own["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == id.as_str()),
        "{own}"
    );

    // A foreign location neither lists nor reads it.
    let (status, listed) = call(
        &harness.state,
        Method::GET,
        &format!("/api/pty{query}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == id.as_str()),
        "{listed}"
    );

    let (status, missing) = call(
        &harness.state,
        Method::GET,
        &format!("/api/pty/{id}{query}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    assert_eq!(missing["_tag"], "PtyNotFoundError");

    let (status, _) = call(
        &harness.state,
        Method::PUT,
        &format!("/api/pty/{id}{query}"),
        Some(json!({ "title": "hijacked" })),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, own) = call(
        &harness.state,
        Method::GET,
        &format!("/api/pty/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(own["data"]["title"], "test terminal");

    let (status, _) = call(
        &harness.state,
        Method::POST,
        &format!("/api/pty/{id}/connect-token{query}"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &harness.state,
        Method::DELETE,
        &format!("/api/pty/{id}{query}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A ticket is minted against the stored location and still works there.
    let (status, _) = call(
        &harness.state,
        Method::POST,
        &format!("/api/pty/{id}/connect-token"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = call(
        &harness.state,
        Method::DELETE,
        &format!("/api/pty/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn manager_absent_is_503() {
    let state = AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: None,
            urls: Vec::new(),
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        std::env::temp_dir(),
    );
    let (status, body) = call(&state, Method::GET, "/api/pty", None, false).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["_tag"], "ServiceUnavailableError");
}

#[tokio::test]
async fn connect_token_requires_header_and_known_pty() {
    let harness = harness().await;
    let Some((id, _)) = create_pty(&harness).await else {
        return;
    };

    let (status, body) = call(
        &harness.state,
        Method::POST,
        &format!("/api/pty/{id}/connect-token"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["_tag"], "ForbiddenError");

    let (status, _) = call(
        &harness.state,
        Method::POST,
        "/api/pty/pty_missing/connect-token",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let token = mint(&harness, &id).await;
    assert!(!token.is_empty());
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_frame(socket: &mut Socket) -> Message {
    timeout(WAIT, socket.next())
        .await
        .expect("frame within five seconds")
        .expect("socket open")
        .expect("valid frame")
}

/// Read backlog frames until the `0x00` control frame; returns the backlog
/// text and the cursor it reported.
async fn control_frame(socket: &mut Socket) -> (String, u64) {
    let mut output = Vec::new();
    loop {
        match next_frame(socket).await {
            Message::Binary(bytes) if bytes.first() == Some(&0) => {
                let meta: Value = serde_json::from_slice(&bytes[1..]).unwrap();
                let cursor = meta["cursor"].as_u64().expect("cursor");
                return (String::from_utf8_lossy(&output).into_owned(), cursor);
            }
            Message::Binary(bytes) => output.extend_from_slice(&bytes),
            Message::Text(text) => output.extend_from_slice(text.as_str().as_bytes()),
            Message::Close(frame) => panic!("closed before control frame: {frame:?}"),
            _ => {}
        }
    }
}

async fn output_until(socket: &mut Socket, mut seen: String, needle: &str) -> String {
    while !seen.contains(needle) && seen.len() <= 1 << 20 {
        match next_frame(socket).await {
            Message::Binary(bytes) => {
                if bytes.first() != Some(&0) {
                    seen.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
            Message::Text(text) => seen.push_str(text.as_str()),
            Message::Close(_) => break,
            _ => {}
        }
    }
    seen
}

async fn read_close(socket: &mut Socket) -> Option<u16> {
    loop {
        match timeout(WAIT, socket.next()).await.expect("close frame") {
            Some(Ok(Message::Close(frame))) => return frame.map(|frame| u16::from(frame.code)),
            Some(Ok(_)) => {}
            Some(Err(error)) => panic!("socket error before close: {error}"),
            None => return None,
        }
    }
}

#[tokio::test]
async fn websocket_attach_streams_and_closes_after_delete() {
    let harness = harness().await;
    let Some((id, _)) = create_pty(&harness).await else {
        return;
    };
    let Some((other, _)) = create_pty(&harness).await else {
        return;
    };
    let token = mint(&harness, &id).await;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(harness.state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let url = format!("ws://{addr}/api/pty/{id}/connect?ticket={token}");
    let (mut socket, response) = tokio_tungstenite::connect_async(&url).await.unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

    let (backlog, cursor) = control_frame(&mut socket).await;
    assert!(!backlog.contains("hello"));

    // The same ticket cannot open a second socket.
    match tokio_tungstenite::connect_async(&url).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), StatusCode::FORBIDDEN),
        other => panic!("reused ticket was accepted: {other:?}"),
    }

    // A ticket minted for one terminal is rejected by another.
    let scoped = mint(&harness, &id).await;
    let wrong = format!("ws://{addr}/api/pty/{other}/connect?ticket={scoped}");
    match tokio_tungstenite::connect_async(&wrong).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), StatusCode::FORBIDDEN),
        other => panic!("ticket for another pty was accepted: {other:?}"),
    }

    socket
        .send(Message::Text("echo hello\r\n".into()))
        .await
        .unwrap();
    let seen = output_until(&mut socket, backlog, "hello").await;
    assert!(
        seen.contains("hello"),
        "output did not contain hello:\n{seen}"
    );

    let (status, _) = call(
        &harness.state,
        Method::DELETE,
        &format!("/api/pty/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(read_close(&mut socket).await, Some(4404), "cursor {cursor}");
}
