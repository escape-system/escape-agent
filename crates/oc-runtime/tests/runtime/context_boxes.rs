//! Context boxes: tools, permissions, prompt index and compaction survival
//! (`docs/25-context-boxes.md`).
//!
//! Every test is offline: temp SQLite, temp config tree, scripted model.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_runtime::BoxTools;
use oc_store::{BoxQuery, Delivery, NewBox, NewInbox};
use oc_tools::Tool;
use serde_json::{Value, json};

use crate::support::*;

fn box_tool(harness: &Harness, name: &str) -> Arc<dyn Tool> {
    BoxTools::new(harness.runtime.clone())
        .registry()
        .get(name)
        .cloned()
        .unwrap_or_else(|| panic!("{name} is registered"))
}

async fn run_tool(
    harness: &Harness,
    session: &str,
    name: &str,
    input: Value,
) -> Result<String, oc_tools::ToolError> {
    let result = box_tool(harness, name)
        .execute(&tool_context(harness, session), input)
        .await?;
    Ok(result.output.unwrap_or_default())
}

/// Waits until the session has no inbox rows and every compaction row is
/// settled. Returns the compaction rows.
async fn wait_compaction(harness: &Harness) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rows: Vec<Value> = harness
            .messages()
            .await
            .into_iter()
            .filter(|message| message.get("type").and_then(Value::as_str) == Some("compaction"))
            .collect();
        let settled = rows.iter().all(|row| {
            matches!(
                row.get("status").and_then(Value::as_str),
                Some("completed" | "failed")
            )
        });
        if settled && harness.inbox().await.is_empty() {
            return rows;
        }
        assert!(Instant::now() < deadline, "timed out: {rows:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn enqueue_compaction(harness: &Harness) {
    harness
        .store
        .enqueue(NewInbox {
            id: oc_types::MessageId::new().into_inner(),
            session_id: harness.session.clone(),
            kind: "compaction".to_owned(),
            payload: json!({
                "time": {"created": oc_types::now_millis()},
                "status": "pending",
                "reason": "manual"
            }),
            delivery: Delivery::Queue,
        })
        .await
        .expect("compaction enqueued");
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
}

#[tokio::test]
async fn box_tools_are_shared_across_the_session_tree() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let child = create_child(&harness, &harness.session, "ses_box_child", "general").await;

    // The child writes; the root reads the same row.
    run_tool(
        &harness,
        &child,
        "box_write",
        json!({
            "key": "findings",
            "content": "chart-limits.js caps pages",
            "kind": "stable",
            "description": "exploration findings"
        }),
    )
    .await
    .expect("child writes a box");

    let listed = run_tool(&harness, &harness.session, "box_list", json!({}))
        .await
        .expect("root lists");
    assert!(listed.contains("findings"), "{listed}");
    assert!(listed.contains("exploration findings"), "{listed}");

    let read = run_tool(
        &harness,
        &harness.session,
        "box_read",
        json!({"key": "findings"}),
    )
    .await
    .expect("root reads");
    assert!(read.contains("chart-limits.js caps pages"), "{read}");

    // Write is create-or-update: an append from the root keeps one row.
    run_tool(
        &harness,
        &harness.session,
        "box_write",
        json!({"key": "findings", "content": "second line", "append": true}),
    )
    .await
    .expect("root appends");
    let page = harness
        .store
        .list_boxes(BoxQuery {
            root_id: harness.session.clone(),
            ..BoxQuery::default()
        })
        .await
        .expect("rows");
    assert_eq!(page.items.len(), 1);
    assert_eq!(
        page.items[0]["content"],
        "chart-limits.js caps pages\nsecond line"
    );
    assert_eq!(page.items[0]["revision"], 2);

    // Delete hides the box from reads and the index.
    run_tool(
        &harness,
        &harness.session,
        "box_delete",
        json!({"key": "findings"}),
    )
    .await
    .expect("delete");
    assert!(
        run_tool(
            &harness,
            &harness.session,
            "box_read",
            json!({"key": "findings"})
        )
        .await
        .is_err(),
        "a deleted box cannot be read"
    );
    let after = run_tool(&harness, &harness.session, "box_list", json!({}))
        .await
        .expect("list after delete");
    assert!(!after.contains("findings"), "{after}");
}

#[tokio::test]
async fn explore_is_read_only_and_a_write_only_agent_cannot_list_boxes() {
    // The explore agent has a deny-all base and gets `box:read` explicitly.
    let harness = Harness::new(
        vec![text_script("done")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let explore = "ses_box_explore".to_owned();
    create_child(&harness, &harness.session, &explore, "explore").await;
    harness
        .store
        .enqueue(NewInbox {
            id: oc_types::MessageId::new().into_inner(),
            session_id: explore.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "hi", "time": {"created": oc_types::now_millis()}}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    harness.runtime.notify(&explore).await.expect("notify");
    harness
        .runtime
        .wait_idle(&explore, Duration::from_secs(10))
        .await
        .expect("idle");

    let requests = harness.model.requests();
    let names: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(
        names.contains(&"box_list") && names.contains(&"box_read"),
        "explore reads boxes: {names:?}"
    );
    assert!(
        !names.contains(&"box_write") && !names.contains(&"box_delete"),
        "explore must not write boxes: {names:?}"
    );

    // A custom agent that allows `box:write` and denies `box:read` is
    // write-only: the read tools are filtered out of its request.
    let harness = Harness::new(
        vec![text_script("done")],
        Arc::new(AllowAll),
        json!({
            "agents": {
                "writer": {
                    "permissions": [
                        {"action": "box:read", "resource": "*", "effect": "deny"}
                    ]
                }
            }
        }),
        None,
    )
    .await;
    let writer = "ses_box_writer".to_owned();
    create_child(&harness, &harness.session, &writer, "writer").await;
    harness
        .store
        .enqueue(NewInbox {
            id: oc_types::MessageId::new().into_inner(),
            session_id: writer.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "hi", "time": {"created": oc_types::now_millis()}}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    harness.runtime.notify(&writer).await.expect("notify");
    harness
        .runtime
        .wait_idle(&writer, Duration::from_secs(10))
        .await
        .expect("idle");

    let requests = harness.model.requests();
    let names: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(
        names.contains(&"box_write") && names.contains(&"box_delete"),
        "the writer keeps box:write: {names:?}"
    );
    assert!(
        !names.contains(&"box_list") && !names.contains(&"box_read"),
        "the writer is denied box:read: {names:?}"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One scenario: per-key visibility on a small tree, then on a truncated page.
async fn box_list_hides_keys_the_caller_cannot_read() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({
            "agents": {
                "secretive": {
                    "permissions": [
                        {"action": "box:read", "resource": "secret*", "effect": "deny"}
                    ]
                }
            }
        }),
        None,
    )
    .await;
    let secretive =
        create_child(&harness, &harness.session, "ses_box_secretive", "secretive").await;
    for (key, content) in [
        ("decisions", "keep the wire shape"),
        ("secret-key", "do not leak"),
        ("scratch", "working note"),
    ] {
        harness
            .store
            .write_box(
                NewBox {
                    root_id: harness.session.clone(),
                    owner_session: harness.session.clone(),
                    key: key.to_owned(),
                    title: None,
                    description: Some(format!("{key} description")),
                    kind: "stable".to_owned(),
                    tags: Vec::new(),
                    content: content.to_owned(),
                },
                false,
            )
            .await
            .expect("box written");
    }

    // A caller allowed `box:read: *` sees every key.
    let root_list = run_tool(&harness, &harness.session, "box_list", json!({}))
        .await
        .expect("root lists");
    assert!(root_list.contains("secret-key"), "{root_list}");
    assert!(root_list.contains("3 box(es)"), "{root_list}");

    // The restricted agent sees the two allowed keys and neither the denied
    // key nor a count that includes it.
    let restricted = run_tool(&harness, &secretive, "box_list", json!({}))
        .await
        .expect("child lists");
    assert!(restricted.contains("decisions"), "{restricted}");
    assert!(restricted.contains("scratch"), "{restricted}");
    assert!(
        !restricted.contains("secret"),
        "a denied key is invisible in the listing: {restricted}"
    );
    assert!(restricted.contains("2 box(es)"), "{restricted}");

    // Metadata mirrors the text: no summary row for the denied key either.
    let result = box_tool(&harness, "box_list")
        .execute(&tool_context(&harness, &secretive), json!({}))
        .await
        .expect("list with metadata");
    let boxes = result
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("boxes"))
        .and_then(Value::as_array)
        .expect("boxes metadata");
    assert_eq!(boxes.len(), 2, "{boxes:?}");
    assert!(
        boxes.iter().all(|row| row["key"] != "secret-key"),
        "{boxes:?}"
    );

    // A filtered page must still report the truthful visible count and the
    // truncation notice. 100 `z…` keys plus one denied `secret-key` fill the
    // 100-row page; the denied row sorts first and is dropped, so the child
    // sees 99 while the store still has more rows than are listed.
    for index in 0..100 {
        harness
            .store
            .write_box(
                NewBox {
                    root_id: harness.session.clone(),
                    owner_session: harness.session.clone(),
                    key: format!("z{index:03}"),
                    title: None,
                    description: None,
                    kind: "stable".to_owned(),
                    tags: Vec::new(),
                    content: format!("filler {index}"),
                },
                false,
            )
            .await
            .expect("filler box");
    }
    let truncated = run_tool(&harness, &secretive, "box_list", json!({}))
        .await
        .expect("truncated list");
    assert!(
        truncated.contains("99 box(es)"),
        "the count only includes visible rows: {truncated}"
    );
    assert!(
        truncated.contains("more boxes exist than are listed"),
        "{truncated}"
    );

    let result = box_tool(&harness, "box_list")
        .execute(&tool_context(&harness, &secretive), json!({}))
        .await
        .expect("truncated list with metadata");
    assert_eq!(
        result
            .metadata
            .as_ref()
            .and_then(|meta| meta["hasMore"].as_bool()),
        Some(true)
    );
}

#[tokio::test]
async fn compact_context_enqueues_a_queued_row_instead_of_compacting_inline() {
    let harness = Harness::new(
        vec![text_script("## Objective\nscheduled handoff")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let message = run_tool(
        &harness,
        &harness.session,
        "compact_context",
        json!({"note": "transcript is long"}),
    )
    .await
    .expect("scheduled");
    assert!(message.contains("queued"), "{message}");

    let inbox = harness.inbox().await;
    assert_eq!(inbox.len(), 1, "one queued row: {inbox:?}");
    assert_eq!(inbox[0]["type"], "compaction");
    assert_eq!(inbox[0]["delivery"], "queue");
    assert_eq!(inbox[0]["payload"]["reason"], "manual");
    assert_eq!(inbox[0]["payload"]["note"], "transcript is long");
    assert!(
        harness
            .messages()
            .await
            .iter()
            .all(|row| row.get("type").and_then(Value::as_str) != Some("compaction")),
        "the tool must not compact inside its own call"
    );

    // The actor runs it at the next boundary.
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    let rows = wait_compaction(&harness).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "completed");
}

#[tokio::test]
async fn compact_context_tool_call_compacts_the_callers_own_session_at_the_next_boundary() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "compact_context",
                "call_compact",
                json!({"note": "self compaction", "expectation": expectation()}),
            ),
            text_script("queued the compaction"),
            text_script("## Objective\nself compaction handoff"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;

    // The tool runs through turn dispatch, exactly as the model would call it.
    // Its wake triggers the follow-up execution, which drains the queued row at
    // that next turn boundary.
    harness.prompt("compact this session").await;
    let rows = wait_compaction(&harness).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["status"], "completed");
    assert_eq!(rows[0]["reason"], "manual");
    assert!(
        rows[0]["summary"]
            .as_str()
            .is_some_and(|summary| !summary.trim().is_empty()),
        "{rows:?}"
    );
    // `harness.messages()` is scoped to the caller's session id, so the row
    // landing here proves the tool targeted the caller and nobody else.
    assert_eq!(harness.inbox().await.len(), 0);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn stable_boxes_survive_two_compactions_and_temporary_boxes_expire() {
    let harness = Harness::new(
        vec![
            text_script("## Objective\nfirst handoff"),
            text_script("## Objective\nsecond handoff"),
        ],
        Arc::new(AllowAll),
        json!({"compaction": {"auto": false}}),
        None,
    )
    .await;
    harness
        .store
        .write_box(
            NewBox {
                root_id: harness.session.clone(),
                owner_session: harness.session.clone(),
                key: "facts".to_owned(),
                title: Some("durable facts".to_owned()),
                description: Some("constants that must survive compaction".to_owned()),
                kind: "stable".to_owned(),
                tags: Vec::new(),
                content: "CQG_HISTORY_DAYS=30".to_owned(),
            },
            false,
        )
        .await
        .expect("stable box");
    harness
        .store
        .write_box(
            NewBox {
                root_id: harness.session.clone(),
                owner_session: harness.session.clone(),
                key: "scratch".to_owned(),
                title: None,
                description: Some("working notes".to_owned()),
                kind: "temporary".to_owned(),
                tags: Vec::new(),
                content: "temporary note".to_owned(),
            },
            false,
        )
        .await
        .expect("temporary box");

    enqueue_compaction(&harness).await;
    let first = wait_compaction(&harness).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["status"], "completed");

    // The summary request carries the box index, and only the index: content
    // must be loaded on demand with box_read. The index is injected into the
    // summary instruction, not the cached system prefix.
    let summary_request = &harness.model.requests()[0];
    let system = summary_request.system.join("\n");
    let prompt_text = summary_request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(oc_llm::ContentPart::as_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        prompt_text.contains("<boxes"),
        "prompt index missing: {prompt_text}"
    );
    assert!(
        prompt_text.contains("[stable] facts"),
        "stable key missing from the index: {prompt_text}"
    );
    let everything = format!("{system}\n{prompt_text}");
    assert!(
        !everything.contains("CQG_HISTORY_DAYS=30"),
        "box content must not be inlined into the prompt"
    );

    // Temporary expired; stable untouched.
    assert!(
        harness
            .store
            .get_box(&harness.session, "scratch", false)
            .await
            .expect("read")
            .is_none(),
        "temporary boxes expire at compaction"
    );
    let expired = harness
        .store
        .get_box(&harness.session, "scratch", true)
        .await
        .expect("read expired")
        .expect("row kept for audit");
    let expiry = expired["timeExpired"].as_i64().expect("timestamp");
    let stable = harness
        .store
        .get_box(&harness.session, "facts", false)
        .await
        .expect("read stable")
        .expect("stable box survives");
    assert_eq!(stable["revision"], 1);
    assert!(stable.get("timeExpired").is_none());

    enqueue_compaction(&harness).await;
    let second = wait_compaction(&harness).await;
    assert_eq!(second.len(), 2, "a second compaction committed");
    assert_eq!(second[1]["status"], "completed");

    // The fact is still available after two generations, and the second
    // compaction did not re-expire the already expired temporary box.
    let stable = harness
        .store
        .get_box(&harness.session, "facts", false)
        .await
        .expect("read")
        .expect("stable box survives two compactions");
    assert_eq!(stable["content"], "CQG_HISTORY_DAYS=30");
    let again = harness
        .store
        .get_box(&harness.session, "scratch", true)
        .await
        .expect("read expired")
        .expect("row");
    assert_eq!(again["timeExpired"].as_i64(), Some(expiry));
    assert_eq!(
        harness.sink.data("session.compaction.ended").len(),
        2,
        "both compactions published their lifecycle end"
    );
}

#[tokio::test]
async fn box_writes_do_not_bust_the_pinned_instruction_prefix() {
    let harness = Harness::new(
        vec![
            text_script("first"),
            text_script("second"),
            text_script("third"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    // The box exists before the first execution, so the epoch pins an index.
    run_tool(
        &harness,
        &harness.session,
        "box_write",
        json!({"key": "early", "content": "known at epoch start"}),
    )
    .await
    .expect("write");
    harness.prompt("one").await;
    let first_system = harness.model.requests()[0].system.clone();
    assert!(
        first_system.join("\n").contains("[stable] early"),
        "the epoch prefix carries the box index"
    );

    run_tool(
        &harness,
        &harness.session,
        "box_write",
        json!({"key": "late", "content": "created mid-epoch"}),
    )
    .await
    .expect("write");

    harness.prompt("two").await;
    let second = &harness.model.requests()[1];
    assert_eq!(
        second.system, first_system,
        "a mid-epoch box write must not rewrite the cached prefix"
    );
    let tail_has_index = second.messages.iter().any(|message| {
        message
            .content
            .iter()
            .filter_map(oc_llm::ContentPart::as_text)
            .any(|text| text.contains("[stable] late"))
    });
    assert!(
        tail_has_index,
        "the change is delivered as a tail supplement"
    );

    // A new actor epoch renders the fresh index into the prefix.
    let restart = harness.restart();
    harness
        .store
        .enqueue(NewInbox {
            id: oc_types::MessageId::new().into_inner(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "three", "time": {"created": oc_types::now_millis()}}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    restart.notify(&harness.session).await.expect("notify");
    restart
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");
    let third = harness
        .model
        .requests()
        .last()
        .expect("third request")
        .system
        .join("\n");
    assert!(third.contains("[stable] late"), "{third}");
    assert!(third.contains("[stable] early"), "{third}");
}

#[tokio::test]
async fn tool_metadata_never_carries_box_bodies() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let sentinel = "BOX_BODY_SENTINEL_9f3c";
    run_tool(
        &harness,
        &harness.session,
        "box_write",
        json!({"key": "secret", "content": sentinel, "description": "body leak probe"}),
    )
    .await
    .expect("write");

    let tools = BoxTools::new(harness.runtime.clone()).registry();
    for (name, input) in [
        ("box_list", json!({})),
        (
            "box_write",
            json!({"key": "secret", "content": sentinel, "append": true}),
        ),
        ("box_read", json!({"key": "secret"})),
    ] {
        let result = tools
            .get(name)
            .unwrap_or_else(|| panic!("{name} is registered"))
            .execute(&tool_context(&harness, &harness.session), input)
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let metadata = result.metadata.expect("metadata");
        assert!(
            !metadata.to_string().contains(sentinel),
            "{name} metadata leaks the box body: {metadata}"
        );
        if name == "box_read" {
            assert!(
                result.output.unwrap_or_default().contains(sentinel),
                "box_read output still carries the body"
            );
        }
    }
}

#[tokio::test]
async fn an_empty_box_body_is_rejected() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    for input in [
        json!({"key": "blank", "content": "   \n\t "}),
        json!({"key": "blank", "content": "", "append": true}),
    ] {
        let error = run_tool(&harness, &harness.session, "box_write", input)
            .await
            .expect_err("an empty body is rejected");
        assert!(error.to_string().contains("empty"), "{error}");
    }
    assert!(
        harness
            .store
            .get_box(&harness.session, "blank", true)
            .await
            .expect("read")
            .is_none(),
        "a rejected write must not create a row"
    );
}

#[tokio::test]
async fn the_index_reports_more_boxes_than_it_shows() {
    let harness = Harness::new(
        vec![text_script("done")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    for index in 0..105 {
        harness
            .store
            .write_box(
                NewBox {
                    root_id: harness.session.clone(),
                    owner_session: harness.session.clone(),
                    key: format!("k{index:03}"),
                    title: None,
                    description: Some(format!("box {index}")),
                    kind: "stable".to_owned(),
                    tags: Vec::new(),
                    content: format!("body {index}"),
                },
                false,
            )
            .await
            .expect("box written");
    }
    harness.prompt("render the index").await;
    let system = harness.model.requests()[0].system.join("\n");
    let rendered = system
        .lines()
        .filter(|line| line.starts_with("- [stable]") || line.starts_with("- [temporary]"))
        .count();
    assert_eq!(rendered, 100, "the index shows exactly its cap: {system}");
    let notice = system
        .lines()
        .find(|line| line.contains("more boxes"))
        .unwrap_or_else(|| panic!("truncation notice missing: {system}"));
    assert!(notice.contains("1 more boxes"), "{notice}");
}
