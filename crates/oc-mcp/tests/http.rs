//! Streamable HTTP tests against local axum fake servers.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Json, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use oc_mcp::McpClient;
use oc_mcp::client::ClientOptions;
use oc_mcp::config::{McpServerConfig, RemoteConfig};
use oc_mcp::transport::Transport;
use oc_mcp::transport::http::{HttpOptions, HttpTransport};
use serde_json::{Value, json};
use tokio::net::TcpListener;

async fn spawn(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{address}/mcp")
}

fn remote(url: &str) -> McpServerConfig {
    McpServerConfig::Remote(RemoteConfig {
        url: url.to_owned(),
        headers: std::collections::BTreeMap::default(),
        oauth: None,
        disabled: None,
        codemode: None,
        timeout: None,
        protocol: None,
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "fake-http", "version": "1.0.0", "title": "Fake HTTP"},
        "instructions": "HTTP instructions"
    })
}

fn tool_list() -> Value {
    json!({
        "tools": [{
            "name": "echo",
            "description": "Echo",
            "inputSchema": {"type": "object", "properties": {"x": {"type": "number"}}}
        }]
    })
}

fn rpc(id: &Value, result: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn json_response(id: &Value, result: &Value, session: Option<&str>) -> Response {
    let mut response = Json(rpc(id, result)).into_response();
    if let Some(session) = session {
        response.headers_mut().insert(
            "Mcp-Session-Id",
            HeaderValue::from_str(session).expect("header"),
        );
    }
    response
}

fn sse_response(body: String, session: Option<&str>) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body))
        .expect("response");
    if let Some(session) = session {
        response.headers_mut().insert(
            "Mcp-Session-Id",
            HeaderValue::from_str(session).expect("header"),
        );
    }
    response
}

#[derive(Default)]
struct JsonState {
    initialize: AtomicU32,
    calls: AtomicU32,
    deleted: AtomicBool,
}

async fn json_post(State(state): State<Arc<JsonState>>, Json(body): Json<Value>) -> Response {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    match body
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "initialize" => {
            state.initialize.fetch_add(1, Ordering::SeqCst);
            json_response(&id, &initialize_result(), Some("sess-1"))
        }
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => json_response(&id, &tool_list(), Some("sess-1")),
        "tools/call" => {
            state.calls.fetch_add(1, Ordering::SeqCst);
            json_response(
                &id,
                &json!({"content": [{"type": "text", "text": "http-ok"}]}),
                Some("sess-1"),
            )
        }
        other => (
            StatusCode::OK,
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("unknown {other}")}
            })),
        )
            .into_response(),
    }
}

async fn json_delete(State(state): State<Arc<JsonState>>) -> StatusCode {
    state.deleted.store(true, Ordering::SeqCst);
    StatusCode::OK
}

#[tokio::test]
async fn http_json_lifecycle_and_session_shutdown() {
    let state = Arc::new(JsonState::default());
    let app = Router::new()
        .route(
            "/mcp",
            post(json_post)
                .get(|| async { StatusCode::METHOD_NOT_ALLOWED })
                .delete(json_delete),
        )
        .with_state(state.clone());
    let url = spawn(app).await;

    let client = McpClient::connect("http", &remote(&url), ClientOptions::new("."))
        .await
        .expect("connect");
    assert_eq!(client.session_id().as_deref(), Some("sess-1"));
    assert_eq!(client.tools().len(), 1);
    let result = client
        .call_tool("echo", json!({"x": 1}))
        .await
        .expect("call");
    assert_eq!(result["content"][0]["text"], "http-ok");
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);

    client.shutdown().await;
    assert!(state.deleted.load(Ordering::SeqCst), "DELETE was not sent");
}

struct SseState {
    initialize: AtomicU32,
}

async fn sse_post(State(state): State<Arc<SseState>>, Json(body): Json<Value>) -> Response {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let method = body
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match method {
        "initialize" => {
            state.initialize.fetch_add(1, Ordering::SeqCst);
            let notification = json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info"}});
            let response = rpc(&id, &initialize_result());
            let body = format!(
                "event: message\ndata: {notification}\n\nevent: message\nid: 7\ndata: {response}\n\n"
            );
            sse_response(body, Some("sse-session"))
        }
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => sse_response(format!("data: {}\n\n", rpc(&id, &tool_list())), None),
        "ping" => json_response(&id, &json!({}), None),
        _ => StatusCode::OK.into_response(),
    }
}

#[tokio::test]
async fn http_sse_responses_and_notifications() {
    let state = Arc::new(SseState {
        initialize: AtomicU32::new(0),
    });
    let app = Router::new()
        .route(
            "/mcp",
            post(sse_post).get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .with_state(state.clone());
    let url = spawn(app).await;

    let client = McpClient::connect("sse", &remote(&url), ClientOptions::new("."))
        .await
        .expect("connect");
    assert_eq!(client.session_id().as_deref(), Some("sse-session"));
    assert_eq!(client.server_info().expect("info").name, "fake-http");
    assert_eq!(client.tools().len(), 1);
    client.ping().await.expect("ping");
    assert_eq!(state.initialize.load(Ordering::SeqCst), 1);
    client.shutdown().await;
}

struct ExpiryState {
    initialize: AtomicU32,
    expire: AtomicBool,
    mode: u16,
    calls: AtomicU32,
}

async fn expiry_post(State(state): State<Arc<ExpiryState>>, Json(body): Json<Value>) -> Response {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    match body
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "initialize" => {
            let count = state.initialize.fetch_add(1, Ordering::SeqCst) + 1;
            json_response(&id, &initialize_result(), Some(&format!("sess-{count}")))
        }
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => json_response(&id, &tool_list(), None),
        "tools/call" => {
            state.calls.fetch_add(1, Ordering::SeqCst);
            if state.expire.swap(false, Ordering::SeqCst) {
                return if state.mode == 404 {
                    StatusCode::NOT_FOUND.into_response()
                } else {
                    (
                        StatusCode::BAD_REQUEST,
                        "Bad Request: Server not initialized",
                    )
                        .into_response()
                };
            }
            json_response(
                &id,
                &json!({"content": [{"type": "text", "text": "after-reconnect"}]}),
                None,
            )
        }
        _ => StatusCode::OK.into_response(),
    }
}

async fn expiry_case(mode: u16) {
    let state = Arc::new(ExpiryState {
        initialize: AtomicU32::new(0),
        expire: AtomicBool::new(true),
        mode,
        calls: AtomicU32::new(0),
    });
    let app = Router::new()
        .route(
            "/mcp",
            post(expiry_post).get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .with_state(state.clone());
    let url = spawn(app).await;
    let client = McpClient::connect("expiry", &remote(&url), ClientOptions::new("."))
        .await
        .expect("connect");
    assert_eq!(state.initialize.load(Ordering::SeqCst), 1);
    let result = client
        .call_tool("echo", json!({}))
        .await
        .expect("call after reconnect");
    assert_eq!(result["content"][0]["text"], "after-reconnect");
    assert_eq!(
        state.initialize.load(Ordering::SeqCst),
        2,
        "reconnect exactly once"
    );
    assert_eq!(state.calls.load(Ordering::SeqCst), 2, "call retried once");
    client.shutdown().await;
}

#[tokio::test]
async fn http_404_is_session_expiry() {
    expiry_case(404).await;
}

#[tokio::test]
async fn http_400_server_not_initialized_is_session_expiry() {
    expiry_case(400).await;
}

struct RetryState {
    attempts: AtomicU32,
}

async fn retry_post(
    State(state): State<Arc<RetryState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let attempt = state.attempts.fetch_add(1, Ordering::SeqCst);
    assert!(
        headers.get("Mcp-Session-Id").is_none(),
        "unexpected session header"
    );
    if attempt == 0 {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "0")],
            "slow down",
        )
            .into_response();
    }
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    json_response(&id, &json!({}), None)
}

#[tokio::test]
async fn http_retry_after_and_transient_retry() {
    let state = Arc::new(RetryState {
        attempts: AtomicU32::new(0),
    });
    let app = Router::new()
        .route(
            "/mcp",
            post(retry_post).get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .with_state(state.clone());
    let url = spawn(app).await;
    let parsed = url::Url::parse(&url).expect("url");
    let transport = HttpTransport::new(HttpOptions {
        server: "retry".to_owned(),
        url: parsed,
        headers: std::collections::BTreeMap::default(),
        protocol_version: None,
    })
    .expect("transport");

    let started = Instant::now();
    transport
        .request("ping", None, Duration::from_secs(3))
        .await
        .expect("eventually succeeds");
    assert_eq!(state.attempts.load(Ordering::SeqCst), 2);
    assert!(
        started.elapsed() >= Duration::from_millis(240),
        "backoff was not applied: {:?}",
        started.elapsed()
    );
    transport.shutdown().await;
}
