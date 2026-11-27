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
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_TYPE, HeaderName, HeaderValue};
use std::sync::RwLock;

use reqwest::{Client, Response};

use crate::catalog::{Protocol, ResolvedModel};
use crate::error::{LlmError, parse_google_error_details};
use crate::event::{LlmEvent, LlmRequest, LlmStream, Timeouts, model_supports_request_modalities};
use crate::hook::LlmTransportHook;
use crate::protocols::{self, Decoder};
use crate::quota::{ProviderRateLimit, extract_rate_limit, now_epoch_ms};
use crate::rate_limit::{ProviderRateLimiter, RateLimitPolicy};
use crate::retry::{RetryPolicy, RetryState, parse_retry_after};
use crate::sse::SseParser;
use crate::token::estimate_request_tokens;
use crate::vertex::{GoogleAdcClient, is_vertex_target, resolve_vertex_base_url};

/// Provider transport.
#[derive(Debug, Clone)]
pub struct LlmClient {
    http: Client,
    retry: RetryPolicy,
    timeouts: Timeouts,
    hook: Arc<RwLock<Option<Arc<dyn LlmTransportHook>>>>,
    rate_limits: Arc<RwLock<BTreeMap<String, ProviderRateLimit>>>,
    rate_limiter: Arc<ProviderRateLimiter>,
    adc: Arc<GoogleAdcClient>,
}

impl LlmClient {
    /// Build a client with the upstream defaults (2s→10s retries, 300s header
    /// timeout, 900s chunk timeout).
    pub fn new() -> Result<Self, LlmError> {
        let http = Client::builder()
            .user_agent(concat!("opencode/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Some(Duration::from_secs(30)))
            .http2_keep_alive_interval(Some(Duration::from_secs(30)))
            .http2_keep_alive_timeout(Duration::from_secs(15))
            .http2_keep_alive_while_idle(true)
            .build()
            .map_err(|error| LlmError::Transport {
                message: format_error_chain(&error),
            })?;
        Ok(Self {
            http,
            retry: RetryPolicy::default(),
            timeouts: Timeouts::default(),
            hook: Arc::new(RwLock::new(None)),
            rate_limits: Arc::new(RwLock::new(BTreeMap::new())),
            rate_limiter: Arc::new(ProviderRateLimiter::new()),
            adc: Arc::new(GoogleAdcClient::default()),
        })
    }

    /// Borrow the proactive GCRA rate limiter for this client.
    #[must_use]
    pub fn rate_limiter(&self) -> &Arc<ProviderRateLimiter> {
        &self.rate_limiter
    }

    /// Install a proactive GCRA + spend-velocity policy for `provider`.
    pub fn set_rate_limit_policy(&self, provider: &str, policy: RateLimitPolicy) {
        self.rate_limiter.set_policy(provider, policy);
    }

    /// Borrow the GCP ADC token cache client.
    #[must_use]
    pub fn adc(&self) -> &Arc<GoogleAdcClient> {
        &self.adc
    }

    /// Borrow the underlying pooled HTTP client for auxiliary requests (e.g. quota probes).
    #[must_use]
    pub fn http_client(&self) -> &Client {
        &self.http
    }

    /// Return the latest HTTP rate-limit header snapshots captured per provider.
    #[must_use]
    pub fn rate_limits(&self) -> Vec<ProviderRateLimit> {
        self.rate_limits
            .read()
            .ok()
            .map(|guard| guard.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Record or update the latest rate-limit header snapshot for a provider.
    pub fn record_rate_limit(&self, snapshot: ProviderRateLimit) {
        if let Ok(mut guard) = self.rate_limits.write() {
            guard.insert(snapshot.provider_id.clone(), snapshot);
        }
    }

    #[must_use]
    pub fn with_hook(self, hook: Arc<dyn LlmTransportHook>) -> Self {
        self.set_hook(Some(hook));
        self
    }

    pub fn set_hook(&self, hook: Option<Arc<dyn LlmTransportHook>>) {
        if let Ok(mut slot) = self.hook.write() {
            *slot = hook;
        }
    }

    #[must_use]
    pub fn hook(&self) -> Option<Arc<dyn LlmTransportHook>> {
        self.hook.read().ok().and_then(|slot| slot.clone())
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
    pub async fn stream(&self, mut request: LlmRequest) -> Result<LlmStream, LlmError> {
        if let Some(hook) = self.hook()
            && hook.handles(&request)
        {
            return hook.stream(request).await;
        }
        if !model_supports_request_modalities(&request.model, &request.messages) {
            return Err(LlmError::InvalidRequest {
                message: format!(
                    "model `{}` supports input modalities {:?}, which does not cover every attachment in this request",
                    request.model.reference, request.model.model.capabilities.input
                ),
            });
        }
        if is_vertex_target(&request.model.provider.id, &request.model.base_url) {
            let settings = request.model.settings();
            request.model.base_url = resolve_vertex_base_url(&request.model.base_url, &settings)?;
        }
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
        let headers = self.resolve_headers(&request, &model).await?;
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
        let est_tokens = u32::try_from(estimate_request_tokens(
            &request.system,
            &request.messages,
            &request.tools,
        ))
        .unwrap_or(u32::MAX);
        let est_cost_usd = model
            .model
            .cost
            .first()
            .map_or(0.0, |c| (f64::from(est_tokens) / 1_000_000.0) * c.input);
        let cred_slot = request
            .options
            .credential_id
            .as_deref()
            .unwrap_or("default");
        let response = self
            .send(
                &url,
                &body,
                &headers,
                &timeouts,
                deadline,
                Some((
                    &model.provider.id,
                    &model.model.id,
                    cred_slot,
                    est_tokens,
                    est_cost_usd,
                )),
            )
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
        Ok(into_llm_stream(state))
    }

    async fn resolve_headers(
        &self,
        request: &LlmRequest,
        model: &ResolvedModel,
    ) -> Result<Vec<(String, String)>, LlmError> {
        let mut headers = BTreeMap::new();
        let is_vertex = is_vertex_target(&model.provider.id, &model.base_url);
        let key = request
            .options
            .api_key
            .clone()
            .or_else(|| model.api_key_from_env());

        if is_vertex {
            let settings = model.settings();
            let token = self
                .adc
                .bearer_token(&self.http, key.as_deref(), &settings)
                .await?;
            headers.insert(
                "Authorization".to_string(),
                format!("Bearer {}", token.as_str()),
            );
        } else {
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
                Protocol::Gemini | Protocol::GeminiInteractions => {
                    if let Some(key) = &key {
                        headers.insert("x-goog-api-key".to_string(), key.clone());
                    }
                }
                Protocol::Unsupported => {}
            }
        }
        for (name, value) in model.headers() {
            headers.insert(name, value);
        }
        if let Some(cache_key) = &request.options.cache_key
            && !cache_key.is_empty()
        {
            headers.insert("x-session-affinity".to_string(), cache_key.clone());
            headers.insert("x-opencode-session-id".to_string(), cache_key.clone());
            headers.insert("x-opencode-session".to_string(), cache_key.clone());
        }
        for (name, value) in &request.options.headers {
            headers.insert(name.clone(), value.clone());
        }
        Ok(headers.into_iter().collect())
    }

    async fn send(
        &self,
        url: &str,
        body: &[u8],
        headers: &[(String, String)],
        timeouts: &Timeouts,
        deadline: Option<Instant>,
        target: Option<(&str, &str, &str, u32, f64)>,
    ) -> Result<Response, LlmError> {
        let mut state = RetryState::new(self.retry.clone());
        loop {
            let permit = if let Some((provider_id, _, cred_slot, est_tokens, est_cost)) = target {
                Some(
                    self.rate_limiter
                        .acquire(provider_id, cred_slot, est_tokens, est_cost)
                        .await,
                )
            } else {
                None
            };
            let result = self
                .attempt(
                    url,
                    body,
                    headers,
                    timeouts.header,
                    deadline,
                    target.map(|(p, m, _, _, _)| (p, m)),
                )
                .await;
            // Release concurrency permit immediately before checking/sleeping on retry
            // so backoff never pins a scarce worker/rate slot.
            drop(permit);
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
        target: Option<(&str, &str)>,
    ) -> Result<Response, LlmError> {
        let mut builder = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .header(ACCEPT_ENCODING, "identity")
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
        if let Some((provider_id, model_id)) = target
            && let Some(snapshot) = extract_rate_limit(
                provider_id,
                model_id,
                status.as_u16(),
                response.headers(),
                now_epoch_ms(),
            )
        {
            self.record_rate_limit(snapshot);
        }
        if status.is_success() {
            return Ok(response);
        }
        Err(classify_http_error(response).await)
    }
}

async fn classify_http_error(response: Response) -> LlmError {
    let status = response.status();
    let header_retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after)
        .map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
        .or_else(|| {
            response
                .headers()
                .get("retry-after-ms")
                .and_then(|value| value.to_str().ok())
                .and_then(|raw| raw.trim().parse::<u64>().ok())
        });
    let should_retry = response
        .headers()
        .get("x-should-retry")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = match tokio::time::timeout(Duration::from_secs(10), response.text()).await {
        Ok(Ok(body)) => body,
        Ok(Err(_)) | Err(_) => String::new(),
    };
    let details = parse_google_error_details(&body);
    let retry_after = match (header_retry_after, details.retry_after_ms) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let body = truncate(&body, 2_000);
    if details.terminal_quota {
        let reason = if details.violations.is_empty() {
            body.clone()
        } else {
            format!("{} ({})", details.violations.join("; "), body)
        };
        return LlmError::QuotaTerminal { message: reason };
    }
    if (body.contains("PROHIBITED_CONTENT") || body.contains("SAFETY")) && status.as_u16() == 400 {
        return LlmError::PolicyBlocked {
            message: format!("HTTP {status}: {body}"),
        };
    }
    if should_retry.as_deref() == Some("false") && status.is_server_error() {
        return LlmError::InvalidRequest {
            message: format!("provider returned HTTP {status}: {body}"),
        };
    }
    LlmError::Http {
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

fn into_llm_stream(state: StreamState) -> LlmStream {
    Box::pin(stream::unfold(state, |mut state| async move {
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
                    if state.decoder.had_completion() {
                        state.close();
                    } else {
                        state.pending.push_back(event);
                    }
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
    }))
}

impl StreamState {
    fn close(&mut self) {
        for event in self.parser.finish() {
            self.pending.extend(self.decoder.event(&event));
        }
        if self.decoder.had_completion() {
            self.pending.extend(self.decoder.finish());
        } else {
            let mut tail = self.decoder.finish();
            tail.retain(|event| !matches!(event, LlmEvent::Finish { .. }));
            self.pending.extend(tail);
            if !self
                .pending
                .iter()
                .any(|event| matches!(event, LlmEvent::ProviderError { .. }))
            {
                self.pending.push_back(LlmEvent::ProviderError {
                    message: "provider stream closed before completion".to_string(),
                });
            }
        }
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
            message: format!("stream read failed: {}", format_error_chain(&error)),
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

fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let msg = cause.to_string();
        if !msg.is_empty() && !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        source = cause.source();
    }
    out
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

    /// Borrow the underlying HTTP client pool.
    #[must_use]
    pub fn http_client(&self) -> &Client {
        self.0.http_client()
    }

    /// Return the latest HTTP rate-limit telemetry captured per provider.
    #[must_use]
    pub fn rate_limits(&self) -> Vec<ProviderRateLimit> {
        self.0.rate_limits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_stream(chunks: Vec<&'static str>, protocol: Protocol) -> LlmStream {
        let body = stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok::<Bytes, reqwest::Error>(Bytes::from_static(chunk.as_bytes()))),
        );
        into_llm_stream(StreamState {
            body: Box::pin(body),
            parser: SseParser::new(),
            decoder: Decoder::new(protocol),
            pending: VecDeque::new(),
            chunk_timeout: Duration::from_secs(5),
            deadline: None,
            finished: false,
        })
    }

    #[tokio::test]
    async fn premature_eof_without_completion_emits_provider_error_not_stop() {
        let mut s = collect_stream(
            vec!["data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n"],
            Protocol::OpenAiChat,
        );
        let mut events = Vec::new();
        while let Some(ev) = s.next().await {
            events.push(ev);
        }
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LlmEvent::Finish { .. })),
            "unexpected Finish on premature EOF: {events:?}"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            LlmEvent::ProviderError { message } if message.contains("closed before completion")
        )));
    }

    #[tokio::test]
    async fn eof_after_finish_reason_without_done_marker_still_completes() {
        let mut s = collect_stream(
            vec![
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            ],
            Protocol::OpenAiChat,
        );
        let mut events = Vec::new();
        while let Some(ev) = s.next().await {
            events.push(ev);
        }
        assert!(events.iter().any(|event| matches!(
            event,
            LlmEvent::Finish { reason, .. } if reason == "stop"
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LlmEvent::ProviderError { .. }))
        );
    }
}
