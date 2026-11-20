//! Model control-plane routes: `GET /api/models/registry`,
//! `PUT /api/config/models` and `GET /api/experimental/usage/models`.
//!
//! Offline: a temp SQLite store, a temp config directory and an inline
//! (`OPENCODE_CONFIG_CONTENT`) configuration document. The PUT writer targets
//! the discovered global directory, which the harness points at a temp
//! `XDG_CONFIG_HOME`, so no process environment is mutated.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::header::{AUTHORIZATION, HeaderMap};
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use oc_config::{ConfigService, LoadOptions};
use oc_llm::Catalog;
use oc_server::{AppState, router};
use oc_store::{NewMessage, NewSession, Store};
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tower::ServiceExt;

const FIXTURE: &str = r#"{
  "test": {
    "id": "test", "name": "Test Provider",
    "models": {
      "alpha": {
        "id": "alpha", "type": "chat", "name": "Alpha",
        "release_date": "2025-01-02",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}},
        "limit": {"context": 100000, "output": 10000},
        "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
        "api": {"chat-completions": {"base_url": "http://unused.invalid", "sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
      },
      "beta": {
        "id": "beta", "type": "chat", "name": "Beta",
        "release_date": "2025-02-02",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}},
        "limit": {"context": 8000, "output": 2000},
        "cost": {"text": {"input": {"price": 3}, "output": {"price": 4}}},
        "api": {"chat-completions": {"base_url": "http://unused.invalid", "sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
      }
    }
  }
}"#;

struct Harness {
    _db: tempfile::TempDir,
    _config: tempfile::TempDir,
    global_file: std::path::PathBuf,
    state: AppState,
}

fn info() -> ServerInfo {
    ServerInfo {
        version: "0.0.0-test".to_owned(),
        pid: 4242,
        hostname: Some("127.0.0.1".to_owned()),
        urls: vec!["http://127.0.0.1:4096".to_owned()],
        paths: ServerPaths {
            tmp: std::env::temp_dir().to_string_lossy().into_owned(),
        },
        capabilities: None,
    }
}

/// Build a harness over an arbitrary catalog snapshot and provider document.
///
/// `provider_id`/`provider_name` name the single configured provider the
/// snapshot carries, so the catalog overlay and the config document agree.
async fn harness_with(
    snapshot: &str,
    provider_id: &str,
    provider_name: &str,
    models_config: Value,
    password: Option<&str>,
) -> Harness {
    let db = tempfile::tempdir().unwrap();
    let store = Store::open(db.path().join("data.sqlite")).await.unwrap();

    let config_dir = tempfile::tempdir().unwrap();
    let xdg = config_dir.path().join("xdg");
    let document = json!({
        "providers": {provider_id: {"name": provider_name}},
        "models": models_config,
    });
    let mut env = HashMap::new();
    env.insert(
        "HOME".to_owned(),
        config_dir.path().to_string_lossy().into_owned(),
    );
    env.insert(
        "USERPROFILE".to_owned(),
        config_dir.path().to_string_lossy().into_owned(),
    );
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        xdg.to_string_lossy().into_owned(),
    );
    env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), document.to_string());
    let config = Arc::new(
        ConfigService::load(LoadOptions {
            directory: config_dir.path().to_path_buf(),
            env,
        })
        .unwrap(),
    );
    let providers = config
        .merged()
        .get("providers")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let catalog = Arc::new(Catalog::from_snapshot_with_config(snapshot, &providers).unwrap());

    let state = AppState::new(
        info(),
        password.map(str::to_owned),
        config_dir.path().to_path_buf(),
    )
    .with_store(Arc::new(store))
    .with_config(config)
    .with_catalog(catalog)
    .without_web_dist();

    Harness {
        global_file: xdg.join("opencode").join("opencode.jsonc"),
        _db: db,
        _config: config_dir,
        state,
    }
}

/// The standard two-model harness (`FIXTURE`, provider `test`).
async fn harness(models_config: Value, password: Option<&str>) -> Harness {
    harness_with(FIXTURE, "test", "Test Provider", models_config, password).await
}

/// `harness` with a synthetic `count`-model catalog instead of [`FIXTURE`].
async fn large_harness(count: usize, models_config: Value) -> Harness {
    harness_with(
        &large_fixture(count),
        "bulk",
        "Bulk Provider",
        models_config,
        None,
    )
    .await
}

async fn send(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(AUTHORIZATION, format!("Basic {token}"));
    }
    let request = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, headers, value)
}

fn authenticate(password: &str) -> String {
    BASE64.encode(format!("opencode:{password}"))
}

/// The registry row for `id`, when present.
fn row<'a>(body: &'a Value, id: &str) -> &'a Value {
    body["data"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == id)
        .unwrap_or_else(|| panic!("model {id} missing from registry: {body}"))
}

async fn seed(store: &Store, session_id: &str, messages: &[(String, Value)]) {
    store
        .create_session(NewSession {
            id: session_id.to_owned(),
            parent_id: None,
            project_id: "prj_models".to_owned(),
            directory: std::env::temp_dir().to_string_lossy().into_owned(),
            title: Some("models control plane".to_owned()),
            agent: Some("engineer".to_owned()),
            model: Some(json!({"providerID": "test", "id": "alpha"})),
            metadata: None,
            permissions: None,
            time_created: oc_types::now_millis(),
        })
        .await
        .unwrap();
    for (id, data) in messages {
        store
            .append_message(NewMessage {
                id: id.clone(),
                session_id: session_id.to_owned(),
                kind: data["type"].as_str().unwrap_or("assistant").to_owned(),
                data: data.clone(),
                time_created: oc_types::now_millis(),
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn registry_applies_hidden_favorite_and_note() {
    let harness = harness(
        json!({
            "hidden": ["test/beta"],
            "favorites": ["test/alpha"],
            "notes": {"test/alpha": "flat-rate tier"},
            "quota": {"default": {"max_cost_usd": 5.0}}
        }),
        None,
    )
    .await;
    let app = router(harness.state);
    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let alpha = row(&body, "alpha");
    assert_eq!(alpha["providerID"], "test");
    assert_eq!(alpha["favorite"], true);
    assert_eq!(alpha["visible"], true);
    assert_eq!(alpha["note"], "flat-rate tier");
    // `usage` is sparse: no assistant message was seeded, so the row has none.
    // The window the rollup would cover still rides on the top-level echo.
    assert_eq!(body["data"]["usage"]["windowHours"], 120);
    assert!(
        alpha.get("usage").is_none(),
        "a model with no usage rows omits `usage`: {alpha}"
    );
    assert_eq!(alpha["quota"]["maxCostUsd"], 5.0);

    let beta = row(&body, "beta");
    assert_eq!(beta["visible"], false, "hidden: {beta}");
    assert_eq!(beta["favorite"], false);
    assert!(
        beta.get("note").is_none(),
        "note omitted when absent: {beta}"
    );
}

#[tokio::test]
async fn put_unknown_ref_is_400_invalid_request_and_writes_nothing() {
    let harness = harness(json!({}), None).await;
    let global_file = harness.global_file.clone();
    let app = router(harness.state);

    let (status, _, body) = send(
        &app,
        Method::PUT,
        "/api/config/models",
        Some(json!({"notes": {"test/alpha": "kept"}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let before = std::fs::read(&global_file).unwrap();

    let (status, _, body) = send(
        &app,
        Method::PUT,
        "/api/config/models",
        Some(json!({"chain": ["nope/missing"]})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["_tag"], "InvalidRequestError");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("invalid_model: nope/missing"),
        "{body}"
    );
    assert_eq!(body["model"], "nope/missing");
    assert_eq!(
        std::fs::read(&global_file).unwrap(),
        before,
        "a rejected patch must not touch the file"
    );
}

#[tokio::test]
async fn put_valid_writes_models_and_quota_change_preserves_notes() {
    let harness = harness(json!({}), None).await;
    let global_file = harness.global_file.clone();
    let app = router(harness.state);

    let (status, headers, body) = send(
        &app,
        Method::PUT,
        "/api/config/models",
        Some(json!({"notes": {"test/alpha": "written first"}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The change is live: no restart banner, and the reload hint names what was
    // applied. `PATCH /api/experimental/config` still requires a restart.
    assert!(
        headers.get("x-opencode-restart").is_none(),
        "PUT /api/config/models must not ask for a restart: {headers:?}"
    );
    assert_eq!(headers.get("x-opencode-config-reloaded").unwrap(), "models");
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");

    let on_disk: Value = jsonc_parser::parse_to_serde_value(
        &std::fs::read_to_string(&global_file).unwrap(),
        &jsonc_parser::ParseOptions::default(),
    )
    .unwrap();
    assert_eq!(on_disk["models"]["notes"]["test/alpha"], "written first");

    // Only `quota` changes: the note written above must survive the deep merge.
    let (status, headers, body) = send(
        &app,
        Method::PUT,
        "/api/config/models",
        Some(json!({"quota": {"default": {"maxCostUsd": 5.0, "maxRequests": 100}}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(headers.get("x-opencode-restart").is_none(), "{headers:?}");
    assert_eq!(headers.get("x-opencode-config-reloaded").unwrap(), "models");

    let on_disk: Value = jsonc_parser::parse_to_serde_value(
        &std::fs::read_to_string(&global_file).unwrap(),
        &jsonc_parser::ParseOptions::default(),
    )
    .unwrap();
    assert_eq!(
        on_disk["models"]["notes"]["test/alpha"], "written first",
        "deep merge dropped the previous note: {on_disk}"
    );
    assert_eq!(on_disk["models"]["quota"]["default"]["max_cost_usd"], 5.0);
    assert_eq!(on_disk["models"]["quota"]["default"]["max_requests"], 100);
    assert_eq!(body["data"]["quota"]["default"]["maxCostUsd"], 5.0);
}

#[tokio::test]
async fn usage_models_rolls_up_assistant_messages_cost_desc() {
    let harness = harness(json!({}), None).await;
    let store = harness.state.store().unwrap().clone();
    let assistant =
        |provider: &str, id: &str, input: i64, output: i64, reasoning: i64, cost: f64| {
            json!({
                "model": {"providerID": provider, "id": id},
                "tokens": {"input": input, "output": output, "reasoning": reasoning},
                "cost": cost,
                "time": {"created": oc_types::now_millis()}
            })
        };
    seed(
        &store,
        "ses_models_a",
        &[
            (
                "msg_a1".to_owned(),
                assistant("test", "alpha", 10, 5, 0, 0.4),
            ),
            (
                "msg_a2".to_owned(),
                assistant("test", "alpha", 20, 5, 1, 0.6),
            ),
            (
                "msg_b1".to_owned(),
                assistant("test", "beta", 100, 200, 7, 2.0),
            ),
            (
                "msg_u1".to_owned(),
                json!({"type": "user", "time": {"created": oc_types::now_millis()}}),
            ),
        ],
    )
    .await;
    let app = router(harness.state);

    let (status, _, body) = send(
        &app,
        Method::GET,
        "/api/experimental/usage/models?days=7",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["windowDays"], 7);
    let rows = body["data"]["models"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "non-assistant rows are skipped: {body}");

    assert_eq!(rows[0]["id"], "beta", "cost-desc order: {body}");
    assert_eq!(rows[0]["providerID"], "test");
    assert_eq!(rows[0]["requests"], 1);
    assert_eq!(rows[0]["tokensInput"], 100);
    assert_eq!(rows[0]["tokensOutput"], 200);
    assert_eq!(rows[0]["tokensReasoning"], 7);
    assert_eq!(rows[0]["costUsd"], 2.0);

    assert_eq!(rows[1]["id"], "alpha");
    assert_eq!(rows[1]["requests"], 2);
    assert_eq!(rows[1]["tokensInput"], 30);
    assert_eq!(rows[1]["tokensOutput"], 10);
    assert_eq!(rows[1]["tokensReasoning"], 1);
    assert_eq!(rows[1]["costUsd"], 1.0);
}

#[tokio::test]
async fn all_three_routes_require_basic_auth() {
    let harness = harness(json!({}), Some("secret")).await;
    let app = router(harness.state);

    for (method, uri) in [
        (Method::GET, "/api/models/registry"),
        (Method::PUT, "/api/config/models"),
        (Method::GET, "/api/experimental/usage/models"),
    ] {
        let (status, _, body) = send(&app, method.clone(), uri, Some(json!({})), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: {body}");
        assert_eq!(body["_tag"], "UnauthorizedError", "{uri}");
    }

    let token = authenticate("secret");
    let (status, _, body) = send(
        &app,
        Method::GET,
        "/api/models/registry",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn put_chain_over_the_cap_is_400_and_writes_nothing() {
    let harness = harness(json!({}), None).await;
    let global_file = harness.global_file.clone();
    let app = router(harness.state);

    let chain: Vec<String> = (0..17).map(|_| "test/alpha".to_owned()).collect();
    let (status, _, body) = send(
        &app,
        Method::PUT,
        "/api/config/models",
        Some(json!({"chain": chain})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["_tag"], "InvalidRequestError");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("the limit is 16"),
        "{body}"
    );
    assert!(
        !global_file.exists(),
        "a rejected patch must not create the config file"
    );
}

#[tokio::test]
async fn put_then_get_reflects_the_new_models_block_without_restart() {
    // The loaded ConfigService is a startup snapshot and there is no watcher,
    // so the GET must read the written document: the UI re-fetches the
    // registry right after a save and must not see the PRE-PUT block.
    let harness = harness(json!({}), None).await;
    let app = router(harness.state.clone());

    let (status, _, body) = send(
        &app,
        Method::PUT,
        "/api/config/models",
        Some(json!({
            "chain": ["test/beta", "test/alpha"],
            "fallback": {"on": ["transport", "quota"], "maxHops": 1},
            "quota": {"reservePercent": 0, "default": {"maxRequests": 40}}
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["chain"], json!(["test/beta", "test/alpha"]));

    // No restart: the writer published the committed block to the live
    // `ConfigService` cell, which is exactly what the runtime re-reads per
    // spawn. Asserting the CELL (not just the echo) is the no-restart proof.
    let cell = harness.state.config().expect("config").models_section();
    assert_eq!(
        cell["chain"],
        json!(["test/beta", "test/alpha"]),
        "the config cell must hold the PUT block without a restart: {cell}"
    );

    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["chain"],
        json!(["test/beta", "test/alpha"]),
        "GET must reflect the PUT without a restart: {body}"
    );
    assert_eq!(
        body["data"]["fallback"],
        json!({"on": ["transport", "quota"], "maxHops": 1}),
        "{body}"
    );
    assert_eq!(body["data"]["quota"]["reservePercent"], 0, "{body}");
    assert_eq!(
        body["data"]["quota"]["default"]["maxRequests"], 40,
        "{body}"
    );
    assert_eq!(
        row(&body, "alpha")["quota"]["maxRequests"],
        40,
        "the default quota applies to every model: {body}"
    );
}

#[tokio::test]
async fn quota_limited_when_requests_cap_is_exceeded_but_cost_is_not() {
    // The spawn resolver refuses on EITHER cap. With a generous cost cap and an
    // exhausted request cap the registry must report `limited`, not "healthy".
    let harness = harness(
        json!({"quota": {"reserve_percent": 0, "default": {"max_requests": 2, "max_cost_usd": 1000.0}}}),
        None,
    )
    .await;
    let store = harness.state.store().unwrap().clone();
    let assistant = |id: &str| {
        json!({
            "model": {"providerID": "test", "id": id},
            "tokens": {"input": 1, "output": 1, "reasoning": 0},
            "cost": 0.01,
            "time": {"created": oc_types::now_millis()}
        })
    };
    seed(
        &store,
        "ses_models_quota",
        &[
            ("msg_q1".to_owned(), assistant("alpha")),
            ("msg_q2".to_owned(), assistant("alpha")),
            ("msg_q3".to_owned(), assistant("alpha")),
        ],
    )
    .await;
    let app = router(harness.state);

    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let quota = &row(&body, "alpha")["quota"];
    assert_eq!(quota["maxRequests"], 2, "{quota}");
    assert_eq!(quota["usedRequests"], 3, "{quota}");
    assert_eq!(
        quota["limited"], true,
        "3/2 requests must be limited despite the untouched cost cap: {quota}"
    );
    assert_eq!(quota["remainingPercent"], 0.0, "{quota}");
}

#[tokio::test]
async fn quota_remaining_percent_is_the_minimum_of_cost_and_requests() {
    // cost 50% used, requests 80% used, reserve 0 -> the tighter cap wins (20).
    let harness = harness(
        json!({"quota": {"reserve_percent": 0, "default": {"max_requests": 5, "max_cost_usd": 2.0}}}),
        None,
    )
    .await;
    let store = harness.state.store().unwrap().clone();
    let assistant = |cost: f64| {
        json!({
            "model": {"providerID": "test", "id": "alpha"},
            "tokens": {"input": 1, "output": 1, "reasoning": 0},
            "cost": cost,
            "time": {"created": oc_types::now_millis()}
        })
    };
    seed(
        &store,
        "ses_models_min",
        &[
            ("msg_m1".to_owned(), assistant(0.25)),
            ("msg_m2".to_owned(), assistant(0.25)),
            ("msg_m3".to_owned(), assistant(0.25)),
            ("msg_m4".to_owned(), assistant(0.25)),
        ],
    )
    .await;
    let app = router(harness.state);

    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let quota = &row(&body, "alpha")["quota"];
    assert_eq!(quota["usedCostUsd"], 1.0, "{quota}");
    assert_eq!(quota["usedRequests"], 4, "{quota}");
    assert_eq!(
        quota["remainingPercent"], 20.0,
        "min(cost 50%, requests 20%) = 20: {quota}"
    );
    assert_eq!(quota["limited"], false, "{quota}");
}

#[tokio::test]
async fn registry_usage_window_follows_the_quota_window() {
    // `models.quota.window_hours` is the rolling window the usage rollup is
    // compared against, so the echo must report the window it actually used
    // instead of a hardcoded 120.
    let configured = harness(json!({"quota": {"window_hours": 5}}), None).await;
    let app = router(configured.state);

    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["usage"]["windowHours"], 5);
    assert_eq!(body["data"]["quota"]["windowHours"], 5);
    // The window the per-model rollup covers is reported on the top-level echo;
    // a model with no rows has no `usage` object to carry it.
    assert!(
        row(&body, "alpha").get("usage").is_none(),
        "no seeded usage means no per-model usage: {body}"
    );

    // Without a configured quota window the documented 120 h default applies.
    let defaults = harness(json!({}), None).await;
    let app = router(defaults.state);
    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["usage"]["windowHours"], 120);
}

#[tokio::test]
async fn usage_models_clamps_days_window() {
    let harness = harness(json!({}), None).await;
    let app = router(harness.state);

    let (status, _, body) = send(
        &app,
        Method::GET,
        "/api/experimental/usage/models?days=0",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["windowDays"], 1);

    let (status, _, body) = send(
        &app,
        Method::GET,
        "/api/experimental/usage/models?days=999",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["windowDays"], 90);
}

#[tokio::test]
async fn put_writes_integer_reserve_percent_that_the_runtime_parser_accepts() {
    let harness = harness(json!({}), None).await;
    let global_file = harness.global_file.clone();
    let app = router(harness.state);

    // The old writer persisted this as `10.0`. `oc-runtime`'s `parse_quota`
    // reads the field with an integer reader, got `None` for the float, emitted
    // `ModelsConfigError::ReservePercent`, and `prepare.rs` turned that into a
    // hard failure for EVERY subagent spawn. A saved quota must therefore be
    // written as an integer, and a float body from an older client must be
    // normalized rather than persisted verbatim.
    let (status, _, body) = send(
        &app,
        Method::PUT,
        "/api/config/models",
        Some(json!({"quota": {"reservePercent": 10.0, "windowHours": 5.0}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let on_disk: Value = jsonc_parser::parse_to_serde_value(
        &std::fs::read_to_string(&global_file).unwrap(),
        &jsonc_parser::ParseOptions::default(),
    )
    .unwrap();
    let quota = &on_disk["models"]["quota"];
    let reserve = &quota["reserve_percent"];
    assert_eq!(
        reserve.as_u64(),
        Some(10),
        "reserve_percent must serialize as an integer: {on_disk:#}"
    );
    assert!(
        reserve.as_f64().is_some_and(|value| value.fract() == 0.0),
        "reserve_percent must have no fractional part: {reserve}"
    );
    assert_eq!(
        quota["window_hours"].as_u64(),
        Some(5),
        "window_hours must serialize as an integer: {on_disk:#}"
    );

    // An integer body keeps the same integer spelling.
    let (status, _, body) = send(
        &app,
        Method::PUT,
        "/api/config/models",
        Some(json!({"quota": {"reservePercent": 25}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let on_disk: Value = jsonc_parser::parse_to_serde_value(
        &std::fs::read_to_string(&global_file).unwrap(),
        &jsonc_parser::ParseOptions::default(),
    )
    .unwrap();
    assert_eq!(
        on_disk["models"]["quota"]["reserve_percent"].as_u64(),
        Some(25)
    );
}

/// A runtime whose model layer is never called: the test only marks and
/// clears entries on the runtime-owned [`oc_llm::ModelHealth`] table.
#[derive(Debug)]
struct HealthOnlyModel;

impl oc_runtime::ModelClient for HealthOnlyModel {
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

/// Attach a runtime so `AppState::model_health()` / `key_pool()` have an owner,
/// and return the resulting router plus the shared health and key tables.
fn attach_runtime(
    state: &AppState,
) -> (axum::Router, Arc<oc_llm::ModelHealth>, Arc<oc_llm::KeyPool>) {
    let config = Arc::clone(state.config().expect("config"));
    let catalog = state.catalog().expect("catalog");
    let store = Arc::clone(state.store().expect("store"));
    let broker = Arc::new(oc_server::PermissionBroker::from_state(state));
    let sink = Arc::new(oc_server::BusSink::from_state(state));
    let runtime = Arc::new(oc_runtime::Runtime::new_with_model_client(
        oc_runtime::RuntimeConfig {
            store,
            config,
            catalog: Arc::new(oc_llm::CatalogCell::from_arc(catalog)),
            client: Arc::new(oc_llm::SharedLlmClient::new(
                oc_llm::LlmClient::new().expect("client"),
            )),
            tools: Arc::new(oc_tools::ToolRegistry::with_defaults()),
            sink,
            permissions: broker.clone(),
        },
        Arc::new(HealthOnlyModel),
    ));
    let health = Arc::clone(runtime.model_health());
    let pool = Arc::clone(runtime.key_pool());
    let state = state.clone().with_runtime(runtime).with_permissions(broker);
    (router(state), health, pool)
}

#[tokio::test]
async fn registry_reports_runtime_model_health_and_drops_it_when_cleared() {
    let harness = harness(json!({}), None).await;
    let (app, health, _pool) = attach_runtime(&harness.state);

    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        row(&body, "alpha").get("unavailable").is_none(),
        "a healthy model omits `unavailable`: {body}"
    );
    assert!(
        body["data"].get("modelHealth").is_none(),
        "an empty health table omits `modelHealth` (never null): {body}"
    );

    health.mark(
        "test/alpha",
        oc_llm::FailureKind::ModelUnavailable,
        oc_types::now_millis(),
        Some("no longer available".to_owned()),
    );

    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let alpha = row(&body, "alpha");
    assert_eq!(alpha["unavailable"]["kind"], "model_unavailable", "{body}");
    assert!(
        alpha["unavailable"]["untilMs"].as_i64().is_some(),
        "untilMs is epoch ms: {body}"
    );
    assert_eq!(
        alpha["unavailable"]["message"], "no longer available",
        "{body}"
    );
    assert!(
        row(&body, "beta").get("unavailable").is_none(),
        "only the marked model is unavailable: {body}"
    );
    let entries = body["data"]["modelHealth"].as_array().expect("entries");
    assert_eq!(entries.len(), 1, "{body}");
    assert_eq!(entries[0]["providerID"], "test");
    assert_eq!(entries[0]["id"], "alpha");
    assert_eq!(entries[0]["reference"], "test/alpha");
    assert_eq!(entries[0]["kind"], "model_unavailable");
    assert!(entries[0]["untilMs"].as_i64().is_some(), "{body}");

    // Unmarking is what a config reload/repair does; both surfaces go away.
    health.clear();
    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(row(&body, "alpha").get("unavailable").is_none(), "{body}");
    assert!(body["data"].get("modelHealth").is_none(), "{body}");
}

#[tokio::test]
async fn registry_reports_key_health_and_drops_it_when_the_key_recovers() {
    let harness = harness(json!({}), None).await;
    let (app, _health, pool) = attach_runtime(&harness.state);

    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"].get("keyHealth").is_none(),
        "a clean key pool omits `keyHealth` (never null): {body}"
    );

    let now = oc_types::now_millis();
    pool.mark("test", Some("cred_alpha"), oc_llm::FailureKind::Quota, now);

    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = body["data"]["keyHealth"]
        .as_array()
        .unwrap_or_else(|| panic!("expected keyHealth: {body}"));
    assert_eq!(entries.len(), 1, "{body}");
    assert_eq!(entries[0]["providerID"], "test");
    assert_eq!(entries[0]["credentialID"], "cred_alpha");
    assert_eq!(entries[0]["kind"], "quota");
    assert_eq!(entries[0]["failures"], 1);
    assert!(
        entries[0]["untilMs"]
            .as_i64()
            .is_some_and(|until| until > now),
        "untilMs is a future epoch ms: {body}"
    );
    // A cooling key must not hide the model it serves: the credential axis and
    // the model axis are separate.
    assert!(
        row(&body, "alpha").get("unavailable").is_none(),
        "a key cooldown must not retire a model: {body}"
    );
    assert!(body["data"].get("modelHealth").is_none(), "{body}");

    // An environment key has no stored credential, so `credentialID` is omitted.
    pool.mark("test", None, oc_llm::FailureKind::Auth, now);
    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = body["data"]["keyHealth"].as_array().expect("entries");
    let env_entry = entries
        .iter()
        .find(|entry| entry.get("credentialID").is_none())
        .unwrap_or_else(|| panic!("expected an env-key entry: {body}"));
    assert_eq!(env_entry["kind"], "auth", "{body}");

    pool.clear();
    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"].get("keyHealth").is_none(), "{body}");
}

#[tokio::test]
async fn put_notes_only_preserves_the_existing_chain() {
    let harness = harness(json!({}), None).await;
    let global_file = harness.global_file.clone();
    let app = router(harness.state.clone());

    let (status, _, body) = send(
        &app,
        Method::PUT,
        "/api/config/models",
        Some(json!({"chain": ["test/beta", "test/alpha"]})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, _, body) = send(
        &app,
        Method::PUT,
        "/api/config/models",
        Some(json!({"notes": {"test/alpha": "kept"}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["chain"],
        json!(["test/beta", "test/alpha"]),
        "a notes-only patch must deep-merge, not replace: {body}"
    );

    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["chain"], json!(["test/beta", "test/alpha"]));
    assert_eq!(row(&body, "alpha")["note"], "kept", "{body}");

    let on_disk: Value = jsonc_parser::parse_to_serde_value(
        &std::fs::read_to_string(&global_file).unwrap(),
        &jsonc_parser::ParseOptions::default(),
    )
    .unwrap();
    assert_eq!(
        on_disk["models"]["chain"],
        json!(["test/beta", "test/alpha"]),
        "deep merge dropped the chain: {on_disk:#}"
    );
}

/// A catalog snapshot with `count` synthetic models under one provider.
///
/// The registry size budget (`docs/15-performance.md`: < 200 KB for 700 models)
/// needs a body that is actually large, so the size test builds one rather than
/// asserting against the two-model `FIXTURE`.
fn large_fixture(count: usize) -> String {
    let mut models = serde_json::Map::new();
    for index in 0..count {
        let id = format!("model-{index:03}");
        models.insert(
            id.clone(),
            json!({
                "id": id,
                "type": "chat",
                "name": format!("Synthetic Model {index:03}"),
                "release_date": "2025-01-02",
                "modalities": {"input": ["text"], "output": ["text"]},
                "capabilities": {"tools": {"supported": true}},
                "limit": {"context": 200_000, "output": 32_000},
                "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
                "api": {"chat-completions": {
                    "base_url": "http://unused.invalid",
                    "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
                }}
            }),
        );
    }
    json!({"bulk": {"id": "bulk", "name": "Bulk Provider", "models": models}}).to_string()
}

/// The registry must stay inside the documented budget: < 200 KB for 700
/// models (`docs/15-performance.md`, contract-v2 P0-E).
#[tokio::test]
async fn registry_body_for_700_models_is_under_200_kb() {
    let harness = large_harness(700, json!({})).await;
    let app = router(harness.state);

    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"]["models"].as_array().unwrap().len(),
        700,
        "the fixture must actually produce 700 rows"
    );

    let bytes = serde_json::to_vec(&body).unwrap().len();
    assert!(
        bytes < 200 * 1024,
        "registry body is {bytes} bytes, over the 200 KB budget"
    );

    // The saving comes from sparse `usage`: 700 rows with no events must emit
    // none of them, and the top-level window echo must still be present.
    for model in body["data"]["models"].as_array().unwrap() {
        assert!(
            model.get("usage").is_none(),
            "a model with no usage rows must omit `usage`: {model}"
        );
    }
    assert_eq!(body["data"]["usage"]["windowHours"], 120);
}

/// `?usage=0` skips the rollup for the first paint, so every per-model `usage`
/// object is omitted — including a model that really has rows. The default and
/// `?usage=1` keep the sparse rollup: a model with rows carries `usage`, a model
/// without rows omits it.
#[tokio::test]
async fn usage_query_parameter_controls_the_per_model_rollup() {
    let harness = harness(json!({}), None).await;
    let store = harness.state.store().unwrap().clone();
    seed(
        &store,
        "ses_usage_param",
        &[(
            "msg_u1".to_owned(),
            json!({
                "model": {"providerID": "test", "id": "alpha"},
                "tokens": {"input": 10, "output": 5, "reasoning": 0},
                "cost": 0.25,
                "time": {"created": oc_types::now_millis()}
            }),
        )],
    )
    .await;
    let app = router(harness.state);

    // Default (no parameter): the model with rows keeps `usage`, the one without
    // rows omits it. Both are sparse, never zero-filled.
    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let alpha = row(&body, "alpha");
    assert_eq!(alpha["usage"]["requests"], 1, "{alpha}");
    assert_eq!(alpha["usage"]["windowHours"], 120, "{alpha}");
    assert_eq!(alpha["usage"]["costUsd"], 0.25, "{alpha}");
    assert!(
        row(&body, "beta").get("usage").is_none(),
        "a zero-usage model must omit `usage`: {body}"
    );

    // `?usage=0`: no per-model usage at all, but the quota rollup still ran.
    let (status, _, body) = send(
        &app,
        Method::GET,
        "/api/models/registry?usage=0",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for model in body["data"]["models"].as_array().unwrap() {
        assert!(
            model.get("usage").is_none(),
            "`?usage=0` must omit `usage` for every model: {model}"
        );
    }
    assert_eq!(
        body["data"]["usage"]["windowHours"], 120,
        "the window echo is not part of the per-model rollup: {body}"
    );

    // `?usage=1` is the explicit full-body form.
    let (status, _, body) = send(
        &app,
        Method::GET,
        "/api/models/registry?usage=1",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(row(&body, "alpha")["usage"]["requests"], 1, "{body}");
}

/// `fallbackDisabled` lists only the opt-in classes an explicit
/// `models.fallback.on` switched off, and is omitted once nothing is missing.
///
/// The recovery classes (`rate_limit`, `quota`) advance the chain whatever the
/// document says, so they can never appear in the list. The default set is
/// complete, so a document without `models.fallback` omits the field entirely.
#[tokio::test]
async fn fallback_disabled_lists_only_the_opted_out_classes() {
    // An explicit `on` naming both opt-in classes switches nothing off.
    let opted_in = harness(
        json!({"fallback": {"on": ["empty_response", "transport"]}}),
        None,
    )
    .await;
    let app = router(opted_in.state);
    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["fallback"]["on"],
        json!(["empty_response", "transport"])
    );
    assert!(
        body["data"].get("fallbackDisabled").is_none(),
        "a complete opt-in set disables nothing: {body}"
    );

    // Naming only the recovery classes switches both opt-in classes off, in
    // `models.fallback.on` payload order.
    let recovery_only = harness(json!({"fallback": {"on": ["rate_limit", "quota"]}}), None).await;
    let app = router(recovery_only.state);
    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["fallbackDisabled"],
        json!(["empty_response", "transport"]),
        "only the opt-in classes can be switched off: {body}"
    );

    // Naming all four leaves nothing disabled: the field is absent, never `[]`.
    let all_four = harness(
        json!({"fallback": {"on": ["empty_response", "transport", "rate_limit", "quota"]}}),
        None,
    )
    .await;
    let app = router(all_four.state);
    let (status, _, body) = send(&app, Method::GET, "/api/models/registry", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"].get("fallbackDisabled").is_none(),
        "an all-four set disables nothing: {body}"
    );
    assert_eq!(
        body["data"]["fallback"]["on"].as_array().unwrap().len(),
        4,
        "the raw configured list round-trips: {body}"
    );
}
