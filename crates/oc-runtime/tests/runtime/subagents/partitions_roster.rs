//! Tool schemas, `partitions` fan-out, hot-start dispatch, and `roster` waits.

use std::sync::Arc;
use std::time::Duration;

use oc_runtime::SubagentTools;
use oc_store::{Delivery, NewInbox, NewSession, NewSessionTree};
use oc_types::MessageId;
use serde_json::{Value, json};

use super::session_messages;
use crate::support::*;

#[tokio::test]
async fn subagent_schemas_match_the_documented_keys() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let registry = SubagentTools::new(harness.runtime.clone()).registry();
    let keys = |name: &str| -> Vec<String> {
        let mut keys: Vec<String> = registry.get(name).expect("tool").schema()["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(
        keys("subagent"),
        [
            "agent",
            "background",
            "description",
            "group",
            "key",
            "lane",
            "model",
            "partition_count",
            "partition_index",
            "partitions",
            "prompt",
            "role",
            "sessionID",
            "spawn"
        ]
    );
    assert_eq!(
        keys("talk"),
        ["kind", "reply_to", "request_reply", "text", "to"]
    );
    assert_eq!(keys("roster"), ["session_ids", "wait_all", "wait_seconds"]);
    assert_eq!(
        keys("session_write"),
        [
            "delivery",
            "interrupt",
            "kind",
            "reply_to",
            "session_id",
            "text"
        ]
    );
    assert_eq!(
        keys("digest"),
        ["max_tokens", "question", "session_id", "tool_call_id"]
    );
    let subagent = registry.get("subagent").expect("subagent").schema();
    assert_eq!(subagent["required"], json!(["description", "prompt"]));
    let agent = subagent["properties"]["agent"]
        .as_object()
        .expect("agent schema");
    let agent_description = agent["description"].as_str().expect("agent description");
    assert!(
        agent_description.contains("general"),
        "spawnable agents are named: {agent_description}"
    );
    assert!(
        subagent["properties"]["description"]["type"] == "string"
            && subagent["properties"]["prompt"]["type"] == "string"
    );
    assert_eq!(subagent["additionalProperties"], false);
    assert_eq!(
        subagent["properties"]["role"]["enum"],
        json!(["worker", "coordinator"])
    );
}

#[tokio::test]
async fn subagent_tool_is_filtered_out_for_agents_that_cannot_spawn() {
    let harness = Harness::new(
        vec![text_script("done")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let session = "ses_explore_root".to_owned();
    harness
        .store
        .create_session_with_tree(
            NewSession {
                id: session.clone(),
                parent_id: None,
                project_id: "prj_runtime_test".to_owned(),
                directory: path(&harness.work),
                title: Some("explore root".to_owned()),
                agent: Some("explore".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: None,
                time_created: oc_types::now_millis(),
            },
            NewSessionTree {
                root_id: session.clone(),
                depth: 0,
                role: "primary".to_owned(),
                spawn_allowed: true,
            },
        )
        .await
        .expect("explore session");
    harness
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "hi", "time": {"created": oc_types::now_millis()}}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    harness.runtime.notify(&session).await.expect("notify");
    harness
        .runtime
        .wait_idle(&session, IDLE_WAIT)
        .await
        .expect("idle");

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    let names: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(
        !names.contains(&"subagent"),
        "the explore agent denies `subagent`: {names:?}"
    );
    assert!(
        names.contains(&"read"),
        "unrelated tools still reach the explore agent: {names:?}"
    );
}

#[tokio::test]
async fn subagent_spawn_records_group_in_metadata() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let tool = subagent_tool(&harness, "subagent");
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "description": "grouped worker",
                "prompt": "do work",
                "background": true,
                "group": "wave-alpha",
                "lane": "cargo"
            }),
        )
        .await
        .expect("spawn grouped subagent");
    let output = result.output.expect("output");
    assert!(output.contains("\"status\":\"running\""));

    let children = harness
        .store
        .children(&harness.session)
        .await
        .expect("children");
    assert_eq!(children.len(), 1);
    let child_id = &children[0].id;
    let session = harness
        .store
        .get_session(child_id)
        .await
        .expect("get session")
        .expect("session exists");
    let metadata = session.get("metadata").expect("metadata exists");
    assert_eq!(metadata["group"], "wave-alpha");
    assert_eq!(metadata["lane"], "cargo");
}

#[tokio::test]
async fn roster_wait_seconds_waits_for_running_child_to_settle() {
    let harness = Harness::new(
        vec![text_script("done after spawn")],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": true}}),
        None,
    )
    .await;
    let spawn = subagent_tool(&harness, "subagent");
    spawn
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "background worker",
                "prompt": "finish quickly",
                "background": true,
            }),
        )
        .await
        .expect("spawn background child");

    let roster = subagent_tool(&harness, "roster");
    let result = roster
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"wait_seconds": 10}),
        )
        .await
        .expect("roster with wait_seconds");
    let body: Value = serde_json::from_str(&result.output.expect("output")).expect("json");
    let children = body["children"].as_array().expect("children");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["status"], "completed");
    assert_eq!(body["wakeReason"], "child_settled");
}

#[tokio::test]
async fn subagent_partitions_spawns_disjoint_slices_and_collects_foreground() {
    let harness = Harness::new_with_setup(
        vec![
            text_script("slice 0 verified"),
            text_script("slice 1 verified"),
        ],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false, "max_children": 4}}),
        None,
        |work| {
            std::fs::create_dir_all(work.join("src")).unwrap();
            std::fs::write(
                work.join("src/auth.rs"),
                "pub fn verify_user(u: &str) -> bool { !u.is_empty() }\n",
            )
            .unwrap();
            std::fs::write(
                work.join("src/billing.rs"),
                "pub fn charge_card(cents: u64) -> bool { cents > 0 }\n",
            )
            .unwrap();
        },
    )
    .await;
    let spawn = subagent_tool(&harness, "subagent");
    let result = spawn
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "parallel module audit",
                "prompt": "Audit verify_user and charge_card modules for panics",
                "partitions": 2,
                "background": false,
            }),
        )
        .await
        .expect("partitioned foreground subagents");

    let output = result.output.expect("output");
    assert!(
        output.contains("<subagent-partition index=\"0\"")
            && output.contains("<subagent-partition index=\"1\""),
        "expected both partitions in combined output: {output}"
    );
    let meta = result.metadata.expect("metadata");
    assert_eq!(meta["status"], "completed");
    assert_eq!(meta["partitions"], 2);

    let children = harness
        .store
        .children(&harness.session)
        .await
        .expect("children");
    assert_eq!(children.len(), 2, "must spawn two partition workers");
    for child in &children {
        let row = harness
            .store
            .get_session(&child.id)
            .await
            .expect("session row")
            .expect("exists");
        assert_eq!(row["metadata"]["partition_count"], 2);
        assert!(row["metadata"]["partition_index"].is_number());
        let msgs = session_messages(&harness.store, &child.id).await;
        let user_text = msgs[0]["text"].as_str().unwrap_or_default();
        assert!(
            user_text.contains("<partition index="),
            "child must receive its <partition> slice: {user_text}"
        );
    }
}

#[tokio::test]
async fn subagent_partition_batch_enforces_child_limit_atomically() {
    let harness = Harness::new_with_setup(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {"max_children_per_session": 2}}),
        None,
        |work| {
            std::fs::create_dir_all(work.join("src")).unwrap();
            std::fs::write(work.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
            std::fs::write(work.join("src/b.rs"), "pub fn beta() {}\n").unwrap();
            std::fs::write(work.join("src/c.rs"), "pub fn gamma() {}\n").unwrap();
        },
    )
    .await;
    let spawn = subagent_tool(&harness, "subagent");
    let err = spawn
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "description": "too many partitions",
                "prompt": "Audit alpha beta gamma",
                "partitions": 3,
                "background": true,
            }),
        )
        .await
        .expect_err("batch exceeding max_children_per_session must fail atomically");
    assert!(
        err.message.contains("SpawnLimit")
            || err.message.contains("limit")
            || err.message.contains("maximum"),
        "unexpected error: {}",
        err.message
    );
    let children = harness
        .store
        .children(&harness.session)
        .await
        .expect("children");
    assert_eq!(children.len(), 0, "no partial children may be created");
}

#[tokio::test]
async fn roster_treats_completed_children_as_settled_and_avoids_stale_queue_spin() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let done_child = create_child(&harness, &harness.session, "ses_roster_done", "general").await;
    harness
        .store
        .mark_session_busy(&done_child, 1_000)
        .await
        .expect("mark child busy");
    harness
        .store
        .mark_session_idle(&done_child, 2_000, "succeeded")
        .await
        .expect("mark child completed");

    let roster = subagent_tool(&harness, "roster");
    let started = std::time::Instant::now();
    let result = roster
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"wait_seconds": 5}),
        )
        .await
        .expect("roster returns immediately for settled child");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "completed child must not stall roster(wait_seconds)"
    );
    let body: Value = serde_json::from_str(&result.output.expect("output")).expect("json");
    assert_eq!(body["wakeReason"], "no_active_children");
    assert_eq!(body["children"][0]["status"], "completed");
    assert_eq!(body["children"][0]["idleOutcome"], "succeeded");
    assert_eq!(body["counts"]["completed"], 1);
    assert_eq!(body["counts"]["busy"], 0);

    // Enqueue one `queue` item while a second child is busy: first `roster(wait_seconds=2)`
    // wakes once immediately with `callerInbox`; second `roster(wait_seconds=1)` does NOT
    // 0ms busy-spin on that same already-reported queue row.
    let busy_child = create_child(&harness, &harness.session, "ses_roster_busy", "general").await;
    harness
        .store
        .mark_session_busy(&busy_child, 3_000)
        .await
        .expect("mark child busy");
    harness
        .store
        .enqueue_materialized(&oc_store::NewInbox {
            id: "msg_stale_q".to_owned(),
            session_id: harness.session.clone(),
            kind: "synthetic".to_owned(),
            payload: json!({"text": "queued note", "metadata": {"source": "peer", "from": busy_child}}),
            delivery: oc_store::Delivery::Queue,
        })
        .await
        .expect("queue item");

    let first_start = std::time::Instant::now();
    let first = roster
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"wait_seconds": 2}),
        )
        .await
        .expect("first roster wakes on unseen queue");
    assert!(first_start.elapsed() < std::time::Duration::from_millis(800));
    let first_body: Value = serde_json::from_str(&first.output.expect("out")).expect("json");
    assert_eq!(first_body["wakeReason"], "inbox");
    assert_eq!(first_body["callerInbox"][0]["id"], "msg_stale_q");

    let second_start = std::time::Instant::now();
    let second = roster
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"wait_seconds": 1}),
        )
        .await
        .expect("second roster waits cleanly instead of 0ms spin");
    assert!(
        second_start.elapsed() >= std::time::Duration::from_millis(700),
        "already-reported queue row must not 0ms-spin roster: {:?}",
        second_start.elapsed()
    );
    let second_body: Value = serde_json::from_str(&second.output.expect("out")).expect("json");
    assert_eq!(second_body["wakeReason"], "timeout");
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn subagent_normalizes_combined_partition_parameters() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let model = crate::support::ScriptedModel::new(vec![
        crate::support::text_script("batch slice A"),
        crate::support::text_script("batch slice B"),
        crate::support::text_script("manual slice 0"),
        crate::support::text_script("manual slice 1"),
    ]);
    let runtime = oc_runtime::Runtime::new_with_model_client(harness.config.clone(), model);
    let tool = SubagentTools::new(runtime.clone()).registry();
    let subagent = tool.get("subagent").expect("subagent tool");
    let ctx = tool_context(&harness, &harness.session);

    // 1. Redundant matching `partitions` + `partition_count` runs batch partitioned spawn cleanly.
    let batch = subagent
        .execute(
            &ctx,
            json!({
                "description": "batch redundant count",
                "prompt": "inspect crates/oc-subagents and crates/oc-types",
                "partitions": 2,
                "partition_count": 2,
                "background": false,
            }),
        )
        .await
        .expect("matching partitions and partition_count succeeds");
    assert_eq!(batch.metadata.as_ref().expect("meta")["partitions"], 2);

    // 2. `partitions` + `partition_index` (without `partition_count`) spawns a single manual slice.
    let slice0 = subagent
        .execute(
            &ctx,
            json!({
                "description": "slice 0 via partitions",
                "prompt": "inspect crates/oc-subagents and crates/oc-types",
                "partitions": 2,
                "partition_index": 0,
                "background": false,
            }),
        )
        .await
        .expect("partitions + partition_index normalizes to manual slice");
    assert_eq!(
        slice0.metadata.as_ref().expect("meta")["status"],
        "completed"
    );

    // 3. Matching `partitions` + `partition_index` + `partition_count` spawns a single manual slice.
    let slice1 = subagent
        .execute(
            &ctx,
            json!({
                "description": "slice 1 combined",
                "prompt": "inspect crates/oc-subagents and crates/oc-types",
                "partitions": 2,
                "partition_index": 1,
                "partition_count": 2,
                "background": false,
            }),
        )
        .await
        .expect("matching partitions + partition_index + partition_count succeeds");
    assert_eq!(
        slice1.metadata.as_ref().expect("meta")["status"],
        "completed"
    );

    // 4. Conflicting numbers still fail explicitly.
    let conflict = subagent
        .execute(
            &ctx,
            json!({
                "description": "conflicting counts",
                "prompt": "work",
                "partitions": 3,
                "partition_count": 2,
                "background": false,
            }),
        )
        .await
        .expect_err("conflicting partition counts must fail");
    assert!(
        conflict.message.contains("does not match partition_count"),
        "{}",
        conflict.message
    );
}

struct HotStartModel {
    gate_rx: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    child_request_tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
    step: std::sync::atomic::AtomicUsize,
}

impl oc_runtime::ModelClient for HotStartModel {
    fn stream(
        &self,
        request: oc_llm::LlmRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<oc_llm::LlmStream, oc_llm::LlmError>> {
        use std::sync::atomic::Ordering;

        use futures_util::StreamExt;
        use futures_util::stream;
        use oc_llm::{LlmEvent, Usage};

        let call_index = self.step.fetch_add(1, Ordering::SeqCst);
        let session = request.options.cache_key.clone().unwrap_or_default();
        if call_index == 0 {
            let gate_rx = self.gate_rx.lock().expect("gate rx").take().expect("gate");
            let input = json!({
                "description": "hot start child",
                "prompt": "report hot status",
                "background": true,
            });
            let input_text = input.to_string();
            return Box::pin(async move {
                let head = stream::iter(vec![
                    LlmEvent::StepStart,
                    LlmEvent::ToolInputStart {
                        id: "call_hot_1".to_owned(),
                        name: "subagent".to_owned(),
                    },
                    LlmEvent::ToolInputDelta {
                        id: "call_hot_1".to_owned(),
                        delta: input_text.clone(),
                    },
                    LlmEvent::ToolInputEnd {
                        id: "call_hot_1".to_owned(),
                        text: input_text,
                    },
                    LlmEvent::ToolCall {
                        id: "call_hot_1".to_owned(),
                        name: "subagent".to_owned(),
                        input,
                    },
                ]);
                let tail = stream::once(async move {
                    let _ = gate_rx.await;
                    stream::iter(vec![
                        LlmEvent::TextStart,
                        LlmEvent::TextDelta {
                            delta: "delegated mid-stream".to_owned(),
                        },
                        LlmEvent::TextEnd {
                            text: "delegated mid-stream".to_owned(),
                        },
                        LlmEvent::Finish {
                            reason: "tool-calls".to_owned(),
                            usage: Usage::default(),
                        },
                    ])
                })
                .flatten();
                Ok(head.chain(tail).boxed())
            });
        }
        if let Some(tx) = self.child_request_tx.lock().expect("child tx").take() {
            let _ = tx.send(session);
        }
        let reply = if call_index == 1 {
            "child hot reply"
        } else {
            "parent final reply"
        };
        Box::pin(async move {
            Ok(stream::iter(vec![
                LlmEvent::StepStart,
                LlmEvent::TextStart,
                LlmEvent::TextDelta {
                    delta: reply.to_owned(),
                },
                LlmEvent::TextEnd {
                    text: reply.to_owned(),
                },
                LlmEvent::Finish {
                    reason: "stop".to_owned(),
                    usage: Usage::default(),
                },
            ])
            .boxed())
        })
    }
}

#[tokio::test]
async fn subagent_hot_starts_mid_stream_before_parent_finish() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use oc_runtime::Runtime;

    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel();
    let (child_request_tx, child_request_rx) = tokio::sync::oneshot::channel();
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let model = Arc::new(HotStartModel {
        gate_rx: Mutex::new(Some(gate_rx)),
        child_request_tx: Mutex::new(Some(child_request_tx)),
        step: AtomicUsize::new(0),
    });
    let runtime = Runtime::new_with_model_client(harness.config.clone(), model);
    harness
        .store
        .patch_session(
            &harness.session,
            oc_store::SessionPatch {
                title: Some("Hot start session".to_owned()),
                ..oc_store::SessionPatch::default()
            },
        )
        .await
        .expect("title");

    harness.enqueue("delegate work").await;
    runtime.notify(&harness.session).await.expect("notify");

    // Child MUST spawn and hit its own LLM request BEFORE gate_tx releases parent's Finish!
    let child_session = tokio::time::timeout(EVENT_WAIT, child_request_rx)
        .await
        .expect("child must hot-start before parent stream finishes")
        .expect("child session id");
    assert_ne!(child_session, harness.session);

    let _ = gate_tx.send(());
    runtime
        .wait_idle(&harness.session, IDLE_WAIT)
        .await
        .expect("parent idle");
    let children = child_sessions(&harness.store, &harness.session).await;
    assert_eq!(children.len(), 1);
    assert_eq!(children[0], child_session);
}

struct GatedPairModel {
    gate1: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    gate2: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    step: std::sync::atomic::AtomicUsize,
}

impl oc_runtime::ModelClient for GatedPairModel {
    fn stream(
        &self,
        _request: oc_llm::LlmRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<oc_llm::LlmStream, oc_llm::LlmError>> {
        use futures_util::StreamExt;
        use futures_util::stream;
        use oc_llm::{LlmEvent, Usage};
        use std::sync::atomic::Ordering;

        let idx = self.step.fetch_add(1, Ordering::SeqCst);
        let gate = if idx == 0 {
            self.gate1.lock().expect("gate1").take()
        } else {
            self.gate2.lock().expect("gate2").take()
        };
        Box::pin(async move {
            if let Some(rx) = gate {
                let _ = rx.await;
            }
            let reply = format!("done-{idx}");
            Ok(stream::iter(vec![
                LlmEvent::StepStart,
                LlmEvent::TextStart,
                LlmEvent::TextDelta {
                    delta: reply.clone(),
                },
                LlmEvent::TextEnd { text: reply },
                LlmEvent::Finish {
                    reason: "stop".to_owned(),
                    usage: Usage::default(),
                },
            ])
            .boxed())
        })
    }
}

#[tokio::test]
async fn roster_wait_all_suppresses_early_wakes_until_all_children_settle() {
    use oc_runtime::Runtime;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    let (gate1_tx, gate1_rx) = tokio::sync::oneshot::channel();
    let (gate2_tx, gate2_rx) = tokio::sync::oneshot::channel();
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": true}}),
        None,
    )
    .await;
    let model = Arc::new(GatedPairModel {
        gate1: Mutex::new(Some(gate1_rx)),
        gate2: Mutex::new(Some(gate2_rx)),
        step: AtomicUsize::new(0),
    });
    let runtime = Runtime::new_with_model_client(harness.config.clone(), model);
    let registry = SubagentTools::new(runtime).registry();
    let spawn = registry.get("subagent").expect("subagent");
    let roster = registry.get("roster").cloned().expect("roster");

    let mut ids = Vec::new();
    for i in 1..=2 {
        let out = spawn
            .execute(
                &tool_context(&harness, &harness.session),
                json!({
                    "description": format!("worker {i}"),
                    "prompt": "work",
                    "background": true,
                }),
            )
            .await
            .expect("spawn bg");
        ids.push(
            out.metadata.expect("meta")["sessionID"]
                .as_str()
                .expect("id")
                .to_owned(),
        );
    }

    let ctx = tool_context(&harness, &harness.session);
    let wait_task = tokio::spawn(async move {
        roster
            .execute(
                &ctx,
                json!({ "wait_seconds": 10, "wait_all": true, "session_ids": ids }),
            )
            .await
    });

    // Finish child 1, ensure wait_all stays waiting while child 2 is still busy.
    let _ = gate1_tx.send(());
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        !wait_task.is_finished(),
        "wait_all must not wake early when only the first child settles"
    );

    // Finish child 2: wait_all returns with wakeReason = all_settled and both settledIDs.
    let _ = gate2_tx.send(());
    let result = tokio::time::timeout(IDLE_WAIT, wait_task)
        .await
        .expect("roster finished")
        .expect("join")
        .expect("roster ok");
    let meta = result.metadata.expect("meta");
    assert_eq!(meta["wakeReason"], "all_settled");
    assert_eq!(meta["settledIDs"].as_array().expect("settled").len(), 2);
}
