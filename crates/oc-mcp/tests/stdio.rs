//! End-to-end tests against the fake stdio MCP server.

use std::collections::BTreeMap;
use std::path::PathBuf;

use oc_mcp::client::ClientOptions;
use oc_mcp::config::{LocalConfig, McpServerConfig};
use oc_mcp::error::McpError;
use oc_mcp::tool::McpContent;
use oc_mcp::{McpClient, McpStatus};
use serde_json::{Map, Value, json};

fn fake_server() -> McpServerConfig {
    McpServerConfig::Local(LocalConfig {
        command: vec![env!("CARGO_BIN_EXE_oc-mcp-fake-server").to_owned()],
        cwd: None,
        environment: BTreeMap::new(),
        disabled: None,
        codemode: None,
        timeout: None,
        protocol: None,
    })
}

fn arguments(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("expected object, got {other}"),
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end test walks the whole stdio lifecycle in order"
)]
async fn stdio_initialize_catalogs_calls_and_shutdown() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    let mut options = ClientOptions::new(directory.clone());
    options.status_handler = Some(std::sync::Arc::new(move |event| {
        sink.lock().expect("events").push(event.status);
    }));
    let client = McpClient::connect("fake", &fake_server(), options)
        .await
        .expect("connect");
    assert_eq!(client.status(), McpStatus::Connected);
    assert_eq!(
        *events.lock().expect("events"),
        vec![McpStatus::Pending, McpStatus::Connected]
    );
    assert_eq!(client.protocol_version().as_deref(), Some("2025-11-25"));
    let info = client.server_info().expect("server info");
    assert_eq!(info.name, "fake-mcp");
    assert_eq!(info.title.as_deref(), Some("Fake MCP Server"));
    assert_eq!(
        client.instructions().as_deref(),
        Some("Fake server instructions.\nSecond line.")
    );
    assert!(client.pid().is_some());

    // tools/list pagination followed to the end; the schema-less tool is dropped.
    let tools = client.tools();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, ["echo", "fail", "media", "structured"]);
    let echo = tools.iter().find(|tool| tool.name == "echo").expect("echo");
    assert_eq!(echo.effective_name, "fake_echo");
    assert_eq!(echo.namespace, "fake");
    assert_eq!(
        echo.schema()["properties"]["expectation"]["type"],
        json!("object")
    );

    // expectation is stripped before dispatch.
    let output = echo
        .execute(
            &client,
            arguments(json!({
                "value": "hi",
                "expectation": {"why": "test", "expect_seconds": 5, "intervene_after_seconds": 10}
            })),
        )
        .await
        .expect("echo");
    assert_eq!(output.text(), r#"echo:{"value":"hi"}"#);

    let fail = tools.iter().find(|tool| tool.name == "fail").expect("fail");
    let error = fail
        .execute(&client, Map::new())
        .await
        .expect_err("fail tool");
    assert!(matches!(
        error,
        McpError::ToolReturnedError { ref message, .. } if message == "boom from fake server"
    ));

    let media = tools
        .iter()
        .find(|tool| tool.name == "media")
        .expect("media");
    let mapped = media.execute(&client, Map::new()).await.expect("media");
    assert_eq!(mapped.content.len(), 5);
    assert_eq!(
        mapped.content[0],
        McpContent::Text {
            text: "text part".to_owned()
        }
    );
    assert_eq!(
        mapped.content[1],
        McpContent::Media {
            data: "aW1n".to_owned(),
            mime_type: "image/png".to_owned(),
            uri: None
        }
    );
    assert_eq!(
        mapped.content[4],
        McpContent::Text {
            text: "inline text".to_owned()
        }
    );

    let structured = tools
        .iter()
        .find(|tool| tool.name == "structured")
        .expect("structured");
    let mapped = structured
        .execute(&client, Map::new())
        .await
        .expect("structured");
    assert_eq!(mapped.structured, Some(json!({"ok": true, "count": 2})));
    assert_eq!(mapped.text(), "");

    // roots/list is answered by the client, not the fake server.
    let roots = client
        .call_tool("roots", json!({}))
        .await
        .expect("roots call");
    let text = roots["content"][0]["text"].as_str().expect("roots text");
    assert!(text.contains("file:"), "roots text was {text}");
    assert!(text.contains("oc-mcp"), "roots text was {text}");

    let resources = client.refresh_resources().await.expect("resources");
    assert_eq!(resources.len(), 1);
    assert_eq!(client.resource_templates().len(), 1);
    let read = client
        .read_resource("file:///fake.txt")
        .await
        .expect("read")
        .expect("resources capability");
    assert_eq!(read["contents"][0]["text"], "fake contents");

    let prompts = client.refresh_prompts().await.expect("prompts");
    assert_eq!(prompts.len(), 1);
    let prompt = client
        .get_prompt("greet", Some(json!({"who": "Ada"})))
        .await
        .expect("prompt");
    assert_eq!(prompt["messages"][0]["content"]["text"], json!("hello Ada"));

    client.ping().await.expect("ping");

    client.shutdown().await;
    assert_eq!(client.status(), McpStatus::Disabled);
    let error = client.ping().await.expect_err("shut down");
    assert!(matches!(error, McpError::NotConnected { .. }));
}

#[tokio::test]
async fn stdio_tool_call_error_is_typed() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let client = McpClient::connect("fake", &fake_server(), ClientOptions::new(directory))
        .await
        .expect("connect");
    let error = client
        .call_tool("does-not-exist", json!({}))
        .await
        .expect_err("unknown tool");
    assert!(matches!(
        error,
        McpError::ToolCall { ref tool, .. } if tool == "does-not-exist"
    ));
    client.shutdown().await;
}
