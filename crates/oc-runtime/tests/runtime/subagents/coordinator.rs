//! Organizer coordinator model routing, selectable `key_health`, upper-session worker spawn, and `builder`.

use std::sync::Arc;
use std::time::Duration;

use oc_runtime::SubagentTools;
use serde_json::{Value, json};

use super::session_messages;
use crate::support::*;

#[tokio::test]
async fn organizer_defaults_to_coordinator_resolves_configured_model_and_sees_key_health_models() {
    let harness = Harness::new(
        vec![
            named_tool_script("key_health", "call_kh", json!({"include_models": true})),
            text_script("parent done"),
            text_script("organizer complete"),
        ],
        Arc::new(AllowAll),
        json!({
            "subagents": { "background_default": false },
            "models": { "chains": { "organizer": ["test/beta", "test/alpha"] } }
        }),
        None,
    )
    .await;
    harness
        .store
        .create_credential(oc_store::NewCredential {
            id: Some("cred_a".to_owned()),
            integration_id: "test".to_owned(),
            label: Some("Primary".to_owned()),
            value: json!({"type": "key", "key": "sk-secret-value"}),
            active: Some(true),
        })
        .await
        .expect("cred");

    harness.prompt("check resources").await;
    let requests = harness.model.requests();
    let kh_text = request_tool_result(&requests[1], 0);
    let kh_meta: Value = serde_json::from_str(&kh_text).expect("json key_health");
    assert_eq!(kh_meta["agentModels"]["organizer"], "test/beta");
    let models = kh_meta["providers"][0]["models"]
        .as_array()
        .expect("models array");
    assert!(models.iter().any(|m| m["ref"] == "test/beta"), "{models:?}");
    assert!(!kh_text.contains("sk-secret-value"));

    let spawn = subagent_tool(&harness, "subagent");
    let spawned = spawn
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "organizer",
                "description": "orchestrate wave",
                "prompt": "coordinate sub-tasks",
                "background": false,
            }),
        )
        .await
        .expect("spawn organizer");
    let meta = spawned.metadata.expect("meta");
    assert_eq!(meta["modelSource"], "chain");
    assert_eq!(meta["model"]["providerID"], "test");
    assert_eq!(meta["model"]["id"], "beta");
    let child_id = meta["sessionID"].as_str().expect("child id");
    let node = harness
        .store
        .session_node(child_id)
        .await
        .expect("node")
        .expect("present");
    assert_eq!(node.agent, "organizer");
    assert_eq!(node.role, "coordinator");
}

#[derive(Debug)]
struct AgHook;

impl oc_llm::LlmTransportHook for AgHook {
    fn handles(&self, _request: &oc_llm::LlmRequest) -> bool {
        false
    }

    fn stream(&self, _request: oc_llm::LlmRequest) -> oc_llm::HookStreamFuture {
        unreachable!()
    }

    fn dynamic_models(&self) -> Vec<oc_llm::ModelInfo> {
        let snap = json!({
            "google": {
                "id": "google",
                "name": "Google",
                "env": ["GOOGLE_API_KEY"],
                "models": {
                    "antigravity-gemini-4-hydrogen": chat_entry("antigravity-gemini-4-hydrogen", "Gemini 4 Hydrogen (Antigravity)", None, true, 200_000)
                }
            }
        });
        oc_llm::Catalog::from_snapshot(&snap.to_string())
            .expect("ag snap")
            .models()
    }
}

fn chat_entry(id: &str, name: &str, status: Option<&str>, tools: bool, ctx: u64) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".to_owned(), json!(id));
    obj.insert("type".to_owned(), json!("chat"));
    obj.insert("name".to_owned(), json!(name));
    obj.insert("release_date".to_owned(), json!("2026-01-01"));
    obj.insert(
        "modalities".to_owned(),
        json!({"input": ["text"], "output": ["text"]}),
    );
    obj.insert(
        "capabilities".to_owned(),
        json!({"tools": {"supported": tools}, "temperature": true}),
    );
    obj.insert(
        "limit".to_owned(),
        json!({"context": ctx, "output": 16_000}),
    );
    obj.insert(
        "cost".to_owned(),
        json!({"text": {"input": {"price": 1}, "output": {"price": 2}}}),
    );
    obj.insert(
        "api".to_owned(),
        json!({"chat-completions": {"base_url": "http://unused.invalid", "sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}),
    );
    if let Some(st) = status {
        obj.insert("status".to_owned(), json!(st));
    }
    Value::Object(obj)
}

struct SingleGateModel {
    gate: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl oc_runtime::ModelClient for SingleGateModel {
    fn stream(
        &self,
        request: oc_llm::LlmRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<oc_llm::LlmStream, oc_llm::LlmError>> {
        use futures_util::StreamExt;
        use futures_util::stream;
        use oc_llm::{LlmEvent, Usage};
        let is_worker = request.messages.iter().any(|msg| {
            msg.content.iter().any(|part| {
                part.as_text()
                    .is_some_and(|text| text.contains("check crate"))
            })
        });
        let gate = if is_worker {
            self.gate.lock().expect("gate").take()
        } else {
            None
        };
        Box::pin(async move {
            if let Some(rx) = gate {
                let _ = rx.await;
            }
            let text = "worker finished cleanly".to_owned();
            Ok(stream::iter(vec![
                LlmEvent::StepStart,
                LlmEvent::TextStart,
                LlmEvent::TextDelta {
                    delta: text.clone(),
                },
                LlmEvent::TextEnd { text },
                LlmEvent::Finish {
                    reason: "stop".to_owned(),
                    usage: Usage::default(),
                },
            ])
            .boxed())
        })
    }
}

async fn seed_selectable_catalog_and_creds(harness: &Harness) {
    let mut embed = chat_entry("embed", "Embed", None, false, 8_192);
    embed["type"] = json!("embedding");
    let custom_catalog = json!({
        "test": {
            "id": "test",
            "name": "Test Provider",
            "env": ["TEST_API_KEY"],
            "models": {
                "alpha": chat_entry("alpha", "Alpha", None, true, 100_000),
                "beta": chat_entry("beta", "Beta", None, true, 100_000),
                "hidden-one": chat_entry("hidden-one", "Hidden One", None, true, 100_000),
                "deprecated-one": chat_entry("deprecated-one", "Deprecated", Some("deprecated"), true, 100_000),
                "no-tools": chat_entry("no-tools", "No Tools", None, false, 100_000),
                "embed": embed,
            }
        },
        "google": {
            "id": "google",
            "name": "Google",
            "env": ["GOOGLE_API_KEY"],
            "models": {
                "gemini-2.5-pro": chat_entry("gemini-2.5-pro", "Raw Gemini 2.5 Pro", None, true, 1_000_000)
            }
        }
    });
    harness.config.catalog.replace(
        oc_llm::Catalog::from_snapshot(&custom_catalog.to_string()).expect("custom catalog"),
    );
    harness.config.client.set_hook(Some(Arc::new(AgHook)));
    for (id, int_id, val) in [
        (
            "cred_test",
            "test",
            json!({"type": "key", "key": "sk-test"}),
        ),
        (
            "cred_google_oauth",
            "google",
            json!({"type": "oauth", "access": "ya29.test"}),
        ),
    ] {
        harness
            .store
            .create_credential(oc_store::NewCredential {
                id: Some(id.to_owned()),
                integration_id: int_id.to_owned(),
                label: Some(int_id.to_owned()),
                value: val,
                active: Some(true),
            })
            .await
            .expect("cred");
    }
}

#[tokio::test]
async fn key_health_filters_to_selectable_models_and_keeps_notes_and_favorites() {
    let harness = Harness::new(
        vec![
            named_tool_script("key_health", "call_kh", json!({"include_models": true})),
            text_script("done"),
        ],
        Arc::new(AllowAll),
        json!({
            "agents": { "builder": { "model": "test/beta" } },
            "models": {
                "chain": ["test/beta"],
                "favorites": ["test/alpha"],
                "hidden": ["test/hidden-one"],
                "notes": { "test/beta": "use sparingly" }
            }
        }),
        None,
    )
    .await;
    seed_selectable_catalog_and_creds(&harness).await;

    harness.prompt("inspect").await;
    let requests = harness.model.requests();
    let raw = request_tool_result(&requests[1], 0);
    assert!(!raw.contains('\n'), "compact JSON expected: {raw}");
    let kh: Value = serde_json::from_str(&raw).expect("parse json");
    assert_eq!(kh["agentModels"]["builder"], "test/beta");

    let providers = kh["providers"].as_array().expect("providers");
    let refs_of = |pid: &str| -> Vec<String> {
        providers
            .iter()
            .find(|p| p["providerID"] == pid)
            .expect("provider")["models"]
            .as_array()
            .expect("models")
            .iter()
            .filter_map(|m| m["ref"].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(
        refs_of("google"),
        vec!["google/antigravity-gemini-4-hydrogen"]
    );
    assert_eq!(refs_of("test"), vec!["test/alpha", "test/beta"]);
    let test_models = providers
        .iter()
        .find(|p| p["providerID"] == "test")
        .unwrap()["models"]
        .as_array()
        .unwrap();
    assert_eq!(test_models[0]["favorite"], true);
    assert_eq!(test_models[1]["note"], "use sparingly");
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn organizer_attaches_workers_to_upper_session_and_routes_reporting() {
    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel();
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({
            "subagents": {
                "background_default": true,
                "digest_model": "test/alpha"
            }
        }),
        None,
    )
    .await;
    // Switch root to /basic (`max_depth = 1`) to prove coordinator parent-attachment
    // works even when depth-2 nesting is prohibited.
    harness
        .store
        .patch_session(
            &harness.session,
            oc_store::SessionPatch {
                metadata: Some(json!({"subagentMode": "basic"})),
                ..oc_store::SessionPatch::default()
            },
        )
        .await
        .expect("patch basic mode");

    let model = Arc::new(SingleGateModel {
        gate: std::sync::Mutex::new(Some(gate_rx)),
    });
    let runtime = oc_runtime::Runtime::new_with_model_client(harness.config.clone(), model)
        .with_scribe(FakeScribe::new("checked cleanly"));
    let registry = SubagentTools::new(runtime.clone()).registry();
    let spawn = registry.get("subagent").expect("subagent");
    let roster = registry.get("roster").cloned().expect("roster");
    let digest = registry.get("digest").expect("digest");
    let talk = registry.get("talk").expect("talk");

    // 1. Root spawns persistent organizer coordinator.
    let org_res = spawn
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "organizer",
                "description": "team leader",
                "prompt": "lead workers",
                "background": true,
            }),
        )
        .await
        .expect("spawn organizer");
    let org_id = org_res.metadata.expect("org meta")["sessionID"]
        .as_str()
        .expect("org id")
        .to_owned();
    runtime
        .wait_idle(&org_id, IDLE_WAIT)
        .await
        .expect("org idle");

    // 2. Organizer spawns a worker: automatically attaches onto Root (`depth = 1`)
    // even under `/basic`, carrying `coordinatorID = org_id`.
    let worker_res = spawn
        .execute(
            &tool_context(&harness, &org_id),
            json!({
                "agent": "builder",
                "description": "run cargo check",
                "prompt": "check crate",
                "background": true,
            }),
        )
        .await
        .expect("organizer spawns builder onto parent session");
    let worker_id = worker_res.metadata.expect("worker meta")["sessionID"]
        .as_str()
        .expect("worker id")
        .to_owned();
    let worker_node = harness
        .store
        .session_node(&worker_id)
        .await
        .expect("node")
        .expect("present");
    assert_eq!(
        worker_node.parent_id.as_deref(),
        Some(harness.session.as_str()),
        "worker sits directly under the upper root session"
    );
    assert_eq!(worker_node.depth, 1);
    let worker_info = harness
        .store
        .get_session(&worker_id)
        .await
        .expect("info")
        .expect("present");
    assert_eq!(worker_info["metadata"]["coordinatorID"], org_id);

    // Non-coordinator worker cannot attach siblings to its parent session.
    let worker_err = spawn
        .execute(
            &tool_context(&harness, &worker_id),
            json!({
                "agent": "explore",
                "description": "illegal",
                "prompt": "nope",
                "spawn": { "attach": "parent" }
            }),
        )
        .await
        .expect_err("non-coordinator cannot attach to parent");
    assert!(
        worker_err.to_string().contains("SpawnForbiddenError"),
        "{worker_err}"
    );

    // 3. Coordinated worker talks upward: omitted `to` routes to organizer, while
    // explicit `to = root` is rejected while coordinator is active.
    talk.execute(
        &tool_context(&harness, &worker_id),
        json!({"kind": "note", "text": "progress to leader"}),
    )
    .await
    .expect("default talk routes to coordinator");
    let org_inbox = harness.store.list_inbox(&org_id).await.expect("org inbox");
    assert!(
        org_inbox
            .iter()
            .any(|row| row.pointer("/payload/text").and_then(Value::as_str)
                == Some("progress to leader")),
        "omitted `to` on coordinated worker delivered to organizer: {org_inbox:?}"
    );
    let direct_root_err = talk
        .execute(
            &tool_context(&harness, &worker_id),
            json!({"to": harness.session, "kind": "note", "text": "bypass leader"}),
        )
        .await
        .expect_err("coordinated worker cannot bypass active coordinator to root");
    assert!(
        direct_root_err.to_string().contains(&org_id),
        "error names coordinator: {direct_root_err}"
    );

    // Drain organizer inbox before testing `roster(wait_all=true)` across coordinated siblings.
    runtime
        .wait_idle(&org_id, IDLE_WAIT)
        .await
        .expect("org idle after talk");
    let ctx_org = tool_context(&harness, &org_id);
    let wait_task = tokio::spawn(async move {
        roster
            .execute(&ctx_org, json!({"wait_seconds": 10, "wait_all": true}))
            .await
    });
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        !wait_task.is_finished(),
        "organizer roster must watch busy coordinated siblings on parent session"
    );
    let _ = gate_tx.send(());
    let roster_out = tokio::time::timeout(IDLE_WAIT, wait_task)
        .await
        .expect("roster finished")
        .expect("join")
        .expect("roster ok");
    let roster_meta = roster_out.metadata.expect("roster meta");
    assert_eq!(roster_meta["wakeReason"], "all_settled");
    assert_eq!(roster_meta["counts"]["total"], 1);
    assert_eq!(roster_meta["counts"]["completed"], 1);
    assert_eq!(roster_meta["siblings"][0]["coordinatorID"], org_id);

    // 4. Completion synthetic landed on organizer, NOT root.
    runtime
        .wait_idle(&org_id, IDLE_WAIT)
        .await
        .expect("org settled");
    let org_msgs = session_messages(&harness.store, &org_id).await;
    assert!(
        org_msgs.iter().any(|m| {
            m.get("type").and_then(Value::as_str) == Some("synthetic")
                && m.pointer("/metadata/childID").and_then(Value::as_str) == Some(&worker_id)
        }),
        "organizer received completion report for sibling worker"
    );
    let root_msgs = session_messages(&harness.store, &harness.session).await;
    assert!(
        !root_msgs.iter().any(|m| {
            m.pointer("/metadata/childID").and_then(Value::as_str) == Some(&worker_id)
        }),
        "root session was NOT spammed with worker completion"
    );

    // 5. Organizer may summarize coordinated sibling via `digest`.
    let digest_res = digest
        .execute(
            &tool_context(&harness, &org_id),
            json!({"session_id": worker_id, "question": "status?"}),
        )
        .await
        .expect("coordinator can digest coordinated sibling");
    assert!(!digest_res.output.unwrap_or_default().is_empty());

    // 6. Verify organizer & builder permission matrices through ConfigService.
    for action in ["shell", "bash", "glob", "grep", "edit", "write", "patch"] {
        assert_eq!(
            harness
                .config
                .config
                .evaluate(Some("organizer"), action, "*"),
            oc_config::Effect::Deny,
            "organizer must deny {action}"
        );
    }
    for action in ["edit", "write", "patch", "subagent"] {
        assert_eq!(
            harness.config.config.evaluate(Some("builder"), action, "*"),
            oc_config::Effect::Deny,
            "builder must deny {action}"
        );
    }
    for action in ["shell", "lane", "read"] {
        assert_eq!(
            harness.config.config.evaluate(Some("builder"), action, "*"),
            oc_config::Effect::Allow,
            "builder must allow {action}"
        );
    }
}
