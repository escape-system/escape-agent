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

    let max_empty_attempts = cfg.empty_response_max_attempts.max(1);
    let mut empty_attempts: u32 = 0;
    let mut strip_thinking = false;
    let mut thinking_retried = false;

    loop {
        let mut last_err: Option<LlmError> = None;
        for (idx, &style) in styles.iter().enumerate() {
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
                        break;
                    }
                    last_err = Some(LlmError::Provider {
                        message: "thinking block ordering error persisted after recovery"
                            .to_owned(),
                    });
                }
                Ok(DispatchOutcome::RetryEmptyResponse) => {
                    empty_attempts += 1;
                    if empty_attempts < max_empty_attempts {
                        tokio::time::sleep(Duration::from_millis(
                            cfg.empty_response_retry_delay_ms,
                        ))
                        .await;
                        last_err = None;
                        break;
                    }
                    return Err(LlmError::Provider {
                        message: format!(
                            "Cloud Code returned an empty response after {empty_attempts} attempts"
                        ),
                    });
                }
                Ok(DispatchOutcome::TryFallbackPool) => {
                    last_err = Some(LlmError::Provider {
                        message: format!("pool `{}` exhausted", style.as_str()),
                    });
                }
                Err(err) => {
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
}
