//! A scripted OpenAI-compatible streaming provider.

use std::convert::Infallible;
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
    /// A role delta, the text split over two content chunks, a `stop` finish
    /// chunk with usage, and `[DONE]`.
    Text(String),
    /// The first request emits a tool call and `finish_reason: "tool_calls"`.
    /// Once the transcript contains a `role: "tool"` message, the provider
    /// answers with `text` instead.
    ToolThenText {
        tool: String,
        arguments: Value,
        text: String,
    },
    /// Answer every request with a non-2xx status.
    Error(u16),
    /// Send SSE headers and then never write a chunk.
    Stall,
}

#[derive(Debug)]
struct FakeState {
    script: Script,
    requests: Arc<Mutex<Vec<Value>>>,
}

/// An ephemeral loopback HTTP server implementing `POST /v1/chat/completions`.
#[derive(Debug)]
pub struct FakeProvider {
    base_url: String,
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
        });
        let app = Router::new()
            .route("/v1/chat/completions", post(chat))
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
            requests: Arc::clone(&state.requests),
            shutdown: Some(tx),
            task: Some(task),
        })
    }

    /// Base URL to put in `providers.fake.settings.baseURL`.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
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

async fn chat(State(state): State<Arc<FakeState>>, Json(body): Json<Value>) -> Response {
    state
        .requests
        .lock()
        .expect("fake requests")
        .push(body.clone());
    match &state.script {
        Script::Text(text) => sse_response(text_body(text)),
        Script::ToolThenText {
            tool,
            arguments,
            text,
        } => {
            if has_tool_result(&body) {
                sse_response(text_body(text))
            } else {
                sse_response(tool_body(tool, arguments))
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

fn has_tool_result(body: &Value) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages
                .iter()
                .any(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        })
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

fn text_body(text: &str) -> String {
    let mut chunks = Vec::new();
    chunks.push(frame(&envelope(&json!({"role": "assistant"}), None)));
    let split = text
        .char_indices()
        .nth(text.chars().count() / 2)
        .map_or(text.len(), |(index, _)| index);
    let (first, second) = text.split_at(split);
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
