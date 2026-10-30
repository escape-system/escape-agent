//! Tool providers, goals, talk events and snapshot-triggering writes.

use std::sync::Arc;

use oc_runtime::{ObserverBinding, RepairChannel, ToolProvider};
use oc_store::NewGoal;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

use crate::support::*;

struct ProbeTool;

#[async_trait::async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &'static str {
        "provider_probe"
    }

    fn description(&self) -> &'static str {
        "probe tool supplied by a host ToolProvider"
    }

    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _ctx: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("probe ok"))
    }
}

struct ProbeProvider;

impl ToolProvider for ProbeProvider {
    fn tools(&self, _session: &oc_types::SessionInfo, _agent: &str) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(ProbeTool)]
    }
}

#[tokio::test]
async fn tool_provider_tools_merge_into_the_registry() {
    let harness = Harness::new(
        vec![text_script("done")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.runtime.add_tool_provider(Arc::new(ProbeProvider));
    harness.prompt("hi").await;

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    let probe = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "provider_probe")
        .expect("provider tool reaches the model request");
    let required = probe.input_schema["required"].as_array().expect("schema");
    assert!(required.iter().any(|value| value == "expectation"));
    assert!(
        requests[0].tools.iter().any(|tool| tool.name == "read"),
        "static tools survive the provider merge"
    );
}

#[tokio::test]
async fn goal_check_without_a_goal_still_calls_the_observer() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let notifier = Arc::new(RecordingNotifier::default());
    let (executor, repairs) = RepairChannel::new();
    let model = FixedObserverModel::text(&json!({"action": "none"}));
    let engine = observer_engine(Arc::clone(&model), notifier, executor);
    let runtime = harness.restart().with_observer(ObserverBinding {
        engine,
        goals: goals(&harness),
        repairs,
    });
    runtime.start().await.expect("start");
    assert!(
        harness
            .store
            .get_goal(&harness.session)
            .await
            .expect("goal lookup")
            .is_none(),
        "the root has no goal"
    );

    runtime
        .request_goal_check(&harness.session)
        .await
        .expect("check runs without a goal");
    assert_eq!(
        model.calls(),
        1,
        "check_now is invoked even when the root has no goal"
    );
}

#[tokio::test]
async fn goal_check_without_an_observer_is_a_config_error() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    assert!(harness.runtime.observer().is_none());
    assert!(harness.runtime.lanes().is_none());
    let error = harness
        .runtime
        .request_goal_check(&harness.session)
        .await
        .expect_err("no observer is configured");
    assert!(matches!(error, oc_runtime::RuntimeError::Config(_)));
}

#[tokio::test]
async fn goal_block_is_injected_into_the_instruction_prefix() {
    let harness = Harness::new(vec![text_script("ok")], Arc::new(AllowAll), json!({}), None).await;
    harness
        .store
        .set_goal(NewGoal {
            id: "gol_prompt".to_owned(),
            root_id: harness.session.clone(),
            objective: "ship the goal prompt".to_owned(),
            criteria: vec!["render the block".to_owned()],
            model: None,
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("goal row");
    harness.prompt("hi").await;

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    let instructions = requests[0].system.last().expect("instruction part");
    assert!(instructions.contains("<goal id=\"gol_prompt\""));
    assert!(instructions.contains("ship the goal prompt"));
    assert!(instructions.contains("render the block"));
}

#[tokio::test]
async fn talk_publishes_the_peer_message_event_after_the_inbox_row() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {"siblings": {"enabled": true}}}),
        None,
    )
    .await;
    let a = create_child(&harness, &harness.session, "ses_peer_a", "general").await;
    let b = create_child(&harness, &harness.session, "ses_peer_b", "general").await;
    let tool = subagent_tool(&harness, "talk");
    tool.execute(
        &tool_context(&harness, &a),
        json!({
            "to": b,
            "kind": "question",
            "text": "status?",
            "request_reply": true,
        }),
    )
    .await
    .expect("siblings may talk");

    let events = harness.sink.data("subagent.peer.message");
    assert_eq!(events.len(), 1, "talk publishes exactly one event");
    let data = &events[0];
    assert_eq!(data["sessionID"], b.as_str());
    assert_eq!(data["from"], a.as_str());
    assert_eq!(data["agent"], "general");
    assert_eq!(data["kind"], "question");
    assert_eq!(data["text"], "status?");
    assert_eq!(data["requestReply"], true);

    let inbox = harness.store.list_inbox(&b).await.expect("inbox");
    assert_eq!(inbox.len(), 1);
    assert_eq!(data["messageID"], inbox[0]["id"]);

    let envelope = harness
        .sink
        .events
        .lock()
        .expect("events")
        .iter()
        .find(|event| event.kind == "subagent.peer.message")
        .cloned()
        .expect("peer message envelope");
    assert!(envelope.durable.is_none(), "peer messages are ephemeral");
    let directory = path(&harness.work);
    assert_eq!(
        envelope
            .location
            .as_ref()
            .map(|location| location.directory.as_str()),
        Some(directory.as_str()),
        "location is the target session directory"
    );
}

#[tokio::test]
async fn patch_authorizes_every_target_and_a_denied_one_fails_the_call() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "patch",
                "call_patch",
                json!({
                    "patchText": "*** Begin Patch\n*** Add File: allowed.txt\n+allowed\n*** Add File: blocked.txt\n+blocked\n*** End Patch",
                    "expectation": expectation(),
                }),
            ),
            text_script("patch was denied"),
        ],
        Arc::new(AllowAll),
        json!({"permission": {"edit": {"*blocked.txt": "deny"}}}),
        None,
    )
    .await;
    harness.prompt("apply the patch").await;

    let failures = harness.sink.data("session.tool.failed");
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0]["error"]["type"], "PermissionDeniedError");
    assert_eq!(failures[0]["executed"], false);
    assert!(
        failures[0]["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("blocked.txt")
    );
    assert!(
        !harness.work.join("allowed.txt").exists(),
        "authorization runs before any hunk is applied"
    );
    assert!(!harness.work.join("blocked.txt").exists());
}

#[tokio::test]
async fn write_snapshots_the_existing_file_before_overwriting_it() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "write",
                "call_snap_write",
                json!({
                    "path": "hello.txt",
                    "content": "replaced\n",
                    "expectation": expectation(),
                }),
            ),
            text_script("wrote it"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("replace the greeting").await;

    assert_eq!(
        std::fs::read_to_string(harness.work.join("hello.txt")).expect("written"),
        "replaced\n"
    );
    let rows = harness
        .store
        .snapshots_after(&harness.session, -1)
        .await
        .expect("snapshots");
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.path, path(&harness.work.join("hello.txt")));
    assert!(row.existed);
    assert!(!row.skipped);
    assert_eq!(row.content.as_deref(), Some(b"hello world\n".as_slice()));
    assert_eq!(row.tool_call_id, "call_snap_write");

    let messages = harness.messages().await;
    let assistant = assistants(&messages)[0];
    let id = assistant["id"].as_str().expect("assistant id");
    assert_eq!(row.message_id, id);
    let seq = harness
        .store
        .message_seq(&harness.session, id)
        .await
        .expect("seq")
        .expect("message exists");
    assert_eq!(row.seq, i64::try_from(seq).expect("fits"));
}

#[tokio::test]
async fn write_snapshots_a_missing_file_without_content() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "write",
                "call_snap_new",
                json!({
                    "path": "fresh.txt",
                    "content": "brand new\n",
                    "expectation": expectation(),
                }),
            ),
            text_script("created it"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("create a file").await;

    assert!(harness.work.join("fresh.txt").exists());
    let rows = harness
        .store
        .snapshots_after(&harness.session, -1)
        .await
        .expect("snapshots");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].path, path(&harness.work.join("fresh.txt")));
    assert!(!rows[0].existed);
    assert!(!rows[0].skipped);
    assert!(rows[0].content.is_none());
}

#[tokio::test]
async fn patch_snapshots_every_target_before_applying() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "patch",
                "call_snap_patch",
                json!({
                    "patchText": "*** Begin Patch\n*** Add File: added.txt\n+new\n*** Update File: hello.txt\n@@\n-hello world\n+goodbye world\n*** End Patch",
                    "expectation": expectation(),
                }),
            ),
            text_script("patched"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("apply the patch").await;

    assert!(
        !harness.sink.has("session.tool.failed"),
        "{:?}",
        harness.sink.data("session.tool.failed")
    );
    let rows = harness
        .store
        .snapshots_after(&harness.session, -1)
        .await
        .expect("snapshots");
    assert_eq!(rows.len(), 2, "{rows:?}");
    let added = rows
        .iter()
        .find(|row| row.path == path(&harness.work.join("added.txt")))
        .expect("added row");
    let updated = rows
        .iter()
        .find(|row| row.path == path(&harness.work.join("hello.txt")))
        .expect("updated row");
    assert!(!added.existed);
    assert!(added.content.is_none());
    assert!(updated.existed);
    assert_eq!(
        updated.content.as_deref(),
        Some(b"hello world\n".as_slice())
    );
    assert!(rows.iter().all(|row| row.tool_call_id == "call_snap_patch"));
}

#[tokio::test]
async fn writes_outside_the_session_directory_are_not_snapshotted() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "write",
                "call_snap_outside",
                json!({
                    "path": "../outside.txt",
                    "content": "external\n",
                    "expectation": expectation(),
                }),
            ),
            text_script("wrote outside"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("write outside").await;

    let root = harness.work.parent().expect("temp root");
    assert!(root.join("outside.txt").exists());
    let rows = harness
        .store
        .snapshots_after(&harness.session, -1)
        .await
        .expect("snapshots");
    assert!(rows.is_empty(), "{rows:?}");
}
