//! Streaming tests against a local fake server. No real network or keys.

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures_util::{StreamExt, stream};
use oc_llm::{
    Catalog, ContentPart, LlmClient, LlmEvent, LlmRequest, LlmStream, RetryPolicy, Timeouts,
    ToolDef,
};
use serde_json::{Value, json};

const FIXTURE: &str = r#"{
  "test": {
    "id": "test",
    "name": "Test",
    "env": [],
    "models": {
      "alpha": {
        "id": "alpha", "type": "chat", "name": "Alpha", "release_date": "2025-01-02",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}, "temperature": true},
        "limit": {"context": 100000, "output": 10000},
        "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
        "api": {"chat-completions": {"base_url": "http://unused.invalid", "sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
      },
      "claude-sonnet-4-5": {
        "id": "claude-sonnet-4-5", "type": "chat", "name": "Claude", "release_date": "2025-09-29",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}, "temperature": true},
        "limit": {"context": 200000, "output": 64000},
        "cost": {"text": {"input": {"price": 3}, "output": {"price": 15}}},
        "api": {"messages": {"base_url": "http://unused.invalid", "sdk": {"aisdk": "@ai-sdk/anthropic"}}}
      },
      "hungerg-free": {
        "id": "hungerg-free", "type": "chat", "name": "HungerG Free", "release_date": "2026-11-24",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": true, "effort": ["max"]}, "temperature": true},
        "limit": {"context": 1000000, "output": 65536},
        "cost": {"text": {"input": {"price": 0}, "output": {"price": 0}}},
        "api": {"chat-completions": {"base_url": "http://unused.invalid", "sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
      },
      "gemini-pro": {
        "id": "gemini-pro", "type": "chat", "name": "Gemini", "release_date": "2025-02-01",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}, "temperature": true},
        "limit": {"context": 1000000, "output": 65536},
        "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
        "api": {"generate-content": {"base_url": "http://unused.invalid", "sdk": {"aisdk": "@ai-sdk/google"}}}
      }
    }
  }
}"#;

#[derive(Default)]
struct Fake {
    hits: AtomicUsize,
    bodies: Mutex<Vec<Value>>,
}

fn sse(chunks: Vec<String>) -> Response {
    let stream = stream::iter(
        chunks
            .into_iter()
            .map(|chunk| Ok::<Bytes, Infallible>(Bytes::from(chunk))),
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .expect("response")
}

async fn openai(State(fake): State<Arc<Fake>>, body: Bytes) -> Response {
    if let Ok(value) = serde_json::from_slice(&body) {
        fake.bodies.lock().unwrap().push(value);
    }
    sse(vec![
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"world\"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_string(),
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15,\"prompt_tokens_details\":{\"cached_tokens\":2}}}\n\n".to_string(),
        "data: [DONE]\n\n".to_string(),
    ])
}

async fn anthropic(State(fake): State<Arc<Fake>>, body: Bytes) -> Response {
    if let Ok(value) = serde_json::from_slice(&body) {
        fake.bodies.lock().unwrap().push(value);
    }
    sse(vec![
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":4,\"cache_read_input_tokens\":2,\"output_tokens\":1}}}\n\n".to_string(),
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_string(),
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n".to_string(),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".to_string(),
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\",\"input\":{}}}\n\n".to_string(),
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"b.txt\\\"}\"}}\n\n".to_string(),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n".to_string(),
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":7}}\n\n".to_string(),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
    ])
}

async fn gemini(State(fake): State<Arc<Fake>>, body: Bytes) -> Response {
    if let Ok(value) = serde_json::from_slice(&body) {
        fake.bodies.lock().unwrap().push(value);
    }
    sse(vec![
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hey\"}]}}]}\n\n".to_string(),
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"c.txt\"}}}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":1,\"totalTokenCount\":6}}\n\n".to_string(),
    ])
}

async fn retry(State(fake): State<Arc<Fake>>) -> Response {
    let hit = fake.hits.fetch_add(1, Ordering::SeqCst);
    if hit == 0 {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "0.05")],
            "slow down",
        )
            .into_response();
    }
    sse(vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string(),
        "data: [DONE]\n\n".to_string(),
    ])
}

async fn slow() -> Response {
    tokio::time::sleep(Duration::from_millis(200)).await;
    sse(vec!["data: [DONE]\n\n".to_string()])
}

/// A canned failure response. `retry_after` is optional so the "no hint" case
/// is byte-identical to a provider that omits `Retry-After`.
fn canned(status: StatusCode, retry_after: Option<&'static str>) -> Response {
    let mut response = Response::builder().status(status);
    if let Some(value) = retry_after {
        response = response.header(header::RETRY_AFTER, value);
    }
    response
        .body(Body::from("canned failure"))
        .expect("response")
}

async fn retry_hint_28() -> Response {
    canned(StatusCode::TOO_MANY_REQUESTS, Some("28"))
}

async fn retry_hint_none() -> Response {
    canned(StatusCode::SERVICE_UNAVAILABLE, None)
}

async fn retry_hint_banana() -> Response {
    canned(StatusCode::TOO_MANY_REQUESTS, Some("banana"))
}

async fn retry_hint_absurd() -> Response {
    canned(StatusCode::SERVICE_UNAVAILABLE, Some("100000"))
}

async fn retry_hint_date() -> Response {
    canned(
        StatusCode::SERVICE_UNAVAILABLE,
        Some("Thu, 01 Jan 2099 00:00:00 GMT"),
    )
}

async fn retry_hint_400() -> Response {
    canned(StatusCode::BAD_REQUEST, Some("28"))
}

async fn stall() -> Response {
    let first = stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"start\"}}]}\n\n",
    ))]);
    let rest = stream::once(async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok::<Bytes, Infallible>(Bytes::from_static(b""))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(first.chain(rest)))
        .expect("response")
}

async fn malformed() -> Response {
    sse(vec!["data: this is not json\n\n".to_string()])
}

async fn spawn() -> (String, tokio::task::JoinHandle<()>, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/chat/completions", post(openai))
        .route("/messages", post(anthropic))
        .route("/models/{id}", post(gemini))
        .route("/retry/chat/completions", post(retry))
        .route("/slow/chat/completions", post(slow))
        .route("/stall/chat/completions", post(stall))
        .route("/bad/chat/completions", post(malformed))
        .route("/hint28/chat/completions", post(retry_hint_28))
        .route("/noheader/chat/completions", post(retry_hint_none))
        .route("/banana/chat/completions", post(retry_hint_banana))
        .route("/absurd/chat/completions", post(retry_hint_absurd))
        .route("/date/chat/completions", post(retry_hint_date))
        .route("/badreq/chat/completions", post(retry_hint_400))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{address}"), handle, fake)
}

fn request(reference: &str, base: String) -> LlmRequest {
    let catalog = Catalog::from_snapshot(FIXTURE).expect("catalog");
    let mut model = catalog.resolve(reference).expect("model");
    model.base_url = base;
    let mut request = LlmRequest::new(model);
    request.system = vec!["You are a test.".to_string()];
    request.messages = vec![
        oc_llm::LlmMessage::user("hello"),
        oc_llm::LlmMessage::new(
            oc_llm::Role::Assistant,
            vec![ContentPart::tool_call("prev", "read", json!({"path": "x"}))],
        ),
        oc_llm::LlmMessage::tool_result("prev", Some("read".to_string()), "ok", false),
    ];
    request.tools = vec![ToolDef::new(
        "read",
        "Read a file",
        json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    )];
    request.options.api_key = Some("test-key".to_string());
    request
}

async fn collect(mut stream: LlmStream) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

fn find_tool_call(events: &[LlmEvent]) -> (&str, &str, &Value) {
    events
        .iter()
        .find_map(|event| match event {
            LlmEvent::ToolCall { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
        .expect("tool call")
}

#[tokio::test(flavor = "multi_thread")]
async fn openai_stream_text_tool_and_usage() {
    let (base, server, fake) = spawn().await;
    let client = LlmClient::new().unwrap();
    let events = collect(client.stream(request("test/alpha", base)).await.unwrap()).await;
    server.abort();

    assert!(events.contains(&LlmEvent::TextStart));
    assert!(events.contains(&LlmEvent::TextDelta {
        delta: "Hello ".to_string()
    }));
    assert!(events.contains(&LlmEvent::TextEnd {
        text: "Hello world".to_string()
    }));
    let (id, name, input) = find_tool_call(&events);
    assert_eq!((id, name), ("call_1", "read"));
    assert_eq!(input, &json!({"path": "a.txt"}));
    let finish = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Finish { reason, usage } => Some((reason, usage)),
            _ => None,
        })
        .expect("finish");
    assert_eq!(finish.0, "tool-calls");
    assert_eq!(finish.1.input_tokens, Some(10));
    assert_eq!(finish.1.output_tokens, Some(5));
    assert_eq!(finish.1.total_tokens, Some(15));
    assert_eq!(finish.1.cache_read_input_tokens, Some(2));

    let bodies = fake.bodies.lock().unwrap();
    let body = bodies.last().expect("request body");
    assert_eq!(body["model"], "alpha");
    assert_eq!(body["stream"], true);
    assert_eq!(body["tools"][0]["function"]["name"], "read");
    assert_eq!(body["messages"][0]["role"], "system");
}

#[tokio::test(flavor = "multi_thread")]
async fn openai_hungerg_max_effort_reaches_the_wire() {
    let (base, server, fake) = spawn().await;
    let client = LlmClient::new().unwrap();
    let events = collect(
        client
            .stream(request("test/hungerg-free#max", base))
            .await
            .unwrap(),
    )
    .await;
    server.abort();

    assert!(events.iter().any(|event| matches!(
        event,
        LlmEvent::Finish { reason, .. } if reason == "tool-calls"
    )));
    let bodies = fake.bodies.lock().unwrap();
    let body = bodies.last().expect("request body");
    assert_eq!(body["model"], "hungerg-free");
    assert_eq!(body["reasoning_effort"], "max");
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_content_blocks() {
    let (base, server, fake) = spawn().await;
    let client = LlmClient::new().unwrap();
    let events = collect(
        client
            .stream(request("test/claude-sonnet-4-5", base))
            .await
            .unwrap(),
    )
    .await;
    server.abort();

    assert!(events.contains(&LlmEvent::TextStart));
    assert!(events.contains(&LlmEvent::TextDelta {
        delta: "Hi".to_string()
    }));
    assert!(events.contains(&LlmEvent::TextEnd {
        text: "Hi".to_string()
    }));
    let (id, name, input) = find_tool_call(&events);
    assert_eq!((id, name), ("toolu_1", "read"));
    assert_eq!(input, &json!({"path": "b.txt"}));
    let finish = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Finish { reason, usage } => Some((reason, usage)),
            _ => None,
        })
        .expect("finish");
    assert_eq!(finish.0, "tool-calls");
    assert_eq!(finish.1.input_tokens, Some(6));
    assert_eq!(finish.1.output_tokens, Some(7));

    let bodies = fake.bodies.lock().unwrap();
    let body = bodies.last().expect("request body");
    assert_eq!(body["model"], "claude-sonnet-4-5");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(
        body["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(
        body["messages"][2]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gemini_function_call() {
    let (base, server, fake) = spawn().await;
    let client = LlmClient::new().unwrap();
    let events = collect(
        client
            .stream(request("test/gemini-pro", base))
            .await
            .unwrap(),
    )
    .await;
    server.abort();

    assert!(events.contains(&LlmEvent::TextDelta {
        delta: "Hey".to_string()
    }));
    let (id, name, input) = find_tool_call(&events);
    assert_eq!(id, "tool_1");
    assert_eq!(name, "read");
    assert_eq!(input, &json!({"path": "c.txt"}));
    let finish = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Finish { reason, usage } => Some((reason, usage)),
            _ => None,
        })
        .expect("finish");
    assert_eq!(finish.0, "tool-calls");
    assert_eq!(finish.1.input_tokens, Some(3));
    assert_eq!(finish.1.output_tokens, Some(3));
    assert_eq!(finish.1.reasoning_tokens, Some(1));

    let bodies = fake.bodies.lock().unwrap();
    let body = bodies.last().expect("request body");
    assert_eq!(body["contents"][0]["role"], "user");
    assert!(body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"].is_object());
    assert!(body["systemInstruction"]["parts"].is_array());
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_json_is_a_provider_error() {
    let (base, server, _) = spawn().await;
    let client = LlmClient::new().unwrap();
    let mut request = request("test/alpha", format!("{base}/bad"));
    request.options.api_key = None;
    let events = collect(client.stream(request).await.unwrap()).await;
    server.abort();
    assert!(events.iter().any(|event| matches!(
        event,
        LlmEvent::ProviderError { message } if message.contains("invalid OpenAI stream event")
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_after_is_honored() {
    let (base, server, fake) = spawn().await;
    let client = LlmClient::new()
        .unwrap()
        .with_retry_policy(RetryPolicy::fixed(
            Duration::from_millis(5),
            Duration::from_millis(10),
            10,
        ));
    let started = Instant::now();
    let events = collect(
        client
            .stream(request("test/alpha", format!("{base}/retry")))
            .await
            .unwrap(),
    )
    .await;
    let elapsed = started.elapsed();
    server.abort();
    assert_eq!(fake.hits.load(Ordering::SeqCst), 2);
    assert!(elapsed >= Duration::from_millis(45), "{elapsed:?}");
    assert!(events.iter().any(|event| matches!(
        event,
        LlmEvent::Finish { reason, .. } if reason == "stop"
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn header_timeout_is_typed() {
    let (base, server, _) = spawn().await;
    let client = LlmClient::new()
        .unwrap()
        .with_retry_policy(RetryPolicy::fixed(
            Duration::from_millis(1),
            Duration::from_millis(2),
            2,
        ))
        .with_timeouts(Timeouts {
            header: Duration::from_millis(50),
            chunk: Duration::from_secs(1),
            total: None,
        });
    let Err(error) = client
        .stream(request("test/alpha", format!("{base}/slow")))
        .await
    else {
        panic!("expected a timeout");
    };
    server.abort();
    assert!(error.to_string().contains("timed out"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn chunk_timeout_closes_stream_with_provider_error() {
    let (base, server, _) = spawn().await;
    let client = LlmClient::new().unwrap().with_timeouts(Timeouts {
        header: Duration::from_secs(1),
        chunk: Duration::from_millis(100),
        total: None,
    });
    let started = Instant::now();
    let events = collect(
        client
            .stream(request("test/alpha", format!("{base}/stall")))
            .await
            .unwrap(),
    )
    .await;
    server.abort();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(events.iter().any(|event| matches!(
        event,
        LlmEvent::ProviderError { message } if message.contains("chunk timed out")
    )));
    assert!(events.contains(&LlmEvent::TextDelta {
        delta: "start".to_string()
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn total_timeout_closes_stream() {
    let (base, server, _) = spawn().await;
    let client = LlmClient::new().unwrap().with_timeouts(Timeouts {
        header: Duration::from_secs(1),
        chunk: Duration::from_secs(2),
        total: Some(Duration::from_millis(150)),
    });
    let events = collect(
        client
            .stream(request("test/alpha", format!("{base}/stall")))
            .await
            .unwrap(),
    )
    .await;
    server.abort();
    assert!(events.iter().any(|event| matches!(
        event,
        LlmEvent::ProviderError { message } if message.contains("total timeout")
    )));
}

/// A one-attempt policy: the client surfaces the error before any wait, so the
/// test asserts on the computed delay instead of sleeping 28 s.
fn no_retry() -> RetryPolicy {
    RetryPolicy::fixed(Duration::from_millis(1), Duration::from_millis(2), 1)
}

/// `LlmStream` is not `Debug`, so `expect_err` cannot be used on it.
async fn stream_error(client: &LlmClient, request: LlmRequest) -> oc_llm::LlmError {
    match client.stream(request).await {
        Ok(_) => panic!("expected the request to fail"),
        Err(error) => error,
    }
}

/// Test A: `Retry-After: 28` reaches the retry decision and yields >= 28 s.
#[tokio::test(flavor = "multi_thread")]
async fn retry_after_header_reaches_the_retry_decision() {
    let (base, server, _) = spawn().await;
    let client = LlmClient::new().unwrap().with_retry_policy(no_retry());
    let error = stream_error(&client, request("test/alpha", format!("{base}/hint28"))).await;
    server.abort();
    assert!(error.is_retryable(), "{error}");
    assert_eq!(error.retry_after_ms(), Some(28_000), "{error}");
    let delay = client
        .retry_policy()
        .delay(1, error.retry_after_ms().map(Duration::from_millis));
    assert!(delay >= Duration::from_secs(28), "{delay:?}");
}

/// Test B: a 503 without `Retry-After` carries no hint and falls back to
/// exponential backoff.
#[tokio::test(flavor = "multi_thread")]
async fn missing_retry_after_header_yields_no_hint() {
    let (base, server, _) = spawn().await;
    let client = LlmClient::new().unwrap().with_retry_policy(no_retry());
    let error = stream_error(&client, request("test/alpha", format!("{base}/noheader"))).await;
    server.abort();
    assert_eq!(error.retry_after_ms(), None, "{error}");
    assert!(error.is_retryable(), "{error}");
    assert_eq!(
        client.retry_policy().delay(1, None),
        Duration::from_millis(1)
    );
}

/// Test C: `Retry-After: banana` is ignored, not zero, and does not panic.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_retry_after_header_is_ignored_over_http() {
    let (base, server, _) = spawn().await;
    let client = LlmClient::new().unwrap().with_retry_policy(no_retry());
    let error = stream_error(&client, request("test/alpha", format!("{base}/banana"))).await;
    server.abort();
    assert_eq!(error.retry_after_ms(), None, "{error}");
    assert!(error.is_retryable(), "{error}");
}

/// Test D: `Retry-After: 100000` is parsed and then clamped to the 60 s cap.
#[tokio::test(flavor = "multi_thread")]
async fn absurd_retry_after_header_is_clamped_over_http() {
    let (base, server, _) = spawn().await;
    let client = LlmClient::new().unwrap().with_retry_policy(no_retry());
    let error = stream_error(&client, request("test/alpha", format!("{base}/absurd"))).await;
    server.abort();
    assert_eq!(error.retry_after_ms(), Some(100_000_000), "{error}");
    let delay = client
        .retry_policy()
        .delay(1, error.retry_after_ms().map(Duration::from_millis));
    assert_eq!(delay, Duration::from_secs(60), "{delay:?}");
}

/// Test E: the HTTP-date form parses end to end and stays bounded by the cap.
#[tokio::test(flavor = "multi_thread")]
async fn http_date_retry_after_parses_over_http() {
    let (base, server, _) = spawn().await;
    let client = LlmClient::new().unwrap().with_retry_policy(no_retry());
    let error = stream_error(&client, request("test/alpha", format!("{base}/date"))).await;
    server.abort();
    let hint = error
        .retry_after_ms()
        .unwrap_or_else(|| panic!("HTTP-date hint missing: {error}"));
    assert!(hint > 60_000, "{hint} ms");
    let delay = client
        .retry_policy()
        .delay(1, Some(Duration::from_millis(hint)));
    assert_eq!(delay, Duration::from_secs(60), "{delay:?}");
}

/// Retryability classification is unchanged: a 400 is returned at once even
/// when the server attaches a hint.
#[tokio::test(flavor = "multi_thread")]
async fn non_retryable_failure_is_returned_immediately() {
    let (base, server, _) = spawn().await;
    let client = LlmClient::new()
        .unwrap()
        .with_retry_policy(RetryPolicy::fixed(
            Duration::from_millis(1),
            Duration::from_millis(2),
            10,
        ));
    let started = Instant::now();
    let error = stream_error(&client, request("test/alpha", format!("{base}/badreq"))).await;
    let elapsed = started.elapsed();
    server.abort();
    assert!(!error.is_retryable(), "{error}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn gemini_prompt_feedback_block_emits_policy_blocked_provider_error() {
    let app = Router::new().route(
        "/models/{*rest}",
        post(|| async {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from(
                    "data: {\"promptFeedback\":{\"blockReason\":\"PROHIBITED_CONTENT\",\"blockReasonMessage\":\"blocked\"}}\n\n",
                ))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = LlmClient::new().unwrap().with_retry_policy(no_retry());
    let mut stream = client
        .stream(request("test/gemini-pro", format!("http://{addr}")))
        .await
        .expect("stream opens");
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev);
    }
    server.abort();
    assert!(events.iter().any(|ev| matches!(
        ev,
        LlmEvent::ProviderError { message }
            if message.contains("policy_blocked")
                && oc_llm::classify_provider_message(message) == oc_llm::FailureKind::PolicyBlocked
    )));
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, LlmEvent::Finish { .. }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gemini_retry_info_and_terminal_quota_failure_work_over_http() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_retry = Arc::clone(&attempts);
    let app = Router::new()
        .route(
            "/terminal/models/{*rest}",
            post(move || {
                let counter = Arc::clone(&attempts_retry);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        json!({
                            "error": {
                                "code": 429,
                                "details": [{
                                    "@type": "type.googleapis.com/google.rpc.QuotaFailure",
                                    "violations": [{ "quotaValue": "0", "description": "enforced_spend_limit_reached" }]
                                }]
                            }
                        })
                        .to_string(),
                    )
                }
            }),
        )
        .route(
            "/hint/models/{*rest}",
            post(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({
                        "error": {
                            "code": 429,
                            "details": [{
                                "@type": "type.googleapis.com/google.rpc.RetryInfo",
                                "retryDelay": "49s"
                            }]
                        }
                    })
                    .to_string(),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = LlmClient::new()
        .unwrap()
        .with_retry_policy(RetryPolicy::fixed(
            Duration::from_millis(1),
            Duration::from_millis(2),
            5,
        ));
    let err = stream_error(
        &client,
        request("test/gemini-pro", format!("http://{addr}/terminal")),
    )
    .await;
    assert!(
        !err.is_retryable(),
        "terminal QuotaFailure must not retry: {err}"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "must abort immediately after 1 attempt"
    );

    let no_retry_client = LlmClient::new().unwrap().with_retry_policy(no_retry());
    let hint_err = stream_error(
        &no_retry_client,
        request("test/gemini-pro", format!("http://{addr}/hint")),
    )
    .await;
    assert_eq!(hint_err.retry_after_ms(), Some(49_000));
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn gemini_interactions_uses_store_false_and_applies_variant_timeouts() {
    let seen = Arc::new(Mutex::new(Vec::<(Value, String)>::new()));
    let seen_cb = Arc::clone(&seen);
    let app = Router::new().route(
        "/interactions",
        post({
            let seen_cb = Arc::clone(&seen_cb);
            move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                let key = headers
                    .get("x-goog-api-key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                seen_cb.lock().unwrap().push((body, key));
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(
                        "data: {\"event_type\":\"content.delta\",\"delta\":{\"type\":\"text\",\"text\":\"managed ok\"}}\n\n\
                         data: {\"event_type\":\"interaction.completed\",\"finish_reason\":\"completed\"}\n\n",
                    ))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut catalog = Catalog::embedded().unwrap().clone();
    oc_llm::catalog::refresh::apply_builtin_providers(&mut catalog);
    let client = LlmClient::new().unwrap().with_retry_policy(no_retry());

    let mut inter_model = catalog
        .resolve("google/antigravity-preview-09-2026")
        .unwrap();
    inter_model.base_url = format!("http://{addr}");
    let mut inter_req = LlmRequest::new(inter_model);
    inter_req.messages = vec![oc_llm::LlmMessage::user("hi")];
    inter_req.options.api_key = Some("AIza-test".to_owned());
    let mut inter_stream = client.stream(inter_req).await.unwrap();
    let mut collected = String::new();
    while let Some(ev) = inter_stream.next().await {
        if let LlmEvent::TextDelta { delta } = ev {
            collected.push_str(&delta);
        }
    }
    assert_eq!(collected, "managed ok");
    assert_eq!(
        Timeouts::for_variant(Some("max"), None).header,
        Duration::from_secs(600)
    );
    assert_eq!(
        Timeouts::for_variant(Some("high"), None).header,
        Duration::from_secs(420)
    );
    assert_eq!(
        Timeouts::for_variant(Some("low"), None).header,
        Duration::from_secs(120)
    );

    server.abort();
    let guard = seen.lock().unwrap();
    assert_eq!(guard[0].0["store"], json!(false));
    assert_eq!(guard[0].1, "AIza-test");
}

#[tokio::test(flavor = "multi_thread")]
async fn vertex_anthropic_stream_raw_predict_and_modality_guard() {
    let seen = Arc::new(Mutex::new(Vec::<(Value, String)>::new()));
    let seen_cb = Arc::clone(&seen);
    let app = Router::new().route(
        "/publishers/anthropic/models/{path}",
        post({
            let seen_cb = Arc::clone(&seen_cb);
            move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                seen_cb.lock().unwrap().push((body, auth));
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(
                        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n\
                         event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"vertex claude ok\"}}\n\n\
                         event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
                    ))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut catalog = Catalog::embedded().unwrap().clone();
    oc_llm::catalog::refresh::apply_builtin_providers(&mut catalog);
    let client = LlmClient::new().unwrap().with_retry_policy(no_retry());

    let mut vertex_model = catalog
        .resolve("google-vertex/claude-opus-5-5@default")
        .unwrap();
    vertex_model.base_url = format!("http://{addr}/publishers/anthropic/models");
    let mut vertex_req = LlmRequest::new(vertex_model);
    vertex_req.messages = vec![oc_llm::LlmMessage::user("hi")];
    vertex_req.options.api_key = Some("ya29.test_adc_token".to_owned());
    let mut v_stream = client.stream(vertex_req.clone()).await.unwrap();
    let mut v_collected = String::new();
    while let Some(ev) = v_stream.next().await {
        if let LlmEvent::TextDelta { delta } = ev {
            v_collected.push_str(&delta);
        }
    }
    assert_eq!(v_collected, "vertex claude ok");

    vertex_req.messages = vec![oc_llm::LlmMessage::new(
        oc_llm::Role::User,
        vec![ContentPart::document(
            "audio/mp3",
            "AAA",
            Some("clip.mp3".to_owned()),
        )],
    )];
    let modality_err = stream_error(&client, vertex_req).await;
    assert!(
        modality_err.to_string().contains("input modalities"),
        "{modality_err}"
    );

    server.abort();
    let guard = seen.lock().unwrap();
    assert_eq!(guard[0].0["anthropic_version"], json!("vertex-2023-10-16"));
    assert!(
        guard[0].0.get("model").is_none(),
        "Vertex Anthropic omits redundant model body key"
    );
    assert_eq!(guard[0].1, "Bearer ya29.test_adc_token");
}
