//! `build_for_state` wiring: configuration, model resolution and opt-in gating.

use std::sync::Arc;

use oc_llm::{Catalog, SharedLlmClient};
use oc_store::NewCredential;
use serde_json::json;
use tempfile::TempDir;

use super::super::build_for_state;
use super::{CATALOG, ENV_CATALOG, config_at, state_at, store_at};

#[tokio::test]
async fn disabled_when_observer_is_off() {
    let root = TempDir::new().expect("tempdir");
    let config = config_at(
        root.path(),
        r#"{"observer": {"enabled": false, "model": "test/alpha"}}"#,
    );
    let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
    let store = store_at(root.path()).await;
    let client = Arc::new(SharedLlmClient::new(
        oc_llm::LlmClient::new().expect("client"),
    ));
    let handle = build_for_state(&state_at(root.path()), store, &config, &catalog, client);
    assert!(handle.is_none());
}

#[tokio::test]
async fn disabled_without_model() {
    let root = TempDir::new().expect("tempdir");
    let config = config_at(root.path(), r#"{"observer": {"enabled": true}}"#);
    let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
    let store = store_at(root.path()).await;
    let client = Arc::new(SharedLlmClient::new(
        oc_llm::LlmClient::new().expect("client"),
    ));
    let handle = build_for_state(&state_at(root.path()), store, &config, &catalog, client);
    assert!(handle.is_none());
}

#[tokio::test]
async fn enabled_with_explicit_model() {
    let root = TempDir::new().expect("tempdir");
    let config = config_at(
        root.path(),
        r#"{"observer": {"model": "test/alpha", "tick_ms": 250}}"#,
    );
    let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
    let store = store_at(root.path()).await;
    let client = Arc::new(SharedLlmClient::new(
        oc_llm::LlmClient::new().expect("client"),
    ));
    let handle = build_for_state(&state_at(root.path()), store, &config, &catalog, client)
        .expect("observer built");
    assert_eq!(handle.engine.config().model.as_deref(), Some("test/alpha"));
    assert_eq!(handle.engine.config().tick_ms, 250);
}

#[tokio::test]
async fn missing_env_credential_does_not_disable_the_observer() {
    assert!(
        std::env::var_os("OC_OBSERVER_TEST_API_KEY").is_none(),
        "test requires the provider credential to stay unset"
    );
    let root = TempDir::new().expect("tempdir");
    let config = config_at(root.path(), r#"{"observer": {"model": "test/alpha"}}"#);
    let catalog = Catalog::from_snapshot(ENV_CATALOG).expect("catalog");
    let store = store_at(root.path()).await;
    let client = Arc::new(SharedLlmClient::new(
        oc_llm::LlmClient::new().expect("client"),
    ));

    // The environment variable alone cannot decide availability: a stored
    // credential may exist, so the observer builds and fails open on the
    // first judge call when no credential is usable.
    let handle = build_for_state(&state_at(root.path()), store, &config, &catalog, client);
    assert!(handle.is_some());
}

#[tokio::test]
async fn stored_credential_with_missing_env_builds_the_observer() {
    assert!(
        std::env::var_os("OC_OBSERVER_TEST_API_KEY").is_none(),
        "test requires the provider credential to stay unset"
    );
    let root = TempDir::new().expect("tempdir");
    let config = config_at(root.path(), r#"{"observer": {"model": "test/alpha"}}"#);
    let catalog = Catalog::from_snapshot(ENV_CATALOG).expect("catalog");
    let store = store_at(root.path()).await;
    store
        .create_credential(NewCredential {
            id: Some("cred_observer".to_owned()),
            integration_id: "test".to_owned(),
            label: None,
            value: json!({"type": "key", "key": "river-key"}),
            active: Some(true),
        })
        .await
        .expect("credential");
    let client = Arc::new(SharedLlmClient::new(
        oc_llm::LlmClient::new().expect("client"),
    ));

    let handle = build_for_state(&state_at(root.path()), store, &config, &catalog, client);
    assert!(handle.is_some());
}

#[tokio::test]
async fn falls_back_to_observer_digest_model() {
    let root = TempDir::new().expect("tempdir");
    let config = config_at(
        root.path(),
        r#"{"observer": {"digest_model": "test/alpha"}}"#,
    );
    let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
    let store = store_at(root.path()).await;
    let client = Arc::new(SharedLlmClient::new(
        oc_llm::LlmClient::new().expect("client"),
    ));
    assert!(build_for_state(&state_at(root.path()), store, &config, &catalog, client).is_some());
}

#[tokio::test]
async fn falls_back_to_subagents_digest_model() {
    let root = TempDir::new().expect("tempdir");
    let config = config_at(
        root.path(),
        r#"{"subagents": {"digest_model": "test/alpha"}}"#,
    );
    let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
    let store = store_at(root.path()).await;
    let client = Arc::new(SharedLlmClient::new(
        oc_llm::LlmClient::new().expect("client"),
    ));
    assert!(build_for_state(&state_at(root.path()), store, &config, &catalog, client).is_some());
}

#[tokio::test]
async fn unknown_model_disables() {
    let root = TempDir::new().expect("tempdir");
    let config = config_at(root.path(), r#"{"observer": {"model": "test/missing"}}"#);
    let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
    let store = store_at(root.path()).await;
    let client = Arc::new(SharedLlmClient::new(
        oc_llm::LlmClient::new().expect("client"),
    ));
    assert!(build_for_state(&state_at(root.path()), store, &config, &catalog, client).is_none());
}
