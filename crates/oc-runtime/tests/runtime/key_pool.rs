//! Credential rotation inside one turn step: one model candidate, many keys.
//!
//! Every test seeds stored credentials only. `std::env::set_var` is deliberately
//! unused: the tests of this binary run in parallel, so a process-global
//! environment mutation would race the provider environment fallback.

use std::sync::Arc;

use oc_store::NewCredential;
use serde_json::json;

use crate::support::*;

/// Store one key for the `test` integration.
///
/// The first credential of an integration becomes active; `active: Some(false)`
/// keeps the current selection, so seeding `primary` then `backup` yields exactly
/// one active row and a two-row pool ordered primary-first.
async fn seed_key(harness: &Harness, id: &str, secret: &str, active: bool) {
    harness
        .store
        .create_credential(NewCredential {
            id: Some(id.to_owned()),
            integration_id: "test".to_owned(),
            label: None,
            value: json!({"type": "key", "key": secret}),
            active: Some(active),
        })
        .await
        .expect("credential");
}

/// A pre-stream `quota` failure: the class a spent key reports.
fn quota_failure() -> Script {
    Script::Failure {
        kind: oc_llm::FailureKind::Quota,
        message: "quota exhausted for this key".to_owned(),
    }
}

fn auth_failure() -> Script {
    Script::Failure {
        kind: oc_llm::FailureKind::Auth,
        message: "invalid api key".to_owned(),
    }
}

/// The backup key serves the same model in the same step: two provider calls,
/// one assistant row, zero fallback events, and the durable primary moved.
#[tokio::test]
async fn quota_on_the_active_key_rotates_to_the_backup_key() {
    let harness = Harness::new(
        vec![quota_failure(), text_script("served by the backup key")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    seed_key(&harness, "cred_primary", "sk-primary", true).await;
    seed_key(&harness, "cred_backup", "sk-backup", false).await;

    harness.prompt("hi").await;

    assert!(
        harness.sink.has("session.execution.succeeded"),
        "{:?}",
        harness.sink.kinds()
    );
    assert_eq!(harness.model.calls(), 2, "one attempt per usable key");

    let requests = harness.model.requests();
    assert_eq!(requests[0].options.api_key.as_deref(), Some("sk-primary"));
    assert_eq!(requests[1].options.api_key.as_deref(), Some("sk-backup"));
    assert_eq!(requests[0].model.model.model_id, "alpha");
    assert_eq!(requests[1].model.model.model_id, "alpha");
    assert_eq!(
        harness.sink.count("session.model.fallback"),
        0,
        "a key rotation never announces a model switch: {:?}",
        harness.sink.kinds()
    );

    let messages = harness.messages().await;
    let rows = assistants(&messages);
    assert_eq!(
        rows.len(),
        1,
        "the rotated attempt reuses the assistant row: {rows:?}"
    );
    assert_eq!(rows[0]["finish"], "stop", "{:?}", rows[0]);

    // The dead key lost the durable primary flag, so a restart does not pick it
    // first.
    let ids: Vec<String> = harness
        .store
        .credential_pool("test")
        .await
        .expect("pool")
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(
        ids,
        vec!["cred_backup".to_owned(), "cred_primary".to_owned()],
        "the primary pointer moved to the healthy key"
    );
}

/// Both keys spent: the chain advances exactly once, and the second model
/// serves the turn.
#[tokio::test]
async fn all_keys_exhausted_advances_the_chain_once() {
    let harness = Harness::new(
        vec![
            quota_failure(),
            quota_failure(),
            text_script("beta served the turn"),
        ],
        Arc::new(AllowAll),
        json!({"models": {
            "chain": ["test/alpha", "test/beta"],
            "fallback": {"on": ["quota"], "max_hops": 2}
        }}),
        None,
    )
    .await;
    seed_key(&harness, "cred_primary", "sk-primary", true).await;
    seed_key(&harness, "cred_backup", "sk-backup", false).await;

    harness.prompt("hi").await;

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        3,
        "two keys on alpha, then one chain advance to beta"
    );
    assert_eq!(requests[0].model.model.model_id, "alpha");
    assert_eq!(requests[1].model.model.model_id, "alpha");
    assert_eq!(requests[2].model.model.model_id, "beta");
    assert_eq!(
        harness.sink.count("session.model.fallback"),
        1,
        "exhausting the pool switches models exactly once: {:?}",
        harness.sink.kinds()
    );
    assert_eq!(
        harness.sink.data("session.model.fallback")[0]["kind"],
        "quota",
        "the switch records the class that exhausted the last key"
    );

    let messages = harness.messages().await;
    let rows = assistants(&messages);
    assert_eq!(
        rows.len(),
        1,
        "the row is rewritten for beta, not duplicated: {rows:?}"
    );
}

/// `auth` rotates the key but never the model: a revoked key says nothing about
/// the next key.
#[tokio::test]
async fn auth_failure_rotates_without_chain_advance() {
    let harness = Harness::new(
        vec![auth_failure(), text_script("the backup key is valid")],
        Arc::new(AllowAll),
        json!({"models": {"chain": ["test/beta"]}}),
        None,
    )
    .await;
    seed_key(&harness, "cred_primary", "sk-revoked", true).await;
    seed_key(&harness, "cred_backup", "sk-valid", false).await;

    harness.prompt("hi").await;

    assert!(
        harness.sink.has("session.execution.succeeded"),
        "{:?}",
        harness.sink.kinds()
    );
    assert_eq!(harness.model.calls(), 2, "the revoked key was retried once");
    assert_eq!(
        harness.sink.count("session.model.fallback"),
        0,
        "an auth failure must not switch models: {:?}",
        harness.sink.kinds()
    );
    let requests = harness.model.requests();
    assert_eq!(requests[0].model.model.model_id, "alpha");
    assert_eq!(requests[1].model.model.model_id, "alpha");
}

/// Rotation rebuilds only the credential: both requests carry the identical
/// model reference.
#[tokio::test]
async fn rotate_uses_the_same_model_reference() {
    let harness = Harness::new(
        vec![quota_failure(), text_script("same model, next key")],
        Arc::new(AllowAll),
        json!({"models": {"chain": ["test/beta"]}}),
        None,
    )
    .await;
    seed_key(&harness, "cred_primary", "sk-primary", true).await;
    seed_key(&harness, "cred_backup", "sk-backup", false).await;

    harness.prompt("hi").await;

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2, "two keys, one model candidate");
    assert_eq!(
        requests[0].model.model.model_id, requests[1].model.model.model_id,
        "rotation must not change the model id"
    );
    assert_eq!(
        requests[0].model.model.provider_id, requests[1].model.model.provider_id,
        "rotation stays on the same provider"
    );
    assert_eq!(
        requests[0].model.variant.is_some(),
        requests[1].model.variant.is_some(),
        "rotation keeps the same variant selection"
    );
    assert_eq!(requests[1].model.model.model_id, "alpha");
}
