//! `session.request.envelope` and the durable-ordering defects it sits beside.
//!
//! Three invariants are asserted here, each as a property of observable state
//! rather than of the code that produces it:
//!
//! 1. the envelope is **log-only** — it lands in the durable event log and adds
//!    no `session_message` row;
//! 2. the envelope is **sufficient** — a consumer that recomputes the base
//!    messages from the rows reproduces `baseMessages`/`baseDigest` exactly, and
//!    the recorded `injections` are the exact texts the provider received;
//! 3. `session.step.streamed` is published **after** the `time.streamed` stamp it
//!    reports is committed.

use std::sync::Arc;

use oc_llm::{Catalog, ContentPart, LlmMessage, Role};
use oc_runtime::prompt::messages_from_store;
use serde_json::{Value, json};

use crate::support::*;

/// The canonical per-message rendering documented on
/// `oc_types::payload::SessionRequestEnvelope`.
///
/// This is an independent implementation of the published table: a consumer of
/// the event log must be able to reproduce `baseDigest` from the durable rows
/// alone, so this test deliberately does not call the runtime's renderer.
fn consumer_render(message: &LlmMessage) -> String {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let parts: Vec<String> = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.clone(),
            ContentPart::Image { media_type, data } => {
                format!("[image {media_type} {} bytes]", data.len())
            }
            ContentPart::Document {
                media_type,
                data,
                filename,
            } => format!(
                "[document {media_type} {} bytes {}]",
                data.len(),
                filename.as_deref().unwrap_or("-")
            ),
            ContentPart::ToolCall { id, name, input } => format!(
                "[tool_call {id} {name} {}]",
                serde_json::to_string(input).unwrap_or_default()
            ),
            ContentPart::ToolResult {
                tool_call_id,
                name,
                content,
                is_error,
            } => format!(
                "[tool_result {tool_call_id} {} {content} error={is_error}]",
                name.as_deref().unwrap_or("-")
            ),
        })
        .collect();
    oc_types::payload::canonical_message(role, &parts)
}

/// Every `session.request.envelope` the session has in its durable log.
async fn envelopes(harness: &Harness) -> Vec<Value> {
    harness
        .store
        .events_after(&harness.session, 0, 1_000)
        .await
        .expect("event log")
        .into_iter()
        .filter(|event| event.kind == "session.request.envelope")
        .map(|event| event.data)
        .collect()
}

/// ITEM 1, invariant 1: the envelope is durable and log-only.
#[tokio::test]
async fn request_envelope_is_durable_and_writes_no_message_row() {
    let harness = Harness::new(
        vec![text_script("answered")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("hi").await;

    let messages = harness.messages().await;
    let kinds: Vec<Option<&str>> = messages
        .iter()
        .map(|message| message.get("type").and_then(Value::as_str))
        .collect();
    assert_eq!(
        kinds,
        vec![Some("user"), Some("assistant"), Some("idle")],
        "one promoted user row, one assistant row and the pre-existing session-status \
         `idle` row: {messages:?}"
    );
    assert!(
        !messages.iter().any(|message| message["type"] == "request"),
        "the envelope must not materialise as a message row"
    );
    assert!(
        messages
            .iter()
            .filter(|message| message["type"] != "idle")
            .count()
            == 2,
        "the envelope added no transcript row beyond the user/assistant pair: {messages:?}"
    );

    let logged = envelopes(&harness).await;
    assert_eq!(
        logged.len(),
        1,
        "one envelope per assistant row: {logged:?}"
    );
    let envelope = &logged[0];
    assert_eq!(envelope["sessionID"], harness.session);
    assert_eq!(envelope["provider"], "test");
    assert_eq!(envelope["model"], "alpha");
    assert_eq!(envelope["step"], 0);
    assert!(
        envelope["assistantMessageID"]
            .as_str()
            .is_some_and(|id| id.starts_with("msg_")),
        "prefixed message id: {envelope}"
    );
    assert!(
        envelope["created"].as_i64().is_some(),
        "epoch-ms creation stamp: {envelope}"
    );
    assert!(
        !envelope.to_string().contains("null"),
        "optional fields are omitted, never null: {envelope}"
    );
}

/// ITEM 1, invariant 2: the base half of the request is recomputable from the
/// rows, and the injection half is replayable byte-for-byte.
#[tokio::test]
async fn request_envelope_reconstructs_the_request_from_the_log() {
    // A cap of 2 with a tool call on every tool-enabled step means the third
    // request is the tool-less final call, so it carries the `max-steps`
    // injection as well as the base rows.
    let harness = Harness::new(
        vec![
            tool_script(
                "call_0",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("done"),
        ],
        Arc::new(AllowAll),
        json!({"agents": {"engineer": {"steps": 2}}}),
        None,
    )
    .await;
    harness.prompt("read it").await;

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        3,
        "two tool-enabled steps plus one tool-less final call"
    );
    let final_call = &requests[2];
    assert_eq!(
        final_call.options.tool_choice.as_deref(),
        Some("none"),
        "the last request of a 2-step budget is the tool-less final call"
    );

    let logged = envelopes(&harness).await;
    assert_eq!(
        logged.len(),
        3,
        "one envelope per assistant row: {logged:?}"
    );
    let envelope = logged.last().expect("final-call envelope");

    // A replaying consumer derives the base messages from the rows that precede
    // the envelope's own assistant row: the row the envelope names did not
    // exist when the request was built.
    let assistant_id = envelope["assistantMessageID"]
        .as_str()
        .expect("assistant id");
    let all = harness.messages().await;
    let own = all
        .iter()
        .position(|message| message["id"] == assistant_id)
        .expect("the envelope names a real assistant row");
    let base = messages_from_store(&all[..own]);

    assert_eq!(
        envelope["baseMessages"].as_u64(),
        Some(u64::try_from(base.len()).expect("fits")),
        "the recorded base count is the row-derived count before the assistant row"
    );
    let rendered: Vec<String> = base.iter().map(consumer_render).collect();
    let recomputed = oc_types::payload::digest_sequence(rendered.iter().map(String::as_str));
    assert_eq!(
        envelope["baseDigest"], recomputed,
        "a consumer reproduces baseDigest from the durable rows alone"
    );

    // Replay: base messages + injections must be exactly the request sent.
    let mut replayed = base.clone();
    for injection in envelope["injections"]
        .as_array()
        .expect("the final call always injects its max-steps prompt")
    {
        let text = injection["text"].as_str().expect("injection text");
        assert_eq!(
            injection["kind"], "max-steps",
            "the final call is the only splice at step 2: {injection}"
        );
        replayed.push(LlmMessage::user(text.to_owned()));
    }
    assert_eq!(
        replayed, final_call.messages,
        "replaying the envelope reproduces the request the provider received"
    );
    assert_eq!(
        final_call.system.join("\n"),
        requests[0].system.join("\n"),
        "the system prefix is stable across the turn"
    );

    // The earlier, tool-enabled steps recorded no injections at all.
    let first = &logged[0];
    assert_eq!(
        first["injections"],
        Value::Null,
        "an omitted injections list is absent, not empty-or-null: {first}"
    );
}

/// ITEM 3: the transport-hook probe must see the request that will be
/// dispatched, not an empty one.
///
/// The fake hook claims ownership only when the request carries messages, which
/// `LlmRequest::new(model)` never does. With the old empty probe it declined,
/// the credential pool was consulted, and the turn rotated through both seeded
/// keys; with the faithful probe it claims ownership and exactly one attempt is
/// made with no credential attached.
#[tokio::test]
async fn hook_probe_sees_the_real_request_and_owns_the_credential_pool() {
    use oc_llm::{Catalog, HookStreamFuture, LlmError, LlmRequest, LlmStream, LlmTransportHook};
    use oc_store::NewCredential;

    #[derive(Debug)]
    struct MessagesHook;

    impl LlmTransportHook for MessagesHook {
        fn handles(&self, request: &LlmRequest) -> bool {
            // Depends on a field the empty request never has.
            !request.messages.is_empty() && request.options.cache_key.is_some()
        }

        fn stream(&self, _request: LlmRequest) -> HookStreamFuture {
            Box::pin(async {
                Err::<LlmStream, LlmError>(LlmError::Provider {
                    message: "the hook must not be reached: the scripted model serves the turn"
                        .to_owned(),
                })
            })
        }
    }

    let harness = Harness::new(
        vec![
            Script::Failure {
                kind: oc_llm::FailureKind::Quota,
                message: "quota exhausted for this key".to_owned(),
            },
            text_script("served"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
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
    harness.config.client.set_hook(Some(Arc::new(MessagesHook)));

    harness.prompt("hi").await;

    assert_eq!(
        harness.model.calls(),
        1,
        "the hook owns the pool, so no key rotation happens: {:?}",
        harness.sink.kinds()
    );
    let requests = harness.model.requests();
    assert_eq!(
        requests[0].options.api_key, None,
        "a hook-owned request carries no pooled credential"
    );
    assert!(
        !requests[0].messages.is_empty(),
        "dispatch and the probe agree that the request has messages"
    );
    assert_eq!(
        requests[0].options.cache_key.as_deref(),
        Some(harness.session.as_str()),
        "the probe saw the same session affinity key dispatch used"
    );

    // Keep the unused import honest: the probe is a `Catalog`-free trait method.
    let _: Option<fn(&Catalog, &str) -> Option<oc_llm::ResolvedModel>> = None;
}

/// ITEM 2: `session.step.streamed` is durable, so the `time.streamed` stamp it
/// reports must already be committed when the event is published.
///
/// The sink runs inside `publish`, i.e. at the exact moment the event becomes
/// visible, and re-reads the assistant row from the database file. The
/// multi-threaded flavor keeps `block_in_place` legal and lets the store's
/// blocking reader run on the blocking pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // one scenario: build store, arm sink, run turn, assert
async fn step_streamed_is_published_only_after_the_stamp_is_committed() {
    use std::sync::Mutex;

    use oc_runtime::{EventSink, ModelClient};
    use oc_store::Store;
    use oc_types::EventEnvelope;

    #[derive(Clone)]
    struct StampCheckingSink {
        store: Arc<Store>,
        assistant_id: Arc<Mutex<Option<String>>>,
        observed: Arc<Mutex<Vec<Option<i64>>>>,
    }

    impl EventSink for StampCheckingSink {
        fn publish(&self, event: EventEnvelope) {
            if event.kind == "session.step.started" {
                if let Some(id) = event.data["assistantMessageID"].as_str() {
                    *self.assistant_id.lock().expect("id") = Some(id.to_owned());
                }
                return;
            }
            if event.kind != "session.step.streamed" {
                return;
            }
            let Some(id) = self.assistant_id.lock().expect("id").clone() else {
                return;
            };
            let store = Arc::clone(&self.store);
            let session = event.data["sessionID"]
                .as_str()
                .expect("session id")
                .to_owned();
            // Reading the row *here* is the whole point: this is the instant the
            // durable event exists, so anything it reports must be on disk.
            let stamp = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(async move { store.get_message(&session, &id).await })
            })
            .expect("row read")
            .and_then(|row| row.pointer("/time/streamed").and_then(Value::as_i64));
            self.observed.lock().expect("observed").push(stamp);
        }
    }

    let root = tempfile::tempdir().expect("temp dir");
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).expect("work dir");
    std::fs::write(work.join("hello.txt"), "hello world\n").expect("file");

    let config = oc_config::ConfigService::load(
        oc_config::LoadOptions::new(&work)
            .with_env("HOME", path(root.path()))
            .with_env("USERPROFILE", path(root.path()))
            .with_env("XDG_CONFIG_HOME", path(&root.path().join("config")))
            .with_env("ProgramData", path(&root.path().join("programdata")))
            .with_env("OPENCODE_CONFIG", path(&root.path().join("missing.json")))
            .with_env("OPENCODE_CONFIG_CONTENT", json!({}).to_string()),
    )
    .expect("config loads");
    let store = Arc::new(
        Store::open(root.path().join("db.sqlite"))
            .await
            .expect("store"),
    );
    let session = "ses_streamed_probe".to_owned();
    store
        .create_session(oc_store::NewSession {
            id: session.clone(),
            parent_id: None,
            project_id: "prj_streamed_probe".to_owned(),
            directory: path(&work),
            title: None,
            agent: Some("engineer".to_owned()),
            model: Some(json!({"providerID": "test", "id": "alpha"})),
            metadata: None,
            permissions: None,
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("session created");

    let checking = StampCheckingSink {
        store: Arc::clone(&store),
        assistant_id: Arc::new(Mutex::new(None)),
        observed: Arc::new(Mutex::new(Vec::new())),
    };
    let model = ScriptedModel::new(vec![text_script("output")]);
    let runtime_config = oc_runtime::RuntimeConfig {
        store: Arc::clone(&store),
        config: Arc::new(config),
        catalog: Arc::new(oc_llm::CatalogCell::from_arc(Arc::new(
            Catalog::from_snapshot(CATALOG).expect("catalog"),
        ))),
        client: Arc::new(oc_llm::SharedLlmClient::new(
            oc_llm::LlmClient::new().expect("client"),
        )),
        tools: Arc::new(oc_tools::ToolRegistry::with_defaults()),
        sink: Arc::new(checking.clone()),
        permissions: Arc::new(AllowAll),
    };
    let runtime = oc_runtime::Runtime::new_with_model_client(
        runtime_config,
        Arc::clone(&model) as Arc<dyn ModelClient>,
    );

    store
        .enqueue(oc_store::NewInbox {
            id: oc_types::MessageId::new().into_inner(),
            session_id: session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "hi", "time": {"created": oc_types::now_millis()}}),
            delivery: oc_store::Delivery::Steer,
        })
        .await
        .expect("enqueued");
    runtime.notify(&session).await.expect("notify");
    runtime
        .wait_idle(&session, std::time::Duration::from_secs(10))
        .await
        .expect("idle");

    let observed = checking.observed.lock().expect("observed").clone();
    assert_eq!(
        observed.len(),
        1,
        "the step produced output, so exactly one session.step.streamed is published"
    );
    assert!(
        observed[0].is_some(),
        "at the moment session.step.streamed was published the assistant row must already \
         carry time.streamed; saw {observed:?}"
    );
    assert!(model.calls() >= 1, "the scripted model served the request");
}
