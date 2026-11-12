//! A scripted multi-protocol SSE provider (`OpenAI` Chat, Anthropic Messages, Gemini).

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Response script for [`FakeProvider`].
#[derive(Debug, Clone)]
pub enum Script {
    /// Text streamed across two deltas followed by a stop finish and usage.
    Text(String),
    /// Emit a tool call on the first turn; once the transcript contains a tool
    /// result for the active wire protocol, respond with `text`.
    ToolThenText {
        tool: String,
        arguments: Value,
        text: String,
    },
    /// Stream partial content and close cleanly without any completion/finish
    /// marker, exercising premature-EOF handling in `oc-llm`.
    PrematureEof,
    /// Answer the first request with HTTP `status` and a `Retry-After` header,
    /// then replay `then` on all subsequent requests.
    RetryAfterOnce {
        status: u16,
        retry_after_secs: u64,
        then: Box<Script>,
    },
    /// Answer every request with a non-2xx status.
    Error(u16),
    /// Send SSE headers and then never write a chunk.
    Stall,
}

#[derive(Debug, Clone, Copy)]
enum WireProtocol {
    OpenAi,
    Anthropic,
    Gemini,
}

#[derive(Debug)]
struct FakeState {
    script: Script,
    requests: Arc<Mutex<Vec<Value>>>,
    hits: AtomicUsize,
}

/// An ephemeral loopback HTTP server implementing `OpenAI` `/v1/chat/completions`,
/// Anthropic `/v1/messages`, and Gemini `/v1beta/models/*:streamGenerateContent`.
#[derive(Debug)]
pub struct FakeProvider {
    base_url: String,
    gemini_base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl FakeProvider {
    /// Bind an ephemeral port and start serving `script`.
    ///
    /// # Errors
    ///
    /// Returns an error when the listener cannot be bound.
    pub async fn start(script: Script) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let state = Arc::new(FakeState {
            script,
            requests: Arc::new(Mutex::new(Vec::new())),
            hits: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/v1/chat/completions", post(openai_chat))
            .route("/chat/completions", post(openai_chat))
            .route("/v1/messages", post(anthropic_messages))
            .route("/messages", post(anthropic_messages))
            .route("/v1beta/models/{model_action}", post(gemini_generate))
            .route("/v1/models/{model_action}", post(gemini_generate))
            .route("/models/{model_action}", post(gemini_generate))
            .with_state(Arc::clone(&state));
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(Self {
            base_url: format!("http://{address}/v1"),
            gemini_base_url: format!("http://{address}/v1beta"),
            requests: Arc::clone(&state.requests),
            shutdown: Some(tx),
            task: Some(task),
        })
    }

    /// Base URL (`.../v1`) for `OpenAI` and Anthropic providers.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Base URL (`.../v1beta`) for Gemini `streamGenerateContent` providers.
    #[must_use]
    pub fn gemini_base_url(&self) -> &str {
        &self.gemini_base_url
    }

    /// Request bodies captured so far, in arrival order.
    #[must_use]
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("fake requests").clone()
    }

    /// Stop accepting connections and wait for the server task.
    pub async fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(mut task) = self.task.take() {
            let timed_out = tokio::time::timeout(std::time::Duration::from_secs(1), &mut task)
                .await
                .is_err();
            if timed_out {
                task.abort();
            }
        }
    }
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn openai_chat(State(state): State<Arc<FakeState>>, Json(body): Json<Value>) -> Response {
    dispatch(&state, WireProtocol::OpenAi, &body)
}

async fn anthropic_messages(
    State(state): State<Arc<FakeState>>,
    Json(body): Json<Value>,
) -> Response {
    dispatch(&state, WireProtocol::Anthropic, &body)
}

async fn gemini_generate(State(state): State<Arc<FakeState>>, Json(body): Json<Value>) -> Response {
    dispatch(&state, WireProtocol::Gemini, &body)
}

fn dispatch(state: &FakeState, protocol: WireProtocol, body: &Value) -> Response {
    state
        .requests
        .lock()
        .expect("fake requests")
        .push(body.clone());
    let hit = state.hits.fetch_add(1, Ordering::SeqCst);
    render_script(&state.script, protocol, body, hit)
}

fn render_script(script: &Script, protocol: WireProtocol, body: &Value, hit: usize) -> Response {
    match script {
        Script::Text(text) => sse_response(text_body_for(protocol, text)),
        Script::ToolThenText {
            tool,
            arguments,
            text,
        } => {
            if has_tool_result(protocol, body) {
                sse_response(text_body_for(protocol, text))
            } else {
                sse_response(tool_body_for(protocol, tool, arguments))
            }
        }
        Script::PrematureEof => sse_response(premature_eof_body_for(protocol)),
        Script::RetryAfterOnce {
            status,
            retry_after_secs,
            then,
        } => {
            if hit == 0 {
                let code = StatusCode::from_u16(*status).unwrap_or(StatusCode::TOO_MANY_REQUESTS);
                (
                    code,
                    [(header::RETRY_AFTER, retry_after_secs.to_string())],
                    Json(json!({"error": {"message": "scripted rate limit"}})),
                )
                    .into_response()
            } else {
                render_script(then, protocol, body, hit)
            }
        }
        Script::Error(status) => {
            let status = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_REQUEST);
            (
                status,
                Json(json!({"error": {"message": "scripted provider error"}})),
            )
                .into_response()
        }
        Script::Stall => {
            let body = Body::from_stream(stream::pending::<Result<&'static [u8], Infallible>>());
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

fn has_tool_result(protocol: WireProtocol, body: &Value) -> bool {
    match protocol {
        WireProtocol::OpenAi => {
            body.get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages
                        .iter()
                        .any(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
                })
        }
        WireProtocol::Anthropic => {
            body.get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message
                            .get("content")
                            .and_then(Value::as_array)
                            .is_some_and(|blocks| {
                                blocks.iter().any(|block| {
                                    block.get("type").and_then(Value::as_str) == Some("tool_result")
                                })
                            })
                    })
                })
        }
        WireProtocol::Gemini => {
            body.get("contents")
                .and_then(Value::as_array)
                .is_some_and(|contents| {
                    contents.iter().any(|item| {
                        item.get("parts")
                            .and_then(Value::as_array)
                            .is_some_and(|parts| {
                                parts
                                    .iter()
                                    .any(|part| part.get("functionResponse").is_some())
                            })
                    })
                })
        }
    }
}

fn text_body_for(protocol: WireProtocol, text: &str) -> String {
    match protocol {
        WireProtocol::OpenAi => text_body(text),
        WireProtocol::Anthropic => anthropic_text_body(text),
        WireProtocol::Gemini => gemini_text_body(text),
    }
}

fn tool_body_for(protocol: WireProtocol, tool: &str, arguments: &Value) -> String {
    match protocol {
        WireProtocol::OpenAi => tool_body(tool, arguments),
        WireProtocol::Anthropic => anthropic_tool_body(tool, arguments),
        WireProtocol::Gemini => gemini_tool_body(tool, arguments),
    }
}

fn premature_eof_body_for(protocol: WireProtocol) -> String {
    match protocol {
        WireProtocol::OpenAi => frame(&envelope(&json!({"content": "partial"}), None)),
        WireProtocol::Anthropic => [
            named_frame(
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_fake_1",
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": "model",
                        "usage": {"input_tokens": 10, "output_tokens": 0},
                    }
                }),
            ),
            named_frame(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                }),
            ),
            named_frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "partial"},
                }),
            ),
        ]
        .concat(),
        WireProtocol::Gemini => frame(&json!({
            "candidates": [{"content": {"parts": [{"text": "partial"}]}}],
        })),
    }
}

fn sse_response(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

fn usage() -> Value {
    json!({
        "prompt_tokens": 10,
        "completion_tokens": 5,
        "total_tokens": 15,
    })
}

fn envelope(delta: &Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl-fake",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000_u64,
        "model": "model",
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
    })
}

fn frame(value: &Value) -> String {
    format!("data: {value}\n\n")
}

fn named_frame(event: &str, value: &Value) -> String {
    format!("event: {event}\ndata: {value}\n\n")
}

fn split_half(text: &str) -> (&str, &str) {
    let split = text
        .char_indices()
        .nth(text.chars().count() / 2)
        .map_or(text.len(), |(index, _)| index);
    text.split_at(split)
}

fn text_body(text: &str) -> String {
    let mut chunks = Vec::new();
    chunks.push(frame(&envelope(&json!({"role": "assistant"}), None)));
    let (first, second) = split_half(text);
    for part in [first, second] {
        if !part.is_empty() {
            chunks.push(frame(&envelope(&json!({"content": part}), None)));
        }
    }
    let mut final_chunk = envelope(&Value::Object(serde_json::Map::new()), Some("stop"));
    final_chunk["usage"] = usage();
    chunks.push(frame(&final_chunk));
    chunks.push("data: [DONE]\n\n".to_owned());
    chunks.concat()
}

fn tool_body(tool: &str, arguments: &Value) -> String {
    let call = json!({
        "index": 0,
        "id": "call_fake_1",
        "type": "function",
        "function": {
            "name": tool,
            "arguments": arguments.to_string(),
        },
    });
    let mut chunks = Vec::new();
    chunks.push(frame(&envelope(&json!({"role": "assistant"}), None)));
    chunks.push(frame(&envelope(&json!({"tool_calls": [call]}), None)));
    let mut final_chunk = envelope(&Value::Object(serde_json::Map::new()), Some("tool_calls"));
    final_chunk["usage"] = usage();
    chunks.push(frame(&final_chunk));
    chunks.push("data: [DONE]\n\n".to_owned());
    chunks.concat()
}

fn anthropic_message_start() -> String {
    named_frame(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": "msg_fake_1",
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": "model",
                "usage": {"input_tokens": 10, "output_tokens": 0},
            }
        }),
    )
}

fn anthropic_text_body(text: &str) -> String {
    let mut chunks = vec![
        anthropic_message_start(),
        named_frame(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            }),
        ),
    ];
    let (first, second) = split_half(text);
    for part in [first, second] {
        if !part.is_empty() {
            chunks.push(named_frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": part},
                }),
            ));
        }
    }
    chunks.push(named_frame(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": 0}),
    ));
    chunks.push(named_frame(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 5},
        }),
    ));
    chunks.push(named_frame(
        "message_stop",
        &json!({"type": "message_stop"}),
    ));
    chunks.concat()
}

fn anthropic_tool_body(tool: &str, arguments: &Value) -> String {
    [
        anthropic_message_start(),
        named_frame(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "call_fake_1",
                    "name": tool,
                    "input": {},
                },
            }),
        ),
        named_frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": arguments.to_string(),
                },
            }),
        ),
        named_frame(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 0}),
        ),
        named_frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 5},
            }),
        ),
        named_frame("message_stop", &json!({"type": "message_stop"})),
    ]
    .concat()
}

fn gemini_text_body(text: &str) -> String {
    let mut chunks = Vec::new();
    let (first, second) = split_half(text);
    for part in [first, second] {
        if !part.is_empty() {
            chunks.push(frame(&json!({
                "candidates": [{"content": {"parts": [{"text": part}]}}],
            })));
        }
    }
    chunks.push(frame(&json!({
        "candidates": [{"content": {"parts": []}, "finishReason": "STOP"}],
        "usageMetadata": {
            "promptTokenCount": 10,
            "candidatesTokenCount": 5,
            "totalTokenCount": 15,
        },
    })));
    chunks.concat()
}

fn gemini_tool_body(tool: &str, arguments: &Value) -> String {
    frame(&json!({
        "candidates": [{
            "content": {
                "parts": [{
                    "functionCall": {
                        "id": "call_fake_1",
                        "name": tool,
                        "args": arguments,
                    }
                }]
            },
            "finishReason": "STOP",
        }],
        "usageMetadata": {
            "promptTokenCount": 10,
            "candidatesTokenCount": 5,
            "totalTokenCount": 15,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn anthropic_and_gemini_tool_then_text_scripts() {
        let fake = FakeProvider::start(Script::ToolThenText {
            tool: "read".to_owned(),
            arguments: json!({"path": "a.txt"}),
            text: "all done".to_owned(),
        })
        .await
        .unwrap();
        let client = reqwest::Client::new();

        // Anthropic turn 1 -> tool_use
        let res1 = client
            .post(format!("{}/messages", fake.base_url()))
            .json(&json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "go"}]}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(res1.status(), StatusCode::OK);
        let body1 = res1.text().await.unwrap();
        assert!(body1.contains("\"type\":\"tool_use\""), "{body1}");
        assert!(body1.contains("\"stop_reason\":\"tool_use\""), "{body1}");

        // Anthropic turn 2 (with tool_result) -> text
        let res2 = client
            .post(format!("{}/messages", fake.base_url()))
            .json(&json!({
                "messages": [{"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_fake_1", "content": "ok"}]}]
            }))
            .send()
            .await
            .unwrap();
        let body2 = res2.text().await.unwrap();
        assert!(body2.contains("\"text_delta\""), "{body2}");
        assert!(body2.contains("\"message_stop\""), "{body2}");

        // Gemini turn 1 -> functionCall
        let gem1 = client
            .post(format!(
                "{}/models/model:streamGenerateContent?alt=sse",
                fake.gemini_base_url()
            ))
            .json(&json!({"contents": [{"role": "user", "parts": [{"text": "go"}]}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(gem1.status(), StatusCode::OK);
        let gbody1 = gem1.text().await.unwrap();
        assert!(gbody1.contains("\"functionCall\""), "{gbody1}");
        assert!(gbody1.contains("\"finishReason\":\"STOP\""), "{gbody1}");

        // Gemini turn 2 (with functionResponse) -> text
        let gem2 = client
            .post(format!("{}/models/model:streamGenerateContent?alt=sse", fake.gemini_base_url()))
            .json(&json!({"contents": [{"role": "user", "parts": [{"functionResponse": {"name": "read", "response": {"content": "ok"}}}]}]}))
            .send()
            .await
            .unwrap();
        let gbody2 = gem2.text().await.unwrap();
        assert!(gbody2.contains("\"text\":\"all \""), "{gbody2}");
        assert!(gbody2.contains("\"text\":\"done\""), "{gbody2}");
    }

    #[tokio::test]
    async fn retry_after_once_and_premature_eof_scripts() {
        let client = reqwest::Client::new();

        let retry = FakeProvider::start(Script::RetryAfterOnce {
            status: 429,
            retry_after_secs: 2,
            then: Box::new(Script::Text("recovered".to_owned())),
        })
        .await
        .unwrap();
        let first = client
            .post(format!("{}/chat/completions", retry.base_url()))
            .json(&json!({"messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            first
                .headers()
                .get(header::RETRY_AFTER)
                .unwrap()
                .to_str()
                .unwrap(),
            "2"
        );
        let second = client
            .post(format!("{}/chat/completions", retry.base_url()))
            .json(&json!({"messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        let sbody = second.text().await.unwrap();
        assert!(sbody.contains("data: [DONE]"), "{sbody}");
        assert_eq!(retry.requests().len(), 2);

        let eof = FakeProvider::start(Script::PrematureEof).await.unwrap();
        let oa = client
            .post(format!("{}/chat/completions", eof.base_url()))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(oa.contains("partial") && !oa.contains("[DONE]"), "{oa}");

        let an = client
            .post(format!("{}/messages", eof.base_url()))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            an.contains("partial") && !an.contains("message_stop"),
            "{an}"
        );

        let ge = client
            .post(format!(
                "{}/models/model:streamGenerateContent?alt=sse",
                eof.gemini_base_url()
            ))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            ge.contains("partial") && !ge.contains("finishReason"),
            "{ge}"
        );
    }
}
