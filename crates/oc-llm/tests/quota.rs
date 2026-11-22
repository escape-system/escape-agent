//! Offline tests for HTTP rate-limit header capture and provider quota probing.

use axum::Router;
use axum::http::header::HeaderName;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use oc_llm::{
    Catalog, LlmClient, LlmEvent, LlmMessage, LlmRequest, ProviderQuotaTarget, QuotaCapability,
    extract_rate_limit, fetch_provider_quota, parse_reset_timestamp, provider_quota_capability,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

async fn spawn_mock_server(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[test]
fn parses_reset_durations_and_rfc3339() {
    let now = 1_700_000_000_000_i64;
    assert_eq!(parse_reset_timestamp("1m30s", now), Some(now + 90_000));
    assert_eq!(parse_reset_timestamp("250ms", now), Some(now + 250));
    assert_eq!(
        parse_reset_timestamp("2026-11-14T00:00:00Z", now),
        Some(1_794_614_400_000)
    );
    assert_eq!(
        parse_reset_timestamp("2026-11-14T03:00:00+03:00", now),
        Some(1_794_614_400_000)
    );
}

#[test]
fn extracts_openai_and_anthropic_rate_limit_headers() {
    let mut oa_headers = HeaderMap::new();
    oa_headers.insert(
        HeaderName::from_static("x-ratelimit-limit-requests"),
        HeaderValue::from_static("500"),
    );
    oa_headers.insert(
        HeaderName::from_static("x-ratelimit-remaining-requests"),
        HeaderValue::from_static("498"),
    );
    oa_headers.insert(
        HeaderName::from_static("x-ratelimit-reset-requests"),
        HeaderValue::from_static("12s"),
    );
    let oa = extract_rate_limit("openai", "gpt-5", 200, &oa_headers, 1_000).unwrap();
    assert_eq!(oa.requests_limit, Some(500));
    assert_eq!(oa.requests_remaining, Some(498));
    assert_eq!(oa.requests_reset_at_ms, Some(13_000));

    let mut ant_headers = HeaderMap::new();
    ant_headers.insert(
        HeaderName::from_static("anthropic-ratelimit-tokens-limit"),
        HeaderValue::from_static("40000"),
    );
    ant_headers.insert(
        HeaderName::from_static("anthropic-ratelimit-tokens-remaining"),
        HeaderValue::from_static("32100"),
    );
    ant_headers.insert(
        HeaderName::from_static("anthropic-ratelimit-tokens-reset"),
        HeaderValue::from_static("2026-11-14T00:00:00Z"),
    );
    let ant = extract_rate_limit("anthropic", "claude-sonnet-4", 200, &ant_headers, 1_000).unwrap();
    assert_eq!(ant.tokens_limit, Some(40_000));
    assert_eq!(ant.tokens_remaining, Some(32_100));
    assert_eq!(ant.tokens_reset_at_ms, Some(1_794_614_400_000));
}

#[tokio::test]
async fn llm_client_captures_rate_limit_headers_from_stream_responses() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("x-ratelimit-limit-requests", "100")
                .header("x-ratelimit-remaining-requests", "99")
                .header("x-ratelimit-limit-tokens", "200000")
                .header("x-ratelimit-remaining-tokens", "199500")
                .body(body.to_owned())
                .unwrap()
                .into_response()
        }),
    );
    let base = spawn_mock_server(app).await;
    let catalog_json = serde_json::to_string(&json!({
        "opencode-go": {
            "id": "opencode-go",
            "name": "OpenCode Go",
            "env": ["OPENCODE_API_KEY"],
            "models": {
                "glm-5": {
                    "id": "glm-5",
                    "type": "chat",
                    "name": "GLM-5",
                    "limit": { "context": 131_072, "output": 8192 },
                    "api": {
                        "chat-completions": {
                            "base_url": format!("{base}/v1"),
                            "sdk": { "opencodeai": "@opencode/ai/providers/openai-compatible" }
                        }
                    }
                }
            }
        }
    }))
    .unwrap();
    let catalog = Catalog::from_snapshot(&catalog_json).unwrap();
    let model = catalog.resolve("opencode-go/glm-5").unwrap();

    let client = LlmClient::new().unwrap();
    let mut req = LlmRequest::new(model);
    req.messages = vec![LlmMessage::user("hello")];
    req.options.api_key = Some("test-key".to_owned());
    req.options.max_output_tokens = Some(16);
    let mut stream = client.stream(req).await.unwrap();
    while let Some(ev) = stream.next().await {
        if matches!(ev, LlmEvent::Finish { .. }) {
            break;
        }
    }

    let limits = client.rate_limits();
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].provider_id, "opencode-go");
    assert_eq!(limits[0].model_id, "glm-5");
    assert_eq!(limits[0].requests_limit, Some(100));
    assert_eq!(limits[0].requests_remaining, Some(99));
    assert_eq!(limits[0].tokens_limit, Some(200_000));
    assert_eq!(limits[0].tokens_remaining, Some(199_500));
}

#[allow(clippy::too_many_lines)]
fn mock_quota_router() -> Router {
    Router::new()
        .route(
            "/openrouter/api/v1/key",
            get(|| async {
                axum::Json(json!({
                    "data": {
                        "label": "sk-or-v1-dev",
                        "usage": 2.5,
                        "usage_daily": 0.4,
                        "usage_weekly": 1.5,
                        "usage_monthly": 2.5,
                        "limit": 10.0,
                        "limit_remaining": 7.5,
                        "limit_reset": "monthly",
                        "is_free_tier": false,
                        "rate_limit": { "requests": 20, "interval": "10s" }
                    }
                }))
            }),
        )
        .route(
            "/openrouter/api/v1/credits",
            get(|| async {
                axum::Json(json!({
                    "data": {
                        "total_credits": 50.0,
                        "total_usage": 12.5
                    }
                }))
            }),
        )
        .route(
            "/deepseek/user/balance",
            get(|| async {
                axum::Json(json!({
                    "is_available": true,
                    "balance_infos": [
                        {
                            "currency": "USD",
                            "total_balance": "14.25",
                            "granted_balance": "4.25",
                            "topped_up_balance": "10.00"
                        }
                    ]
                }))
            }),
        )
        .route(
            "/moonshot/v1/users/me/balance",
            get(|| async {
                axum::Json(json!({
                    "data": {
                        "available_balance": 32.5,
                        "cash_balance": 20.0,
                        "voucher_balance": 12.5
                    }
                }))
            }),
        )
        .route(
            "/siliconflow/v1/user/info",
            get(|| async {
                axum::Json(json!({
                    "data": {
                        "name": "sf-user",
                        "status": "normal",
                        "balance": "8.5",
                        "chargeBalance": "5.0",
                        "totalBalance": "13.5"
                    }
                }))
            }),
        )
        .route(
            "/api/monitor/usage/quota/limit",
            get(|| async {
                axum::Json(json!({
                    "code": 200,
                    "success": true,
                    "data": {
                        "limits": [
                            {
                                "type": "TOKENS_LIMIT",
                                "unit": 3,
                                "number": 5,
                                "usage": 40_000_000,
                                "currentValue": 10_000_000,
                                "remaining": 30_000_000,
                                "percentage": 25,
                                "nextResetTime": 1_794_618_000_000_i64
                            }
                        ]
                    }
                }))
            }),
        )
        .route(
            "/v1/token_plan/remains",
            get(|| async {
                axum::Json(json!({
                    "base_resp": { "status_code": 0, "status_msg": "success" },
                    "data": {
                        "current_interval_total_count": 5_000_000,
                        "current_interval_usage_count": 3_500_000,
                        "current_interval_reset_time": "2026-11-14T03:00:00Z"
                    }
                }))
            }),
        )
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn probes_live_provider_quotas_and_balances_cleanly() {
    let base = spawn_mock_server(mock_quota_router()).await;
    let http = reqwest::Client::new();

    // OpenRouter with management key
    let or = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "openrouter".to_owned(),
            api_key: "sk-or-test".to_owned(),
            management_key: Some("sk-or-mgmt".to_owned()),
            base_url: Some(format!("{base}/openrouter/api/v1")),
        },
    )
    .await;
    assert_eq!(or.status, "ok");
    assert_eq!(or.key_label.as_deref(), Some("sk-or-v1-dev"));
    assert_eq!(or.rate_limit_rule.as_deref(), Some("20 req / 10s"));
    assert_eq!(or.windows.len(), 2);
    assert_eq!(or.windows[0].id, "account_credits");
    assert_eq!(or.windows[0].remaining, Some(37.5));
    assert_eq!(or.windows[1].id, "key_limit");
    assert_eq!(or.windows[1].remaining, Some(7.5));
    let or_json: Value = serde_json::to_value(&or).unwrap();
    assert!(or_json.get("error").is_none(), "no null error field");

    // DeepSeek
    let ds = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "deepseek".to_owned(),
            api_key: "sk-ds".to_owned(),
            management_key: None,
            base_url: Some(format!("{base}/deepseek")),
        },
    )
    .await;
    assert_eq!(ds.status, "ok");
    assert_eq!(ds.balances.len(), 1);
    assert_eq!(ds.balances[0].remaining, Some(14.25));
    assert_eq!(ds.balances[0].currency, "USD");

    // Moonshot
    let ms = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "moonshotai".to_owned(),
            api_key: "sk-ms".to_owned(),
            management_key: None,
            base_url: Some(format!("{base}/moonshot/v1")),
        },
    )
    .await;
    assert_eq!(ms.status, "ok");
    assert_eq!(ms.balances[0].remaining, Some(32.5));

    // SiliconFlow
    let sf = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "siliconflow".to_owned(),
            api_key: "sk-sf".to_owned(),
            management_key: None,
            base_url: Some(format!("{base}/siliconflow/v1")),
        },
    )
    .await;
    assert_eq!(sf.status, "ok");
    assert_eq!(sf.balances[0].remaining, Some(13.5));

    // Z.AI Coding Plan
    let zai = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "zai-coding-plan".to_owned(),
            api_key: "sk-zai".to_owned(),
            management_key: None,
            base_url: Some(format!("{base}/api/coding/paas/v4")),
        },
    )
    .await;
    assert_eq!(zai.status, "ok");
    assert_eq!(zai.windows.len(), 1);
    assert_eq!(zai.windows[0].label, "5-Hour Token Quota");
    assert_eq!(zai.windows[0].used_percent, Some(25.0));
    assert_eq!(zai.windows[0].remaining_percent, Some(75.0));

    // MiniMax Token Plan
    let mm = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "minimax-coding-plan".to_owned(),
            api_key: "sk-mm".to_owned(),
            management_key: None,
            base_url: Some(format!("{base}/anthropic/v1")),
        },
    )
    .await;
    assert_eq!(mm.status, "ok");
    assert_eq!(mm.windows.len(), 1);
    assert_eq!(mm.windows[0].remaining, Some(3_500_000.0));
    assert_eq!(mm.windows[0].used_percent, Some(30.0));

    // OpenCode Go is now a live-API provider; the probe itself is covered by
    // the dedicated `opencode_go_*` tests below.
    let (cap, note) = provider_quota_capability("opencode-go");
    assert_eq!(cap, QuotaCapability::LiveApi);
    assert!(note.is_some());
}

#[test]
fn capability_table_matches_real_provider_ids() {
    let (google, note) = provider_quota_capability("google");
    assert_eq!(google, QuotaCapability::RateLimitHeadersOnly);
    assert!(note.is_some_and(|text| text.contains("rate-limit headers")));

    // `google-antigravity` is not a provider id; only `google` exists and its
    // Antigravity quota is aggregated by oc-server, so the probe table must not
    // claim a live quota API for the phantom id.
    let (phantom, _) = provider_quota_capability("google-antigravity");
    assert_eq!(phantom, QuotaCapability::RateLimitHeadersOnly);

    let (deepseek, note) = provider_quota_capability("deepseek");
    assert_eq!(deepseek, QuotaCapability::LiveApi);
    assert!(note.is_none());

    let (nous, note) = provider_quota_capability("nous");
    assert_eq!(nous, QuotaCapability::RateLimitHeadersOnly);
    assert!(note.is_some_and(|text| text.contains("portal.nousresearch.com")));
}

#[tokio::test]
async fn numeric_reset_times_render_as_epoch_millis_not_1970() {
    let app = Router::new()
        .route(
            "/api/monitor/usage/quota/limit",
            get(|| async {
                axum::Json(json!({
                    "success": true,
                    "data": {
                        "limits": [{
                            "type": "TOKENS_LIMIT",
                            "unit": 3,
                            "number": 5,
                            "usage": 100.0,
                            "currentValue": 10.0,
                            "remaining": 90.0,
                            "percentage": 10
                            ,"nextResetTime": 1_794_618_000_i64
                        }]
                    }
                }))
            }),
        )
        .route(
            "/v1/token_plan/remains",
            get(|| async {
                axum::Json(json!({
                    "base_resp": { "status_code": 0, "status_msg": "success" },
                    "data": {
                        "current_interval_total_count": 100,
                        "current_interval_usage_count": 40,
                        "current_interval_reset_time": 1_794_618_000_i64
                    }
                }))
            }),
        );
    let base = spawn_mock_server(app).await;
    let http = reqwest::Client::new();

    let zai = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "zai".to_owned(),
            api_key: "sk-zai".to_owned(),
            management_key: None,
            base_url: Some(format!("{base}/gateway")),
        },
    )
    .await;
    assert_eq!(zai.status, "ok");
    assert_eq!(zai.windows[0].reset_at_ms, Some(1_794_618_000_000));

    let mm = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "minimax".to_owned(),
            api_key: "sk-mm".to_owned(),
            management_key: None,
            base_url: Some(format!("{base}/gateway")),
        },
    )
    .await;
    assert_eq!(mm.status, "ok");
    assert_eq!(mm.windows[0].reset_at_ms, Some(1_794_618_000_000));
}

#[tokio::test]
async fn unusable_probe_bodies_report_partial_without_inventing_values() {
    let app = Router::new()
        .route(
            "/empty/user/balance",
            get(|| async { axum::Json(json!({ "unexpected": true })) }),
        )
        .route(
            "/empty/users/me/balance",
            get(|| async { axum::Json(json!({ "data": {} })) }),
        )
        .route(
            "/empty/user/info",
            get(|| async { axum::Json(json!({ "unexpected": true })) }),
        );
    let base = spawn_mock_server(app).await;
    let http = reqwest::Client::new();

    let cases = [
        ("deepseek", "DeepSeek"),
        ("moonshotai", "Moonshot"),
        ("siliconflow", "SiliconFlow"),
    ];
    for (provider_id, provider_name) in cases {
        let result = fetch_provider_quota(
            &http,
            &ProviderQuotaTarget {
                provider_id: provider_id.to_owned(),
                api_key: "sk-test".to_owned(),
                management_key: None,
                base_url: Some(format!("{base}/empty")),
            },
        )
        .await;
        assert_eq!(result.status, "partial", "{provider_id}");
        assert!(result.windows.is_empty(), "{provider_id}");
        assert!(result.balances.is_empty(), "{provider_id}");
        assert!(result.error.is_none(), "{provider_id}");
        assert_eq!(result.warnings.len(), 1, "{provider_id}");
        assert_eq!(
            result.warnings[0],
            format!("{provider_name} quota response contained no usable fields"),
            "{provider_id}"
        );
    }
}

#[tokio::test]
async fn plan_probes_report_partial_on_bodies_without_windows() {
    let app = Router::new()
        .route(
            "/api/monitor/usage/quota/limit",
            get(|| async { axum::Json(json!({ "success": true, "data": {} })) }),
        )
        .route(
            "/v1/token_plan/remains",
            get(|| async { axum::Json(json!({ "base_resp": { "status_code": 0 } })) }),
        )
        .route(
            "/empty-or/key",
            get(|| async { axum::Json(json!({ "data": { "label": "no-quota" } })) }),
        );
    let base = spawn_mock_server(app).await;
    let http = reqwest::Client::new();

    // Z.AI, MiniMax, and OpenRouter share the origin-based target shape below;
    // each must report `partial` rather than a fabricated `ok`.
    let cases = [
        ("zai", "Z.AI", format!("{base}/gateway")),
        ("minimax", "MiniMax", format!("{base}/gateway")),
        ("openrouter", "OpenRouter", format!("{base}/empty-or")),
    ];
    for (provider_id, provider_name, base_url) in cases {
        let result = fetch_provider_quota(
            &http,
            &ProviderQuotaTarget {
                provider_id: provider_id.to_owned(),
                api_key: "sk-test".to_owned(),
                management_key: None,
                base_url: Some(base_url),
            },
        )
        .await;
        assert_eq!(result.status, "partial", "{provider_id}");
        assert!(result.windows.is_empty(), "{provider_id}");
        assert!(result.error.is_none(), "{provider_id}");
        assert_eq!(
            result.warnings,
            vec![format!(
                "{provider_name} quota response contained no usable fields"
            )],
            "{provider_id}"
        );
    }
}

#[tokio::test]
async fn probe_errors_redact_secrets_and_name_only_the_endpoint_path() {
    let app = Router::new().route(
        "/leak/user/balance",
        get(|| async {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":"invalid key sk-or-secret-abc123","hint":"Bearer leaked.token"}"#,
            )
        }),
    );
    let base = spawn_mock_server(app).await;
    let http = reqwest::Client::new();

    let result = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "deepseek".to_owned(),
            api_key: "sk-test".to_owned(),
            management_key: None,
            base_url: Some(format!("{base}/leak")),
        },
    )
    .await;

    assert_eq!(result.status, "error");
    let error = result.error.unwrap();
    assert!(!error.contains("sk-or-secret"), "{error}");
    assert!(!error.contains("leaked.token"), "{error}");
    assert!(error.contains("[redacted]"), "{error}");
    assert!(error.contains("/leak/user/balance"), "{error}");
    let host = base.trim_start_matches("http://");
    assert!(!error.contains(host), "full URL leaked: {error}");
}

#[test]
fn quota_facade_keeps_public_module_paths() {
    // `oc-server` imports these through the `oc_llm::quota::{...}` module path.
    let _client: reqwest::Client = oc_llm::quota::default_http_client();
    assert!(oc_llm::quota::now_epoch_ms() > 1_600_000_000_000);
    let _: fn(&str) -> (QuotaCapability, Option<&'static str>) =
        oc_llm::quota::provider_quota_capability;
    let _: fn(&str, i64) -> Option<i64> = oc_llm::quota::parse_reset_timestamp;
    let _: fn(&str) -> Option<i64> = oc_llm::quota::parse_rfc3339_ms;
    let window = oc_llm::quota::QuotaWindow {
        id: "w".to_owned(),
        label: "W".to_owned(),
        unit: "USD".to_owned(),
        limit: None,
        used: None,
        remaining: None,
        used_percent: None,
        remaining_percent: None,
        limited: None,
        reset_at_ms: None,
        reset_label: None,
    };
    let json = serde_json::to_value(&window).unwrap();
    assert!(json.get("limit").is_none(), "optional fields are omitted");
}

/// The verified `opencode-go` usage body: three percent-only windows.
fn opencode_go_usage_body() -> Value {
    json!({
        "usage": {
            "rolling": {
                "status": "ok",
                "percent": 0,
                "resetsAt": "2026-11-14T17:47:39.000Z"
            },
            "weekly": {
                "status": "ok",
                "percent": 79,
                "resetsAt": "2026-11-16T00:00:00.000Z"
            },
            "monthly": {
                "status": "ok",
                "percent": 46,
                "resetsAt": "2026-12-08T00:59:56.000Z"
            }
        }
    })
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn opencode_go_probe_parses_percent_windows() {
    let app = Router::new().route(
        "/zen/go/v1/usage",
        get(|| async { axum::Json(opencode_go_usage_body()) }),
    );
    let base = spawn_mock_server(app).await;
    let http = reqwest::Client::new();

    let result = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "opencode-go".to_owned(),
            api_key: "oc_sk_test_key".to_owned(),
            management_key: None,
            // The catalog base for `opencode-go`; the probe must reduce it to
            // the origin and append its own path.
            base_url: Some(format!("{base}/inference/go/openai/v1")),
        },
    )
    .await;

    assert_eq!(result.status, "ok");
    assert!(result.error.is_none(), "{:?}", result.error);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert!(result.balances.is_empty());
    assert_eq!(result.windows.len(), 3);

    let ids: Vec<&str> = result.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, ["rolling", "weekly", "monthly"]);
    let labels: Vec<&str> = result.windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(labels, ["5-Hour Rolling", "Weekly", "Monthly"]);

    let used: Vec<Option<f64>> = result.windows.iter().map(|w| w.used_percent).collect();
    assert_eq!(used, [Some(0.0), Some(79.0), Some(46.0)]);
    let remaining: Vec<Option<f64>> = result.windows.iter().map(|w| w.remaining_percent).collect();
    assert_eq!(remaining, [Some(100.0), Some(21.0), Some(54.0)]);

    let resets: Vec<Option<i64>> = result.windows.iter().map(|w| w.reset_at_ms).collect();
    assert_eq!(
        resets,
        [
            Some(1_794_678_459_000),
            Some(1_794_787_200_000),
            Some(1_796_691_596_000),
        ]
    );
    for window in &result.windows {
        assert_eq!(window.unit, "percent");
        assert_eq!(window.limited, Some(false), "{}", window.id);
        assert_eq!(window.limit, Some(100.0));
        assert!(window.reset_label.is_none());
    }

    let json = serde_json::to_value(&result).unwrap();
    assert!(json.get("error").is_none(), "no null error field");
}

#[tokio::test]
async fn opencode_go_probe_reports_partial_on_empty_usage() {
    let app = Router::new().route(
        "/zen/go/v1/usage",
        get(|| async { axum::Json(json!({ "usage": {} })) }),
    );
    let base = spawn_mock_server(app).await;
    let http = reqwest::Client::new();

    let result = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "opencode-go".to_owned(),
            api_key: "oc_sk_test_key".to_owned(),
            management_key: None,
            base_url: Some(format!("{base}/inference/go/openai/v1")),
        },
    )
    .await;

    assert_eq!(result.status, "partial");
    assert!(result.windows.is_empty());
    assert!(result.error.is_none());
    assert_eq!(
        result.warnings,
        vec!["OpenCode Go usage response contained no usable fields".to_owned()]
    );
}

#[tokio::test]
async fn opencode_go_probe_redacts_unauthorized_errors() {
    let app = Router::new().route(
        "/zen/go/v1/usage",
        get(|headers: HeaderMap| async move {
            // Echo the presented key back to prove the shared sanitizer strips
            // OpenCode's `oc_sk_` token format from the cached error.
            let auth = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({
                    "type": "error",
                    "error": { "type": "AuthError", "message": "Unauthorized" },
                    "echo": auth,
                    "key": "oc_sk_live_abc123"
                })),
            )
        }),
    );
    let base = spawn_mock_server(app).await;
    let http = reqwest::Client::new();

    let result = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "opencode-go".to_owned(),
            api_key: "oc_sk_secret_key_value".to_owned(),
            management_key: None,
            base_url: Some(format!("{base}/inference/go/openai/v1")),
        },
    )
    .await;

    assert_eq!(result.status, "error");
    assert!(result.windows.is_empty());
    let error = result.error.unwrap();
    assert!(!error.contains("oc_sk_"), "{error}");
    assert!(error.contains("[redacted]"), "{error}");
    assert!(error.contains("/zen/go/v1/usage"), "{error}");
    let host = base.trim_start_matches("http://");
    assert!(!error.contains(host), "full URL leaked: {error}");
}

#[tokio::test]
async fn opencode_go_probe_reduces_custom_base_url_to_origin() {
    // Only the origin-relative route exists: a probe that kept the configured
    // `/whatever` path would 404 instead of returning windows.
    let app = Router::new().route(
        "/zen/go/v1/usage",
        get(|| async { axum::Json(opencode_go_usage_body()) }),
    );
    let base = spawn_mock_server(app).await;
    let http = reqwest::Client::new();

    let result = fetch_provider_quota(
        &http,
        &ProviderQuotaTarget {
            provider_id: "opencode-go".to_owned(),
            api_key: "oc_sk_test_key".to_owned(),
            management_key: None,
            base_url: Some(format!("{base}/whatever")),
        },
    )
    .await;

    assert_eq!(result.status, "ok", "{:?}", result.error);
    assert_eq!(result.windows.len(), 3);
}

#[test]
fn opencode_go_is_live_api_while_opencode_stays_headers_only() {
    let (go, note) = provider_quota_capability("opencode-go");
    assert_eq!(go, QuotaCapability::LiveApi);
    let note = note.expect("opencode-go has a capability note");
    assert!(note.contains("OpenCode Go usage API"), "{note}");
    assert!(note.contains("rolling 5h / weekly / monthly"), "{note}");

    let (zen, note) = provider_quota_capability("opencode");
    assert_eq!(zen, QuotaCapability::RateLimitHeadersOnly);
    assert!(note.is_some());
}
