//! Offline HTTP scenario tests for unified multi-provider quota, balance,
//! rate-limit header telemetry, `OpenRouter` management key, and local usage routes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::routing::{get, post};
use oc_antigravity::{
    ANTIGRAVITY_PROVIDER_ID, AntigravityConfig, AntigravityEndpoints, AntigravityService,
    AntigravityTokenExchange,
};
use oc_config::{ConfigService, LoadOptions};
use oc_llm::{Catalog, LlmClient, ProviderRateLimit, SharedLlmClient};
use oc_server::{AppState, router};
use oc_store::{CredentialPatch, NewCredential, NewMessage, NewSession, Store};
use oc_types::{ServerInfo, ServerPaths, now_millis};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower::ServiceExt;

#[derive(Clone, Default)]
struct MockProviderCounters {
    openrouter_key: Arc<AtomicUsize>,
    openrouter_credits: Arc<AtomicUsize>,
    deepseek_balance: Arc<AtomicUsize>,
    ag_quota: Arc<AtomicUsize>,
    opencode_go_usage: Arc<AtomicUsize>,
}

struct MockServer {
    base_url: String,
    counters: MockProviderCounters,
}

fn mock_openrouter_routes(counters: MockProviderCounters) -> Router {
    let key_ctr = counters.openrouter_key;
    let cred_ctr = counters.openrouter_credits;
    Router::new()
        .route(
            "/api/v1/key",
            get(move |headers: HeaderMap| {
                let ctr = Arc::clone(&key_ctr);
                async move {
                    if headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())
                        != Some("Bearer sk-or-test-123")
                    {
                        return (
                            StatusCode::UNAUTHORIZED,
                            axum::Json(json!({"error": {"message": "bad key"}})),
                        );
                    }
                    ctr.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        axum::Json(json!({
                            "data": {
                                "label": "openrouter-prod",
                                "limit": 100.0,
                                "limit_remaining": 62.5,
                                "usage": 37.5,
                                "usage_daily": 1.5,
                                "usage_weekly": 8.0,
                                "usage_monthly": 24.0,
                                "is_free_tier": false,
                                "rate_limit": { "requests": 200, "interval": "10s" }
                            }
                        })),
                    )
                }
            }),
        )
        .route(
            "/api/v1/credits",
            get(move |headers: HeaderMap| {
                let ctr = Arc::clone(&cred_ctr);
                async move {
                    if headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())
                        != Some("Bearer sk-or-mgmt-secret")
                    {
                        return (
                            StatusCode::UNAUTHORIZED,
                            axum::Json(json!({"error": {"message": "bad management key"}})),
                        );
                    }
                    ctr.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        axum::Json(json!({
                            "data": { "total_credits": 250.0, "total_usage": 80.0 }
                        })),
                    )
                }
            }),
        )
}

fn mock_deepseek_and_ag_routes(counters: MockProviderCounters) -> Router {
    let ds_ctr = counters.deepseek_balance;
    let ag_ctr = counters.ag_quota;
    Router::new()
        .route(
            "/user/balance",
            get(move |headers: HeaderMap| {
                let ctr = Arc::clone(&ds_ctr);
                async move {
                    if headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())
                        != Some("Bearer sk-ds-test-456")
                    {
                        return (
                            StatusCode::UNAUTHORIZED,
                            axum::Json(json!({"error": "bad deepseek key"})),
                        );
                    }
                    ctr.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        axum::Json(json!({
                            "is_available": true,
                            "balance_infos": [{
                                "currency": "USD",
                                "total_balance": "19.50",
                                "granted_balance": "2.00",
                                "topped_up_balance": "17.50"
                            }]
                        })),
                    )
                }
            }),
        )
        .route(
            "/v1internal:fetchAvailableModels",
            post(|| async {
                axum::Json(json!({
                    "models": {
                        "claude-sonnet-4-6": {
                            "displayName": "Claude Sonnet 4.6",
                            "quotaInfo": {
                                "remainingFraction": 0.90,
                                "resetTime": "2030-01-01T00:00:00Z"
                            }
                        }
                    }
                }))
            }),
        )
        .route(
            "/v1internal:retrieveUserQuota",
            post(move || {
                let ctr = Arc::clone(&ag_ctr);
                async move {
                    ctr.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({
                        "buckets": [{
                            "modelId": "gemini-2.5-pro",
                            "remainingFraction": 0.72,
                            "resetTime": "2030-01-01T00:00:00Z"
                        }]
                    }))
                }
            }),
        )
        .route("/version", get(|| async { "1.0.0\n" }))
}

/// Live `GET /zen/go/v1/usage` probe: percent windows with `resetsAt`.
///
/// The response shape is the one the real endpoint returns:
/// `{"usage":{"rolling":{"status":"ok","percent":0,"resetsAt":"…"},…}}`.
fn mock_opencode_go_routes(counters: MockProviderCounters) -> Router {
    let usage_ctr = counters.opencode_go_usage;
    Router::new().route(
        "/zen/go/v1/usage",
        get(move |headers: HeaderMap| {
            let ctr = Arc::clone(&usage_ctr);
            async move {
                if headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())
                    != Some("Bearer oc-go-test-key")
                {
                    return (
                        StatusCode::UNAUTHORIZED,
                        axum::Json(json!({
                            "type": "error",
                            "error": {"type": "AuthError", "message": "Unauthorized"}
                        })),
                    );
                }
                ctr.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    axum::Json(json!({
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
                    })),
                )
            }
        }),
    )
}

async fn start_mock_upstream() -> MockServer {
    let counters = MockProviderCounters::default();
    let app = mock_openrouter_routes(counters.clone())
        .merge(mock_deepseek_and_ag_routes(counters.clone()))
        .merge(mock_opencode_go_routes(counters.clone()));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    MockServer {
        base_url: format!("http://{addr}"),
        counters,
    }
}

struct QuotaHarness {
    _db: tempfile::TempDir,
    _cfg: tempfile::TempDir,
    state: AppState,
    upstream: MockServer,
}

fn build_config_and_catalog(
    cfg_dir: &std::path::Path,
    base_url: &str,
) -> (Arc<ConfigService>, Catalog) {
    let config_json = json!({
        "model": "opencode-go/glm-5",
        "providers": {
            "openrouter": {
                "name": "OpenRouter",
                "options": { "baseURL": format!("{base_url}/api/v1") }
            },
            "deepseek": {
                "name": "DeepSeek",
                "options": { "baseURL": base_url }
            },
            "opencode-go": {
                "name": "OpenCode Go",
                "settings": {
                    "apiKey": "oc-go-test-key",
                    "baseURL": format!("{base_url}/v1")
                }
            }
        }
    });
    std::fs::write(cfg_dir.join("opencode.json"), config_json.to_string()).unwrap();

    let home = cfg_dir.to_string_lossy().into_owned();
    let mut env = HashMap::new();
    env.insert("HOME".to_owned(), home.clone());
    env.insert("USERPROFILE".to_owned(), home);
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        cfg_dir.join("xdg").to_string_lossy().into_owned(),
    );
    env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());

    let config = Arc::new(
        ConfigService::load(LoadOptions {
            directory: cfg_dir.to_path_buf(),
            env,
        })
        .unwrap(),
    );

    let snapshot = catalog_snapshot(base_url);
    let catalog = Catalog::from_snapshot(&snapshot.to_string()).unwrap();
    (config, catalog)
}

/// The models.dev-style provider snapshot the harness installs.
fn catalog_snapshot(base_url: &str) -> Value {
    json!({
        "openrouter": {
            "name": "OpenRouter",
            "models": {
                "anthropic/claude-sonnet-4": {
                    "id": "anthropic/claude-sonnet-4",
                    "api": {
                        "chat-completions": {
                            "base_url": format!("{base_url}/api/v1"),
                            "sdk": {"opencodeai": "@opencode/ai/openrouter"}
                        }
                    }
                }
            }
        },
        "deepseek": {
            "name": "DeepSeek",
            "models": {
                "deepseek-chat": {
                    "id": "deepseek-chat",
                    "api": {
                        "chat-completions": {
                            "base_url": base_url,
                            "sdk": {"opencodeai": "@opencode/ai/deepseek"}
                        }
                    }
                }
            }
        },
        "opencode-go": {
            "name": "OpenCode Go",
            "models": {
                "glm-5": {
                    "id": "glm-5",
                    "api": {
                        "chat-completions": {
                            "base_url": format!("{base_url}/v1"),
                            "sdk": {"opencodeai": "@opencode/ai/openai-compatible"}
                        }
                    }
                }
            }
        },
        // Reached through an environment variable only: no credential row, no
        // `providers` entry. The quota report must still list it.
        "envonly": {
            "name": "Env Only",
            "env": ["PATH"],
            "models": {
                "env-model": {
                    "id": "env-model",
                    "api": {
                        "chat-completions": {
                            "base_url": base_url,
                            "sdk": {"opencodeai": "@opencode/ai/openai-compatible"}
                        }
                    }
                }
            }
        },
        // Known to the catalog, but with no credential, no config key and no
        // environment variable: nothing may surface it in the quota report, not
        // even a captured rate-limit snapshot.
        "uncredentialed": {
            "name": "Uncredentialed",
            "models": {
                "uncred-model": {
                    "id": "uncred-model",
                    "api": {
                        "chat-completions": {
                            "base_url": base_url,
                            "sdk": {"opencodeai": "@opencode/ai/openai-compatible"}
                        }
                    }
                }
            }
        }
    })
}

async fn seed_store_fixtures(store: &Store, directory: &str) {
    store
        .create_credential(NewCredential {
            id: Some("cred_openrouter".to_owned()),
            integration_id: "openrouter".to_owned(),
            label: Some("OpenRouter API Key".to_owned()),
            value: json!({ "type": "key", "key": "sk-or-test-123" }),
            active: Some(true),
        })
        .await
        .unwrap();
    store
        .create_credential(NewCredential {
            id: Some("cred_deepseek".to_owned()),
            integration_id: "deepseek".to_owned(),
            label: Some("DeepSeek API Key".to_owned()),
            value: json!({ "type": "key", "key": "sk-ds-test-456" }),
            active: Some(true),
        })
        .await
        .unwrap();

    let now = now_millis();
    store
        .create_session(NewSession {
            id: "ses_quota_history".to_owned(),
            parent_id: None,
            project_id: "prj_quota".to_owned(),
            directory: directory.to_owned(),
            title: Some("Quota history".to_owned()),
            agent: None,
            model: None,
            metadata: None,
            permissions: None,
            time_created: now - 45 * 86_400_000,
        })
        .await
        .unwrap();

    let usage_points = [
        (
            "msg_q1",
            "glm-5",
            2 * 3_600_000,
            1000_i64,
            500_i64,
            0.25_f64,
        ),
        ("msg_q2", "glm-5", 12 * 3_600_000, 2000, 1000, 0.50),
        ("msg_q3", "kimi-k2.5", 3 * 86_400_000, 3000, 1000, 1.00),
        ("msg_q4", "kimi-k2.5", 20 * 86_400_000, 4000, 2000, 2.00),
        ("msg_q5", "qwen3-coder", 40 * 86_400_000, 5000, 5000, 4.00),
    ];
    for (id, model_id, age_ms, input, output, cost) in usage_points {
        let created = now - age_ms;
        store
            .append_message(NewMessage {
                id: id.to_owned(),
                session_id: "ses_quota_history".to_owned(),
                kind: "assistant".to_owned(),
                data: json!({
                    "model": { "providerID": "opencode-go", "id": model_id },
                    "tokens": { "input": input, "output": output, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                    "cost": cost,
                    "time": { "created": created, "completed": created + 100 }
                }),
                time_created: created,
            })
            .await
            .unwrap();
    }
}

async fn seed_antigravity(store: Arc<Store>, base_url: &str) -> Arc<AntigravityService> {
    let endpoints = AntigravityEndpoints {
        oauth_auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
        oauth_token_url: format!("{base_url}/token"),
        oauth_userinfo_url: format!("{base_url}/userinfo"),
        antigravity_endpoint: base_url.to_owned(),
        gemini_cli_endpoint: base_url.to_owned(),
        manifest_url: format!("{base_url}/version"),
    };
    let antigravity = Arc::new(
        AntigravityService::in_memory_for_test(
            Some(store),
            AntigravityConfig::default(),
            endpoints,
        )
        .await
        .unwrap(),
    );
    antigravity
        .upsert_exchange_result(
            AntigravityTokenExchange {
                refresh: "1//mock-refresh|proj-1|".to_owned(),
                refresh_token: "1//mock-refresh".to_owned(),
                access: "ya29.mock-access".to_owned(),
                expires: now_millis() + 3_600_000,
                email: Some("quota-user@example.com".to_owned()),
                project_id: "proj-1".to_owned(),
                explicit_project_id: None,
                managed_project_id: None,
            },
            Some("Work Pool".to_owned()),
        )
        .await
        .unwrap();
    antigravity
}

/// The seeded pieces a test may mutate **before** `AppState` is built.
///
/// The quota path re-reads credentials and Antigravity accounts on every
/// request, so mutating these handles after `state_from` is equally valid.
struct HarnessParts {
    db: tempfile::TempDir,
    cfg: tempfile::TempDir,
    store: Arc<Store>,
    llm_client: LlmClient,
    antigravity: Arc<AntigravityService>,
    config: Arc<ConfigService>,
    catalog: Catalog,
    upstream: MockServer,
}

async fn harness_parts() -> HarnessParts {
    let upstream = start_mock_upstream().await;
    let db = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(db.path().join("data.sqlite")).await.unwrap());
    let cfg_dir = tempfile::tempdir().unwrap();
    let (config, mut catalog) = build_config_and_catalog(cfg_dir.path(), &upstream.base_url);

    seed_store_fixtures(&store, &cfg_dir.path().to_string_lossy()).await;
    let antigravity = seed_antigravity(Arc::clone(&store), &upstream.base_url).await;
    antigravity.overlay_catalog(&mut catalog);

    let llm_client = LlmClient::new().unwrap();
    llm_client.record_rate_limit(ProviderRateLimit {
        provider_id: "opencode-go".to_owned(),
        model_id: "glm-5".to_owned(),
        observed_at_ms: now_millis(),
        status: 200,
        requests_limit: Some(120),
        requests_remaining: Some(98),
        requests_reset_at_ms: Some(now_millis() + 45_000),
        requests_reset_raw: Some("45s".to_owned()),
        tokens_limit: Some(200_000),
        tokens_remaining: Some(185_500),
        tokens_reset_at_ms: Some(now_millis() + 60_000),
        tokens_reset_raw: Some("60s".to_owned()),
        retry_after_ms: None,
    });

    HarnessParts {
        db,
        cfg: cfg_dir,
        store,
        llm_client,
        antigravity,
        config,
        catalog,
        upstream,
    }
}

fn state_from(parts: &HarnessParts) -> AppState {
    AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 9999,
            hostname: Some("127.0.0.1".to_owned()),
            urls: vec!["http://127.0.0.1:4096".to_owned()],
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        parts.cfg.path().to_path_buf(),
    )
    .with_store(Arc::clone(&parts.store))
    .with_config(Arc::clone(&parts.config))
    .with_catalog(Arc::new(parts.catalog.clone()))
    .with_antigravity(Arc::clone(&parts.antigravity))
    .with_llm(Arc::new(SharedLlmClient::new(parts.llm_client.clone())))
}

async fn setup_harness() -> QuotaHarness {
    let parts = harness_parts().await;
    let state = state_from(&parts);
    QuotaHarness {
        _db: parts.db,
        _cfg: parts.cfg,
        state,
        upstream: parts.upstream,
    }
}

/// Append one assistant usage row for `provider_id` to a fresh session.
async fn seed_usage(store: &Store, session_id: &str, provider_id: &str, age_ms: i64) {
    let now = now_millis();
    let created = now - age_ms;
    let exists = store.get_session(session_id).await.unwrap().is_some();
    if !exists {
        store
            .create_session(NewSession {
                id: session_id.to_owned(),
                parent_id: None,
                project_id: "prj_quota".to_owned(),
                directory: std::env::temp_dir().to_string_lossy().into_owned(),
                title: Some("Quota extra history".to_owned()),
                agent: None,
                model: None,
                metadata: None,
                permissions: None,
                time_created: now - age_ms - 1_000,
            })
            .await
            .unwrap();
    }
    let id = format!("msg_{session_id}_{created}");
    store
        .append_message(NewMessage {
            id,
            session_id: session_id.to_owned(),
            kind: "assistant".to_owned(),
            data: json!({
                "model": { "providerID": provider_id, "id": "extra-model" },
                "tokens": { "input": 10, "output": 5, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                "cost": 0.10,
                "time": { "created": created, "completed": created + 100 }
            }),
            time_created: created,
        })
        .await
        .unwrap();
}

/// Deactivate one credential row, asserting the store applied the patch.
async fn deactivate_credential(store: &Store, id: &str) {
    let patched = store
        .update_credential(
            id,
            CredentialPatch {
                label: None,
                value: None,
                active: Some(false),
            },
        )
        .await
        .unwrap()
        .expect("credential exists");
    assert_eq!(patched["active"], false, "{patched:?}");
}

/// Create a credential row that is then deactivated.
///
/// `create_credential` always activates the first row of an integration, so a
/// deactivation has to be a second, explicit patch.
async fn seed_inactive_credential(store: &Store, id: &str, integration_id: &str, key: &str) {
    store
        .create_credential(NewCredential {
            id: Some(id.to_owned()),
            integration_id: integration_id.to_owned(),
            label: Some("deactivated".to_owned()),
            value: json!({ "type": "key", "key": key }),
            active: Some(true),
        })
        .await
        .unwrap();
    deactivate_credential(store, id).await;
}

fn provider_ids(body: &Value) -> Vec<String> {
    body["data"]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["providerID"].as_str().map(str::to_owned))
        .collect()
}

fn assert_absent(body: &Value, provider_id: &str) {
    let ids = provider_ids(body);
    assert!(
        !ids.iter().any(|id| id == provider_id),
        "{provider_id} must be absent, got {ids:?}"
    );
}

async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Option<Value>) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 21).await.unwrap();
    let value = if bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&bytes).unwrap())
    };
    (status, value)
}

fn assert_no_nulls(value: &Value, path: &str) {
    match value {
        Value::Null => panic!("unexpected JSON null at {path}"),
        Value::Array(items) => {
            for (idx, item) in items.iter().enumerate() {
                assert_no_nulls(item, &format!("{path}[{idx}]"));
            }
        }
        Value::Object(map) => {
            for (key, val) in map {
                assert_no_nulls(val, &format!("{path}.{key}"));
            }
        }
        _ => {}
    }
}

fn find_provider<'a>(body: &'a Value, id: &str) -> &'a Value {
    body["data"]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["providerID"] == id)
        .unwrap_or_else(|| panic!("missing provider {id} in {body}"))
}

async fn verify_openrouter_management_flow(app: &Router, counters: &MockProviderCounters) {
    let (status, _) = call(
        app,
        Method::PUT,
        "/api/quotas/openrouter/management-key",
        Some(json!({ "key": "   " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, put_resp) = call(
        app,
        Method::PUT,
        "/api/quotas/openrouter/management-key",
        Some(json!({ "key": "sk-or-mgmt-secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{put_resp:?}");
    assert_eq!(put_resp.unwrap()["data"]["configured"], true);

    // A PUT invalidates the cached `openrouter` entry, and `GET /api/quotas` is
    // cache-first: the blocking refresh is what re-probes with the new key.
    let (status, after_mgmt) = call(app, Method::POST, "/api/quotas/refresh", None).await;
    assert_eq!(status, StatusCode::OK);
    let after_mgmt = after_mgmt.unwrap();
    assert_no_nulls(&after_mgmt, "$");
    assert!(!after_mgmt.to_string().contains("sk-or-mgmt-secret"));
    assert_eq!(counters.openrouter_credits.load(Ordering::SeqCst), 1);

    let or_with_credits = find_provider(&after_mgmt, "openrouter");
    assert_eq!(or_with_credits["hasManagementKey"], true);
    assert!(
        or_with_credits["windows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == "account_credits" && w["remaining"] == 170.0),
        "expected account_credits window in {or_with_credits:?}"
    );

    let (status, integrations) = call(app, Method::GET, "/api/integration", None).await;
    assert_eq!(status, StatusCode::OK);
    let int_rows = integrations.unwrap()["data"].as_array().unwrap().clone();
    assert!(
        int_rows
            .iter()
            .all(|row| row["id"] != "openrouter-management")
    );

    let (status, _) = call(
        app,
        Method::DELETE,
        "/api/quotas/openrouter/management-key",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, cleared) = call(app, Method::GET, "/api/quotas", None).await;
    assert_eq!(status, StatusCode::OK);
    let cleared = cleared.unwrap();
    let cleared_or = find_provider(&cleared, "openrouter");
    assert_eq!(cleared_or["hasManagementKey"], false);
}

#[tokio::test]
async fn quotas_probes_live_apis_caches_and_manages_openrouter_key() {
    let harness = setup_harness().await;
    let app = router(harness.state.clone());

    let (status, refreshed) = call(&app, Method::POST, "/api/quotas/refresh", None).await;
    assert_eq!(status, StatusCode::OK, "{refreshed:?}");
    let refreshed = refreshed.unwrap();
    assert_no_nulls(&refreshed, "$");
    assert!(!refreshed.to_string().contains("sk-or-test-123"));
    assert!(!refreshed.to_string().contains("sk-ds-test-456"));

    assert_eq!(
        harness
            .upstream
            .counters
            .openrouter_key
            .load(Ordering::SeqCst),
        1
    );
    assert_eq!(
        harness
            .upstream
            .counters
            .openrouter_credits
            .load(Ordering::SeqCst),
        0
    );
    assert_eq!(
        harness
            .upstream
            .counters
            .deepseek_balance
            .load(Ordering::SeqCst),
        1
    );
    assert!(harness.upstream.counters.ag_quota.load(Ordering::SeqCst) >= 1);

    // openrouter, deepseek, opencode-go (live usage API) and google/Antigravity
    // all report windows/balances; the env-only provider is headers-only.
    assert_eq!(refreshed["data"]["summary"]["liveQuotaCount"], 4);
    let providers = refreshed["data"]["providers"].as_array().unwrap();
    let ids: Vec<&str> = providers
        .iter()
        .filter_map(|p| p["providerID"].as_str())
        .collect();
    assert_eq!(
        ids,
        ["deepseek", "google", "opencode-go", "openrouter", "envonly"],
        "only active/config/env-backed providers may appear, ranked by capability: {providers:?}"
    );
    assert_eq!(refreshed["data"]["summary"]["providerCount"], ids.len());
    assert_eq!(
        harness
            .upstream
            .counters
            .opencode_go_usage
            .load(Ordering::SeqCst),
        1,
        "opencode-go must be probed once per force refresh"
    );
    assert_eq!(
        providers
            .iter()
            .filter(|p| p["providerID"] == ANTIGRAVITY_PROVIDER_ID)
            .count(),
        1,
        "Antigravity/Google must appear once without duplicate cards: {providers:?}"
    );

    assert_live_provider_cards(&refreshed);

    // Subsequent GET hits the 60s TTL cache.
    let (status, cached_resp) = call(&app, Method::GET, "/api/quotas", None).await;
    assert_eq!(status, StatusCode::OK);
    let cached_resp = cached_resp.unwrap();
    assert_no_nulls(&cached_resp, "$");
    assert_eq!(find_provider(&cached_resp, "openrouter")["cached"], true);
    assert_eq!(
        harness
            .upstream
            .counters
            .openrouter_key
            .load(Ordering::SeqCst),
        1
    );
    assert_eq!(
        harness
            .upstream
            .counters
            .deepseek_balance
            .load(Ordering::SeqCst),
        1
    );

    verify_openrouter_management_flow(&app, &harness.upstream.counters).await;
}

/// `GET /api/quotas` is cache-first: a cold cache answers immediately with an
/// unprobed snapshot plus `refreshing: true`, exactly one background probe pass
/// follows, and a second GET inside the TTL re-probes nothing.
#[tokio::test]
async fn quotas_get_is_cache_first_and_refreshes_in_the_background() {
    let harness = setup_harness().await;
    let app = router(harness.state.clone());
    let counters = harness.upstream.counters.clone();

    // Cold cache: the read path never awaits a provider probe.
    let started = std::time::Instant::now();
    let (status, cold) = call(&app, Method::GET, "/api/quotas", None).await;
    let elapsed = started.elapsed();
    println!("cold cache-first GET elapsed = {elapsed:?}");
    assert_eq!(status, StatusCode::OK, "{cold:?}");
    let cold = cold.unwrap();
    assert_no_nulls(&cold, "$");
    assert!(
        elapsed < std::time::Duration::from_millis(300),
        "a cache-first GET must not await a probe: {elapsed:?}"
    );
    assert_eq!(
        cold["data"]["refreshing"], true,
        "a missing snapshot reports the background refresh: {cold}"
    );
    // The provider set still comes from credentials/config (it needs no probe),
    // but nothing in it carries live probe data yet.
    let cold_openrouter = find_provider(&cold, "openrouter");
    assert!(
        cold_openrouter.get("cached").is_none(),
        "nothing has been probed yet: {cold_openrouter}"
    );
    assert!(
        cold_openrouter.get("windows").is_none(),
        "a cold snapshot carries no live windows: {cold_openrouter}"
    );
    assert!(
        find_provider(&cold, "deepseek").get("balances").is_none(),
        "a cold snapshot carries no live balance: {cold}"
    );

    // The background pass fills the snapshot; bounded poll instead of a sleep.
    let mut polled = Value::Null;
    for _ in 0..80 {
        let (status, body) = call(&app, Method::GET, "/api/quotas", None).await;
        assert_eq!(status, StatusCode::OK);
        let body = body.unwrap();
        if body["data"].get("refreshing").is_none() {
            polled = body;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        !polled.is_null(),
        "the background refresh never completed within 2 s"
    );
    assert_no_nulls(&polled, "$");
    let openrouter = find_provider(&polled, "openrouter");
    assert!(
        openrouter["windows"]
            .as_array()
            .is_some_and(|windows| !windows.is_empty()),
        "the background refresh must land the probed windows: {openrouter}"
    );
    assert_eq!(
        openrouter["cached"], true,
        "the polled read serves the cache: {openrouter}"
    );

    // Inside the TTL another GET re-probes nothing.
    let probes = counters.openrouter_key.load(Ordering::SeqCst);
    assert_eq!(probes, 1, "exactly one probe pass ran");
    let (status, warm) = call(&app, Method::GET, "/api/quotas", None).await;
    assert_eq!(status, StatusCode::OK);
    let warm = warm.unwrap();
    assert!(
        warm["data"].get("refreshing").is_none(),
        "a fresh snapshot needs no refresh: {warm}"
    );
    assert_eq!(counters.openrouter_key.load(Ordering::SeqCst), probes);
    assert_eq!(counters.deepseek_balance.load(Ordering::SeqCst), 1);

    // `?refresh=true` stays blocking and re-probes.
    let (status, forced) = call(&app, Method::GET, "/api/quotas?refresh=true", None).await;
    assert_eq!(status, StatusCode::OK);
    let forced = forced.unwrap();
    assert_eq!(find_provider(&forced, "openrouter")["cached"], false);
    assert_eq!(counters.openrouter_key.load(Ordering::SeqCst), probes + 1);

    // `POST /api/quotas/refresh` returns the fresh snapshot synchronously too.
    let (status, posted) = call(&app, Method::POST, "/api/quotas/refresh", None).await;
    assert_eq!(status, StatusCode::OK);
    let posted = posted.unwrap();
    assert_no_nulls(&posted, "$");
    assert_eq!(find_provider(&posted, "openrouter")["cached"], false);
    assert_eq!(counters.openrouter_key.load(Ordering::SeqCst), probes + 2);
}

#[tokio::test]
async fn quotas_aggregates_opencode_go_local_usage_and_captured_rate_limits() {
    let harness = setup_harness().await;
    let app = router(harness.state.clone());

    // Prime the cache: `GET /api/quotas` is cache-first and never probes, so the
    // blocking `POST /api/quotas/refresh` is the path that fills the snapshot.
    let (status, resp) = call(&app, Method::POST, "/api/quotas/refresh", None).await;
    assert_eq!(status, StatusCode::OK, "{resp:?}");
    let body = resp.unwrap();
    assert_no_nulls(&body, "$");

    let summary = &body["data"]["summary"];
    assert!((summary["spend5hUsd"].as_f64().unwrap() - 0.25).abs() < 1e-6);
    assert!((summary["spend24hUsd"].as_f64().unwrap() - 0.75).abs() < 1e-6);
    assert!((summary["spend7dUsd"].as_f64().unwrap() - 1.75).abs() < 1e-6);
    assert!((summary["spend30dUsd"].as_f64().unwrap() - 3.75).abs() < 1e-6);

    let go = find_provider(&body, "opencode-go");
    // `opencode-go` is a live-capability provider: the usage endpoint
    // (`/zen/go/v1/usage`) reports rolling/weekly/monthly percent windows.
    assert_eq!(go["capability"], "live_api");
    assert_eq!(go["status"], "ok");
    assert_eq!(go["cached"], false);
    assert!(
        go["capabilityNote"]
            .as_str()
            .unwrap()
            .contains("OpenCode Go")
    );
    let windows = go["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 3, "{windows:?}");
    let expected_windows = [
        (
            "rolling",
            "5-Hour Rolling",
            0.0_f64,
            100.0_f64,
            1_794_678_459_000_i64,
        ),
        ("weekly", "Weekly", 79.0, 21.0, 1_794_787_200_000),
        ("monthly", "Monthly", 46.0, 54.0, 1_796_691_596_000),
    ];
    for (index, (id, label, used, remaining, reset_at_ms)) in
        expected_windows.into_iter().enumerate()
    {
        let window = &windows[index];
        assert_eq!(window["id"], id);
        assert_eq!(window["label"], label);
        assert_eq!(window["unit"], "percent");
        assert_eq!(window["limit"], 100.0);
        assert_eq!(window["usedPercent"], used, "{id}");
        assert_eq!(window["remainingPercent"], remaining, "{id}");
        assert_eq!(window["resetAtMs"], reset_at_ms, "{id}");
        assert_eq!(window["limited"], false, "{id}");
    }
    // Captured rate-limit telemetry still rides along on the live card.
    assert_eq!(go["rateLimit"]["modelId"], "glm-5");
    assert_eq!(go["rateLimit"]["requestsLimit"], 120);
    assert_eq!(go["rateLimit"]["requestsRemaining"], 98);
    assert_eq!(go["rateLimit"]["tokensLimit"], 200_000);
    assert_eq!(go["rateLimit"]["tokensRemaining"], 185_500);

    let usage = &go["localUsage"];
    assert_eq!(usage["fiveHours"]["steps"], 1);
    assert_eq!(usage["fiveHours"]["totalTokens"], 1500);
    assert_eq!(usage["day"]["steps"], 2);
    assert_eq!(usage["day"]["totalTokens"], 4500);
    assert_eq!(usage["week"]["steps"], 3);
    assert_eq!(usage["week"]["totalTokens"], 8500);
    assert_eq!(usage["month"]["steps"], 4);
    assert_eq!(usage["month"]["totalTokens"], 14500);
    assert_eq!(usage["allTime"]["steps"], 5);
    assert_eq!(usage["allTime"]["totalTokens"], 24500);
    assert!((usage["allTime"]["costUsd"].as_f64().unwrap() - 7.75).abs() < 1e-6);

    let top_models = usage["topModels"].as_array().unwrap();
    assert_eq!(top_models[0]["modelID"], "qwen3-coder");
    assert_eq!(top_models[1]["modelID"], "kimi-k2.5");
    assert_eq!(top_models[2]["modelID"], "glm-5");
}

/// Live-capability provider cards: `openrouter`, `deepseek` and Antigravity.
fn assert_live_provider_cards(refreshed: &Value) {
    let openrouter = find_provider(refreshed, "openrouter");
    assert_eq!(openrouter["status"], "ok");
    assert_eq!(openrouter["capability"], "live_api");
    assert_eq!(openrouter["cached"], false);
    assert_eq!(openrouter["hasManagementKey"], false);
    assert_eq!(openrouter["keyLabel"], "openrouter-prod");
    assert_eq!(openrouter["rateLimitRule"], "200 req / 10s");
    assert_eq!(openrouter["windows"][0]["remaining"], 62.5);
    assert_eq!(openrouter["spendSummary"]["daily"], 1.5);

    let deepseek = find_provider(refreshed, "deepseek");
    assert_eq!(deepseek["status"], "ok");
    assert_eq!(deepseek["balances"][0]["currency"], "USD");
    assert_eq!(deepseek["balances"][0]["total"], 19.5);
    assert_eq!(deepseek["balances"][0]["granted"], 2.0);
    assert_eq!(deepseek["balances"][0]["toppedUp"], 17.5);

    let antigravity = find_provider(refreshed, ANTIGRAVITY_PROVIDER_ID);
    assert_eq!(antigravity["capability"], "live_api");
    assert_eq!(antigravity["status"], "ok");
    assert_eq!(antigravity["keyLabel"], "quota-user@example.com");
    assert!(!antigravity["accounts"].as_array().unwrap().is_empty());
    assert!(!antigravity["windows"].as_array().unwrap().is_empty());
}

/// A model client that is never called: these tests only read the
/// runtime-owned key-rotation table, never stream.
#[derive(Debug)]
struct NoopModel;

impl oc_runtime::ModelClient for NoopModel {
    fn stream(
        &self,
        _request: oc_llm::LlmRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<oc_llm::LlmStream, oc_llm::LlmError>> {
        Box::pin(async {
            Ok(futures_util::StreamExt::boxed(
                futures_util::stream::empty::<oc_llm::LlmEvent>(),
            ))
        })
    }
}

/// Attach a runtime so `AppState::key_pool()` has an owner, returning the router
/// and the pool the test marks.
fn state_with_key_pool(parts: &HarnessParts) -> (Router, Arc<oc_llm::KeyPool>) {
    let state = state_from(parts);
    let config = Arc::clone(&parts.config);
    let catalog = Arc::clone(state.catalog().expect("catalog"));
    let store = Arc::clone(state.store().expect("store"));
    let broker = Arc::new(oc_server::PermissionBroker::from_state(&state));
    let sink = Arc::new(oc_server::BusSink::from_state(&state));
    let runtime = Arc::new(oc_runtime::Runtime::new_with_model_client(
        oc_runtime::RuntimeConfig {
            store,
            config,
            catalog,
            client: Arc::new(SharedLlmClient::new(LlmClient::new().expect("client"))),
            tools: Arc::new(oc_tools::ToolRegistry::with_defaults()),
            sink,
            permissions: broker.clone(),
        },
        Arc::new(NoopModel),
    ));
    let pool = Arc::clone(runtime.key_pool());
    let state = state.with_runtime(runtime).with_permissions(broker);
    (router(state), pool)
}

/// Per-provider `keys`: stored credentials in rotation order joined with the
/// runtime's key cooldowns. A provider with no credential row omits `keys`, and
/// a cooled key reports its deadline without hiding the model or the provider.
#[tokio::test]
async fn quotas_expose_per_provider_keys_and_key_cooldowns() {
    let parts = harness_parts().await;
    // A second `deepseek` key: `create_credential` deactivates its siblings, so
    // re-activate the first one to get exactly one active row out of two.
    parts
        .store
        .create_credential(NewCredential {
            id: Some("cred_deepseek_backup".to_owned()),
            integration_id: "deepseek".to_owned(),
            label: Some("DeepSeek Backup".to_owned()),
            value: json!({ "type": "key", "key": "sk-ds-backup-789" }),
            active: Some(false),
        })
        .await
        .unwrap();
    let (app, pool) = state_with_key_pool(&parts);

    let (status, body) = call(&app, Method::POST, "/api/quotas/refresh", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let body = body.unwrap();
    assert_no_nulls(&body, "$");

    let deepseek = find_provider(&body, "deepseek");
    let keys = deepseek["keys"].as_array().expect("deepseek keys");
    assert_eq!(keys.len(), 2, "{deepseek}");
    assert_eq!(
        keys.iter().filter(|k| k["active"] == true).count(),
        1,
        "exactly one credential is the stored selection: {deepseek}"
    );
    assert_eq!(keys[0]["active"], true, "active key sorts first: {keys:?}");
    let labels: Vec<&str> = keys.iter().filter_map(|k| k["label"].as_str()).collect();
    assert!(labels.contains(&"DeepSeek API Key"), "{labels:?}");
    assert!(labels.contains(&"DeepSeek Backup"), "{labels:?}");
    // No per-key usage attribution exists; it must be zero, never invented.
    assert_eq!(keys[0]["usedCostUsd"], 0.0, "{deepseek}");
    assert_eq!(keys[0]["usedRequests"], 0, "{deepseek}");
    assert!(
        keys[0].get("cooldownUntilMs").is_none(),
        "a healthy key omits the cooldown: {keys:?}"
    );

    // A provider backed only by config/env owns no credential row: no `keys`.
    let envonly = find_provider(&body, "envonly");
    assert!(
        envonly.get("keys").is_none(),
        "`keys` is omitted without credentials: {envonly}"
    );

    // Cool one deepseek key: it reports the deadline and the failure class, and
    // the sibling key plus an unrelated provider stay untouched.
    let now = now_millis();
    pool.mark(
        "deepseek",
        Some("cred_deepseek_backup"),
        oc_llm::FailureKind::Quota,
        now,
    );
    let (status, body) = call(&app, Method::GET, "/api/quotas", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let body = body.unwrap();
    let deepseek = find_provider(&body, "deepseek");
    let keys = deepseek["keys"].as_array().expect("deepseek keys");
    let cooling = keys
        .iter()
        .find(|k| k["credentialID"] == "cred_deepseek_backup")
        .expect("the marked key is listed");
    assert_eq!(cooling["limited"], true, "{cooling}");
    assert_eq!(cooling["kind"], "quota", "{cooling}");
    assert_eq!(cooling["failures"], 1, "{cooling}");
    assert!(
        cooling["cooldownUntilMs"]
            .as_i64()
            .is_some_and(|until| until > now),
        "the cooldown must be a future epoch ms: {cooling}"
    );
    let healthy = keys
        .iter()
        .find(|k| k["credentialID"] == "cred_deepseek")
        .expect("the other key is listed");
    assert_eq!(healthy["limited"], false, "{healthy}");
    assert!(healthy.get("cooldownUntilMs").is_none(), "{healthy}");
    // The provider itself is still connected and fully usable.
    assert_eq!(deepseek["connected"], true, "{deepseek}");
    for other in ["openrouter", "opencode-go"] {
        let entry = find_provider(&body, other);
        for key in entry["keys"].as_array().into_iter().flatten() {
            assert_eq!(key["limited"], false, "{other} must be unaffected: {key}");
        }
    }
}

/// A captured rate-limit header snapshot for `provider_id`.
fn rate_limit_snapshot(provider_id: &str) -> ProviderRateLimit {
    let now = now_millis();
    ProviderRateLimit {
        provider_id: provider_id.to_owned(),
        model_id: "captured-model".to_owned(),
        observed_at_ms: now,
        status: 200,
        requests_limit: Some(60),
        requests_remaining: Some(59),
        requests_reset_at_ms: Some(now + 60_000),
        requests_reset_raw: Some("60s".to_owned()),
        tokens_limit: None,
        tokens_remaining: None,
        tokens_reset_at_ms: None,
        tokens_reset_raw: None,
        retry_after_ms: None,
    }
}

/// Only providers with a genuinely active credential belong in the report:
/// historical usage, a captured rate-limit snapshot and a deactivated
/// credential are all insufficient, while env/config-only providers stay.
#[tokio::test]
async fn quotas_omit_providers_without_an_active_credential() {
    let parts = harness_parts().await;

    // (a) An inactive credential plus historical usage for a provider the
    // catalog does not even know: neither half may create a card.
    seed_inactive_credential(&parts.store, "cred_ghost", "ghost", "sk-ghost-1").await;
    seed_usage(&parts.store, "ses_ghost_usage", "ghost", 1_000).await;

    // (c) Captured rate-limit telemetry is not a connection: `moonshotai` owns
    // no credential and is not in the catalog, `uncredentialed` is in the
    // catalog but owns no credential, config key or env var.
    for provider_id in ["moonshotai", "uncredentialed"] {
        parts
            .llm_client
            .record_rate_limit(rate_limit_snapshot(provider_id));
    }
    seed_usage(&parts.store, "ses_uncred_usage", "uncredentialed", 2_000).await;

    let app = router(state_from(&parts));

    let (status, refreshed) = call(&app, Method::POST, "/api/quotas/refresh", None).await;
    assert_eq!(status, StatusCode::OK, "{refreshed:?}");
    let refreshed = refreshed.unwrap();
    assert_no_nulls(&refreshed, "$");
    assert_absent(&refreshed, "ghost");
    assert_absent(&refreshed, "moonshotai");
    assert_absent(&refreshed, "uncredentialed");

    // (d) An env-backed provider owns no credential row and must stay listed.
    let envonly = find_provider(&refreshed, "envonly");
    assert_eq!(envonly["connected"], true);
    assert_eq!(envonly["connectionSource"], "env");
    assert_eq!(envonly["status"], "local_only");
    assert_eq!(
        parts
            .upstream
            .counters
            .deepseek_balance
            .load(Ordering::SeqCst),
        1,
        "deepseek must be probed while its credential is active"
    );

    // (b) Deactivating the credential hides the provider and stops the probe.
    deactivate_credential(&parts.store, "cred_deepseek").await;
    let (status, after) = call(&app, Method::POST, "/api/quotas/refresh", None).await;
    assert_eq!(status, StatusCode::OK, "{after:?}");
    let after = after.unwrap();
    assert_no_nulls(&after, "$");
    assert_absent(&after, "deepseek");
    assert_eq!(
        provider_ids(&after),
        ["google", "opencode-go", "openrouter", "envonly"],
        "only actively connected providers remain: {after}"
    );
    assert_eq!(
        parts
            .upstream
            .counters
            .deepseek_balance
            .load(Ordering::SeqCst),
        1,
        "a deactivated credential must never be used as a probe key"
    );
}

/// A pool whose every account is disabled is not a connection: `google` must
/// disappear from the report and no upstream refresh may run for it.
#[tokio::test]
async fn quotas_hide_google_when_every_antigravity_account_is_disabled() {
    let parts = harness_parts().await;
    let app = router(state_from(&parts));

    let (status, before) = call(&app, Method::POST, "/api/quotas/refresh", None).await;
    assert_eq!(status, StatusCode::OK, "{before:?}");
    let before = before.unwrap();
    assert_no_nulls(&before, "$");
    let google = find_provider(&before, "google");
    assert_eq!(google["connected"], true);
    assert_eq!(google["connectionSource"], "oauth");
    let ag_probes = parts.upstream.counters.ag_quota.load(Ordering::SeqCst);
    assert!(ag_probes >= 1, "a usable account is probed");

    parts
        .antigravity
        .set_account_enabled("quota-user@example.com", false)
        .expect("account exists");

    let (status, after) = call(&app, Method::POST, "/api/quotas/refresh", None).await;
    assert_eq!(status, StatusCode::OK, "{after:?}");
    let after = after.unwrap();
    assert_no_nulls(&after, "$");
    assert_absent(&after, "google");
    assert_eq!(
        parts.upstream.counters.ag_quota.load(Ordering::SeqCst),
        ag_probes,
        "a pool with no usable account must not be probed"
    );
}
