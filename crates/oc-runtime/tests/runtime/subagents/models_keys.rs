//! Model selection, step-limit reporting, key pinning, and balanced healthy-key rotation.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::session_messages;
use crate::support::*;

#[tokio::test]
async fn subagent_model_override_resolves_and_unavailable_models_fail_before_spawn() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {
            "background_default": true,
            "default_model": "test/alpha",
            "agents": {"general": {"model": "test/alpha"}},
        }}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    tool.execute(
        &tool_context(&harness, &harness.session),
        json!({
            "agent": "general",
            "description": "modelled",
            "prompt": "work",
            "model": "test/alpha",
        }),
    )
    .await
    .expect("spawn with an available model");
    let children = child_sessions(&harness.store, &harness.session).await;
    assert_eq!(children.len(), 1);
    let session = harness
        .store
        .get_session(&children[0])
        .await
        .expect("session")
        .expect("exists");
    assert_eq!(session["model"]["providerID"], "test");
    assert_eq!(session["model"]["id"], "alpha");

    // An unknown explicit call model fails loudly instead of falling back to
    // agent/default/parent.
    let err = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "missing",
                "prompt": "work",
                "model": "test/missing",
            }),
        )
        .await
        .expect_err("an unavailable call model must fail loudly without fallback");
    assert!(
        err.message.contains("test/missing") && err.message.contains("not_available"),
        "{}",
        err.message
    );
    assert_eq!(
        child_sessions(&harness.store, &harness.session).await.len(),
        1,
        "failed explicit call model must not spawn a child"
    );
}

#[tokio::test]
async fn parent_completion_message_carries_the_subagent_metadata() {
    let harness = Harness::new(
        vec![text_script("finished work")],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false}}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "labelled child",
                "prompt": "work",
                "background": true,
            }),
        )
        .await
        .expect("background spawn");
    let child = serde_json::from_str::<Value>(result.output.as_deref().expect("output"))
        .expect("json")["sessionID"]
        .as_str()
        .expect("child")
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let messages = session_messages(&harness.store, &harness.session).await;
        if let Some(message) = messages.iter().find(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        }) {
            assert_eq!(message["metadata"]["agent"], "general");
            assert_eq!(message["metadata"]["state"], "completed");
            let text = message["text"].as_str().expect("envelope");
            assert!(
                text.starts_with(&format!(
                    "<subagent sessionID=\"{child}\" state=\"completed\" description=\"labelled child\">"
                )),
                "{text}"
            );
            assert!(text.contains("finished work"));
            break;
        }
        assert!(Instant::now() < deadline, "no completion message");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn subagent_step_limit_is_reported_to_the_parent() {
    // The child agent gets a budget of one tool-enabled step, so its scripted
    // read exhausts the budget and the next (final, tool-less) call only
    // produces a summary. Without the qualifier the parent would see an
    // ordinary `state="completed"` result and plan on a truncated answer.
    let harness = Harness::new(
        vec![
            tool_script(
                "read_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("partial summary"),
        ],
        Arc::new(AllowAll),
        json!({
            "agents": {"general": {"steps": 1}},
            "subagents": {"background_default": false},
        }),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "truncated child",
                "prompt": "work",
                "background": true,
            }),
        )
        .await
        .expect("background spawn");
    let child = serde_json::from_str::<Value>(result.output.as_deref().expect("output"))
        .expect("json")["sessionID"]
        .as_str()
        .expect("child")
        .to_owned();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let messages = session_messages(&harness.store, &harness.session).await;
        if let Some(message) = messages.iter().find(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        }) {
            assert_eq!(message["metadata"]["state"], "completed");
            assert_eq!(message["metadata"]["stepLimit"], true);
            let steps = message["metadata"]["steps"]
                .as_u64()
                .expect("steps recorded on the envelope metadata");
            assert_eq!(
                steps, 1,
                "the reported count is the configured budget, not an arbitrary number"
            );
            let text = message["text"].as_str().expect("envelope");
            assert!(
                text.contains(&format!(
                    "\n[step budget exhausted after {steps} steps; result may be incomplete]"
                )),
                "{text}"
            );
            assert!(text.contains("partial summary"), "{text}");
            break;
        }
        assert!(Instant::now() < deadline, "no step-limit report");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn subagent_that_stops_on_its_own_has_no_step_limit_marker() {
    // Negative control for the qualifier: the child answers in one tool-less
    // step, so its turn never hits the budget. The parent envelope must stay
    // byte-identical to the ordinary completion (no `stepLimit`, no `steps`,
    // no appended caveat) and no `session.execution.step_limit` event may be
    // committed for the child.
    let harness = Harness::new(
        vec![text_script("child final")],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false}}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "plain child",
                "prompt": "work",
                "background": true,
            }),
        )
        .await
        .expect("background spawn");
    let child = serde_json::from_str::<Value>(result.output.as_deref().expect("output"))
        .expect("json")["sessionID"]
        .as_str()
        .expect("child")
        .to_owned();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let messages = session_messages(&harness.store, &harness.session).await;
        if let Some(message) = messages.iter().find(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        }) {
            assert_eq!(message["metadata"]["state"], "completed");
            assert!(
                message["metadata"].get("stepLimit").is_none(),
                "an unexhausted budget leaves no marker: {}",
                message["metadata"]
            );
            assert!(
                message["metadata"].get("steps").is_none(),
                "an unexhausted budget records no step count: {}",
                message["metadata"]
            );
            let text = message["text"].as_str().expect("envelope");
            assert!(text.contains("child final"), "{text}");
            assert!(!text.contains("step budget exhausted"), "{text}");
            break;
        }
        assert!(Instant::now() < deadline, "no completion message");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let logged = harness
        .store
        .events_after(&child, 0, 1_000)
        .await
        .expect("event log");
    assert!(
        !logged
            .iter()
            .any(|event| event.kind == "session.execution.step_limit"),
        "the child's own log carries no qualifier: {:?}",
        logged.iter().map(|event| &event.kind).collect::<Vec<_>>()
    );
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn subagent_key_pins_credential_and_inherits_parent_credential() {
    let harness = Harness::new(
        vec![
            text_script("child pinned output"),
            text_script("inherited child output"),
            text_script("normalized label output"),
            text_script("provider alias output"),
        ],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false}}),
        None,
    )
    .await;
    harness
        .store
        .create_credential(oc_store::NewCredential {
            id: Some("cred_parent".to_owned()),
            integration_id: "test".to_owned(),
            label: Some("OpenCode 1".to_owned()),
            value: json!({"type": "key", "key": "sk-parent"}),
            active: Some(true),
        })
        .await
        .expect("cred 1");
    harness
        .store
        .create_credential(oc_store::NewCredential {
            id: Some("cred_worker".to_owned()),
            integration_id: "test".to_owned(),
            label: Some("OpenCode 2".to_owned()),
            value: json!({"type": "key", "key": "sk-worker"}),
            active: Some(false),
        })
        .await
        .expect("cred 2");
    harness
        .store
        .patch_session(
            &harness.session,
            oc_store::SessionPatch {
                model: Some(json!({
                    "providerID": "test",
                    "id": "alpha",
                    "credentialID": "cred_parent"
                })),
                ..oc_store::SessionPatch::default()
            },
        )
        .await
        .expect("patch parent model");

    let spawn = subagent_tool(&harness, "subagent");
    // Explicit label matches `cred_worker`
    spawn
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "description": "pinned key child",
                "prompt": "use key 2",
                "key": "OpenCode 2",
                "background": false,
            }),
        )
        .await
        .expect("spawn with explicit key");
    // Omitted `key` inherits parent's `cred_parent` via ModelSource::Parent
    spawn
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "description": "inherited key child",
                "prompt": "use parent key",
                "background": false,
            }),
        )
        .await
        .expect("spawn inheriting parent key");

    // Normalized label (case/punctuation-insensitive, e.g. `opencode-2` -> `OpenCode 2`)
    // and provider/default aliases resolve cleanly instead of failing with InvalidKeyError.
    spawn
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "description": "normalized label child",
                "prompt": "use normalized label",
                "key": "opencode-2",
                "background": false,
            }),
        )
        .await
        .expect("spawn with normalized key label");
    spawn
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "description": "provider alias key child",
                "prompt": "use provider alias",
                "key": "test",
                "background": false,
            }),
        )
        .await
        .expect("spawn with provider alias falls back cleanly");

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].options.api_key.as_deref(), Some("sk-worker"));
    assert_eq!(requests[1].options.api_key.as_deref(), Some("sk-parent"));
    assert_eq!(requests[2].options.api_key.as_deref(), Some("sk-worker"));
    assert_eq!(requests[3].options.api_key.as_deref(), Some("sk-parent"));

    // Invalid key returns clear error listing available keys
    let err = spawn
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "description": "bad key",
                "prompt": "fail",
                "key": "nonexistent_key",
                "background": false,
            }),
        )
        .await
        .expect_err("unknown key fails");
    assert!(err.message.contains("InvalidKeyError"), "{}", err.message);
    assert!(err.message.contains("cred_worker"), "{}", err.message);
}

#[tokio::test]
async fn subagent_balances_healthy_keys_and_rotates_off_cooling_parent_key() {
    use std::collections::HashSet;

    let harness = Harness::new(
        vec![
            text_script("first child"),
            text_script("second child"),
            text_script("rotated child"),
        ],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false}}),
        None,
    )
    .await;
    for (id, label, key, active) in [
        ("cred_1", "Key One", "sk-balanced-1", true),
        ("cred_2", "Key Two", "sk-balanced-2", false),
    ] {
        harness
            .store
            .create_credential(oc_store::NewCredential {
                id: Some(id.to_owned()),
                integration_id: "test".to_owned(),
                label: Some(label.to_owned()),
                value: json!({"type": "key", "key": key}),
                active: Some(active),
            })
            .await
            .expect("cred");
    }

    let spawn = subagent_tool(&harness, "subagent");
    for idx in 1..=2 {
        spawn
            .execute(
                &tool_context(&harness, &harness.session),
                json!({
                    "description": format!("balanced {idx}"),
                    "prompt": "work",
                    "background": false,
                }),
            )
            .await
            .expect("balanced spawn");
    }
    let first_two_keys: HashSet<String> = harness
        .model
        .requests()
        .iter()
        .filter_map(|req| req.options.api_key.clone())
        .collect();
    assert_eq!(
        first_two_keys,
        HashSet::from(["sk-balanced-1".to_owned(), "sk-balanced-2".to_owned()]),
        "unpinned parent automatically balances spawns across healthy keys"
    );

    // Pin parent to cred_1, then mark cred_1 cooling: child must rotate away from cred_1 to cred_2.
    harness
        .store
        .patch_session(
            &harness.session,
            oc_store::SessionPatch {
                model: Some(json!({
                    "providerID": "test",
                    "id": "alpha",
                    "credentialID": "cred_1"
                })),
                ..oc_store::SessionPatch::default()
            },
        )
        .await
        .expect("patch parent");
    harness.runtime.key_pool().mark(
        "test",
        Some("cred_1"),
        oc_llm::FailureKind::Quota,
        oc_types::now_millis(),
    );
    spawn
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "description": "avoid cooling parent key",
                "prompt": "work",
                "background": false,
            }),
        )
        .await
        .expect("spawn rotates off cooling parent key");
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2].options.api_key.as_deref(),
        Some("sk-balanced-2")
    );
}
