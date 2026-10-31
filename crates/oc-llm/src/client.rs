//! Provider HTTP client: request construction, retries, timeouts, and SSE decoding.
//!
//! Retries happen before the response body is consumed. Once events have been
//! emitted a failed read cannot be replayed without duplicating output, so the
//! stream reports a typed [`LlmEvent::ProviderError`] instead. Dropping the
//! returned stream drops the HTTP body and cancels any in-flight read.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::stream::{self, BoxStream, StreamExt};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue};
use reqwest::{Client, Response};

use crate::catalog::{Protocol, ResolvedModel};
use crate::error::LlmError;
use crate::event::{LlmEvent, LlmRequest, LlmStream, Timeouts};
use crate::protocols::{self, Decoder};
use crate::retry::{RetryPolicy, RetryState, parse_retry_after};
use crate::sse::SseParser;

/// Provider transport.
#[derive(Debug, Clone)]
pub struct LlmClient {
    http: Client,
    retry: RetryPolicy,
    timeouts: Timeouts,
}

impl LlmClient {
    /// Build a client with the upstream defaults (2s→10s retries, 300s header
    /// timeout, 900s chunk timeout).
    pub fn new() -> Result<Self, LlmError> {
        let http = Client::builder()
            .build()
            .map_err(|error| LlmError::Transport {
                message: error.to_string(),
            })?;
        Ok(Self {
            http,
            retry: RetryPolicy::default(),
            timeouts: Timeouts::default(),
        })
    }

    #[must_use]
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    #[must_use]
    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    #[must_use]
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry
    }

    /// Start a provider stream. Any connection/HTTP error is retried according to
    /// the policy; once the response headers arrive this returns a stream.
    #[allow(
        clippy::result_large_err,
        reason = "the public API returns the typed LlmError directly rather than boxing it"
    )]
    pub async fn stream(&self, request: LlmRequest) -> Result<LlmStream, LlmError> {
        let model = request.model.clone();
        if model.protocol == Protocol::Unsupported {
            return Err(LlmError::UnsupportedProtocol {
                provider: model.provider.id.clone(),
                model: model.model.id.clone(),
                protocol: model
                    .model
                    .package
                    .clone()
                    .or_else(|| Some(model.provider.package.clone()))
                    .filter(|package| !package.is_empty())
                    .unwrap_or_else(|| "unknown".to_string()),
            });
        }
        let wire = protocols::build(&request, model.protocol)?;
        let headers = Self::headers(&request, &model);
        let timeouts = request
            .options
            .timeouts
            .clone()
            .unwrap_or_else(|| self.timeouts.clone());
        let deadline = timeouts.total.map(|total| Instant::now() + total);
        let url = join_url(&model.base_url, &wire.path);
        let body = serde_json::to_vec(&wire.body).map_err(|error| LlmError::InvalidRequest {
            message: format!("could not encode request body: {error}"),
        })?;
        let response = self
            .send(&url, &body, &headers, &timeouts, deadline)
            .await?;
        let chunk_timeout = timeouts.chunk;
        let state = StreamState {
            body: Box::pin(response.bytes_stream()),
            parser: SseParser::new(),
            decoder: Decoder::new(model.protocol),
            pending: VecDeque::new(),
            chunk_timeout,
            deadline,
            finished: false,
        };
        Ok(Box::pin(stream::unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((event, state));
                }
                if state.finished || state.decoder.is_finished() {
                    return None;
                }
                let next = match read_chunk(&mut state).await {
                    Ok(next) => next,
                    Err(event) => {
                        state.pending.push_back(event);
                        state.finished = true;
                        continue;
                    }
                };
                match next {
                    None => {
                        state.close();
                        state.finished = true;
                    }
                    Some(bytes) => {
                        let events = state.parser.push(&bytes);
                        for event in events {
                            state.pending.extend(state.decoder.event(&event));
                        }
                        if state.decoder.is_finished() {
                            for event in state.parser.finish() {
                                state.pending.extend(state.decoder.event(&event));
                            }
                            state.pending.extend(state.decoder.finish());
                        }
                    }
                }
            }
        })))
    }

    fn headers(request: &LlmRequest, model: &ResolvedModel) -> Vec<(String, String)> {
        let mut headers = BTreeMap::new();
        let key = request
            .options
            .api_key
            .clone()
            .or_else(|| model.api_key_from_env());
        match model.protocol {
            Protocol::OpenAiChat => {
                if let Some(key) = &key {
                    headers.insert("Authorization".to_string(), format!("Bearer {key}"));
                }
            }
            Protocol::AnthropicMessages => {
                if let Some(key) = &key {
                    headers.insert("x-api-key".to_string(), key.clone());
                }
                headers.insert("anthropic-version".to_string(), "2023-06-01".to_string());
            }
            Protocol::Gemini => {
                if let Some(key) = &key {
                    headers.insert("x-goog-api-key".to_string(), key.clone());
                }
            }
            Protocol::Unsupported => {}
        }
        for (name, value) in model.headers() {
            headers.insert(name, value);
        }
        if let Some(cache_key) = &request.options.cache_key
            && !cache_key.is_empty()
        {
            headers.insert("x-session-affinity".to_string(), cache_key.clone());
            headers.insert("x-opencode-session-id".to_string(), cache_key.clone());
        }
        for (name, value) in &request.options.headers {
            headers.insert(name.clone(), value.clone());
        }
        headers.into_iter().collect()
    }

    async fn send(
        &self,
        url: &str,
        body: &[u8],
        headers: &[(String, String)],
        timeouts: &Timeouts,
        deadline: Option<Instant>,
    ) -> Result<Response, LlmError> {
        let mut state = RetryState::new(self.retry.clone());
        loop {
            let result = self
                .attempt(url, body, headers, timeouts.header, deadline)
                .await;
            let error = match result {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            let Some(delay) = state.next_delay(&error) else {
                return Err(error);
            };
            if let Some(deadline) = deadline {
                let wake = Instant::now() + delay;
                if wake >= deadline {
                    return Err(LlmError::Timeout {
                        phase: "retrying the provider",
                    });
                }
                tokio::time::sleep_until(tokio::time::Instant::from_std(wake)).await;
            } else {
                tokio::time::sleep(delay).await;
            }
        }
    }

    async fn attempt(
        &self,
        url: &str,
        body: &[u8],
        headers: &[(String, String)],
        header_timeout: Duration,
        deadline: Option<Instant>,
    ) -> Result<Response, LlmError> {
        let mut builder = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .body(body.to_vec());
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                LlmError::InvalidRequest {
                    message: format!("invalid header name `{name}`: {error}"),
                }
            })?;
            let value = HeaderValue::from_str(value).map_err(|error| LlmError::InvalidRequest {
                message: format!("invalid value for header `{name}`: {error}"),
            })?;
            builder = builder.header(name, value);
        }

        let header_deadline = Instant::now() + header_timeout;
        let limit = deadline.map_or(header_deadline, |deadline| deadline.min(header_deadline));
        let response =
            match tokio::time::timeout_at(tokio::time::Instant::from_std(limit), builder.send())
                .await
            {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => {
                    return Err(LlmError::Timeout {
                        phase: "waiting for response headers",
                    });
                }
            };

        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after)
            .map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
        let should_retry = response
            .headers()
            .get("x-should-retry")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = match tokio::time::timeout(Duration::from_secs(10), response.text()).await {
            Ok(Ok(body)) => body,
            Ok(Err(_)) | Err(_) => String::new(),
        };
        let body = truncate(&body, 2_000);
        if should_retry.as_deref() == Some("false") && status.is_server_error() {
            return Err(LlmError::InvalidRequest {
                message: format!("provider returned HTTP {status}: {body}"),
            });
        }
        Err(LlmError::Http {
            status: status.as_u16(),
            message: if body.is_empty() {
                status
                    .canonical_reason()
                    .unwrap_or("provider error")
                    .to_string()
            } else {
                body
            },
            retry_after_ms: retry_after,
        })
    }
}

struct StreamState {
    body: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    parser: SseParser,
    decoder: Decoder,
    pending: VecDeque<LlmEvent>,
    chunk_timeout: Duration,
    deadline: Option<Instant>,
    finished: bool,
}

impl StreamState {
    fn close(&mut self) {
        for event in self.parser.finish() {
            self.pending.extend(self.decoder.event(&event));
        }
        self.pending.extend(self.decoder.finish());
    }
}

#[allow(
    clippy::result_large_err,
    reason = "stream read failures are returned as the already-public LlmEvent payload"
)]
async fn read_chunk(state: &mut StreamState) -> Result<Option<Bytes>, LlmEvent> {
    let limit = state.deadline.map_or(
        tokio::time::Instant::now() + state.chunk_timeout,
        |deadline| {
            tokio::time::Instant::from_std(deadline.min(Instant::now() + state.chunk_timeout))
        },
    );
    match tokio::time::timeout_at(limit, state.body.next()).await {
        Ok(Some(Ok(bytes))) => Ok(Some(bytes)),
        Ok(Some(Err(error))) => Err(LlmEvent::ProviderError {
            message: format!("stream read failed: {error}"),
        }),
        Ok(None) => Ok(None),
        Err(_)
            if state
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline) =>
        {
            Err(LlmEvent::ProviderError {
                message: "provider stream exceeded its total timeout".to_string(),
            })
        }
        Err(_) => Err(LlmEvent::ProviderError {
            message: "provider stream chunk timed out".to_string(),
        }),
    }
}

fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect()
}

/// Shared client used by runtimes that clone a handle into each session.
#[derive(Debug, Clone)]
pub struct SharedLlmClient(pub Arc<LlmClient>);

impl SharedLlmClient {
    pub fn new(client: LlmClient) -> Self {
        Self(Arc::new(client))
    }
}
