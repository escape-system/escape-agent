//! `oc_llm::LlmTransportHook` implementation routing Google/Antigravity model requests
//! through Cloud Code Assist (`daily-cloudcode-pa.googleapis.com` / `cloudcode-pa.googleapis.com`)
//! with multi-account failover, token refresh, thinking recovery, dual-pool fallback, and
//! empty-response retries.

mod pool;
mod single;
mod stream;
mod validation;

use crate::accounts::ModelFamily;
use crate::constants::{ANTIGRAVITY_PROVIDER_ID, HeaderStyle};
use crate::rotation::RateLimitReason;
use crate::service::AntigravityService;
use crate::transform::model_resolver::resolve_antigravity_model;
use oc_llm::{
    Catalog, HookStreamFuture, LlmError, LlmRequest, LlmStream, LlmTransportHook, ModelInfo,
    ResolvedModel,
};
use pool::dispatch_pool_stream;
use std::time::Duration;

#[cfg(test)]
use crate::service::now_ms;
#[cfg(test)]
use futures_util::StreamExt;
#[cfg(test)]
use oc_llm::LlmEvent;

enum DispatchOutcome {
    Stream(LlmStream),
    RetryStripThinking,
    RetryEmptyResponse,
    TryFallbackPool,
}

enum SingleAttempt {
    Ready(LlmStream),
    RetryStripThinking,
    RetryEmptyResponse,
    Unauthorized,
    ValidationRequired {
        reason: String,
        url: Option<String>,
        body: String,
    },
    RateLimited {
        reason: RateLimitReason,
        retry_after_ms: Option<i64>,
        status: u16,
        body: String,
    },
    RetryableFailure {
        status: u16,
        body: String,
    },
    Fatal(LlmError),
}

impl LlmTransportHook for AntigravityService {
    fn handles(&self, request: &LlmRequest) -> bool {
        if !request
            .model
            .provider
            .id
            .eq_ignore_ascii_case(ANTIGRAVITY_PROVIDER_ID)
        {
            return false;
        }

        let model_id = request.model.model.id.trim();
        if model_id.starts_with("antigravity-") || model_id.to_ascii_lowercase().contains("claude")
        {
            return true;
        }

        if request
            .model
            .base_url
            .contains("cloudcode-pa.googleapis.com")
        {
            return true;
        }

        if self.account_count() == 0 {
            return false;
        }

        // Standard Gemini model IDs route through Antigravity/Gemini CLI OAuth pool
        // unless an explicit non-OAuth Google API key (e.g. `AIza...` or test key) is supplied.
        match request
            .options
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => true,
            Some(key) => key.starts_with("ya29."),
        }
    }

    fn stream(&self, request: LlmRequest) -> HookStreamFuture {
        let svc = self.clone();
        Box::pin(async move { execute_antigravity_stream(svc, request).await })
    }

    fn resolve_model(&self, catalog: &Catalog, reference: &str) -> Option<ResolvedModel> {
        self.resolve_catalog_model(catalog, reference)
    }

    fn dynamic_models(&self) -> Vec<ModelInfo> {
        self.antigravity_model_infos()
    }
}

/// Outcome of one empty-response observation for a single style/pool.
#[derive(Debug, PartialEq, Eq)]
enum EmptyRetry {
    /// Retry this pool after `backoff_ms`.
    Backoff { backoff_ms: u64 },
    /// This pool's budget is exhausted; surface the canonical error.
    Exhausted { attempts: u32 },
}

/// Pure decision helper so backoff growth and per-pool isolation are testable
/// without driving a real HTTP stream. `attempts` is this pool's own counter.
fn record_empty_attempt(cfg: &crate::config::AntigravityConfig, attempts: &mut u32) -> EmptyRetry {
    *attempts += 1;
    let max = cfg.empty_response_max_attempts.max(1);
    if *attempts < max {
        EmptyRetry::Backoff {
            backoff_ms: cfg.empty_response_backoff_ms(*attempts - 1),
        }
    } else {
        EmptyRetry::Exhausted {
            attempts: *attempts,
        }
    }
}

async fn execute_antigravity_stream(
    svc: AntigravityService,
    request: LlmRequest,
) -> std::result::Result<LlmStream, LlmError> {
    let cfg = svc.config();
    let variant = request.model.variant.as_ref().map(|v| v.id.as_str());
    let initial = resolve_antigravity_model(&request.model.model.id, variant, cfg.cli_first);

    let mut styles = vec![initial.header_style];
    if cfg.quota_fallback && initial.family == ModelFamily::Gemini {
        let fallback = match initial.header_style {
            HeaderStyle::Antigravity => HeaderStyle::GeminiCli,
            HeaderStyle::GeminiCli => HeaderStyle::Antigravity,
        };
        styles.push(fallback);
    }

    // Empty-response attempts are tracked per header style/pool: exhausting the
    // primary pool's attempts must not consume the fallback pool's budget.
    let mut empty_attempts: Vec<u32> = vec![0; styles.len()];
    let mut strip_thinking = false;
    let mut thinking_retried = false;
    let mut start_idx: usize = 0;

    loop {
        let mut last_err: Option<LlmError> = None;
        for (idx, &style) in styles.iter().enumerate().skip(start_idx) {
            let allow_override = idx > 0 || (cfg.cli_first && style != initial.header_style);
            let override_style = allow_override.then_some(style);
            match dispatch_pool_stream(&svc, &request, style, override_style, strip_thinking).await
            {
                Ok(DispatchOutcome::Stream(stream)) => return Ok(stream),
                Ok(DispatchOutcome::RetryStripThinking) => {
                    if !thinking_retried {
                        thinking_retried = true;
                        strip_thinking = true;
                        last_err = None;
                        start_idx = idx;
                        break;
                    }
                    last_err = Some(LlmError::Provider {
                        message: "thinking block ordering error persisted after recovery"
                            .to_owned(),
                    });
                }
                Ok(DispatchOutcome::RetryEmptyResponse) => {
                    match record_empty_attempt(&cfg, &mut empty_attempts[idx]) {
                        // Exponential backoff: base * 2^(attempts-1), capped.
                        EmptyRetry::Backoff { backoff_ms } => {
                            tracing::warn!(
                                attempt = empty_attempts[idx],
                                max_attempts = cfg.empty_response_max_attempts,
                                style = style.as_str(),
                                pool_index = idx,
                                backoff_ms,
                                exhausted = false,
                                "antigravity: empty stream retry"
                            );
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            last_err = None;
                            start_idx = idx;
                            break;
                        }
                        EmptyRetry::Exhausted { attempts } => {
                            tracing::warn!(
                                attempt = attempts,
                                max_attempts = cfg.empty_response_max_attempts,
                                style = style.as_str(),
                                pool_index = idx,
                                exhausted = true,
                                "antigravity: empty stream attempts exhausted"
                            );
                            let err = LlmError::Provider {
                                message: format!(
                                    "Cloud Code returned an empty response after {attempts} attempts"
                                ),
                            };
                            if idx + 1 < styles.len() {
                                last_err = Some(err);
                                start_idx = idx + 1;
                                continue;
                            }
                            return Err(err);
                        }
                    }
                }
                Ok(DispatchOutcome::TryFallbackPool) => {
                    start_idx = idx + 1;
                    last_err = Some(LlmError::Provider {
                        message: format!("pool `{}` exhausted", style.as_str()),
                    });
                }
                Err(err) => {
                    start_idx = idx + 1;
                    last_err = Some(err);
                }
            }
        }

        if let Some(err) = last_err {
            return Err(err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AntigravityTokenExchange;
    use crate::config::AntigravityConfig;
    use crate::constants::AntigravityEndpoints;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::Response;
    use axum::routing::post;
    use oc_llm::{Catalog, LlmClient, LlmMessage};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    #[test]
    fn empty_retry_backoff_and_pool_counters_are_independent() {
        let cfg = AntigravityConfig {
            empty_response_max_attempts: 2,
            empty_response_retry_delay_ms: 1000,
            empty_response_max_delay_ms: 20_000,
            ..AntigravityConfig::default()
        };
        // Two header styles/pools each get their own attempt counter.
        let mut counters = [0u32, 0u32];
        assert_eq!(
            record_empty_attempt(&cfg, &mut counters[0]),
            EmptyRetry::Backoff { backoff_ms: 1000 }
        );
        assert_eq!(
            record_empty_attempt(&cfg, &mut counters[1]),
            EmptyRetry::Backoff { backoff_ms: 1000 }
        );
        // Exhausting the primary pool does not consume the fallback's budget.
        assert_eq!(
            record_empty_attempt(&cfg, &mut counters[0]),
            EmptyRetry::Exhausted { attempts: 2 }
        );
        assert_eq!(
            record_empty_attempt(&cfg, &mut counters[1]),
            EmptyRetry::Exhausted { attempts: 2 }
        );

        // Default config produces an exponential (not fixed) delay sequence.
        let defaults = AntigravityConfig::default();
        let delays: Vec<u64> = (0..4)
            .map(|n| defaults.empty_response_backoff_ms(n))
            .collect();
        assert_eq!(delays, vec![2000, 4000, 8000, 16_000]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn gateway_streams_and_fails_over_on_429() {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/v1internal:streamGenerateContent",
                post({
                    let hits = hits.clone();
                    move || {
                        let hits = hits.clone();
                        async move {
                            let n = hits.fetch_add(1, Ordering::SeqCst);
                            if n == 0 {
                                return Response::builder()
                                    .status(StatusCode::TOO_MANY_REQUESTS)
                                    .body(axum::body::Body::from(
                                        r#"{"error":{"message":"RATE_LIMIT_EXCEEDED","details":[{"retryDelay":"5s"}]}}"#,
                                    ))
                                    .expect("resp");
                            }
                            let sse = concat!(
                                "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Antigravity OK\"}]},\"finishReason\":\"STOP\"}]}}\n\n",
                                "data: [DONE]\n\n"
                            );
                            Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "text/event-stream")
                                .body(axum::body::Body::from(sse))
                                .expect("sse")
                        }
                    }
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoints = AntigravityEndpoints::mock(&format!("http://127.0.0.1:{port}"));
        let svc =
            AntigravityService::in_memory_for_test(None, AntigravityConfig::default(), endpoints)
                .await
                .expect("svc");

        let future_exp = now_ms() + 3_600_000;
        for (idx, email) in ["one@ex.com", "two@ex.com"].into_iter().enumerate() {
            svc.upsert_exchange_result(
                AntigravityTokenExchange {
                    refresh: format!("1//tok_{idx}|proj_{idx}"),
                    refresh_token: format!("1//tok_{idx}"),
                    access: format!("ya29.tok_{idx}"),
                    expires: future_exp,
                    email: Some(email.to_owned()),
                    project_id: format!("proj_{idx}"),
                    explicit_project_id: Some(format!("proj_{idx}")),
                    managed_project_id: None,
                },
                None,
            )
            .await
            .expect("add account");
        }

        let mut catalog = Catalog::from_snapshot("{}").expect("empty");
        svc.overlay_catalog(&mut catalog);
        let resolved = catalog
            .resolve("google/antigravity-gemini-3.1-pro#low")
            .expect("resolve");

        let client = LlmClient::new().expect("client");
        client.set_hook(Some(Arc::new(svc)));

        let mut req = LlmRequest::new(resolved);
        req.messages = vec![LlmMessage::user("hi")];
        let mut stream = client.stream(req).await.expect("stream");
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            if let LlmEvent::TextDelta { delta } = ev {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, "Antigravity OK");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn shared_client_hook_resolves_gemini4_and_dynamic_models() {
        let svc = AntigravityService::in_memory_for_test(
            None,
            AntigravityConfig::default(),
            crate::AntigravityEndpoints::default(),
        )
        .await
        .expect("svc");

        let client = oc_llm::SharedLlmClient::new(LlmClient::new().expect("client"));
        let hook: Arc<dyn LlmTransportHook> = Arc::new(svc);
        client.set_hook(Some(hook));

        let empty = Catalog::from_snapshot("{}").expect("empty");
        let hydrogen = client
            .resolve_model(&empty, "google/antigravity-gemini-4-hydrogen#xhigh")
            .expect("hydrogen resolves via hook");
        assert_eq!(hydrogen.model.id, "antigravity-gemini-4-hydrogen");
        assert_eq!(
            hydrogen.variant.as_ref().map(|v| v.id.as_str()),
            Some("xhigh")
        );
        assert!(
            client
                .dynamic_models()
                .iter()
                .any(|m| m.id == "antigravity-gemini-4-hydrogen")
        );
    }

    /// Serve `app` on an ephemeral port and point mock endpoints at it.
    async fn mock_endpoints(app: Router) -> AntigravityEndpoints {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        AntigravityEndpoints::mock(&format!("http://127.0.0.1:{port}"))
    }

    /// Seed the two accounts both in-band tests rotate between.
    async fn add_two_accounts(svc: &AntigravityService) {
        let future_exp = now_ms() + 3_600_000;
        for (idx, email) in ["one@ex.com", "two@ex.com"].into_iter().enumerate() {
            svc.upsert_exchange_result(
                AntigravityTokenExchange {
                    refresh: format!("1//tok_{idx}|proj_{idx}"),
                    refresh_token: format!("1//tok_{idx}"),
                    access: format!("ya29.tok_{idx}"),
                    expires: future_exp,
                    email: Some(email.to_owned()),
                    project_id: format!("proj_{idx}"),
                    explicit_project_id: Some(format!("proj_{idx}")),
                    managed_project_id: None,
                },
                None,
            )
            .await
            .expect("add account");
        }
    }

    /// Resolve the shared test model against a catalog overlaid by `svc`.
    fn resolve_pro(svc: &AntigravityService) -> oc_llm::ResolvedModel {
        let mut catalog = Catalog::from_snapshot("{}").expect("empty");
        svc.overlay_catalog(&mut catalog);
        catalog
            .resolve("google/antigravity-gemini-3.1-pro#low")
            .expect("resolve")
    }

    /// An in-band SSE error frame on an HTTP 200 stream must attribute the failure to the
    /// serving account and rotate to the next one instead of killing the turn.
    #[tokio::test(flavor = "multi_thread")]
    async fn gateway_rotates_on_in_band_quota_error() {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1internal:streamGenerateContent",
            post({
                let hits = hits.clone();
                move || {
                    let hits = hits.clone();
                    async move {
                        let n = hits.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            // HTTP 200 carrying an in-band quota frame, then EOF.
                            return Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "text/event-stream")
                                .body(axum::body::Body::from(
                                    "data: {\"error\":{\"message\":\"Resource has been exhausted (e.g. check quota).\"}}\n\n",
                                ))
                                .expect("resp");
                        }
                        let sse = concat!(
                            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Antigravity OK\"}]},\"finishReason\":\"STOP\"}]}}\n\n",
                            "data: [DONE]\n\n"
                        );
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .body(axum::body::Body::from(sse))
                            .expect("sse")
                    }
                }
            }),
        );

        let endpoints = mock_endpoints(app).await;
        let svc =
            AntigravityService::in_memory_for_test(None, AntigravityConfig::default(), endpoints)
                .await
                .expect("svc");
        add_two_accounts(&svc).await;

        let resolved = resolve_pro(&svc);

        let client = LlmClient::new().expect("client");
        client.set_hook(Some(Arc::new(svc.clone())));

        let mut req = LlmRequest::new(resolved);
        req.messages = vec![LlmMessage::user("hi")];
        let mut stream = client
            .stream(req)
            .await
            .expect("stream rotates to account 2");
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            if let LlmEvent::TextDelta { delta } = ev {
                text.push_str(&delta);
            }
        }

        assert_eq!(text, "Antigravity OK");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "second account must be tried"
        );

        let status = svc.list_accounts_status().expect("status");
        // `select_for_request` starts at the Gemini sticky cursor, so the account the mock
        // exhausted is not necessarily index 0: identify it by its cooldown, not by position.
        let cooling: Vec<&_> = status
            .iter()
            .filter(|acct| !acct.rate_limits.is_empty())
            .collect();
        assert_eq!(
            cooling.len(),
            1,
            "exactly one account must carry the in-band cooldown, got {status:#?}"
        );
        assert!(
            cooling[0]
                .rate_limits
                .keys()
                .any(|k| k.starts_with("gemini-antigravity")),
            "cooldown must be recorded under the Gemini family key: {:?}",
            cooling[0].rate_limits
        );

        let served: Vec<&_> = status
            .iter()
            .filter(|acct| acct.rate_limits.is_empty())
            .collect();
        assert_eq!(served.len(), 1, "one account must be left untouched");
        assert!(
            served[0].last_used > 0,
            "the untouched account must be the one that served the request"
        );
    }

    /// When every account is exhausted in-band the surfaced error must be a pool-level
    /// cooling-down message naming a reset time, never the raw provider sentence.
    #[tokio::test(flavor = "multi_thread")]
    async fn all_accounts_cooling_down_is_reported() {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1internal:streamGenerateContent",
            post({
                let hits = hits.clone();
                move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .body(axum::body::Body::from(
                                "data: {\"error\":{\"message\":\"Resource has been exhausted (e.g. check quota).\"}}\n\n",
                            ))
                            .expect("resp")
                    }
                }
            }),
        );

        let endpoints = mock_endpoints(app).await;
        let cfg = AntigravityConfig {
            // No per-turn wait budget: the pool must report instead of sleeping.
            max_rate_limit_wait_seconds: 0,
            ..AntigravityConfig::default()
        };
        let svc = AntigravityService::in_memory_for_test(None, cfg, endpoints)
            .await
            .expect("svc");
        add_two_accounts(&svc).await;

        let resolved = resolve_pro(&svc);

        let client = LlmClient::new().expect("client");
        client.set_hook(Some(Arc::new(svc)));

        let mut req = LlmRequest::new(resolved);
        req.messages = vec![LlmMessage::user("hi")];
        let err = client
            .stream(req)
            .await
            .err()
            .expect("both accounts exhausted must error");
        let message = err.to_string();

        assert!(
            message.contains("cooling down"),
            "expected a cooling-down message, got: {message}"
        );
        assert!(
            message.contains("until"),
            "expected a reset time in the message, got: {message}"
        );
        assert!(
            !message.contains("Resource has been exhausted"),
            "raw provider text must not surface: {message}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2, "each account tried once");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn gateway_rotates_on_empty_response_and_forwards_cache_key_as_session_id() {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen_sessions = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
        let app = Router::new().route(
            "/v1internal:streamGenerateContent",
            post({
                let hits = hits.clone();
                let seen_sessions = seen_sessions.clone();
                move |body: axum::body::Bytes| {
                    let hits = hits.clone();
                    let seen_sessions = seen_sessions.clone();
                    async move {
                        let sid = serde_json::from_slice::<serde_json::Value>(&body)
                            .ok()
                            .and_then(|v| {
                                v.pointer("/request/sessionId")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_owned)
                            });
                        seen_sessions.lock().expect("lock").push(sid);
                        let n = hits.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            return Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "text/event-stream")
                                .body(axum::body::Body::from(
                                    "data: {\"response\":{\"candidates\":[]}}\n\ndata: [DONE]\n\n",
                                ))
                                .expect("resp");
                        }
                        let sse = concat!(
                            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Second OK\"}]},\"finishReason\":\"STOP\"}]}}\n\n",
                            "data: [DONE]\n\n"
                        );
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .body(axum::body::Body::from(sse))
                            .expect("sse")
                    }
                }
            }),
        );

        let endpoints = mock_endpoints(app).await;
        let cfg = AntigravityConfig {
            empty_response_retry_delay_ms: 10,
            empty_response_max_delay_ms: 10,
            ..AntigravityConfig::default()
        };
        let svc = AntigravityService::in_memory_for_test(None, cfg, endpoints)
            .await
            .expect("svc");
        add_two_accounts(&svc).await;
        let resolved = resolve_pro(&svc);

        let client = LlmClient::new().expect("client");
        client.set_hook(Some(Arc::new(svc)));

        let mut req = LlmRequest::new(resolved);
        req.options.cache_key = Some("ses_real_session_123".to_owned());
        req.messages = vec![LlmMessage::user("hi")];
        let mut stream = client
            .stream(req)
            .await
            .expect("second account succeeds without exhausting");
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            if let LlmEvent::TextDelta { delta } = ev {
                text.push_str(&delta);
            }
        }

        assert_eq!(text, "Second OK");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let sessions = seen_sessions.lock().expect("lock");
        assert_eq!(
            sessions.as_slice(),
            &[
                Some("ses_real_session_123".to_owned()),
                Some("ses_real_session_123".to_owned())
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn gateway_advances_to_fallback_pool_when_primary_exhausts_empty_responses() {
        let user_agents = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let app = Router::new().route(
            "/v1internal:streamGenerateContent",
            post({
                let user_agents = user_agents.clone();
                move |headers: axum::http::HeaderMap| {
                    let user_agents = user_agents.clone();
                    async move {
                        let ua = headers
                            .get("user-agent")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        let is_antigravity = ua.contains("antigravity");
                        user_agents.lock().expect("lock").push(ua);
                        if is_antigravity {
                            return Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "text/event-stream")
                                .body(axum::body::Body::from(
                                    "data: {\"response\":{\"candidates\":[]}}\n\ndata: [DONE]\n\n",
                                ))
                                .expect("empty resp");
                        }
                        let sse = concat!(
                            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"GeminiCli fallback OK\"}]},\"finishReason\":\"STOP\"}]}}\n\n",
                            "data: [DONE]\n\n"
                        );
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .body(axum::body::Body::from(sse))
                            .expect("fallback sse")
                    }
                }
            }),
        );

        let endpoints = mock_endpoints(app).await;
        let cfg = AntigravityConfig {
            cli_first: false,
            quota_fallback: true,
            empty_response_max_attempts: 2,
            empty_response_retry_delay_ms: 5,
            empty_response_max_delay_ms: 5,
            ..AntigravityConfig::default()
        };
        let svc = AntigravityService::in_memory_for_test(None, cfg, endpoints)
            .await
            .expect("svc");
        let future_exp = now_ms() + 3_600_000;
        svc.upsert_exchange_result(
            AntigravityTokenExchange {
                refresh: "1//tok_single|proj_single".to_owned(),
                refresh_token: "1//tok_single".to_owned(),
                access: "ya29.tok_single".to_owned(),
                expires: future_exp,
                email: Some("single@ex.com".to_owned()),
                project_id: "proj_single".to_owned(),
                explicit_project_id: Some("proj_single".to_owned()),
                managed_project_id: None,
            },
            None,
        )
        .await
        .expect("add single account");

        let mut catalog = Catalog::from_snapshot("{}").expect("empty");
        svc.overlay_catalog(&mut catalog);
        let hydrogen = catalog
            .resolve("google/antigravity-gemini-4-hydrogen#high")
            .expect("resolve hydrogen");

        let client = LlmClient::new().expect("client");
        client.set_hook(Some(Arc::new(svc)));

        let mut req = LlmRequest::new(hydrogen);
        req.messages = vec![LlmMessage::user("hi")];
        let mut stream = client
            .stream(req)
            .await
            .expect("gemini-cli fallback pool succeeds after antigravity pool empty exhaustion");
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            if let LlmEvent::TextDelta { delta } = ev {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, "GeminiCli fallback OK");
        let uas = user_agents.lock().expect("lock");
        assert!(uas.iter().any(|u| u.contains("antigravity")));
        assert!(uas.iter().any(|u| !u.contains("antigravity")));
    }
}
