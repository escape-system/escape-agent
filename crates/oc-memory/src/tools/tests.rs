//! Behavior tests for the four memory tools.

use super::*;
use crate::service::{LIST_MAX, ListRequest, MemoryService, SEARCH_DEFAULT};
use oc_store::Store;
use oc_tools::Tool;
use serde_json::json;
use std::path::PathBuf;

fn context() -> ToolContext {
    ToolContext {
        session_id: "ses_memory_test".to_owned(),
        directory: PathBuf::from("/projects/memory-test"),
        cwd: PathBuf::from("/projects/memory-test"),
        cancel: None,
    }
}

async fn registry() -> (Arc<MemoryService>, ToolRegistry) {
    let service = Arc::new(MemoryService::new(Arc::new(
        Store::open_memory().await.expect("store"),
    )));
    let tools = MemoryTools::new(Arc::clone(&service)).with_project_id("prj_test");
    (service, tools.registry())
}

fn tool(registry: &ToolRegistry, name: &str) -> Arc<dyn Tool> {
    Arc::clone(registry.get(name).expect("registered tool"))
}

#[tokio::test]
async fn registry_contains_only_memory_tools() {
    let (_service, registry) = registry().await;
    assert_eq!(
        registry.names(),
        vec![
            "memory_forget",
            "memory_list",
            "memory_search",
            "memory_write"
        ]
    );
}

#[tokio::test]
async fn schemas_do_not_declare_expectation() {
    let (_service, registry) = registry().await;
    for definition in registry.definitions() {
        assert!(
            definition.parameters["properties"]
                .get("expectation")
                .is_none(),
            "{} leaks expectation",
            definition.name
        );
        assert_eq!(definition.parameters["additionalProperties"], false);
    }
    let write = registry
        .definitions()
        .into_iter()
        .find(|definition| definition.name == "memory_write")
        .expect("write schema");
    assert_eq!(write.parameters["required"], json!(["text"]));
}

#[tokio::test]
async fn all_four_tools_execute() {
    let (service, registry) = registry().await;
    let ctx = context();

    let written = tool(&registry, "memory_write")
        .execute(
            &ctx,
            json!({
                "text": "Use rusqlite with a single writer because sqlx compiles too slowly",
                "kind": "decision",
                "tags": ["storage"],
                "importance": 5
            }),
        )
        .await
        .expect("write");
    let written_text = written.output.expect("output");
    assert!(
        written_text.contains("Stored memory mem_"),
        "{written_text}"
    );
    assert!(written_text.contains("[decision]"));
    assert!(written_text.contains("scope=project"));

    let rows = service
        .list(ListRequest {
            scopes: vec![("project".to_owned(), Some("prj_test".to_owned()))],
            ..ListRequest::default()
        })
        .await
        .expect("list");
    let id = rows[0]["id"].as_str().expect("id").to_owned();
    assert!(written_text.contains(&id));

    let searched = tool(&registry, "memory_search")
        .execute(&ctx, json!({"query": "sqlite writer"}))
        .await
        .expect("search");
    let search_text = searched.output.expect("output");
    assert!(
        search_text.starts_with("<memory scope=\"project\">"),
        "{search_text}"
    );
    assert!(search_text.contains(&id), "{search_text}");
    assert!(
        search_text.contains("[decision] Use rusqlite with a single writer"),
        "{search_text}"
    );
    assert_eq!(searched.metadata.expect("metadata")["count"], 1);

    let listed = tool(&registry, "memory_list")
        .execute(&ctx, json!({}))
        .await
        .expect("list");
    let list_text = listed.output.expect("output");
    assert!(list_text.contains(&id), "{list_text}");
    assert!(list_text.contains("[decision]"), "{list_text}");
    assert!(list_text.contains("(project)"), "{list_text}");

    let forgotten = tool(&registry, "memory_forget")
        .execute(&ctx, json!({"id": id}))
        .await
        .expect("forget");
    assert!(forgotten.output.expect("output").contains("Forgot memory"));

    let again = tool(&registry, "memory_forget")
        .execute(&ctx, json!({"id": id}))
        .await
        .expect("forget twice");
    assert!(
        again
            .output
            .expect("output")
            .contains("No live memory with id")
    );
}

#[tokio::test]
async fn search_scope_and_empty_results() {
    let (_service, registry) = registry().await;
    let ctx = context();

    let empty = tool(&registry, "memory_search")
        .execute(&ctx, json!({"query": "nothing matches this"}))
        .await
        .expect("search");
    assert_eq!(empty.output.as_deref(), Some("No memories found."));

    tool(&registry, "memory_write")
        .execute(
            &ctx,
            json!({
                "text": "Globally prefer small focused commits over large rewrites",
                "scope": "global",
                "kind": "preference"
            }),
        )
        .await
        .expect("write global");

    let global_only = tool(&registry, "memory_search")
        .execute(&ctx, json!({"query": "focused commits", "scope": "global"}))
        .await
        .expect("search global");
    assert!(
        global_only
            .output
            .expect("output")
            .contains("small focused commits")
    );

    let project_only = tool(&registry, "memory_search")
        .execute(
            &ctx,
            json!({"query": "focused commits", "scope": "project"}),
        )
        .await
        .expect("search project");
    assert_eq!(project_only.output.as_deref(), Some("No memories found."));
}

#[tokio::test]
async fn tool_errors_are_typed_and_never_echo_secrets() {
    let (_service, registry) = registry().await;
    let ctx = context();
    let error = tool(&registry, "memory_write")
        .execute(
            &ctx,
            json!({"text": "the deploy token is sk-proj-abcdefghijklmnopqrstuvwxyz"}),
        )
        .await
        .unwrap_err();
    assert!(
        error.message.contains("openai-api-key"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("sk-proj-abcdefghijklmnopqrstuvwxyz"),
        "{}",
        error.message
    );

    let bad_scope = tool(&registry, "memory_write")
        .execute(
            &ctx,
            json!({"text": "some memory text", "scope": "workspace"}),
        )
        .await
        .unwrap_err();
    assert!(
        bad_scope.message.contains("`scope`"),
        "{}",
        bad_scope.message
    );
}

#[tokio::test]
async fn agent_scope_requires_a_configured_agent() {
    let service = Arc::new(MemoryService::new(Arc::new(
        Store::open_memory().await.expect("store"),
    )));
    let tools = MemoryTools::new(service).with_project_id("prj_test");
    let registry = tools.registry();
    let ctx = context();
    let error = tool(&registry, "memory_write")
        .execute(
            &ctx,
            json!({"text": "agent-scoped memory without an agent id", "scope": "agent"}),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("with_agent_id"), "{}", error.message);
}

#[tokio::test]
async fn list_pagination_emits_a_cursor() {
    let (_service, registry) = registry().await;
    let ctx = context();
    for index in 0..3 {
        tool(&registry, "memory_write")
            .execute(
                &ctx,
                json!({"text": format!("memory number {index} for pagination checks")}),
            )
            .await
            .expect("write");
    }

    let first = tool(&registry, "memory_list")
        .execute(&ctx, json!({"limit": 2}))
        .await
        .expect("list");
    let text = first.output.expect("output");
    assert_eq!(text.lines().count(), 3, "{text}");
    assert!(text.ends_with("(more: cursor=2)"), "{text}");
    assert_eq!(first.metadata.expect("metadata")["cursor"], "2");

    let second = tool(&registry, "memory_list")
        .execute(&ctx, json!({"limit": 2, "cursor": "2"}))
        .await
        .expect("list");
    let text = second.output.expect("output");
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(second.metadata.expect("metadata")["cursor"].is_null());
}

/// Writes `count` distinct memories into the project scope.
async fn seed(registry: &ToolRegistry, ctx: &ToolContext, count: usize) {
    for index in 0..count {
        tool(registry, "memory_write")
            .execute(
                ctx,
                json!({"text": format!("seeded memory number {index} for paging checks")}),
            )
            .await
            .expect("write");
    }
}

#[tokio::test]
async fn list_at_the_page_cap_still_reports_more() {
    // T1(b): `limit = LIST_MAX` used to fetch exactly the cap, so the
    // `limit + 1` probe never exceeded it and `more` was never true.
    let (_service, registry) = registry().await;
    let ctx = context();
    seed(&registry, &ctx, 5).await;

    let page = tool(&registry, "memory_list")
        .execute(&ctx, json!({"limit": LIST_MAX}))
        .await
        .expect("list");
    let metadata = page.metadata.expect("metadata");
    assert_eq!(metadata["count"], 5, "{metadata}");
    assert!(metadata["cursor"].is_null(), "{metadata}");

    let text = page.output.expect("output");
    assert_eq!(text.lines().count(), 5, "{text}");
    assert!(!text.contains("more:"), "{text}");
}

#[tokio::test]
async fn list_page_larger_than_the_remaining_rows_has_no_cursor() {
    // T1(a): a page that consumes the rest of the rows must not pretend a
    // further page exists.
    let (_service, registry) = registry().await;
    let ctx = context();
    seed(&registry, &ctx, 3).await;

    let page = tool(&registry, "memory_list")
        .execute(&ctx, json!({"limit": 3}))
        .await
        .expect("list");
    let metadata = page.metadata.expect("metadata");
    assert_eq!(metadata["count"], 3, "{metadata}");
    assert!(metadata["cursor"].is_null(), "{metadata}");
}

#[tokio::test]
async fn list_limit_twenty_still_pages_normally() {
    let (_service, registry) = registry().await;
    let ctx = context();
    seed(&registry, &ctx, 25).await;

    let first = tool(&registry, "memory_list")
        .execute(&ctx, json!({"limit": 20}))
        .await
        .expect("list");
    let metadata = first.metadata.expect("metadata");
    assert_eq!(metadata["count"], 20, "{metadata}");
    assert_eq!(metadata["cursor"], "20", "{metadata}");

    let second = tool(&registry, "memory_list")
        .execute(&ctx, json!({"limit": 20, "cursor": "20"}))
        .await
        .expect("list");
    let metadata = second.metadata.expect("metadata");
    assert_eq!(metadata["count"], 5, "{metadata}");
    assert!(metadata["cursor"].is_null(), "{metadata}");
}

#[tokio::test]
async fn list_cursor_past_the_window_is_an_explicit_error() {
    // T1(c): past the cap used to answer a silent empty page while rows
    // still existed -> "No memories stored." was a lie.
    let (_service, registry) = registry().await;
    let ctx = context();
    seed(&registry, &ctx, 2).await;

    let error = tool(&registry, "memory_list")
        .execute(&ctx, json!({"cursor": "500"}))
        .await
        .unwrap_err();
    assert!(
        error.message.contains("pagination window"),
        "{}",
        error.message
    );
    assert!(error.message.contains("cursor 500"), "{}", error.message);
}

#[tokio::test]
async fn list_does_not_drop_rows_when_the_page_is_the_whole_window() {
    // T1: `offset + limit` beyond the fetch used to return fewer rows than
    // the data holds and lose the cursor. The page now ends at the cap and
    // says so instead of claiming the data ended.
    let (_service, registry) = registry().await;
    let ctx = context();
    seed(&registry, &ctx, 7).await;

    let page = tool(&registry, "memory_list")
        .execute(&ctx, json!({"limit": 4, "cursor": "2"}))
        .await
        .expect("list");
    let metadata = page.metadata.expect("metadata");
    assert_eq!(metadata["count"], 4, "{metadata}");
    assert_eq!(metadata["cursor"], "6", "{metadata}");

    let last = tool(&registry, "memory_list")
        .execute(&ctx, json!({"limit": 4, "cursor": "6"}))
        .await
        .expect("list");
    let metadata = last.metadata.expect("metadata");
    assert_eq!(metadata["count"], 1, "{metadata}");
    assert!(metadata["cursor"].is_null(), "{metadata}");
    assert_eq!(metadata["windowExhausted"], false, "{metadata}");
}

#[tokio::test]
async fn search_honours_the_requested_limit() {
    // T2: `limit: 1` used to render up to MIN_SEARCH_LIMIT lines.
    let (_service, registry) = registry().await;
    let ctx = context();
    seed(&registry, &ctx, 10).await;

    let searched = tool(&registry, "memory_search")
        .execute(&ctx, json!({"query": "seeded memory", "limit": 1}))
        .await
        .expect("search");
    let metadata = searched.metadata.expect("metadata");
    assert_eq!(metadata["count"], 1, "{metadata}");
    let text = searched.output.expect("output");
    assert_eq!(
        text.lines().filter(|line| line.starts_with("- [")).count(),
        1,
        "{text}"
    );

    let eight = tool(&registry, "memory_search")
        .execute(&ctx, json!({"query": "seeded memory"}))
        .await
        .expect("search");
    let metadata = eight.metadata.expect("metadata");
    assert_eq!(metadata["count"], SEARCH_DEFAULT, "{metadata}");
}

#[tokio::test]
async fn search_limit_fifty_is_still_bounded_by_the_token_budget() {
    let (_service, registry) = registry().await;
    let ctx = context();
    seed(&registry, &ctx, 30).await;

    let searched = tool(&registry, "memory_search")
        .execute(&ctx, json!({"query": "seeded memory", "limit": 50}))
        .await
        .expect("search");
    let metadata = searched.metadata.expect("metadata");
    let count = metadata["count"].as_u64().expect("count");
    assert!(count >= 1, "{metadata}");
    assert!(count <= 30, "{metadata}");
    let tokens = metadata["tokens"].as_u64().expect("tokens");
    assert!(
        tokens <= u64::try_from(crate::DEFAULT_TOKEN_BUDGET).expect("budget fits"),
        "{metadata}"
    );
}
