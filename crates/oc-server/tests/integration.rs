//! Integration and credential routes through the real router.
//!
//! Every assertion is a documented status or shape: credentials persist in
//! `oc-store`, the integration registry is derived from configured providers and
//! remote MCP servers, and the OAuth/command/well-known routes report their
//! missing engines truthfully instead of faking success.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use oc_config::{ConfigService, LoadOptions};
use oc_llm::Catalog;
use oc_mcp::McpConfig;
use oc_server::data::mcp::McpManager;
use oc_server::{AppState, router};
use oc_store::Store;
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    _db: tempfile::TempDir,
    _config: tempfile::TempDir,
    state: AppState,
}

async fn harness() -> Harness {
    let db = tempfile::tempdir().unwrap();
    let store = Store::open(db.path().join("data.sqlite")).await.unwrap();

    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        config_dir.path().join("opencode.json"),
        r#"{
            "model": "test/model",
            "providers": {
                "test": {"name": "Test Provider", "env": ["OC_TEST_PROVIDER_KEY"]},
                "off": {"name": "Off Provider", "disabled": true}
            }
        }"#,
    )
    .unwrap();
    let home = config_dir.path().to_string_lossy().into_owned();
    let mut env = HashMap::new();
    env.insert("HOME".to_owned(), home.clone());
    env.insert("USERPROFILE".to_owned(), home.clone());
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        config_dir.path().join("xdg").to_string_lossy().into_owned(),
    );
    // Shadow ambient inline config; whitespace is ignored by the loader.
    env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());
    let config = ConfigService::load(LoadOptions {
        directory: config_dir.path().to_path_buf(),
        env,
    })
    .unwrap();

    let mcp = McpConfig::from_value(&json!({
        "servers": {
            "demo": {"type": "remote", "url": "https://mcp.invalid/mcp"},
            "plain": {"type": "local", "command": ["definitely-not-spawned-mcp"]}
        }
    }))
    .unwrap();

    // Providers the catalog knows; `test` is also configured, so its row must
    // take the configured name and env list, while `catalog-only` is unconfigured.
    // Each model needs a resolvable `api` route or `Catalog::from_snapshot`
    // drops the provider.
    let catalog = Catalog::from_snapshot(
        r#"{
            "test": {
                "name": "Snapshot Test",
                "env": ["OC_TEST_SNAPSHOT_KEY"],
                "models": {
                    "snapshot": {
                        "id": "snapshot",
                        "api": {
                            "chat-completions": {
                                "base_url": "https://mock.invalid/v1",
                                "sdk": {"opencodeai": "@opencode/ai/providers/snapshot/chat"}
                            }
                        }
                    }
                }
            },
            "catalog-only": {
                "name": "Catalog Only",
                "env": ["OC_TEST_CATALOG_ONLY_KEY_UNSET"],
                "models": {
                    "mock": {
                        "id": "mock",
                        "api": {
                            "chat-completions": {
                                "base_url": "https://mock.invalid/v1",
                                "sdk": {"opencodeai": "@opencode/ai/providers/mock/chat"}
                            }
                        }
                    }
                }
            }
        }"#,
    )
    .unwrap();

    let state = AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: Some("127.0.0.1".to_owned()),
            urls: vec!["http://127.0.0.1:4096".to_owned()],
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        config_dir.path().to_path_buf(),
    )
    .with_store(Arc::new(store))
    .with_config(Arc::new(config))
    .with_catalog(Arc::new(catalog))
    .with_mcp(Arc::new(McpManager::new(mcp, std::env::temp_dir())));

    Harness {
        _db: db,
        _config: config_dir,
        state,
    }
}

async fn call(
    app: &axum::Router,
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
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let value = if bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&bytes).unwrap())
    };
    (status, value)
}

async fn credentials(app: &axum::Router) -> Vec<Value> {
    let (status, body) = call(app, Method::GET, "/api/credential", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    body.unwrap()["data"].as_array().unwrap().clone()
}

/// The derived integration id of the remote MCP server named `demo`.
async fn demo_integration_id(app: &axum::Router) -> String {
    let (status, body) = call(app, Method::GET, "/api/integration", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let body = body.unwrap();
    let rows = body["data"].as_array().unwrap();
    let row = rows
        .iter()
        .find(|row| row["metadata"]["source"] == "mcp")
        .expect("remote MCP integration row");
    row["id"].as_str().unwrap().to_owned()
}

/// Provider ids in a `/api/provider` response.
fn provider_ids(body: &Value) -> Vec<String> {
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|provider| provider["id"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn credential_lifecycle() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/credential",
        Some(json!({
            "id": "cred_test",
            "integrationID": "test",
            "label": "mine",
            "value": {"type": "key", "key": "sk-1"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let entry = body.unwrap()["data"].clone();
    assert_eq!(entry["id"], "cred_test");
    assert_eq!(entry["integrationID"], "test");
    assert_eq!(entry["label"], "mine");
    assert_eq!(entry["active"], json!(true));
    assert_eq!(entry["value"], json!({"type": "key", "key": "sk-1"}));

    let listed = credentials(&app).await;
    assert_eq!(listed.len(), 1, "{listed:?}");

    let (status, body) = call(&app, Method::GET, "/api/credential/cred_test", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.unwrap()["data"]["id"], "cred_test");

    let (status, body) = call(
        &app,
        Method::PATCH,
        "/api/credential/cred_test",
        Some(json!({"label": "renamed"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");
    assert!(body.is_none());

    let (_, body) = call(&app, Method::GET, "/api/credential/cred_test", None).await;
    assert_eq!(body.unwrap()["data"]["label"], "renamed");

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/credential/cred_test/activate",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let (status, body) = call(&app, Method::DELETE, "/api/credential/cred_test", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");
    assert!(body.is_none());

    let (status, body) = call(&app, Method::GET, "/api/credential/cred_test", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "NotFoundError");

    // Unknown credentials are the upstream no-op for update, activate and
    // remove; only the extra `GET` reports the missing row.
    for (method, uri) in [
        (Method::DELETE, "/api/credential/cred_missing"),
        (Method::POST, "/api/credential/cred_missing/activate"),
    ] {
        let (status, body) = call(&app, method, uri, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{uri}: {body:?}");
    }
    let (status, _) = call(
        &app,
        Method::PATCH,
        "/api/credential/cred_missing",
        Some(json!({"label": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn activate_switches_the_integration_selection() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    for (id, activate) in [("cred_a", None), ("cred_b", Some(false))] {
        let mut body = json!({
            "id": id,
            "integrationID": "test",
            "value": {"type": "key", "key": "sk"},
        });
        if let Some(activate) = activate {
            body["activate"] = json!(activate);
        }
        let (status, response) = call(&app, Method::POST, "/api/credential", Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{response:?}");
    }

    let flags: Vec<(String, bool)> = credentials(&app)
        .await
        .iter()
        .map(|entry| {
            (
                entry["id"].as_str().unwrap().to_owned(),
                entry["active"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        flags,
        vec![("cred_a".to_owned(), true), ("cred_b".to_owned(), false)]
    );

    let (status, _) = call(&app, Method::POST, "/api/credential/cred_b/activate", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let flags: Vec<(String, bool)> = credentials(&app)
        .await
        .iter()
        .map(|entry| {
            (
                entry["id"].as_str().unwrap().to_owned(),
                entry["active"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        flags,
        vec![("cred_a".to_owned(), false), ("cred_b".to_owned(), true)]
    );
}

#[tokio::test]
async fn credential_create_conflicts_and_schema_errors() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let body = json!({
        "id": "cred_dup",
        "integrationID": "test",
        "value": {"type": "key", "key": "sk"}
    });
    let (status, _) = call(&app, Method::POST, "/api/credential", Some(body.clone())).await;
    assert_eq!(status, StatusCode::OK);

    let (status, response) = call(&app, Method::POST, "/api/credential", Some(body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let body = response.unwrap();
    assert_eq!(body["_tag"], "ConflictError");
    assert_eq!(body["resource"], "cred_dup");

    for invalid in [
        json!({"integrationID": "test", "value": {"type": "nope"}}),
        json!({"integrationID": "test"}),
        json!({"integrationID": "test", "value": {"type": "key", "key": "sk"}, "extra": 1}),
        json!({"integrationID": "test", "value": {"type": "key", "key": "sk", "extra": 1}}),
        json!({"integrationID": "test", "value": {"type": "key", "key": 1}}),
        json!({"integrationID": "test", "value": "sk"}),
        json!({"value": {"type": "key", "key": "sk"}}),
    ] {
        let (status, response) = call(&app, Method::POST, "/api/credential", Some(invalid)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response:?}");
        assert_eq!(response.unwrap()["_tag"], "SchemaError");
    }

    let (status, response) = call(
        &app,
        Method::PATCH,
        "/api/credential/cred_dup",
        Some(json!({"label": "x", "value": "nope"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response.unwrap()["_tag"], "SchemaError");
}

#[tokio::test]
async fn oauth_and_external_values_round_trip() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/credential",
        Some(json!({
            "integrationID": "demo",
            "value": {
                "type": "oauth",
                "methodID": "m1",
                "refresh": "r",
                "access": "a",
                "expires": 1234,
                "metadata": {"scope": "read"}
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let entry = body.unwrap()["data"].clone();
    assert_eq!(
        entry["value"],
        json!({
            "type": "oauth",
            "methodID": "m1",
            "refresh": "r",
            "access": "a",
            "expires": 1234,
            "metadata": {"scope": "read"}
        })
    );

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/credential",
        Some(json!({
            "integrationID": "demo",
            "value": {"type": "external", "methodID": "m2"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let entry = body.unwrap()["data"].clone();
    assert_eq!(
        entry["value"],
        json!({"type": "external", "methodID": "m2"})
    );
    assert!(
        entry["value"].get("metadata").is_none(),
        "absent metadata must be omitted: {entry:?}"
    );
}

#[tokio::test]
async fn integration_list_derives_from_providers_and_mcp() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(&app, Method::GET, "/api/integration", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let body = body.unwrap();
    assert!(body["location"]["directory"].is_string(), "{body:?}");
    let rows = body["data"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(
        !rows.iter().any(|row| row["id"] == "off"),
        "disabled provider must not be listed: {rows:?}"
    );

    let provider = rows
        .iter()
        .find(|row| row["id"] == "test")
        .expect("configured provider row");
    assert_eq!(provider["name"], "Test Provider");
    let methods = provider["methods"].as_array().unwrap();
    assert!(
        methods
            .iter()
            .any(|method| method["type"] == "key" && method["label"] == "Manually enter API Key"),
        "{methods:?}"
    );
    assert!(
        methods
            .iter()
            .any(|method| method["type"] == "env"
                && method["names"] == json!(["OC_TEST_PROVIDER_KEY"])),
        "{methods:?}"
    );

    // The catalog provider is present even though nothing configures it.
    let catalog_only = rows
        .iter()
        .find(|row| row["id"] == "catalog-only")
        .expect("catalog provider row");
    assert_eq!(catalog_only["name"], "Catalog Only");
    let methods = catalog_only["methods"].as_array().unwrap();
    assert_eq!(methods.len(), 2, "{methods:?}");
    assert_eq!(methods[0]["type"], "key");
    assert_eq!(methods[1]["type"], "env");
    assert_eq!(
        methods[1]["names"],
        json!(["OC_TEST_CATALOG_ONLY_KEY_UNSET"])
    );

    let mcp = rows
        .iter()
        .find(|row| row["metadata"]["source"] == "mcp")
        .expect("remote MCP row");
    assert_eq!(mcp["name"], "demo");
    assert!(mcp["id"].as_str().unwrap().starts_with("mcp_"), "{mcp:?}");
    let methods = mcp["methods"].as_array().unwrap();
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0]["type"], "oauth");
    assert_eq!(methods[0]["id"], mcp["id"]);

    let (status, body) = call(&app, Method::GET, "/api/integration/test", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.unwrap()["data"]["id"], "test");

    let (status, body) = call(&app, Method::GET, "/api/integration/unknown", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let body = body.unwrap();
    assert_eq!(body["_tag"], "IntegrationNotFoundError");
    assert_eq!(body["integrationID"], "unknown");
}

#[tokio::test]
async fn catalog_provider_connects_and_becomes_available() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    // Present in the registry and connectable before any credential exists.
    let (status, body) = call(&app, Method::GET, "/api/integration/catalog-only", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let row = body.unwrap()["data"].clone();
    assert_eq!(row["name"], "Catalog Only");
    assert!(
        row["methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|method| method["type"] == "key"),
        "{row:?}"
    );

    // Not offered by /api/provider yet: no config, credential or env var.
    let (status, body) = call(&app, Method::GET, "/api/provider", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let available = provider_ids(&body.unwrap());
    assert!(
        !available.iter().any(|id| id == "catalog-only"),
        "{available:?}"
    );

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/integration/catalog-only/connect/key",
        Some(json!({"key": "sk-catalog"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let entries = credentials(&app).await;
    assert!(
        entries
            .iter()
            .any(|entry| entry["integrationID"] == "catalog-only"
                && entry["value"]["key"] == "sk-catalog"),
        "{entries:?}"
    );

    // The stored credential makes the provider available to the model routes.
    let (status, body) = call(&app, Method::GET, "/api/provider", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let available = provider_ids(&body.unwrap());
    assert!(
        available.iter().any(|id| id == "catalog-only"),
        "{available:?}"
    );

    let (status, body) = call(&app, Method::GET, "/api/provider/catalog-only", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.unwrap()["data"]["id"], "catalog-only");

    // And the connection is visible on the integration row.
    let (_, body) = call(&app, Method::GET, "/api/integration/catalog-only", None).await;
    let connections = body.unwrap()["data"]["connections"].clone();
    let first = &connections.as_array().unwrap()[0];
    assert_eq!(first["type"], "credential");
    assert_eq!(first["method"], "key");
}

#[tokio::test]
async fn connect_key_stores_a_credential() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/integration/test/connect/key",
        Some(json!({"key": "sk-live"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let entries = credentials(&app).await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["integrationID"], "test");
    // The first unnamed credential takes the integration name, like upstream.
    assert_eq!(entries[0]["label"], "Test Provider");
    assert_eq!(
        entries[0]["value"],
        json!({"type": "key", "key": "sk-live"})
    );

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/integration/test/connect/key",
        Some(json!({"key": "sk-live-2", "label": "second"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let labels: Vec<String> = credentials(&app)
        .await
        .iter()
        .map(|entry| entry["label"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        labels.iter().any(|label| label == "Test Provider"),
        "{labels:?}"
    );
    assert!(labels.iter().any(|label| label == "second"), "{labels:?}");

    // The connection shows up on the integration, newest first.
    let (_, body) = call(&app, Method::GET, "/api/integration/test", None).await;
    let connections = body.unwrap()["data"]["connections"].clone();
    let first = &connections.as_array().unwrap()[0];
    assert_eq!(first["type"], "credential");
    assert_eq!(first["label"], "second");
    assert_eq!(first["method"], "key");

    // A non-empty answer has nowhere to go: the key method has no form.
    let (status, body) = call(
        &app,
        Method::POST,
        "/api/integration/test/connect/key",
        Some(json!({"key": "sk", "answer": {"a": "b"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["_tag"], "InvalidRequestError");

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/integration/missing/connect/key",
        Some(json!({"key": "sk"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "IntegrationNotFoundError");

    // The remote MCP server registers only an OAuth method.
    let mcp_id = demo_integration_id(&app).await;
    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/integration/{mcp_id}/connect/key"),
        Some(json!({"key": "sk"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "InvalidRequestError");

    // A malformed body is a schema error, not a silent success.
    let (status, body) = call(
        &app,
        Method::POST,
        "/api/integration/test/connect/key",
        Some(json!({"key": "sk", "unknown": true})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["_tag"], "SchemaError");
}

#[tokio::test]
async fn connect_external_needs_a_registered_method() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/integration/test/connect/external",
        Some(json!({"methodID": "ext-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    let body = body.unwrap();
    assert_eq!(body["_tag"], "InvalidRequestError");
    assert_eq!(body["kind"], "integration_authorization");

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/integration/missing/connect/external",
        Some(json!({"methodID": "ext-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "IntegrationNotFoundError");
}

#[tokio::test]
async fn oauth_and_command_routes_report_missing_engines() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let mcp_id = demo_integration_id(&app).await;

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/integration/{mcp_id}/connect/oauth"),
        Some(json!({"methodID": mcp_id})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "NotImplementedError");

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/integration/{mcp_id}/connect/oauth"),
        Some(json!({"methodID": "other"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "InvalidRequestError");

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/integration/{mcp_id}/connect/oauth/att_1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let body = body.unwrap();
    assert_eq!(body["_tag"], "IntegrationAttemptNotFoundError");
    assert_eq!(body["integrationID"], mcp_id);
    assert_eq!(body["attemptID"], "att_1");

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/integration/{mcp_id}/connect/oauth/att_1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/integration/{mcp_id}/connect/oauth/att_1/complete"),
        Some(json!({"code": "abc"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "IntegrationAttemptNotFoundError");

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/integration/test/connect/command",
        Some(json!({"methodID": "cmd-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "InvalidRequestError");

    let (status, body) = call(
        &app,
        Method::GET,
        "/api/integration/test/connect/command/att_2",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "IntegrationAttemptNotFoundError");

    let (status, _) = call(
        &app,
        Method::DELETE,
        "/api/integration/test/connect/command/att_2",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn wellknown_discovery_is_unconfigured() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(
        &app,
        Method::GET,
        "/api/experimental/integration/wellknown",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "ServiceUnavailableError");

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/experimental/integration/wellknown",
        Some(json!({"url": "https://example.com/.well-known/opencode"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "ServiceUnavailableError");

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/experimental/integration/wellknown",
        Some(json!({"url": "file:///etc/passwd"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["_tag"], "InvalidRequestError");

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/experimental/integration/wellknown",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["_tag"], "SchemaError");
}

#[tokio::test]
async fn credential_store_absence_is_503() {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: Some("127.0.0.1".to_owned()),
            urls: vec!["http://127.0.0.1:4096".to_owned()],
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        directory.path().to_path_buf(),
    );
    let app = router(state);
    let (status, body) = call(&app, Method::GET, "/api/credential", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body.unwrap()["_tag"], "ServiceUnavailableError");

    // The registry is readable without a store: there is nothing to list, and an
    // unknown id is still the documented 404.
    let (status, body) = call(&app, Method::GET, "/api/integration", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.unwrap()["data"], json!([]));

    let (status, body) = call(&app, Method::GET, "/api/integration/none", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "IntegrationNotFoundError");
}
