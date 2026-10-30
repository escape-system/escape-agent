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
