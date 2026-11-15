//! End-to-end HTTP scenario tests for Antigravity OAuth, account pool control plane,
//! dynamic model discovery, multi-account streaming failover, and `google_search`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use futures_util::StreamExt;
use oc_antigravity::{
    ANTIGRAVITY_METHOD_ID, ANTIGRAVITY_PROVIDER_ID, AntigravityConfig, AntigravityEndpoints,
    AntigravityService, AntigravityTokenExchange, overlay_catalog_models,
};
use oc_config::{ConfigService, LoadOptions};
use oc_llm::{Catalog, LlmClient, LlmEvent, LlmMessage, LlmRequest};
use oc_runtime::ToolProvider;
use oc_server::data::antigravity_tools::AntigravityToolProvider;
use oc_server::{AppState, catalog_from_config, router};
use oc_store::Store;
use oc_tools::ToolContext;
use oc_types::{
    ProjectId, ServerInfo, ServerPaths, SessionId, SessionInfo, SessionLocation, SessionTimes,
    Tokens,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower::ServiceExt;

struct MockCloudUpstream {
    base_url: String,
    stream_hits: Arc<AtomicUsize>,
}

fn mock_auth_routes() -> Router {
    Router::new()
        .route(
            "/token",
            post(|| async {
                axum::Json(json!({
                    "access_token": "ya29.mock_access_1",
                    "expires_in": 3600,
                    "refresh_token": "1//mock_refresh_1",
                    "token_type": "Bearer"
                }))
            }),
        )
        .route(
            "/userinfo",
            get(|| async {
                axum::Json(json!({
                    "email": "tester@example.com",
                    "verified_email": true
                }))
            }),
        )
        .route(
            "/v1internal:loadCodeAssist",
            post(|| async {
                axum::Json(json!({
                    "cloudaicompanionProject": { "id": "mock-proj-123" },
                    "currentTier": { "id": "standard-tier", "name": "Standard" }
                }))
            }),
        )
}

fn mock_catalog_routes() -> Router {
    Router::new()
        .route(
            "/v1internal:fetchAvailableModels",
            post(|| async {
                axum::Json(json!({
                    "models": {
                        "gemini-3.5-pro-ultra": {
                            "displayName": "Gemini 3.5 Pro Ultra",
                            "supportsThinking": true,
                            "maxTokens": 1_048_576,
                            "maxOutputTokens": 65_536,
                            "quotaInfo": {
                                "remainingFraction": 0.9,
                                "resetTime": "2030-01-01T00:00:00Z"
                            }
                        },
                        "claude-sonnet-4-6": {
                            "displayName": "Claude Sonnet 4.6",
                            "supportsThinking": true,
                            "quotaInfo": {
                                "remainingFraction": 0.85,
                                "resetTime": "2030-01-01T00:00:00Z"
                            }
                        }
                    }
                }))
            }),
        )
        .route(
            "/v1internal:retrieveUserQuota",
            post(|| async {
                axum::Json(json!({
                    "buckets": [
                        {
                            "modelId": "gemini-2.5-pro",
                            "remainingFraction": 0.75,
                            "resetTime": "2030-01-01T00:00:00Z"
                        }
                    ]
                }))
            }),
        )
}

async fn start_mock_upstream() -> MockCloudUpstream {
    let stream_hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&stream_hits);

    let app = Router::new()
        .merge(mock_auth_routes())
        .merge(mock_catalog_routes())
        .route(
            "/v1internal:generateContent",
            post(|| async {
                axum::Json(json!({
                    "response": {
                        "candidates": [{
                            "content": {
                                "parts": [{ "text": "Rust 1.85 adds edition 2024 features [1]." }]
                            },
                            "groundingMetadata": {
                                "webSearchQueries": ["rust edition 2024"],
                                "groundingChunks": [{
                                    "web": { "title": "Rust Blog", "uri": "https://blog.rust-lang.org/" }
                                }]
                            }
                        }]
                    }
                }))
            }),
        )
        .route(
            "/v1internal:streamGenerateContent",
            post(move || {
                let hits = Arc::clone(&hits_clone);
                async move {
                    let n = hits.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        Response::builder()
                            .status(StatusCode::TOO_MANY_REQUESTS)
                            .header("Content-Type", "application/json")
                            .body(Body::from(
                                r#"{"error":{"message":"Resource has been exhausted (RATE_LIMIT_EXCEEDED)"}}"#,
                            ))
                            .unwrap()
                    } else {
                        let sse = concat!(
                            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello from Antigravity HTTP!\"}]},\"finishReason\":\"STOP\"}]}}\n\n",
                            "data: [DONE]\n\n"
                        );
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "text/event-stream")
                            .body(Body::from(sse))
                            .unwrap()
                    }
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    MockCloudUpstream {
        base_url: format!("http://{addr}"),
        stream_hits,
    }
}

struct TestHarness {
    _db: tempfile::TempDir,
    cfg_dir: tempfile::TempDir,
    service: Arc<AntigravityService>,
    state: AppState,
    upstream: MockCloudUpstream,
}

async fn setup_harness() -> TestHarness {
    let upstream = start_mock_upstream().await;
    let db = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(db.path().join("data.sqlite")).await.unwrap());

    let cfg_dir = tempfile::tempdir().unwrap();
    std::fs::write(cfg_dir.path().join("opencode.json"), "{}").unwrap();
    let home = cfg_dir.path().to_string_lossy().into_owned();
    let mut env = HashMap::new();
    env.insert("HOME".to_owned(), home.clone());
    env.insert("USERPROFILE".to_owned(), home);
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        cfg_dir.path().join("xdg").to_string_lossy().into_owned(),
    );
    env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());

    let config = Arc::new(
        ConfigService::load(LoadOptions {
            directory: cfg_dir.path().to_path_buf(),
            env,
        })
        .unwrap(),
    );

    let endpoints = AntigravityEndpoints {
        oauth_auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
        oauth_token_url: format!("{}/token", upstream.base_url),
        oauth_userinfo_url: format!("{}/userinfo", upstream.base_url),
        antigravity_endpoint: upstream.base_url.clone(),
        gemini_cli_endpoint: upstream.base_url.clone(),
        manifest_url: format!("{}/version", upstream.base_url),
    };

    let service = Arc::new(
        AntigravityService::in_memory_for_test(
            Some(Arc::clone(&store)),
            AntigravityConfig::default(),
            endpoints,
        )
        .await
        .unwrap(),
    );

    let mut catalog = catalog_from_config(config.as_ref()).unwrap();
    service.overlay_catalog(&mut catalog);

    let state = AppState::new(
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
        cfg_dir.path().to_path_buf(),
    )
    .with_store(store)
    .with_config(config)
    .with_catalog(Arc::new(catalog))
    .with_antigravity(Arc::clone(&service));

    TestHarness {
        _db: db,
        cfg_dir,
        service,
        state,
        upstream,
    }
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

async fn verify_oauth_connect_flow(app: &Router) {
    // 1. Google integration exposes Antigravity OAuth method alongside key/env.
    let (status, google_row) = call(app, Method::GET, "/api/integration/google", None).await;
    assert_eq!(status, StatusCode::OK, "{google_row:?}");
    let methods = google_row.unwrap()["data"]["methods"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        methods.iter().any(|m| {
            m.get("id").and_then(Value::as_str) == Some(ANTIGRAVITY_METHOD_ID)
                && m.get("type").and_then(Value::as_str) == Some("oauth")
        }),
        "google methods missing antigravity oauth: {methods:?}"
    );

    // 2. Start OAuth in manual code mode via HTTP form answer.
    let (status, start_body) = call(
        app,
        Method::POST,
        "/api/integration/google/connect/oauth",
        Some(json!({
            "methodID": ANTIGRAVITY_METHOD_ID,
            "label": "Work Account",
            "answer": { "noBrowser": true, "projectId": "explicit-proj" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{start_body:?}");
    let attempt = &start_body.unwrap()["data"];
    let attempt_id = attempt["attemptID"].as_str().unwrap().to_owned();
    assert_eq!(attempt["mode"], "code");
    assert!(
        attempt["url"]
            .as_str()
            .unwrap()
            .contains("accounts.google.com")
    );

    // Extract generated state from consent URL so completion passes CSRF state check.
    let consent_url = url::Url::parse(attempt["url"].as_str().unwrap()).unwrap();
    let oauth_state = consent_url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .unwrap();

    // 3. Poll shows pending before completion.
    let (status, poll_body) = call(
        app,
        Method::GET,
        &format!("/api/integration/google/connect/oauth/{attempt_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{poll_body:?}");
    assert_eq!(poll_body.unwrap()["data"]["status"], "pending");

    // 4. Complete OAuth with callback URL carrying code + state.
    let (status, complete_body) = call(
        app,
        Method::POST,
        &format!("/api/integration/google/connect/oauth/{attempt_id}/complete"),
        Some(json!({
            "code": format!("http://localhost:51121/oauth-callback?code=4/mock_auth_code&state={oauth_state}")
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{complete_body:?}");

    // Poll now reports complete.
    let (status, poll_done) = call(
        app,
        Method::GET,
        &format!("/api/integration/google/connect/oauth/{attempt_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(poll_done.unwrap()["data"]["status"], "complete");

    verify_post_oauth_accounts_and_models(app).await;
}

async fn verify_post_oauth_accounts_and_models(app: &Router) {
    // 5. Account appears in /api/credential AND /api/antigravity/accounts.
    let (status, creds_body) = call(app, Method::GET, "/api/credential", None).await;
    assert_eq!(status, StatusCode::OK);
    let creds = creds_body.unwrap()["data"].as_array().unwrap().clone();
    assert_eq!(creds.len(), 1);
    assert_eq!(creds[0]["integrationID"], ANTIGRAVITY_PROVIDER_ID);

    let (status, accts_body) = call(app, Method::GET, "/api/antigravity/accounts", None).await;
    assert_eq!(status, StatusCode::OK, "{accts_body:?}");
    let accts_data = accts_body.unwrap();
    assert_eq!(accts_data["data"]["count"], 1);
    assert_eq!(
        accts_data["data"]["accounts"][0]["email"],
        "tester@example.com"
    );

    // 5b. OAuth completion auto-refreshes upstream models immediately so `/api/model`
    // includes both built-in Gemini 4 / Claude 5.5 models and newly discovered models.
    let (status, auto_models_body) = call(app, Method::GET, "/api/model", None).await;
    assert_eq!(status, StatusCode::OK, "{auto_models_body:?}");
    let auto_models = auto_models_body.unwrap()["data"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        auto_models
            .iter()
            .any(|m| m["providerID"] == "google" && m["id"] == "antigravity-gemini-3.5-pro-ultra"),
        "expected OAuth completion to auto-refresh discovered models"
    );
    assert!(
        auto_models
            .iter()
            .any(|m| m["providerID"] == "google" && m["id"] == "antigravity-gemini-4-hydrogen"),
        "expected built-in Gemini 4 Hydrogen"
    );
    assert!(
        auto_models
            .iter()
            .any(|m| m["providerID"] == "google" && m["id"] == "antigravity-claude-opus-5-5"),
        "expected built-in Claude Opus 5.5"
    );
}

async fn verify_models_and_selection(app: &Router) -> String {
    // 6. Refresh quotas + models discovers `gemini-3.5-pro-ultra` dynamically in `/api/model`.
    let (status, refresh_body) =
        call(app, Method::POST, "/api/antigravity/models/refresh", None).await;
    assert_eq!(status, StatusCode::OK, "{refresh_body:?}");

    let (status, models_body) = call(app, Method::GET, "/api/model", None).await;
    assert_eq!(status, StatusCode::OK, "{models_body:?}");
    let models = models_body.unwrap()["data"].as_array().unwrap().clone();
    assert!(
        models
            .iter()
            .any(|m| m["providerID"] == "google" && m["id"] == "antigravity-gemini-3.5-pro-ultra"),
        "expected dynamically discovered model in /api/model"
    );

    // 7. Create a session and select newly discovered Antigravity model.
    let (status, ses_body) = call(
        app,
        Method::POST,
        "/api/session",
        Some(json!({ "title": "AG Test" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ses_body:?}");
    let sid = ses_body.unwrap()["data"]["id"].as_str().unwrap().to_owned();
    let (status, model_select_body) = call(
        app,
        Method::POST,
        &format!("/api/session/{sid}/model"),
        Some(json!({
            "model": { "providerID": "google", "id": "antigravity-gemini-3.5-pro-ultra" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{model_select_body:?}");
    sid
}

async fn verify_failover_streaming(harness: &TestHarness, app: &Router) {
    // 8. Add a second account so streaming tests 429 failover end-to-end.
    harness
        .service
        .upsert_exchange_result(
            AntigravityTokenExchange {
                refresh: "1//mock_refresh_2|proj-second".to_owned(),
                refresh_token: "1//mock_refresh_2".to_owned(),
                access: "ya29.mock_access_2".to_owned(),
                expires: 4_000_000_000_000,
                email: Some("backup@example.com".to_owned()),
                project_id: "proj-second".to_owned(),
                explicit_project_id: Some("proj-second".to_owned()),
                managed_project_id: None,
            },
            Some("Backup".to_owned()),
        )
        .await
        .unwrap();

    let (status, _) = call(
        app,
        Method::POST,
        "/api/antigravity/accounts/backup@example.com/disable",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, enabled_body) = call(
        app,
        Method::POST,
        "/api/antigravity/accounts/backup@example.com/enable",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(enabled_body.unwrap()["data"]["enabled"], true);

    let mut tmp_cat = Catalog::from_snapshot("{}").unwrap();
    overlay_catalog_models(&mut tmp_cat, None);
    let llm = LlmClient::new().unwrap();
    llm.set_hook(Some(Arc::clone(&harness.service) as _));
    let shared_llm = oc_llm::SharedLlmClient::new(llm.clone());
    assert!(
        shared_llm
            .resolve_model(&tmp_cat, "google/antigravity-gemini-3.5-pro-ultra")
            .is_ok(),
        "expected dynamic hook resolution on stale startup catalog"
    );
    let resolved = shared_llm
        .resolve_model(&tmp_cat, "google/antigravity-gemini-3.1-pro#low")
        .unwrap();
    let mut req = LlmRequest::new(resolved);
    req.messages = vec![LlmMessage::user("hello")];
    let mut stream = llm.stream(req).await.unwrap();
    let mut collected = String::new();
    while let Some(ev) = stream.next().await {
        if let LlmEvent::TextDelta { delta } = ev {
            collected.push_str(&delta);
        }
    }
    assert_eq!(collected, "Hello from Antigravity HTTP!");
    assert_eq!(harness.upstream.stream_hits.load(Ordering::SeqCst), 2);
}

async fn verify_google_search_and_cleanup(harness: &TestHarness, app: &Router, sid: String) {
    // 9. Execute `google_search` tool via AntigravityToolProvider.
    let dummy_session = SessionInfo {
        id: SessionId::from_wire(sid),
        parent_id: None,
        fork: None,
        project_id: ProjectId::from_wire("prj_test"),
        agent: None,
        model: None,
        cost: 0.0,
        tokens: Tokens::default(),
        outcome: None,
        time: SessionTimes {
            created: 1,
            updated: 1,
            busy: None,
            idle: None,
            viewed: None,
            archived: None,
        },
        title: Some("t".to_owned()),
        location: SessionLocation::new(harness.cfg_dir.path().to_string_lossy().into_owned()),
        subpath: None,
        metadata: None,
        permissions: None,
        revert: None,
    };
    let provider = AntigravityToolProvider::new(Arc::clone(&harness.service));
    let tools = provider.tools(&dummy_session, "build");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "google_search");

    let ctx = ToolContext {
        session_id: "ses_1".to_owned(),
        directory: harness.cfg_dir.path().to_path_buf(),
        cwd: harness.cfg_dir.path().to_path_buf(),
        cancel: None,
    };
    let tool_res = tools[0]
        .execute(&ctx, json!({ "query": "rust edition 2024" }))
        .await
        .unwrap();
    let out = tool_res.output.unwrap_or_default();
    assert!(out.contains("Rust 1.85"), "{out}");
    assert!(out.contains("https://blog.rust-lang.org/"), "{out}");

    // 10. Deleting second account removes it cleanly; cancelling an unknown OAuth attempt is 204.
    let (status, _) = call(
        app,
        Method::DELETE,
        "/api/antigravity/accounts/backup@example.com",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, accts_after) = call(app, Method::GET, "/api/antigravity/accounts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accts_after.unwrap()["data"]["count"], 1);

    let (status, _) = call(
        app,
        Method::DELETE,
        "/api/integration/google/connect/oauth/att_nonexistent",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn antigravity_http_oauth_models_accounts_quotas_and_search_work_end_to_end() {
    let harness = setup_harness().await;
    let app = router(harness.state.clone());
    verify_oauth_connect_flow(&app).await;
    let sid = verify_models_and_selection(&app).await;
    verify_failover_streaming(&harness, &app).await;
    verify_google_search_and_cleanup(&harness, &app, sid).await;
}

/// A rate-limited account exposes one derived `cooldownUntilMs`: the maximum of
/// its still-future `rateLimits` deadlines. An account that is not cooling omits
/// the field entirely (never null).
///
/// The cooldown is produced the real way — the mock upstream answers the first
/// `streamGenerateContent` with `429`, the gateway marks that account, and the
/// request fails over to the second account.
#[tokio::test]
async fn antigravity_accounts_expose_the_max_rate_limit_deadline_as_a_cooldown() {
    let harness = setup_harness().await;
    seed_account(&harness.service, "primary@example.com", "proj-primary").await;
    seed_account(&harness.service, "backup@example.com", "proj-backup").await;

    let mut catalog = Catalog::from_snapshot("{}").unwrap();
    overlay_catalog_models(&mut catalog, None);
    let llm = LlmClient::new().unwrap();
    llm.set_hook(Some(Arc::clone(&harness.service) as _));
    let shared_llm = oc_llm::SharedLlmClient::new(llm.clone());
    let resolved = shared_llm
        .resolve_model(&catalog, "google/antigravity-gemini-3.1-pro#low")
        .unwrap();
    let mut req = LlmRequest::new(resolved);
    req.messages = vec![LlmMessage::user("hello")];
    let mut stream = llm.stream(req).await.unwrap();
    let mut collected = String::new();
    while let Some(ev) = stream.next().await {
        if let LlmEvent::TextDelta { delta } = ev {
            collected.push_str(&delta);
        }
    }
    assert_eq!(
        collected, "Hello from Antigravity HTTP!",
        "failover succeeded"
    );

    let app = router(harness.state.clone());
    let (status, body) = call(&app, Method::GET, "/api/antigravity/accounts", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let accounts = body.unwrap()["data"]["accounts"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(accounts.len(), 2, "{accounts:?}");

    let cooling: Vec<&Value> = accounts
        .iter()
        .filter(|account| account.get("cooldownUntilMs").is_some())
        .collect();
    assert_eq!(
        cooling.len(),
        1,
        "exactly the rate-limited account carries a cooldown: {accounts:?}"
    );
    let now = oc_types::now_millis();
    let deadline = cooling[0]["cooldownUntilMs"].as_i64().expect("epoch ms");
    assert!(
        deadline > now,
        "the cooldown is a future deadline: {deadline} vs {now}"
    );
    // It equals the maximum still-future entry of the raw per-key map, which is
    // also on the wire: the derived field invents nothing.
    let raw_max = cooling[0]["rateLimits"]
        .as_object()
        .expect("rateLimits")
        .values()
        .filter_map(Value::as_i64)
        .filter(|until| *until > now)
        .max()
        .expect("at least one future deadline");
    assert_eq!(deadline, raw_max, "{}", cooling[0]);
    // The healthy account omits the field.
    let healthy = accounts
        .iter()
        .find(|account| account.get("cooldownUntilMs").is_none())
        .expect("one account is not cooling");
    assert_eq!(healthy["enabled"], true, "{healthy}");
}

async fn seed_account(service: &AntigravityService, email: &str, project: &str) {
    service
        .upsert_exchange_result(
            AntigravityTokenExchange {
                refresh: format!("1//{email}|{project}"),
                refresh_token: format!("1//{email}"),
                access: format!("ya29.{project}"),
                expires: 4_000_000_000_000,
                email: Some(email.to_owned()),
                project_id: project.to_owned(),
                explicit_project_id: Some(project.to_owned()),
                managed_project_id: None,
            },
            None,
        )
        .await
        .expect("account seeded");
}

/// An empty pool cannot answer a refresh, and that is a conflict with the pool
/// state (`409`, kind `no_accounts`) rather than a missing resource. A specific
/// unknown account id — and an `?id=` filter that matches nothing — stay `404`.
#[tokio::test]
async fn antigravity_empty_pool_conflicts_and_unknown_accounts_are_not_found() {
    let harness = setup_harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(&app, Method::POST, "/api/antigravity/quotas", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body:?}");
    let body = body.unwrap();
    assert_eq!(body["_tag"], "InvalidRequestError");
    assert_eq!(body["kind"], "no_accounts");

    // `models/refresh` runs the same upstream probe, so it reports the same conflict.
    let (status, body) = call(&app, Method::POST, "/api/antigravity/models/refresh", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body:?}");
    assert_eq!(body.unwrap()["kind"], "no_accounts");

    // Reading an empty pool succeeds; no probe ran, so no report and no error.
    let (status, body) = call(&app, Method::GET, "/api/antigravity/quotas", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let data = body.unwrap()["data"].clone();
    assert_eq!(data["accounts"].as_array().unwrap().len(), 0);
    assert!(data.get("report").is_none(), "no probe ran, so no report");
    assert!(data.get("error").is_none(), "{data:?}");

    // `?refresh=true` reports the failed probe instead of inventing a refresh.
    let (status, body) = call(
        &app,
        Method::GET,
        "/api/antigravity/quotas?refresh=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let data = body.unwrap()["data"].clone();
    assert!(data.get("report").is_none(), "{data:?}");
    assert!(
        data["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()),
        "a failed refresh must be reported: {data:?}"
    );

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/antigravity/accounts/nobody@example.com/select",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "AntigravityAccountNotFoundError");

    let (status, body) = call(
        &app,
        Method::GET,
        "/api/antigravity/quotas?id=nobody@example.com",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "AntigravityAccountNotFoundError");
}

/// `GET /api/antigravity/quotas` filters by `?id=` (index, email, label or hash)
/// and only adds `report` when `?refresh=true` actually ran the live probe.
#[tokio::test]
async fn antigravity_quota_reads_filter_by_id_and_refresh_reports_the_probe() {
    let harness = setup_harness().await;
    seed_account(&harness.service, "quota@example.com", "proj-quota").await;
    let app = router(harness.state.clone());

    let (status, body) = call(&app, Method::GET, "/api/antigravity/quotas", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let data = body.unwrap()["data"].clone();
    assert_eq!(data["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(data["accounts"][0]["email"], "quota@example.com");
    assert!(data.get("report").is_none(), "{data:?}");
    assert!(data.get("discoveredModels").is_some(), "{data:?}");

    for wanted in ["quota@example.com", "0"] {
        let (status, body) = call(
            &app,
            Method::GET,
            &format!("/api/antigravity/quotas?id={wanted}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "id={wanted}: {body:?}");
        let data = body.unwrap()["data"].clone();
        let accounts = data["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 1, "id={wanted}");
        assert_eq!(accounts[0]["email"], "quota@example.com");
    }

    // `?refresh=false` is the cached read.
    let (status, body) = call(
        &app,
        Method::GET,
        "/api/antigravity/quotas?refresh=false",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert!(body.unwrap()["data"].get("report").is_none());

    // `?refresh=true` runs the probe and returns its truthful outcome.
    let (status, body) = call(
        &app,
        Method::GET,
        "/api/antigravity/quotas?refresh=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let data = body.unwrap()["data"].clone();
    let report = data.get("report").expect("refresh must report its outcome");
    assert!(
        report["discoveredModelCount"].as_u64().unwrap_or(0) >= 1,
        "{report:?}"
    );
    assert_eq!(
        report["accounts"].as_array().unwrap().len(),
        1,
        "{report:?}"
    );
    assert!(data.get("error").is_none(), "{data:?}");
    assert_eq!(data["accounts"][0]["email"], "quota@example.com");

    // The filter composes with the refresh.
    let (status, body) = call(
        &app,
        Method::GET,
        "/api/antigravity/quotas?refresh=true&id=quota@example.com",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let data = body.unwrap()["data"].clone();
    assert!(data.get("report").is_some(), "{data:?}");
    assert_eq!(data["accounts"].as_array().unwrap().len(), 1);
}

/// `tier` must reach the wire. `loadCodeAssist` reports `currentTier`, the
/// service caches it on the account, and `/api/antigravity/accounts` serializes
/// it; before that probe the field is omitted, never `null`.
///
/// The account is seeded with no project ID on purpose: an explicit `projectId`
/// short-circuits `loadCodeAssist`, so only a project-less account exercises the
/// producer the mock answers (`currentTier: standard-tier`).
#[tokio::test]
async fn antigravity_accounts_expose_the_tier_reported_by_load_code_assist() {
    let harness = setup_harness().await;
    harness
        .service
        .upsert_exchange_result(
            AntigravityTokenExchange {
                refresh: "1//tier_refresh".to_owned(),
                refresh_token: "1//tier_refresh".to_owned(),
                access: "ya29.tier_access".to_owned(),
                expires: 4_000_000_000_000,
                email: Some("tier@example.com".to_owned()),
                project_id: String::new(),
                explicit_project_id: None,
                managed_project_id: None,
            },
            None,
        )
        .await
        .expect("account seeded");
    let app = router(harness.state.clone());

    // No probe has run, so the tier is unknown and the optional field is absent.
    let (status, body) = call(&app, Method::GET, "/api/antigravity/accounts", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let account = body.unwrap()["data"]["accounts"][0].clone();
    assert!(account.get("tier").is_none(), "{account:?}");

    // The refresh resolves the project through `loadCodeAssist`, which also
    // reports the subscription tier the mock serves.
    let (status, body) = call(&app, Method::POST, "/api/antigravity/models/refresh", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");

    let (status, body) = call(&app, Method::GET, "/api/antigravity/accounts", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let account = body.unwrap()["data"]["accounts"][0].clone();
    assert_eq!(
        account["tier"], "standard-tier",
        "expected the `currentTier` reported by loadCodeAssist on the wire: {account:?}"
    );
}
