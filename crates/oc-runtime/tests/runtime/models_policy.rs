//! `models` control-plane policy in the spawn chooser: chain precedence, quota
//! denial, fail-loud exhaustion and the no-config regression.
//!
//! Every test is offline: temp config, temp SQLite, scripted model, and the
//! usage rollup is seeded by writing assistant rows directly.

use std::sync::Arc;

use oc_store::{NewCredential, NewMessage};
use serde_json::{Value, json};

use crate::support::*;

/// Seed one priced assistant message so `Store::model_usage` reports it.
async fn seed_usage(harness: &Harness, provider: &str, model: &str, cost: f64) {
    harness
        .store
        .append_message(NewMessage {
            id: format!("msg_seed_{provider}_{model}"),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "model": { "providerID": provider, "id": model },
                "cost": cost,
                "tokens": { "input": 10, "output": 5, "reasoning": 0 },
            }),
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("seed usage");
}

fn spawn_input(background: bool) -> Value {
    json!({
        "agent": "general",
        "description": "quota probe",
        "prompt": "work",
        "background": background,
    })
}

fn rejected(metadata: &Value) -> &Vec<Value> {
    metadata["rejectedModels"]
        .as_array()
        .unwrap_or_else(|| panic!("rejectedModels is reported: {metadata}"))
}

/// (g) No `models` section: the chooser keeps the historical resolution and the
/// result carries the parent model with no quota/rejectedModels noise.
#[tokio::test]
async fn no_models_config_keeps_the_historical_spawn_report() {
    let harness = Harness::new(
        vec![text_script("child final")],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": true}}),
        None,
    )
    .await;
    // Usage far above any plausible budget: with no `models.quota` nothing may
    // be denied.
    seed_usage(&harness, "test", "alpha", 99.0).await;

    let result = subagent_tool(&harness, "subagent")
        .execute(&tool_context(&harness, &harness.session), spawn_input(true))
        .await
        .expect("no models config never refuses a spawn");
    let metadata = result.metadata.expect("metadata");
    assert_eq!(metadata["status"], "running");
    assert_eq!(metadata["model"]["providerID"], "test");
    assert_eq!(metadata["model"]["id"], "alpha");
    // The parent session model is the only candidate.
    assert_eq!(metadata["modelSource"], "parent");
    assert!(metadata.get("quota").is_none(), "{metadata}");
    assert!(metadata.get("rejectedModels").is_none(), "{metadata}");
    assert_eq!(
        child_sessions(&harness.store, &harness.session).await.len(),
        1
    );
}

/// A chain entry that wins reports `modelSource: "chain"`. The chain is a
/// candidate in declared order: after an explicit call model and *before*
/// `agents.<id>.model`, `subagents.default_model` and the inherited parent.
#[tokio::test]
async fn chain_entry_wins_and_reports_the_chain_source() {
    let harness = Harness::new(
        vec![text_script("child final")],
        Arc::new(AllowAll),
        json!({
            "models": {"chain": ["test/beta"]},
            // Every lower-precedence source names a runnable model, so picking
            // `beta` proves the chain outranks all three of them.
            "agents": {"general": {"model": "test/alpha"}},
            "subagents": {"background_default": true, "default_model": "test/alpha"},
        }),
        None,
    )
    .await;
    let result = subagent_tool(&harness, "subagent")
        .execute(&tool_context(&harness, &harness.session), spawn_input(true))
        .await
        .expect("a chain entry the catalog can run is spawned");
    let metadata = result.metadata.expect("metadata");
    assert_eq!(metadata["model"]["id"], "beta");
    assert_eq!(
        metadata["modelSource"], "chain",
        "the chain beats agents.<id>.model, subagents.default_model and the parent"
    );
    assert!(
        metadata.get("rejectedModels").is_none(),
        "nothing was skipped: {metadata}"
    );
}

/// THE regression the contract exists for: an explicit call model must beat the
/// configured chain, even when the chain's only entry is a retired model.
///
/// Before the fix `models.chain` was *prepended* to the candidate list, so the
/// chain shadowed the caller's `model:` argument and every spawn died on the
/// chain's provider.
#[tokio::test]
async fn call_model_beats_a_chain_entry_the_catalog_cannot_run() {
    let harness = Harness::new(
        vec![text_script("child final")],
        Arc::new(AllowAll),
        json!({"models": {"chain": ["test/retired"]}}),
        None,
    )
    .await;
    let mut input = spawn_input(true);
    input["model"] = json!("test/beta");
    let result = subagent_tool(&harness, "subagent")
        .execute(&tool_context(&harness, &harness.session), input)
        .await
        .expect("the call model is usable, so the spawn succeeds");
    let metadata = result.metadata.expect("metadata");
    assert_eq!(metadata["model"]["id"], "beta");
    assert_eq!(
        metadata["modelSource"], "call",
        "an explicit call model always wins"
    );
    assert!(
        metadata.get("rejectedModels").is_none(),
        "the retired chain entry is never evaluated: {metadata}"
    );
}

/// A chain whose first entry is unusable falls through to the next entry. The
/// skipped reference is reported exactly once, with its typed reason and source,
/// and nothing else is reported.
#[tokio::test]
async fn chain_falls_through_a_retired_entry_and_reports_it() {
    let harness = Harness::new(
        vec![text_script("child final")],
        Arc::new(AllowAll),
        json!({"models": {"chain": ["test/retired", "test/beta"]}}),
        None,
    )
    .await;
    let result = subagent_tool(&harness, "subagent")
        .execute(&tool_context(&harness, &harness.session), spawn_input(true))
        .await
        .expect("the second chain entry is usable");
    let metadata = result.metadata.expect("metadata");
    assert_eq!(metadata["model"]["id"], "beta");
    assert_eq!(metadata["modelSource"], "chain");
    // The inherited parent (test/alpha) is a different, runnable model and is
    // never reached, so the report holds exactly one entry.
    assert_eq!(
        rejected(&metadata),
        &vec![json!({
            "reference": "test/retired",
            "source": "chain",
            "reason": "not_available",
        })],
        "{metadata}"
    );
}

/// A call model the catalog does not know is not fatal: resolution falls through
/// to the chain instead of refusing the spawn, and the call reference is
/// reported as a `call` source that was `not_available`.
#[tokio::test]
async fn unknown_call_model_falls_through_to_the_chain() {
    let harness = Harness::new(
        vec![text_script("child final")],
        Arc::new(AllowAll),
        json!({"models": {"chain": ["test/beta"]}}),
        None,
    )
    .await;
    let mut input = spawn_input(true);
    input["model"] = json!("test/not-in-catalog");
    let result = subagent_tool(&harness, "subagent")
        .execute(&tool_context(&harness, &harness.session), input)
        .await
        .expect("a bad call model falls through to a usable chain entry");
    let metadata = result.metadata.expect("metadata");
    assert_eq!(metadata["model"]["id"], "beta");
    assert_eq!(metadata["modelSource"], "chain");
    let skipped = rejected(&metadata)
        .iter()
        .find(|skip| skip["reference"] == "test/not-in-catalog")
        .unwrap_or_else(|| panic!("the call reference is named: {metadata}"));
    assert_eq!(skipped["source"], "call");
    assert_eq!(skipped["reason"], "not_available");
}

/// A retired-model provider message always advances the chain, even when
/// `models.fallback.on` is explicitly empty (the model no longer exists;
/// retrying it is pointless, switching is not).
#[tokio::test]
async fn model_unavailable_message_advances_the_chain_with_empty_fallback_on() {
    let message = "Claude Opus 4.6 is no longer available. Please switch to Claude Opus 5.5.";
    assert_eq!(
        oc_llm::classify_provider_message(message),
        oc_llm::FailureKind::ModelUnavailable,
        "the provider text classifies as a retired model"
    );
    let harness = Harness::new(
        vec![
            Script::Failure {
                kind: oc_llm::FailureKind::ModelUnavailable,
                message: message.to_owned(),
            },
            text_script("recovered on beta"),
        ],
        Arc::new(AllowAll),
        json!({"models": {
            "chain": ["test/alpha", "test/beta"],
            "fallback": {"on": []}
        }}),
        None,
    )
    .await;
    harness.prompt("hi").await;

    assert!(
        harness.sink.has("session.execution.succeeded"),
        "{:?}",
        harness.sink.kinds()
    );
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2, "one attempt per candidate");
    assert_eq!(requests[0].model.model.model_id, "alpha");
    assert_eq!(requests[1].model.model.model_id, "beta");
    let fallbacks = harness.sink.data("session.model.fallback");
    assert_eq!(fallbacks.len(), 1, "{:?}", harness.sink.kinds());
    assert_eq!(fallbacks[0]["kind"], "model_unavailable");
    assert_eq!(fallbacks[0]["from"]["id"], "alpha");
    assert_eq!(fallbacks[0]["to"]["id"], "beta");
}

/// (d) A `models.chain` entry the catalog cannot run is skipped, not fatal: the
/// resolver falls through to the inherited parent model. This used to be
/// refused (the chain was a hard override); under the current contract the
/// chain is only a preference order.
#[tokio::test]
async fn unusable_chain_entry_falls_through_to_the_parent_model() {
    let harness = Harness::new(
        vec![text_script("child final")],
        Arc::new(AllowAll),
        json!({"models": {"chain": ["test/missing"]}}),
        None,
    )
    .await;
    let result = subagent_tool(&harness, "subagent")
        .execute(&tool_context(&harness, &harness.session), spawn_input(true))
        .await
        .expect("a bad chain entry falls through to the parent model");
    let metadata = result.metadata.expect("metadata");
    assert_eq!(metadata["modelSource"], "parent");
    assert_eq!(metadata["model"]["id"], "alpha");
    let skipped = rejected(&metadata)
        .iter()
        .find(|skip| skip["reference"] == "test/missing")
        .unwrap_or_else(|| panic!("test/missing is named: {metadata}"));
    assert_eq!(skipped["source"], "chain");
    assert_eq!(skipped["reason"], "not_available");
}

/// NO-USABLE-CANDIDATE: every candidate is denied, so the error names each
/// skipped reference with its reason *and* the source it was proposed with
/// (`reference (reason, source)`), and no child row exists.
#[tokio::test]
async fn no_usable_candidate_names_every_skip() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"models": {
            "chain": ["test/retired"],
            "quota": {
                "reserve_percent": 0,
                "window_hours": 5,
                "default": {"max_cost_usd": 5.0}
            }
        }}),
        None,
    )
    .await;
    // The parent model (`test/alpha`) is exhausted; the chain's only entry is
    // absent from the catalog. Nothing is left to pick.
    seed_usage(&harness, "test", "alpha", 5.0).await;

    let error = subagent_tool(&harness, "subagent")
        .execute(
            &tool_context(&harness, &harness.session),
            spawn_input(false),
        )
        .await
        .expect_err("no usable candidate must not spawn");
    assert!(
        error.message.contains("test/alpha (denied, parent)"),
        "names the reference, reason and source: {}",
        error.message
    );
    assert!(
        error
            .message
            .contains("test/retired (not_available, chain)"),
        "names every skip: {}",
        error.message
    );
    assert!(
        child_sessions(&harness.store, &harness.session)
            .await
            .is_empty(),
        "no child session is created"
    );
}

/// (e) Every candidate denied: the tool error names each model and its reason,
/// and no child session exists. No `chain` is configured here on purpose: the
/// quota must also cover the inherited parent model.
#[tokio::test]
async fn quota_exhausted_for_every_candidate_fails_loudly() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"models": {
            "quota": {
                "reserve_percent": 10,
                "window_hours": 5,
                "default": {"max_cost_usd": 5.0}
            }
        }}),
        None,
    )
    .await;
    // 4.80 spent against a 5.00 budget with a 10% reserve => usable 4.50.
    seed_usage(&harness, "test", "alpha", 4.80).await;

    let error = subagent_tool(&harness, "subagent")
        .execute(
            &tool_context(&harness, &harness.session),
            spawn_input(false),
        )
        .await
        .expect_err("an exhausted candidate list must not spawn");
    assert!(
        error.message.contains("test/alpha"),
        "names the denied model: {}",
        error.message
    );
    assert!(
        error.message.contains("quota_cost 4.80/4.50"),
        "names the reason: {}",
        error.message
    );
    assert!(
        child_sessions(&harness.store, &harness.session)
            .await
            .is_empty(),
        "no child session is created"
    );
}

/// (f) Exactly at the reserve boundary the model is denied; one cent below it is
/// still spawnable.
#[tokio::test]
async fn reserve_threshold_denies_at_the_boundary_and_allows_below_it() {
    let quota = json!({
        "reserve_percent": 10,
        "window_hours": 5,
        "default": {"max_cost_usd": 5.0}
    });

    let below = Harness::new(
        vec![text_script("child final")],
        Arc::new(AllowAll),
        json!({"models": {"quota": quota.clone()}}),
        None,
    )
    .await;
    // 4.49 < 4.50 usable: still spawnable.
    seed_usage(&below, "test", "alpha", 4.49).await;
    let result = subagent_tool(&below, "subagent")
        .execute(&tool_context(&below, &below.session), spawn_input(true))
        .await
        .expect("below the reserve the candidate is usable");
    let metadata = result.metadata.expect("metadata");
    let snapshot = metadata.get("quota").expect("quota snapshot");
    assert_eq!(snapshot["providerID"], "test");
    assert_eq!(snapshot["windowHours"], 5);
    assert_eq!(snapshot["limited"], false);

    let boundary = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"models": {"quota": quota}}),
        None,
    )
    .await;
    seed_usage(&boundary, "test", "alpha", 4.50).await;
    let error = subagent_tool(&boundary, "subagent")
        .execute(
            &tool_context(&boundary, &boundary.session),
            spawn_input(false),
        )
        .await
        .expect_err("at the reserve boundary the model is exhausted");
    assert!(
        error.message.contains("quota_cost 4.50/4.50"),
        "{}",
        error.message
    );
    assert!(
        child_sessions(&boundary.store, &boundary.session)
            .await
            .is_empty()
    );
}

/// A request-count budget denies on the same reserve arithmetic.
#[tokio::test]
async fn request_budget_denies_when_the_window_is_used_up() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"models": {
            "quota": {"reserve_percent": 0, "window_hours": 5,
                      "default": {"max_requests": 3}}
        }}),
        None,
    )
    .await;
    // `seed_usage` writes one row; two more reach the 3-request budget.
    seed_usage(&harness, "test", "alpha", 0.0).await;
    for index in 0..2 {
        harness
            .store
            .append_message(NewMessage {
                id: format!("msg_seed_extra_{index}"),
                session_id: harness.session.clone(),
                kind: "assistant".to_owned(),
                data: json!({
                    "model": { "providerID": "test", "id": "alpha" },
                    "cost": 0.0,
                    "tokens": { "input": 1, "output": 1, "reasoning": 0 },
                }),
                time_created: oc_types::now_millis(),
            })
            .await
            .expect("seed request");
    }
    let error = subagent_tool(&harness, "subagent")
        .execute(
            &tool_context(&harness, &harness.session),
            spawn_input(false),
        )
        .await
        .expect_err("the request budget is spent");
    assert!(
        error.message.contains("quota_requests 3/3"),
        "{}",
        error.message
    );
}

/// A malformed `models` value refuses the spawn with the field name instead of
/// deciding on a half-read policy.
#[tokio::test]
async fn malformed_models_config_refuses_before_spawn() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"models": {"chain": "not-an-array"}}),
        None,
    )
    .await;
    let error = subagent_tool(&harness, "subagent")
        .execute(
            &tool_context(&harness, &harness.session),
            spawn_input(false),
        )
        .await
        .expect_err("a malformed policy must not pick a model");
    assert!(error.message.contains("models.chain"), "{}", error.message);
    assert!(
        child_sessions(&harness.store, &harness.session)
            .await
            .is_empty()
    );
}

/// (c) A non-retryable failure class never advances the chain.
///
/// `auth` fails identically on every model, so switching would burn a request to
/// learn nothing. `FallbackPolicy::allows` requires `kind.is_retryable()`, and
/// this is the regression that keeps that guard honest: the chain is configured
/// and runnable, so a fallback *would* happen if the class were retryable.
#[tokio::test]
async fn auth_failure_does_not_advance_the_chain() {
    let harness = Harness::new(
        vec![Script::Failure {
            kind: oc_llm::FailureKind::Auth,
            message: "invalid api key".to_owned(),
        }],
        Arc::new(AllowAll),
        json!({"models": {"chain": ["test/beta"]}}),
        None,
    )
    .await;
    harness.prompt("hi").await;
    harness.wait_for_event("session.execution.failed").await;

    assert_eq!(
        harness.sink.count("session.model.fallback"),
        0,
        "an auth failure must not switch models: {:?}",
        harness.sink.kinds()
    );
    assert_eq!(
        harness.model.calls(),
        1,
        "no second attempt was made for a permanent failure"
    );
    assert_eq!(
        harness.model.requests()[0].model.model.model_id,
        "alpha",
        "the turn stayed on the session model"
    );
}

/// (c) The same guard with a two-key pool: `auth` rotates the key but never the
/// model, so both keys are tried on the session model and the chain is still
/// never advanced.
///
/// This is the case that separates the two rotation axes: exhausting the pool is
/// what makes a *retryable* class advance the chain (`key_pool`'s
/// `all_keys_exhausted_advances_the_chain_once`), while a permanent class stays
/// put no matter how many keys it consumed.
#[tokio::test]
async fn auth_failure_on_two_keys_does_not_advance_the_chain() {
    let harness = Harness::new(
        vec![
            Script::Failure {
                kind: oc_llm::FailureKind::Auth,
                message: "invalid api key (primary)".to_owned(),
            },
            Script::Failure {
                kind: oc_llm::FailureKind::Auth,
                message: "invalid api key (backup)".to_owned(),
            },
        ],
        Arc::new(AllowAll),
        json!({"models": {"chain": ["test/beta"]}}),
        None,
    )
    .await;
    // First credential of an integration activates; the second keeps the
    // selection, so the pool is exactly [primary, backup].
    for (id, secret, active) in [
        ("cred_primary", "sk-primary", true),
        ("cred_backup", "sk-backup", false),
    ] {
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

    harness.prompt("hi").await;
    harness.wait_for_event("session.execution.failed").await;

    // One attempt per key, both on the session model: the pool was exhausted
    // before the chain was ever consulted.
    assert_eq!(
        harness.model.calls(),
        2,
        "one attempt per usable key, no more"
    );
    let requests = harness.model.requests();
    assert_eq!(requests[0].options.api_key.as_deref(), Some("sk-primary"));
    assert_eq!(requests[1].options.api_key.as_deref(), Some("sk-backup"));
    assert_eq!(requests[0].model.model.model_id, "alpha");
    assert_eq!(requests[1].model.model.model_id, "alpha");
    assert_eq!(
        harness.sink.count("session.model.fallback"),
        0,
        "a permanent class never switches models, even across keys: {:?}",
        harness.sink.kinds()
    );
    assert!(
        harness
            .model
            .requests()
            .iter()
            .all(|request| request.model.model.model_id != "beta"),
        "the configured chain entry was never requested"
    );

    // The turn still fails as one row: rotation rewrites the same assistant row
    // instead of leaving a second one behind.
    let messages = harness.messages().await;
    let rows = assistants(&messages);
    assert_eq!(rows.len(), 1, "one failed assistant row: {rows:?}");
    assert!(rows[0].get("error").is_some(), "{}", rows[0]);
    assert_ne!(rows[0]["finish"], "stop", "{}", rows[0]);
}

/// (h) A retryable `empty_response` advances the chain by one hop: the second
/// candidate serves the retry, the single assistant row is attributed to it, and
/// `session.model.fallback` records both ends of the switch plus the class.
#[tokio::test]
async fn empty_response_advances_the_chain_and_emits_the_fallback_event() {
    let harness = Harness::new(
        vec![
            Script::Failure {
                kind: oc_llm::FailureKind::EmptyResponse,
                message: "alpha returned no output".to_owned(),
            },
            text_script("recovered on beta"),
        ],
        Arc::new(AllowAll),
        json!({"models": {
            "chain": ["test/alpha", "test/beta"],
            "fallback": {"on": ["empty_response"], "max_hops": 1}
        }}),
        None,
    )
    .await;
    harness.prompt("hi").await;

    assert!(
        harness.sink.has("session.execution.succeeded"),
        "{:?}",
        harness.sink.kinds()
    );
    // One attempt per model, in chain order: the retry went to the second one.
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2, "one attempt per candidate");
    assert_eq!(requests[0].model.model.model_id, "alpha");
    assert_eq!(requests[1].model.model.model_id, "beta");

    // The durable fallback record names both ends of the switch and the class.
    let fallbacks = harness.sink.data("session.model.fallback");
    assert_eq!(fallbacks.len(), 1, "{:?}", harness.sink.kinds());
    assert_eq!(fallbacks[0]["kind"], "empty_response");
    assert_eq!(fallbacks[0]["attempt"], 1);
    assert_eq!(fallbacks[0]["from"]["providerID"], "test");
    assert_eq!(fallbacks[0]["from"]["id"], "alpha");
    assert_eq!(fallbacks[0]["to"]["providerID"], "test");
    assert_eq!(fallbacks[0]["to"]["id"], "beta");

    // One assistant row, attributed to the model that actually answered.
    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants.len(), 1, "{messages:?}");
    assert_eq!(assistants[0]["model"]["id"], "beta");
    assert_eq!(assistants[0]["content"][0]["text"], "recovered on beta");
    assert_eq!(assistants[0]["finish"], "stop");
    assert!(assistants[0].get("error").is_none(), "{}", assistants[0]);
}

/// (i) `max_hops`, not the candidate count, bounds the attempts: one hop over a
/// two-entry chain (plus the inherited session model, so three candidates) makes
/// exactly two provider attempts, and the surfaced error is the classified
/// provider failure that ended the run — never a synthesized "chain exhausted"
/// message — with no success row.
///
/// `stream_step` rebuilds `original` from the failure of each attempt
/// (`crates/oc-runtime/src/turn/stream.rs`), so the error that reaches
/// `session.execution.failed` is the last attempted model's real failure; the
/// earlier attempt's error is not retained anywhere. Pinning that keeps the
/// "no synthesized error" contract honest instead of asserting the first
/// message the implementation does not preserve.
#[tokio::test]
async fn two_transport_failures_stop_at_max_hops_and_keep_the_original_error() {
    let harness = Harness::new(
        vec![
            Script::Failure {
                kind: oc_llm::FailureKind::Transport,
                message: "alpha transport boom".to_owned(),
            },
            Script::Failure {
                kind: oc_llm::FailureKind::Transport,
                message: "beta transport boom".to_owned(),
            },
            Script::Failure {
                kind: oc_llm::FailureKind::Transport,
                message: "gamma transport boom".to_owned(),
            },
        ],
        Arc::new(AllowAll),
        json!({"models": {
            "chain": ["test/beta", "test/gamma"],
            "fallback": {"on": ["transport"], "max_hops": 1}
        }}),
        None,
    )
    .await;
    harness.prompt("hi").await;
    harness.wait_for_event("session.execution.failed").await;

    // 1 + max_hops attempts, not one per candidate (alpha + the two chain
    // entries are three candidates; the third is never requested).
    assert_eq!(harness.model.calls(), 2, "the cap is 1 + max_hops");
    let requests = harness.model.requests();
    assert_eq!(requests[0].model.model.model_id, "alpha");
    assert_eq!(requests[1].model.model.model_id, "beta");
    assert_eq!(harness.sink.count("session.model.fallback"), 1);

    // The surfaced error is the provider failure that ended the run: typed,
    // classified, and not a synthesized exhaustion sentence.
    let failures = harness.sink.data("session.execution.failed");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["error"]["type"], "ProviderError");
    let message = failures[0]["error"]["message"]
        .as_str()
        .expect("the failed execution carries a message");
    assert!(
        message.contains("provider failure (transport)"),
        "{message}"
    );
    assert!(message.contains("beta transport boom"), "{message}");
    assert!(!message.contains("chain"), "{message}");

    // No success row: no completed step, no answered assistant row.
    assert!(!harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.sink.count("session.step.ended"), 0);
    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants.len(), 1, "{messages:?}");
    assert!(assistants[0].get("error").is_some(), "{}", assistants[0]);
    assert_ne!(assistants[0]["finish"], "stop");
}

/// The live-config regression: `models.fallback.on = ["empty_response",
/// "transport"]` — exactly the `on` list in the shipped config — used to make a
/// quota failure kill the turn on the only model that was tried, because an
/// explicit `on` was treated as the complete allowlist.
///
/// `Quota` and `RateLimit` are recovery classes: the chain exists to recover
/// from them, so they advance regardless of `on`
/// (`FallbackPolicy::allows`/`recovery_kinds`). This proves it end to end with
/// the real document shape, not just at the unit level.
#[tokio::test]
async fn quota_advances_the_chain_even_when_on_omits_it() {
    let harness = Harness::new(
        vec![
            Script::Failure {
                kind: oc_llm::FailureKind::Quota,
                message: "Resource has been exhausted (e.g. check quota).".to_owned(),
            },
            text_script("recovered on beta"),
        ],
        Arc::new(AllowAll),
        json!({"models": {
            "chain": ["test/alpha", "test/beta"],
            "fallback": {"on": ["empty_response", "transport"]}
        }}),
        None,
    )
    .await;
    harness.prompt("hi").await;
    harness.wait_for_event("session.execution.succeeded").await;

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2, "one attempt per candidate: {requests:?}");
    assert_eq!(requests[0].model.model.model_id, "alpha");
    assert_eq!(requests[1].model.model.model_id, "beta");
    assert_eq!(
        harness.sink.count("session.model.fallback"),
        1,
        "quota advanced exactly one hop: {:?}",
        harness.sink.kinds()
    );
    let fallbacks = harness.sink.data("session.model.fallback");
    assert_eq!(fallbacks[0]["kind"], "quota");
    assert_eq!(fallbacks[0]["from"]["id"], "alpha");
    assert_eq!(fallbacks[0]["to"]["id"], "beta");

    // The raw configured `on` list is never rewritten behind the user's back.
    assert_eq!(
        harness.config.config.merged()["models"]["fallback"]["on"],
        json!(["empty_response", "transport"])
    );
}

/// The same narrow `on` list with `rate_limit`: a throttled window also clears
/// by moving to the next chain entry, so it must not be gated on `on` either.
#[tokio::test]
async fn rate_limit_advances_the_chain_with_the_same_narrow_on() {
    let harness = Harness::new(
        vec![
            Script::Failure {
                kind: oc_llm::FailureKind::RateLimit,
                message: "429 Too Many Requests: rate limit reached".to_owned(),
            },
            text_script("recovered on beta"),
        ],
        Arc::new(AllowAll),
        json!({"models": {
            "chain": ["test/alpha", "test/beta"],
            "fallback": {"on": ["empty_response", "transport"]}
        }}),
        None,
    )
    .await;
    harness.prompt("hi").await;
    harness.wait_for_event("session.execution.succeeded").await;

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2, "one attempt per candidate: {requests:?}");
    assert_eq!(requests[1].model.model.model_id, "beta");
    assert_eq!(
        harness.sink.count("session.model.fallback"),
        1,
        "{:?}",
        harness.sink.kinds()
    );
    assert_eq!(
        harness.sink.data("session.model.fallback")[0]["kind"],
        "rate_limit"
    );
}

/// An `on = []` document (deliberate "never fall back" for the opt-in classes)
/// still recovers from quota, which is the pre-existing behavior this file
/// already pinned at the unit level.
#[tokio::test]
async fn empty_on_still_advances_on_quota() {
    let harness = Harness::new(
        vec![
            Script::Failure {
                kind: oc_llm::FailureKind::Quota,
                message: "quota exceeded for this project".to_owned(),
            },
            text_script("recovered on beta"),
        ],
        Arc::new(AllowAll),
        json!({"models": {
            "chain": ["test/alpha", "test/beta"],
            "fallback": {"on": []}
        }}),
        None,
    )
    .await;
    harness.prompt("hi").await;
    harness.wait_for_event("session.execution.succeeded").await;
    assert_eq!(harness.sink.count("session.model.fallback"), 1);
    assert_eq!(harness.model.requests()[1].model.model.model_id, "beta");
}

// ---------------------------------------------------------------------------
// Compaction model resolution: `models.chains.compaction` (else `models.chain`)
// with the turn model as the final fallback, exactly one fallback hop, and the
// `session.compaction.ended` payload / completed row naming the model that
// ACTUALLY produced the summary.
// ---------------------------------------------------------------------------

/// Waits until the inbox row is consumed and every compaction row settled,
/// returning the compaction rows.
async fn wait_compaction(harness: &Harness) -> Vec<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let rows = compaction_rows(harness).await;
        let settled = rows.iter().all(|row| {
            matches!(
                row.get("status").and_then(Value::as_str),
                Some("completed" | "failed")
            )
        });
        if settled && harness.inbox().await.is_empty() && !rows.is_empty() {
            return rows;
        }
        assert!(std::time::Instant::now() < deadline, "timed out: {rows:?}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn compaction_rows(harness: &Harness) -> Vec<Value> {
    harness
        .messages()
        .await
        .into_iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("compaction"))
        .collect()
}

/// Enqueues the same `compaction` inbox row `POST /api/session/:sid/compact`
/// writes and wakes the actor.
async fn enqueue_compaction(harness: &Harness) {
    harness
        .store
        .enqueue(oc_store::NewInbox {
            id: oc_types::MessageId::new().into_inner(),
            session_id: harness.session.clone(),
            kind: "compaction".to_owned(),
            payload: json!({
                "time": {"created": oc_types::now_millis()},
                "status": "pending",
                "reason": "manual"
            }),
            delivery: oc_store::Delivery::Queue,
        })
        .await
        .expect("compaction enqueued");
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
}

/// A quota failure on the first `models.chains.compaction` entry advances to the
/// second one, and BOTH surfaces — the completed compaction row and the
/// `session.compaction.ended` payload — name the model that actually produced
/// the summary.
#[tokio::test]
async fn compaction_advances_on_quota() {
    let harness = Harness::new(
        vec![
            Script::Failure {
                kind: oc_llm::FailureKind::Quota,
                message: "Resource has been exhausted (e.g. check quota).".to_owned(),
            },
            text_script("## Objective\nsummarized on beta"),
        ],
        Arc::new(AllowAll),
        json!({"models": {"chains": {"compaction": ["test/alpha", "test/beta"]}}}),
        None,
    )
    .await;
    enqueue_compaction(&harness).await;
    let rows = wait_compaction(&harness).await;

    assert_eq!(rows.len(), 1, "exactly one compaction row: {rows:?}");
    assert_eq!(rows[0]["status"], "completed");
    assert_eq!(
        rows[0]["model"]["id"], "beta",
        "the completed row names the model that produced the summary"
    );
    assert_eq!(
        harness.model.calls(),
        2,
        "alpha then beta, no third attempt"
    );

    let ended = harness.sink.data("session.compaction.ended");
    assert_eq!(ended.len(), 1, "one ended event: {ended:?}");
    assert_eq!(
        ended[0]["model"]["id"], "beta",
        "the event reports the producing model, not the session's model: {ended:?}"
    );
    assert!(
        !harness.sink.has("session.model.fallback"),
        "a compaction hop is not a session model fallback"
    );
    // `session.model` is NOT rewritten by a compaction.
    let session = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("session row")
        .expect("session exists");
    assert_eq!(
        session["model"]["id"], "alpha",
        "compaction never rewrites the session's configured model: {session}"
    );
}

/// `Auth` says the *key* is wrong, not that the model is unusable: compaction
/// makes exactly one attempt and surfaces the typed error.
#[tokio::test]
async fn compaction_does_not_advance_on_auth() {
    let harness = Harness::new(
        vec![Script::Failure {
            kind: oc_llm::FailureKind::Auth,
            message: "invalid api key".to_owned(),
        }],
        Arc::new(AllowAll),
        json!({"models": {"chains": {"compaction": ["test/alpha", "test/beta"]}}}),
        None,
    )
    .await;
    enqueue_compaction(&harness).await;
    let rows = wait_compaction(&harness).await;

    assert_eq!(rows.len(), 1, "exactly one compaction row: {rows:?}");
    assert_eq!(rows[0]["status"], "failed");
    assert_eq!(
        rows[0]["error"]["message"], "invalid api key",
        "the provider text is preserved: {rows:?}"
    );
    assert_eq!(
        harness.model.calls(),
        1,
        "an auth failure must not spend a second candidate"
    );
    assert!(harness.inbox().await.is_empty(), "the row is consumed");
}

/// No `models.chains.compaction` and no `models.chain`: compaction runs on
/// exactly the model the session already uses, as it always did.
#[tokio::test]
async fn compaction_without_a_chain_uses_the_turn_model() {
    let harness = Harness::new(
        vec![text_script("## Objective\nsummarized on the turn model")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    enqueue_compaction(&harness).await;
    let rows = wait_compaction(&harness).await;

    assert_eq!(rows.len(), 1, "exactly one compaction row: {rows:?}");
    assert_eq!(rows[0]["status"], "completed");
    assert_eq!(
        rows[0]["model"]["id"], "alpha",
        "without a chain the turn model produces the summary"
    );
    assert_eq!(harness.model.calls(), 1);
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].model.model.model_id, "alpha",
        "the request went to the turn model"
    );
    let ended = harness.sink.data("session.compaction.ended");
    assert_eq!(ended.len(), 1, "one ended event: {ended:?}");
    assert_eq!(ended[0]["model"]["id"], "alpha");
}
