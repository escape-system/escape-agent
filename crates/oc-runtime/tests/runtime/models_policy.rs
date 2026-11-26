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

/// `models.chain` does not override explicit `agents.<id>.model`,
/// `subagents.default_model`, or `parent` when `model:` is omitted.
#[tokio::test]
async fn chain_does_not_override_agent_or_parent_selection() {
    let harness = Harness::new(
        vec![text_script("child final")],
        Arc::new(AllowAll),
        json!({
            "models": {"chain": ["test/retired"]},
            "agents": {"general": {"model": "test/beta"}},
            "subagents": {"background_default": true, "default_model": "test/alpha"},
        }),
        None,
    )
    .await;
    let result = subagent_tool(&harness, "subagent")
        .execute(&tool_context(&harness, &harness.session), spawn_input(true))
        .await
        .expect("agent model wins without consulting models.chain");
    let metadata = result.metadata.expect("metadata");
    assert_eq!(metadata["model"]["id"], "beta");
    assert_eq!(metadata["modelSource"], "agent");
    assert!(
        metadata.get("rejectedModels").is_none(),
        "retired chain entry is never evaluated: {metadata}"
    );
}

/// An explicit `model:` call argument always wins, even when a retired chain
/// entry is present.
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

/// When `model:` is omitted and `models.chain` contains retired entries, the
/// spawn still resolves cleanly to `parent` without spurious chain skips.
#[tokio::test]
async fn retired_chain_entry_is_ignored_when_model_is_omitted() {
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
        .expect("omitted model resolves to parent instead of chain");
    let metadata = result.metadata.expect("metadata");
    assert_eq!(metadata["model"]["id"], "alpha");
    assert_eq!(metadata["modelSource"], "parent");
    assert!(metadata.get("rejectedModels").is_none(), "{metadata}");
}

/// An explicit `model:` that the catalog does not know fails loudly instead of
/// silently falling back to `models.chain`, `agent`, or `parent`.
#[tokio::test]
async fn unknown_call_model_fails_loudly_without_fallback() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"models": {"chain": ["test/beta"]}}),
        None,
    )
    .await;
    let mut input = spawn_input(true);
    input["model"] = json!("test/not-in-catalog");
    let error = subagent_tool(&harness, "subagent")
        .execute(&tool_context(&harness, &harness.session), input)
        .await
        .expect_err("explicit unknown call model must not fall back");
    assert!(
        error.message.contains("test/not-in-catalog") && error.message.contains("not_available"),
        "{}",
        error.message
    );
    assert!(
        child_sessions(&harness.store, &harness.session)
            .await
            .is_empty(),
        "failed explicit call model must not spawn a child"
    );
}

/// `model_unavailable` mid-turn does not automatically switch to `models.chain`.
#[tokio::test]
async fn model_unavailable_message_does_not_switch_models_mid_turn() {
    let message = "Claude Opus 4.6 is no longer available. Please switch to Claude Opus 5.5.";
    assert_eq!(
        oc_llm::classify_provider_message(message),
        oc_llm::FailureKind::ModelUnavailable,
        "the provider text classifies as a retired model"
    );
    let harness = Harness::new(
        vec![Script::Failure {
            kind: oc_llm::FailureKind::ModelUnavailable,
            message: message.to_owned(),
        }],
        Arc::new(AllowAll),
        json!({"models": {
            "chain": ["test/alpha", "test/beta"],
            "fallback": {"on": ["empty_response", "transport"], "max_hops": 2}
        }}),
        None,
    )
    .await;
    harness.prompt("hi").await;
    harness.wait_for_event("session.execution.failed").await;

    assert_eq!(harness.model.calls(), 1, "no fallback attempt");
    assert_eq!(harness.model.requests()[0].model.model.model_id, "alpha");
    assert_eq!(harness.sink.count("session.model.fallback"), 0);
}

/// Retired `models.chain` entries do not block or alter parent fallback-free
/// selection.
#[tokio::test]
async fn unusable_chain_entry_still_lets_parent_model_win() {
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
        .expect("parent model spawns cleanly");
    let metadata = result.metadata.expect("metadata");
    assert_eq!(metadata["modelSource"], "parent");
    assert_eq!(metadata["model"]["id"], "alpha");
}

/// NO-USABLE-CANDIDATE: every active candidate is denied by quota, so the error
/// names the denied candidate with its source (`test/alpha (denied, parent)`).
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

/// Helper asserting a provider failure never switches models mid-turn regardless
/// of `models.chain` / `models.fallback`.
async fn assert_turn_fails_without_fallback(
    kind: oc_llm::FailureKind,
    message: &str,
    models_cfg: Value,
) {
    let expected_attempts = match kind {
        oc_llm::FailureKind::Transport | oc_llm::FailureKind::EmptyResponse => 3,
        _ => 1,
    };
    let scripts = (0..expected_attempts)
        .map(|_| Script::Failure {
            kind,
            message: message.to_owned(),
        })
        .collect();
    let harness = Harness::new(
        scripts,
        Arc::new(AllowAll),
        json!({ "models": models_cfg }),
        None,
    )
    .await;
    harness.prompt("hi").await;
    harness.wait_for_event("session.execution.failed").await;

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        expected_attempts,
        "only bounded same-model attempts are tried: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|request| request.model.model.model_id == "alpha"),
        "every attempt stayed on chosen model alpha without chain fallback: {requests:?}"
    );
    assert_eq!(harness.sink.count("session.model.fallback"), 0);
    assert!(!harness.sink.has("session.execution.succeeded"));

    let failures = harness.sink.data("session.execution.failed");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["error"]["type"], "ProviderError");
    let err_msg = failures[0]["error"]["message"].as_str().unwrap_or("");
    assert!(err_msg.contains(message), "{err_msg}");

    let messages = harness.messages().await;
    let rows = assistants(&messages);
    assert_eq!(rows.len(), 1, "{messages:?}");
    assert!(rows[0].get("error").is_some(), "{}", rows[0]);
    assert_ne!(rows[0]["finish"], "stop");
}

#[tokio::test]
async fn empty_response_does_not_fallback_to_chain() {
    assert_turn_fails_without_fallback(
        oc_llm::FailureKind::EmptyResponse,
        "alpha returned no output",
        json!({
            "chain": ["test/alpha", "test/beta"],
            "fallback": {"on": ["empty_response"], "max_hops": 2}
        }),
    )
    .await;
}

#[tokio::test]
async fn transport_failure_does_not_fallback_to_chain() {
    assert_turn_fails_without_fallback(
        oc_llm::FailureKind::Transport,
        "alpha transport boom",
        json!({
            "chain": ["test/beta", "test/gamma"],
            "fallback": {"on": ["transport"], "max_hops": 2}
        }),
    )
    .await;
}

#[tokio::test]
async fn quota_failure_does_not_fallback_to_chain() {
    assert_turn_fails_without_fallback(
        oc_llm::FailureKind::Quota,
        "Resource has been exhausted (e.g. check quota).",
        json!({
            "chain": ["test/alpha", "test/beta"],
            "fallback": {"on": ["empty_response", "transport", "quota"], "max_hops": 2}
        }),
    )
    .await;
}

#[tokio::test]
async fn rate_limit_failure_does_not_fallback_to_chain() {
    assert_turn_fails_without_fallback(
        oc_llm::FailureKind::RateLimit,
        "429 Too Many Requests: rate limit reached",
        json!({
            "chain": ["test/alpha", "test/beta"],
            "fallback": {"on": ["rate_limit"], "max_hops": 2}
        }),
    )
    .await;
}

#[tokio::test]
async fn transport_stream_closed_recovers_on_retry_within_same_model() {
    let harness = Harness::new(
        vec![
            Script::Failure {
                kind: oc_llm::FailureKind::Transport,
                message: "provider stream closed before completion".to_owned(),
            },
            text_script("recovered after drop"),
        ],
        Arc::new(AllowAll),
        json!({ "models": { "chain": ["test/alpha", "test/beta"] } }),
        None,
    )
    .await;
    harness.prompt("hi").await;
    harness.wait_for_event("session.execution.succeeded").await;

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2, "retried once on transient drop");
    assert!(
        requests
            .iter()
            .all(|request| request.model.model.model_id == "alpha"),
        "retry stayed on chosen model alpha: {requests:?}"
    );
    assert_eq!(harness.sink.count("session.model.fallback"), 0);
}

/// Waits until the inbox row is consumed and every compaction row settled,
/// returning the compaction rows.
async fn wait_compaction(harness: &Harness) -> Vec<Value> {
    let deadline = std::time::Instant::now() + EVENT_WAIT;
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
        let inbox = harness.inbox().await;
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {EVENT_WAIT:?} waiting for a settled compaction row and an \
             empty inbox; saw {} row(s) {rows:?} and {} inbox row(s) {inbox:?}",
            rows.len(),
            inbox.len()
        );
        tokio::time::sleep(POLL).await;
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

/// Regression: models whose upstream `modelID` differs from `id` (or contains a
/// provider slash, like `nous/google/gemini-4-hydrogen` or an aliased
/// `nous/gemini-4-hydrogen`) must stay selectable by `subagent` and preserve
/// their catalog `id` when persisted and resolved on child turns.
#[tokio::test]
async fn nous_slash_and_aliased_models_resolve_in_subagent_and_turn() {
    let harness = Harness::new(
        vec![
            text_script("aliased nous ok"),
            text_script("direct nous ok"),
        ],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false}}),
        None,
    )
    .await;
    let mut catalog = harness.config.catalog.current().as_ref().clone();
    oc_llm::catalog::refresh::apply_builtin_providers(&mut catalog);
    catalog
        .apply_config(&json!({
            "nous": {
                "models": {
                    "gemini-4-hydrogen": {
                        "name": "Gemini 4 Hydrogen (via Nous)",
                        "modelID": "google/gemini-4-hydrogen",
                        "type": "chat",
                        "package": "@ai-sdk/openai-compatible"
                    }
                }
            }
        }))
        .expect("apply nous overlay");
    harness.config.catalog.replace(catalog);

    let tool = subagent_tool(&harness, "subagent");

    let alias_result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "nous alias",
                "prompt": "work",
                "model": "nous/gemini-4-hydrogen",
                "background": false,
            }),
        )
        .await
        .expect("aliased nous model spawns and completes");
    let alias_meta = alias_result.metadata.expect("alias metadata");
    assert_eq!(alias_meta["model"]["providerID"], "nous");
    assert_eq!(alias_meta["model"]["id"], "gemini-4-hydrogen");

    let direct_result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "nous direct",
                "prompt": "work",
                "model": "nous/google/gemini-4-hydrogen",
                "background": false,
            }),
        )
        .await
        .expect("direct slash-id nous model spawns and completes");
    let direct_meta = direct_result.metadata.expect("direct metadata");
    assert_eq!(direct_meta["model"]["providerID"], "nous");
    assert_eq!(direct_meta["model"]["id"], "google/gemini-4-hydrogen");

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].model.model.id, "gemini-4-hydrogen");
    assert_eq!(requests[0].model.model_id(), "google/gemini-4-hydrogen");
    assert_eq!(requests[1].model.model.id, "google/gemini-4-hydrogen");
    assert_eq!(requests[1].model.model_id(), "google/gemini-4-hydrogen");
}

/// No automatic compaction fallback: a failure on the first compaction
/// candidate fails compaction without trying a second model.
#[tokio::test]
async fn compaction_does_not_fallback_on_quota() {
    let harness = Harness::new(
        vec![Script::Failure {
            kind: oc_llm::FailureKind::Quota,
            message: "Resource has been exhausted (e.g. check quota).".to_owned(),
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
        harness.model.calls(),
        1,
        "no fallback candidate attempted in compaction"
    );
    assert!(!harness.sink.has("session.model.fallback"));
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
    // The row commits before its event is published, so the sink is awaited
    // rather than read the instant the row appears.
    harness.wait_for_events("session.compaction.ended", 1).await;
    let ended = harness.sink.data("session.compaction.ended");
    assert_eq!(
        ended.len(),
        1,
        "one ended event per compaction run: {ended:?}"
    );
    assert_eq!(ended[0]["model"]["id"], "alpha");
}
