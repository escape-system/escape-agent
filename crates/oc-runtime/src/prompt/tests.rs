use super::*;
use oc_llm::Role;
use oc_types::ModelRef;
use serde_json::json;

#[test]
fn default_prompt_lists_tools_and_directory() {
    let tools = vec!["read".to_owned(), "shell".to_owned()];
    let prompt = default_system_prompt(&tools, "/work");
    assert!(prompt.contains("read, shell"));
    assert!(prompt.contains("/work"));
    assert!(prompt.contains("expectation"));
}

#[test]
fn delegation_guidance_appears_only_with_the_subagent_tool() {
    let with = default_system_prompt(&["read".to_owned(), "subagent".to_owned()], "/work");
    assert!(with.contains("`subagent`"), "{with}");
    assert!(with.contains("`explore`"), "{with}");
    assert!(with.contains("background: true"), "{with}");
    let without = default_system_prompt(&["read".to_owned()], "/work");
    assert!(!without.contains("`subagent`"), "{without}");
    assert!(!without.contains("background: true"), "{without}");
}

#[test]
fn skill_write_guidance_appears_with_tool() {
    let with = default_system_prompt(&["read".to_owned(), "skill_write".to_owned()], "/work");
    assert!(with.contains("`skill_write`"), "{with}");
    assert!(with.contains("repeatable"), "{with}");
    let without = default_system_prompt(&["read".to_owned()], "/work");
    assert!(!without.contains("`skill_write`"), "{without}");
}

#[test]
fn system_prefix_is_two_parts() {
    let parts = system_parts(Some("custom"), &[], "/work");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], "custom");
    assert!(parts[1].contains("<env>"));
}

#[test]
fn compaction_summary_is_the_context_boundary() {
    let messages = vec![
        json!({"type": "user", "text": "old"}),
        json!({"type": "compaction", "status": "completed", "summary": "handoff"}),
        json!({"type": "user", "text": "new"}),
    ];
    let converted = messages_from_store(&messages);
    assert_eq!(converted.len(), 2);
    assert_eq!(converted[0].content[0].as_text(), Some("handoff"));
    assert_eq!(converted[1].content[0].as_text(), Some("new"));
}

#[test]
fn compaction_keep_id_replays_the_retained_tail() {
    let messages = vec![
        json!({"id": "m1", "type": "user", "text": "old"}),
        json!({"id": "m2", "type": "user", "text": "keep"}),
        json!({"id": "m3", "type": "assistant", "time": {"created": 1, "completed": 2},
                   "content": [{"type": "text", "text": "answer"}]}),
        json!({"id": "c1", "type": "compaction", "status": "completed",
                   "summary": "handoff", "keepID": "m2"}),
        json!({"id": "m4", "type": "user", "text": "new"}),
    ];
    let converted = messages_from_store(&messages);
    let texts: Vec<&str> = converted
        .iter()
        .map(|message| message.content[0].as_text().unwrap_or_default())
        .collect();
    assert_eq!(texts, vec!["handoff", "keep", "answer", "new"]);
}

#[test]
fn assistant_tool_calls_are_replayed_with_results() {
    let messages = vec![json!({
        "type": "assistant",
        "content": [
            {"type": "text", "text": "checking"},
            {"type": "tool", "id": "call_1", "name": "read",
             "state": {"status": "completed", "input": {"path": "a"},
                       "content": [{"type": "text", "text": "body"}]},
             "time": {"created": 1}}
        ]
    })];
    let converted = messages_from_store(&messages);
    assert_eq!(converted.len(), 2);
    assert_eq!(converted[0].role, Role::Assistant);
    assert_eq!(converted[1].role, Role::Tool);
}

#[test]
fn tool_file_data_uri_does_not_inline_base64_into_tool_text_and_emits_image_part() {
    let big_b64 = "A".repeat(120_000);
    let uri = format!("data:image/png;base64,{big_b64}");
    let messages = vec![json!({
        "type": "assistant",
        "content": [
            {"type": "tool", "id": "call_img", "name": "read",
             "state": {
                 "status": "completed",
                 "input": {"filePath": "shot.png"},
                 "content": [{"type": "file", "uri": uri, "mime": "image/png"}]
             },
             "time": {"created": 1}}
        ]
    })];
    let converted = messages_from_store(&messages);
    assert_eq!(converted.len(), 3);
    assert_eq!(converted[0].role, Role::Assistant);
    assert_eq!(converted[1].role, Role::Tool);
    let oc_llm::ContentPart::ToolResult { content, .. } = &converted[1].content[0] else {
        panic!("expected ToolResult part");
    };
    assert!(
        !content.contains(&big_b64),
        "raw base64 must never be inlined into plain-text Role::Tool content"
    );
    assert_eq!(content, "[file image/png: 90000 bytes]");
    assert_eq!(converted[2].role, Role::User);
    assert_eq!(
        converted[2].content,
        vec![oc_llm::ContentPart::image("image/png", big_b64)]
    );
    // Token estimation charges IMAGE_TOKENS (1500), not 30,000+ text tokens.
    let estimated = oc_llm::estimate_messages_tokens(&converted);
    assert!(
        estimated < 2_000,
        "expected image token estimate (< 2000), got {estimated}"
    );
}

#[test]
fn model_references_parse_both_shapes() {
    assert_eq!(
        model_ref(&json!("test/alpha#fast")),
        Some(ModelRef {
            provider_id: "test".to_owned(),
            id: "alpha".to_owned(),
            variant: Some("fast".to_owned()),
        })
    );
    assert_eq!(
        model_ref(&json!({"providerID": "test", "id": "alpha"})),
        Some(ModelRef {
            provider_id: "test".to_owned(),
            id: "alpha".to_owned(),
            variant: None,
        })
    );
}
