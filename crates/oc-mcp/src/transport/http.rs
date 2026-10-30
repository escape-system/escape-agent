//! Streamable HTTP MCP transport: POST JSON with JSON or SSE responses.
//!
//! Implements session ids (`Mcp-Session-Id`), resumable standalone streams
//! (`Last-Event-ID`), expiry detection (404, or 400 "Server not initialized"
//! when a session was in use) and the upstream transient-retry policy.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use tokio::time::Instant;
use url::Url;

use crate::error::{McpError, McpResult, RpcError};
use crate::protocol::{
    Message, Notification, Request, RequestId, Response, SseDecoder, codes, sse_message,
};
use crate::transport::{
    Transport, TransportEventHandler, TransportFactory, parse_retry_after, with_retries,
};

/// Maximum bytes read from one HTTP response body.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Error bodies are truncated to this many bytes.
const ERROR_BODY_BYTES: usize = 8 * 1024;
/// DELETE timeout during shutdown.
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(1);
/// Timeout for posting notifications and responses to server requests.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Options for a streamable HTTP connection.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    /// Server name for logs and errors.
    pub server: String,
    /// Configured endpoint URL.
    pub url: Url,
    /// Extra headers from config (already placeholder-expanded).
    pub headers: BTreeMap<String, String>,
    /// Protocol revision for the `MCP-Protocol-Version` header.
    pub protocol_version: Option<String>,
}

/// Streamable HTTP transport.
pub struct HttpTransport {
    server: String,
    http: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    last_event_id: Mutex<Option<String>>,
    next_id: AtomicI64,
    handler: RwLock<Option<Arc<dyn TransportEventHandler>>>,
    closed: AtomicBool,
    stream_started: AtomicBool,
    self_ref: Mutex<Weak<Self>>,
}

impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransport")
            .field("server", &self.server)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl HttpTransport {
    /// Builds a transport (does not connect).
    pub fn new(options: HttpOptions) -> McpResult<Arc<Self>> {
        if !matches!(options.url.scheme(), "http" | "https") {
            return Err(McpError::Config(format!(
                "MCP URL for {} must use http or https",
                options.server
            )));
        }
        let mut headers = HeaderMap::new();
        for (name, value) in &options.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                McpError::Config(format!("invalid header name {name:?}: {error}"))
            })?;
            let value = HeaderValue::from_str(value)
                .map_err(|error| McpError::Config(format!("invalid header value: {error}")))?;
            headers.insert(name, value);
        }
        let http = reqwest::Client::builder()
            .user_agent(format!("opencode/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| McpError::Transport(error.to_string()))?;
        let transport = Arc::new(Self {
            server: options.server,
            http,
            url: options.url,
            headers,
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(options.protocol_version),
            last_event_id: Mutex::new(None),
            next_id: AtomicI64::new(0),
            handler: RwLock::new(None),
            closed: AtomicBool::new(false),
            stream_started: AtomicBool::new(false),
            self_ref: Mutex::new(Weak::new()),
        });
        *lock(&transport.self_ref) = Arc::downgrade(&transport);
        Ok(transport)
    }

    fn handler(&self) -> Option<Arc<dyn TransportEventHandler>> {
        self.handler
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn session_snapshot(&self) -> Option<String> {
        lock(&self.session_id).clone()
    }

    fn build(&self, method: Method) -> reqwest::RequestBuilder {
        let mut request = self.http.request(method, self.url.clone());
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        if let Some(session) = self.session_snapshot() {
            request = request.header("Mcp-Session-Id", session);
        }
        if let Some(version) = lock(&self.protocol_version).as_deref() {
            request = request.header("MCP-Protocol-Version", version);
        }
        request
    }

    fn send_failure(
        &self,
        status: StatusCode,
        body: String,
        headers: Option<&HeaderMap>,
    ) -> McpError {
        let session = headers
            .and_then(|headers| headers.get("mcp-session-id"))
            .is_some()
            || self.session_snapshot().is_some();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return McpError::NeedsAuth {
                server: self.server.clone(),
                message: format!("HTTP {status}: {body}"),
            };
        }
        if session
            && (status == StatusCode::NOT_FOUND
                || (status == StatusCode::BAD_REQUEST && body.contains("Server not initialized")))
        {
            return McpError::SessionExpired;
        }
        McpError::Http {
            status: status.as_u16(),
            body,
            retry_after: headers.and_then(|headers| {
                parse_retry_after(
                    headers
                        .get("retry-after")
                        .and_then(|value| value.to_str().ok()),
                )
            }),
        }
    }

    async fn read_capped(&self, response: reqwest::Response, limit: usize) -> McpResult<String> {
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| McpError::Network(error.to_string()))?;
            if body.len() + chunk.len() > limit {
                let take = limit.saturating_sub(body.len());
                body.extend_from_slice(&chunk[..take]);
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    fn record_session(&self, headers: &HeaderMap) {
        if let Some(value) = headers
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *lock(&self.session_id) = Some(value.to_owned());
        }
    }

    /// Sends one pre-serialized message and returns the raw response.
    async fn post_raw(
        &self,
        body: Vec<u8>,
        timeout: Duration,
        expected: Option<&RequestId>,
    ) -> McpResult<Value> {
        let request = self
            .build(Method::POST)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body);
        let response = match tokio::time::timeout(timeout, request.send()).await {
            Err(_) => return Err(McpError::Timeout(timeout)),
            Ok(Err(error)) => {
                if error.is_timeout() {
                    return Err(McpError::Timeout(timeout));
                }
                return Err(McpError::Network(error.to_string()));
            }
            Ok(Ok(response)) => response,
        };
        let status = response.status();
        let headers = response.headers().clone();
        self.record_session(&headers);
        if !status.is_success() {
            let body = self.read_capped(response, ERROR_BODY_BYTES).await?;
            return Err(self.send_failure(status, body, Some(&headers)));
        }
        if status == StatusCode::ACCEPTED || status == StatusCode::NO_CONTENT {
            return match expected {
                None => Ok(Value::Null),
                Some(_) => Err(McpError::Protocol(
                    "server accepted the request without a response".to_owned(),
                )),
            };
        }
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content_type.contains("text/event-stream") {
            self.consume_sse(response, expected, timeout).await
        } else {
            let body = self.read_capped(response, MAX_BODY_BYTES).await?;
            self.consume_json(&body, expected).await
        }
    }

    async fn consume_json(&self, body: &str, expected: Option<&RequestId>) -> McpResult<Value> {
        if body.trim().is_empty() {
            return match expected {
                None => Ok(Value::Null),
                Some(_) => Err(McpError::Protocol("empty response body".to_owned())),
            };
        }
        let value: Value = serde_json::from_str(body)
            .map_err(|error| McpError::Protocol(format!("invalid JSON response: {error}")))?;
        let messages: Vec<Value> = match value {
            Value::Array(items) => items,
            value => vec![value],
        };
        let mut result = None;
        for value in messages {
            let message = Message::from_value(value)?;
            if let Some(found) = self.handle_inbound(message, expected).await? {
                result = Some(found);
            }
        }
        match (result, expected) {
            (Some(Ok(value)), _) => Ok(value),
            (Some(Err(error)), _) => Err(error),
            (None, None) => Ok(Value::Null),
            (None, Some(id)) => Err(McpError::Protocol(format!(
                "no JSON-RPC response for request {id}"
            ))),
        }
    }

    async fn consume_sse(
        &self,
        response: reqwest::Response,
        expected: Option<&RequestId>,
        timeout: Duration,
    ) -> McpResult<Value> {
        let deadline = Instant::now() + timeout;
        let mut decoder = SseDecoder::new();
        let mut stream = response.bytes_stream();
        let mut result: Option<McpResult<Value>> = None;
        loop {
            if result.is_some() {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(McpError::Timeout(timeout));
            }
            let chunk = match tokio::time::timeout(remaining, stream.next()).await {
                Err(_) => return Err(McpError::Timeout(timeout)),
                Ok(None) => break,
                Ok(Some(Err(error))) => return Err(McpError::Network(error.to_string())),
                Ok(Some(Ok(chunk))) => chunk,
            };
            for event in decoder.feed(&chunk) {
                if let Some(id) = event.id.as_deref()
                    && !id.is_empty()
                {
                    *lock(&self.last_event_id) = Some(id.to_owned());
                }
                if event.data.trim().is_empty() {
                    continue;
                }
                let message = match sse_message(&event) {
                    Ok(message) => message,
                    Err(error) => {
                        if let Some(handler) = self.handler() {
                            handler.on_error(error.to_string());
                        }
                        continue;
                    }
                };
                if let Some(found) = self.handle_inbound(message, expected).await? {
                    result = Some(found);
                    break;
                }
            }
        }
        match (result, expected) {
            (Some(Ok(value)), _) => Ok(value),
            (Some(Err(error)), _) => Err(error),
            (None, None) => Ok(Value::Null),
            (None, Some(id)) => Err(McpError::Protocol(format!(
                "SSE stream ended without a response for request {id}"
            ))),
        }
    }

    /// Handles one inbound message; returns a typed result when it answered
    /// `expected`.
    async fn handle_inbound(
        &self,
        message: Message,
        expected: Option<&RequestId>,
    ) -> McpResult<Option<McpResult<Value>>> {
        match message {
            Message::Response(response) => {
                if expected != Some(&response.id) {
                    tracing::debug!(server = %self.server, id = %response.id, "unmatched MCP response");
                    return Ok(None);
                }
                let result = match (response.result, response.error) {
                    (_, Some(error)) => Err(McpError::Rpc(error)),
                    (Some(result), None) => Ok(result),
                    (None, None) => Err(McpError::Protocol(
                        "response has neither result nor error".to_owned(),
                    )),
                };
                Ok(Some(result))
            }
            Message::Notification(notification) => {
                if let Some(handler) = self.handler() {
                    handler
                        .on_notification(&notification.method, notification.params)
                        .await;
                }
                Ok(None)
            }
            Message::Request(request) => {
                let result = match self.handler() {
                    Some(handler) => handler.on_request(&request.method, request.params).await,
                    None => Err(RpcError {
                        code: codes::METHOD_NOT_FOUND,
                        message: format!("unsupported method: {}", request.method),
                        data: None,
                    }),
                };
                let response = match result {
                    Ok(result) => Response::success(request.id, result),
                    Err(error) => Response::failure(request.id, error),
                };
                if let Err(error) = self.send_response(Message::Response(response)).await {
                    tracing::warn!(server = %self.server, %error, "failed to answer MCP server request");
                }
                Ok(None)
            }
        }
    }

    /// Sends a response to a server request without parsing the reply body.
    async fn send_response(&self, message: Message) -> McpResult<()> {
        let body = message.to_line()?;
        let request = self
            .build(Method::POST)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body);
        let response = tokio::time::timeout(RESPONSE_TIMEOUT, request.send())
            .await
            .map_err(|_| McpError::Timeout(RESPONSE_TIMEOUT))?
            .map_err(|error| McpError::Network(error.to_string()))?;
        let status = response.status();
        self.record_session(response.headers());
        if status.is_success() {
            let _ = tokio::time::timeout(
                RESPONSE_TIMEOUT,
                self.read_capped(response, ERROR_BODY_BYTES),
            )
            .await;
            Ok(())
        } else {
            let body = tokio::time::timeout(
                RESPONSE_TIMEOUT,
                self.read_capped(response, ERROR_BODY_BYTES),
            )
            .await
            .map_err(|_| McpError::Timeout(RESPONSE_TIMEOUT))??;
            Err(self.send_failure(status, body, None))
        }
    }

    async fn post_notification(&self, message: Message) -> McpResult<()> {
        let body = message.to_line()?;
        let request = self
            .build(Method::POST)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body);
        let response = tokio::time::timeout(RESPONSE_TIMEOUT, request.send())
            .await
            .map_err(|_| McpError::Timeout(RESPONSE_TIMEOUT))?
            .map_err(|error| McpError::Network(error.to_string()))?;
        let status = response.status();
        self.record_session(response.headers());
        if status.is_success() {
            let _ = tokio::time::timeout(
                RESPONSE_TIMEOUT,
                self.read_capped(response, ERROR_BODY_BYTES),
            )
            .await;
            Ok(())
        } else {
            let body = tokio::time::timeout(
                RESPONSE_TIMEOUT,
                self.read_capped(response, ERROR_BODY_BYTES),
            )
            .await
            .map_err(|_| McpError::Timeout(RESPONSE_TIMEOUT))??;
            Err(self.send_failure(status, body, None))
        }
    }

    fn next_request_id(&self) -> RequestId {
        RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst) + 1)
    }

    async fn event_stream(&self) -> McpResult<()> {
        let mut request = self.build(Method::GET).header(ACCEPT, "text/event-stream");
        if let Some(last) = lock(&self.last_event_id).clone() {
            request = request.header("Last-Event-ID", last);
        }
        let response = request
            .send()
            .await
            .map_err(|error| McpError::Network(error.to_string()))?;
        let status = response.status();
        self.record_session(response.headers());
        if status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_IMPLEMENTED {
            // Server has no standalone stream; notifications arrive on POSTs.
            return Ok(());
        }
        if status == StatusCode::NOT_FOUND {
            return Err(McpError::SessionExpired);
        }
        if !status.is_success() {
            let body = self.read_capped(response, ERROR_BODY_BYTES).await?;
            return Err(self.send_failure(status, body, None));
        }
        let mut decoder = SseDecoder::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if self.closed.load(Ordering::SeqCst) {
                return Ok(());
            }
            let chunk = chunk.map_err(|error| McpError::Network(error.to_string()))?;
            for event in decoder.feed(&chunk) {
                if let Some(id) = event.id.as_deref()
                    && !id.is_empty()
                {
                    *lock(&self.last_event_id) = Some(id.to_owned());
                }
                if event.data.trim().is_empty() {
                    continue;
                }
                match sse_message(&event) {
                    Ok(message) => {
                        let _ = self.handle_inbound(message, None).await?;
                    }
                    Err(error) => {
                        if let Some(handler) = self.handler() {
                            handler.on_error(error.to_string());
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> McpResult<Value> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::Closed);
        }
        self.ensure_stream();
        let id = self.next_request_id();
        let body = Message::Request(Request::new(id.clone(), method, params)).to_line()?;
        let deadline = Instant::now() + timeout;
        let value = with_retries(deadline, timeout, |remaining| {
            let body = body.clone();
            let id = id.clone();
            async move { self.post_raw(body, remaining, Some(&id)).await }
        })
        .await;
        match value {
            Ok(value) => Ok(value),
            Err(McpError::Timeout(elapsed)) => {
                let cancelled = Notification::new(
                    "notifications/cancelled",
                    Some(serde_json::json!({
                        "requestId": id,
                        "reason": format!("request timed out after {elapsed:?}"),
                    })),
                );
                let _ = self
                    .post_notification(Message::Notification(cancelled))
                    .await;
                Err(McpError::Timeout(elapsed))
            }
            Err(error) => Err(error),
        }
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> McpResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::Closed);
        }
        self.post_notification(Message::Notification(Notification::new(method, params)))
            .await?;
        self.ensure_stream();
        Ok(())
    }

    fn set_event_handler(&self, handler: Arc<dyn TransportEventHandler>) {
        *self
            .handler
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handler);
    }

    fn set_protocol_version(&self, version: &str) {
        *lock(&self.protocol_version) = Some(version.to_owned());
    }

    fn session_id(&self) -> Option<String> {
        self.session_snapshot()
    }

    async fn shutdown(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let session = self.session_snapshot();
        if session.is_some() {
            let request = self.build(Method::DELETE);
            let _ = tokio::time::timeout(TERMINATE_TIMEOUT, request.send()).await;
        }
    }
}

impl HttpTransport {
    /// Starts the standalone GET stream once the session and protocol version
    /// exist. No-op before initialization and after shutdown.
    fn ensure_stream(&self) {
        if self.closed.load(Ordering::SeqCst) || lock(&self.protocol_version).is_none() {
            return;
        }
        if let Some(transport) = lock(&self.self_ref).upgrade() {
            transport.spawn_event_stream();
        }
    }

    fn spawn_event_stream(self: &Arc<Self>) {
        if self.stream_started.swap(true, Ordering::SeqCst) || self.closed.load(Ordering::SeqCst) {
            return;
        }
        let transport = self.clone();
        tokio::spawn(async move {
            if let Err(error) = transport.event_stream().await
                && let Some(handler) = transport.handler()
            {
                handler.on_error(format!("MCP event stream ended: {error}"));
            }
        });
    }
}

/// Builds fresh HTTP transports for reconnect.
#[derive(Debug)]
pub struct HttpFactory {
    options: HttpOptions,
}

impl HttpFactory {
    /// Creates a factory from HTTP options.
    #[must_use]
    pub fn new(options: HttpOptions) -> Self {
        Self { options }
    }
}

#[async_trait]
impl TransportFactory for HttpFactory {
    async fn create(&self) -> McpResult<Arc<dyn Transport>> {
        HttpTransport::new(self.options.clone()).map(|transport| transport as Arc<dyn Transport>)
    }
}
