//! Model/provider route tests with a tiny three-provider catalog fixture.
//!
//! The fixture separates the three availability sources: `test` is configured
//! in the `providers` map, `other` has a stored credential, and `third` is
//! reachable through environment variables that every supported platform sets
//! (`PATH` plus a platform home variable). A fourth provider created by
//! configuration is disabled and must never surface.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use oc_config::{ConfigService, LoadOptions};
use oc_llm::Catalog;
use oc_server::{AppState, router};
use oc_store::{NewCredential, Store};
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tower::ServiceExt;

const FIXTURE: &str = r#"{
  "test": {
    "id": "test",
    "name": "Test Provider",
    "env": ["TEST_API_KEY"],
    "models": {
      "alpha": {
        "id": "alpha", "type": "chat", "name": "Alpha",
        "release_date": "2025-01-02",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}, "temperature": true},
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
        "cost": {"text": {"input": {"price": 1}, "output": {"price": 1}}},
        "api": {"chat-completions": {"base_url": "http://unused.invalid", "sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
      },
      "pic": {
        "id": "pic", "type": "image", "name": "Pic",
        "release_date": "2025-03-02",
        "modalities": {"input": ["text"], "output": ["image"]},
        "capabilities": {"tools": {"supported": false}},
        "limit": {"context": 1000, "output": 1000},
        "api": {"chat-completions": {"base_url": "http://unused.invalid", "sdk": {"aisdk": "@ai-sdk/openai/images"}}}
      }
    }
  },
  "other": {
    "id": "other",
    "name": "Other Provider",
    "models": {
      "gamma": {
        "id": "gamma", "type": "chat", "name": "Gamma",
        "release_date": "2025-04-02",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}},
        "limit": {"context": 4000, "output": 1000},
        "api": {"chat-completions": {"base_url": "http://unused.invalid", "sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
      }
    }
  },
  "third": {
    "id": "third",
    "name": "Third Provider",
    "env": ["PATH", "HOME", "USERPROFILE", "SystemRoot"],
    "models": {
      "delta": {
        "id": "delta", "type": "chat", "name": "Delta",
        "release_date": "2025-05-02",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}},
        "limit": {"context": 2000, "output": 500},
        "api": {"chat-completions": {"base_url": "http://unused.invalid", "sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
      }
    }
  }
}"#;

struct Harness {
    _db: tempfile::TempDir,
    _config: tempfile::TempDir,
    state: AppState,
}

async fn harness(model: Option<&str>, catalog: bool) -> Harness {
    let db = tempfile::tempdir().unwrap();
    let store = Store::open(db.path().join("data.sqlite")).await.unwrap();
    store
        .create_credential(NewCredential {
            id: Some("cred_other".to_owned()),
            integration_id: "other".to_owned(),
            label: None,
            value: json!({"type": "key", "key": "sk-other"}),
            active: Some(true),
        })
        .await
        .unwrap();

    let config_dir = tempfile::tempdir().unwrap();
    let mut document = json!({
        "providers": {
            "test": {"name": "Test Provider", "env": ["TEST_API_KEY"]},
            "off": {
                "name": "Off Provider",
                "disabled": true,
                "package": "@ai-sdk/openai-compatible",
                "settings": {"baseURL": "http://off.invalid"},
                "models": {
                    "omega": {
                        "name": "Omega",
                        "type": "chat",
                        "limit": {"context": 1024, "output": 128}
                    }
                }
            }
        }
    });
    if let Some(reference) = model {
        document["model"] = json!(reference);
    }
    std::fs::write(
        config_dir.path().join("opencode.json"),
        document.to_string(),
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
    let config = Arc::new(
        ConfigService::load(LoadOptions {
            directory: config_dir.path().to_path_buf(),
            env,
        })
        .unwrap(),
    );

    let mut state = AppState::new(
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
    .with_config(Arc::clone(&config));
    if catalog {
        let providers = config
            .merged()
            .get("providers")
            .cloned()
            .unwrap_or_else(|| json!({}));
        state = state.with_catalog(Arc::new(
            Catalog::from_snapshot_with_config(FIXTURE, &providers).unwrap(),
        ));
    }

    Harness {
        _db: db,
        _config: config_dir,
        state,
    }
}

async fn call(app: &axum::Router, uri: &str) -> (StatusCode, Option<Value>) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
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

fn ids(rows: &[Value], key: &str) -> BTreeSet<String> {
    rows.iter()
        .filter_map(|row| row[key].as_str())
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn absent_catalog_is_503_on_every_route() {
    let harness = harness(Some("test/alpha"), false).await;
    let app = router(harness.state);

    for uri in [
        "/api/model",
        "/api/model/default",
        "/api/provider",
        "/api/provider/test",
    ] {
        let (status, body) = call(&app, uri).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        let body = body.unwrap();
        assert_eq!(body["_tag"], "ServiceUnavailableError", "{uri}");
        assert_eq!(body["message"], "model catalog not loaded", "{uri}");
    }
}

#[tokio::test]
async fn models_use_the_location_envelope_and_filter_types() {
    let harness = harness(Some("test/alpha"), true).await;
    let app = router(harness.state);

    let (status, body) = call(&app, "/api/model").await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert!(body["location"]["directory"].is_string());
    assert!(body["location"]["project"]["id"].is_string());
    let models = body["data"].as_array().unwrap();
    assert_eq!(
        models.len(),
        5,
        "configured, credentialed and env-backed providers only"
    );
    let expected: BTreeSet<String> = ["other", "test", "third"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(ids(models, "providerID"), expected);

    let alpha = models
        .iter()
        .find(|model| model["id"] == "alpha")
        .expect("alpha listed");
    assert_eq!(alpha["modelID"], "alpha");
    assert_eq!(alpha["providerID"], "test");
    assert_eq!(alpha["name"], "Alpha");
    assert_eq!(alpha["type"], "chat");
    assert_eq!(alpha["capabilities"]["tools"], true);
    assert!(alpha["variants"].is_array());
    assert!(alpha["time"]["released"].is_i64());
    assert_eq!(alpha["limit"]["context"], 100_000);

    let (status, body) = call(&app, "/api/model?types=chat").await;
    assert_eq!(status, StatusCode::OK);
    let models = body.unwrap()["data"].as_array().unwrap().len();
    assert_eq!(models, 4);

    let (status, body) = call(&app, "/api/model?types=chat&types=image").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"].as_array().unwrap().len(), 5);

    let (status, body) = call(&app, "/api/model?types=CHAT,image").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"].as_array().unwrap().len(), 5);

    let (status, body) = call(&app, "/api/model?types=video").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.unwrap()["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn providers_list_only_available_providers() {
    let harness = harness(Some("test/alpha"), true).await;
    let app = router(harness.state);

    let (status, body) = call(&app, "/api/provider").await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert!(body["location"]["directory"].is_string());
    let providers = body["data"].as_array().unwrap();
    assert_eq!(providers.len(), 3);
    let expected: BTreeSet<String> = ["other", "test", "third"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(ids(providers, "id"), expected);
    let test = providers
        .iter()
        .find(|provider| provider["id"] == "test")
        .unwrap();
    assert_eq!(test["name"], "Test Provider");
    assert_eq!(test["activation"], "auto");
    assert!(test["package"].is_string());

    let (status, body) = call(&app, "/api/provider/other").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["name"], "Other Provider");

    let (status, body) = call(&app, "/api/provider/third").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["name"], "Third Provider");
}

#[tokio::test]
async fn unknown_or_unavailable_provider_is_404() {
    let harness = harness(Some("test/alpha"), true).await;
    let app = router(harness.state);

    for id in ["nope", "off"] {
        let (status, body) = call(&app, &format!("/api/provider/{id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
        let body = body.unwrap();
        assert_eq!(body["_tag"], "ProviderNotFoundError", "{id}");
        assert_eq!(body["providerID"], id, "{id}");
    }
}

#[tokio::test]
async fn disabled_provider_models_are_hidden() {
    let harness = harness(Some("test/alpha"), true).await;
    let app = router(harness.state);

    let (_, body) = call(&app, "/api/model").await;
    let models = body.unwrap()["data"].as_array().unwrap().clone();
    assert!(
        models.iter().all(|model| model["providerID"] != "off"),
        "{models:?}"
    );
    assert!(
        models.iter().all(|model| model["id"] != "omega"),
        "{models:?}"
    );
}

#[tokio::test]
async fn default_model_resolves_the_config_reference() {
    let harness = harness(Some("test/alpha"), true).await;
    let app = router(harness.state);

    let (status, body) = call(&app, "/api/model/default").await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert!(body["location"]["directory"].is_string());
    assert_eq!(body["data"]["id"], "alpha");
    assert_eq!(body["data"]["providerID"], "test");
}

#[tokio::test]
async fn default_model_is_404_when_unset_unknown_or_unavailable() {
    let unset = harness(None, true).await;
    let app = router(unset.state);
    let (status, body) = call(&app, "/api/model/default").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let body = body.unwrap();
    assert_eq!(body["_tag"], "ModelNotFoundError");
    assert_eq!(body["providerID"], "");
    assert_eq!(body["modelID"], "");

    let unknown_model = harness(Some("test/nope"), true).await;
    let app = router(unknown_model.state);
    let (status, body) = call(&app, "/api/model/default").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let body = body.unwrap();
    assert_eq!(body["_tag"], "ModelNotFoundError");
    assert_eq!(body["providerID"], "test");
    assert_eq!(body["modelID"], "nope");

    let unknown_provider = harness(Some("nope/alpha"), true).await;
    let app = router(unknown_provider.state);
    let (status, body) = call(&app, "/api/model/default").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["providerID"], "nope");

    let disabled = harness(Some("off/omega"), true).await;
    let app = router(disabled.state);
    let (status, body) = call(&app, "/api/model/default").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let body = body.unwrap();
    assert_eq!(body["_tag"], "ModelNotFoundError");
    assert_eq!(body["providerID"], "off");
    assert_eq!(body["modelID"], "omega");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("not available")),
        "{body:?}"
    );
}

#[tokio::test]
async fn model_response_has_json_content_type() {
    // Guard the content type of the envelope; the generated client parses JSON.
    let harness = harness(Some("test/alpha"), true).await;
    let app = router(harness.state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/model")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/json"
    );
}

#[test]
fn catalog_from_config_applies_the_providers_overlay() {
    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        config_dir.path().join("opencode.json"),
        json!({
            "providers": {
                "custom-provider": {
                    "name": "Custom Provider",
                    "package": "@ai-sdk/openai-compatible",
                    "models": {
                        "custom-model": {
                            "name": "Custom Model",
                            "type": "chat",
                            "limit": {"context": 4096, "output": 512}
                        }
                    }
                }
            }
        })
        .to_string(),
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

    let catalog = oc_server::catalog_from_config(&config).unwrap();
    let provider = catalog
        .provider("custom-provider")
        .expect("config-only provider exists");
    assert_eq!(provider.name, "Custom Provider");
    assert!(catalog.model("custom-provider", "custom-model").is_some());
    // The embedded snapshot is still present.
    assert!(catalog.providers().len() > 1);
}
