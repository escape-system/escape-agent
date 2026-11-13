//! Scenario tests for the MCP, LSP and plugin status/control routes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::body::to_bytes;
use axum::extract::Json as AxumJson;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use jsonc_parser::ParseOptions;
use oc_config::{ConfigService, LoadOptions};
use oc_lsp::{LspConfig, LspServerConfig, LspSupervisor, SupervisorOptions};
use oc_mcp::McpConfig;
use oc_plugin::{Plugin, PluginRegistry, RpcError, RpcNamespace, Tier};
use oc_server::data::lsp_tools::{
    LspDiagnosticsTool, LspEditDiagnosticsHook, LspSemanticOracle, LspTool,
};
use oc_server::data::mcp::McpManager;
use oc_server::data::mcp_tools::{McpToolAdapter, McpToolProvider};
use oc_server::{AppState, router};
use oc_tools::{Tool, ToolContext};
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower::ServiceExt;

fn state(directory: &std::path::Path) -> AppState {
    AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: Some("127.0.0.1".to_owned()),
            urls: vec!["http://127.0.0.1:4096".to_owned()],
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        directory.to_path_buf(),
    )
}

async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Option<Value>) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let value = if bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&bytes).unwrap())
    };
    (status, value)
}

fn manager(config_dir: &Path) -> McpManager {
    let config = McpConfig::from_value(&json!({
        "servers": {
            "alpha": {"type": "local", "command": ["definitely-not-spawned-mcp"]},
            "off": {
                "type": "remote",
                "url": "https://mcp.invalid/mcp",
                "disabled": true
            }
        }
    }))
    .unwrap();
    McpManager::new(config, std::env::temp_dir()).with_config_dir(config_dir)
}

fn read_jsonc(path: &Path) -> Value {
    jsonc_parser::parse_to_serde_value(
        &std::fs::read_to_string(path).unwrap(),
        &ParseOptions::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn mcp_without_manager_is_503() {
    let workspace = tempfile::tempdir().unwrap();
    let app = router(state(workspace.path()));
    let (status, _) = call(&app, Method::GET, "/api/mcp", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _) = call(&app, Method::GET, "/api/mcp/resource", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn mcp_status_never_connects() {
    let workspace = tempfile::tempdir().unwrap();
    let app = router(state(workspace.path()).with_mcp(Arc::new(manager(workspace.path()))));

    let (status, value) = call(&app, Method::GET, "/api/mcp", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = value.unwrap()["data"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 2);
    let alpha = rows.iter().find(|row| row["name"] == "alpha").unwrap();
    assert_eq!(alpha["type"], "local");
    assert_eq!(alpha["status"], "disconnected");
    assert!(alpha.get("tools").is_none(), "no catalog before connecting");
    let off = rows.iter().find(|row| row["name"] == "off").unwrap();
    assert_eq!(off["type"], "remote");
    assert_eq!(off["status"], "disabled");

    let (status, value) = call(&app, Method::GET, "/api/mcp/resource", None).await;
    assert_eq!(status, StatusCode::OK);
    let data = value.unwrap()["data"].clone();
    assert_eq!(data["resources"].as_array().unwrap().len(), 0);
    assert_eq!(data["templates"].as_array().unwrap().len(), 0);
    assert!(
        data.get("servers").is_none(),
        "catalog is merged, not per server"
    );
}

#[tokio::test]
async fn mcp_put_and_delete_persist_global_config() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("opencode.jsonc"),
        "{\n // keep me\n \"model\": \"fake/model\",\n}",
    )
    .unwrap();
    let app = router(state(workspace.path()).with_mcp(Arc::new(manager(workspace.path()))));

    let (status, _) = call(
        &app,
        Method::PUT,
        "/api/experimental/mcp/alpha",
        Some(json!({"config": {"type": "local", "command": ["x"]}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let document = read_jsonc(&workspace.path().join("opencode.jsonc"));
    assert_eq!(document["model"], "fake/model", "unrelated keys survive");
    assert_eq!(document["mcp"]["servers"]["alpha"]["command"], json!(["x"]));

    let (status, _) = call(
        &app,
        Method::PUT,
        "/api/experimental/mcp/beta",
        Some(json!({"config": {"type": "remote", "url": "https://example.invalid/mcp"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let document = read_jsonc(&workspace.path().join("opencode.jsonc"));
    assert_eq!(
        document["mcp"]["servers"]["beta"]["url"],
        "https://example.invalid/mcp"
    );

    let (status, _) = call(&app, Method::DELETE, "/api/experimental/mcp/beta", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let document = read_jsonc(&workspace.path().join("opencode.jsonc"));
    assert!(document["mcp"]["servers"].get("beta").is_none());
    assert!(document["mcp"]["servers"].get("alpha").is_some());

    let (status, _) = call(&app, Method::DELETE, "/api/experimental/mcp/beta", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &app,
        Method::PUT,
        "/api/experimental/mcp/broken",
        Some(json!({"config": {"type": "local"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn mcp_control_errors_are_honest() {
    let workspace = tempfile::tempdir().unwrap();
    let app = router(state(workspace.path()).with_mcp(Arc::new(manager(workspace.path()))));

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/experimental/mcp/unknown/connect",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/experimental/mcp/unknown/disconnect",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/experimental/mcp/off/connect",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---- fake streamable-HTTP MCP server -------------------------------------

fn fake_initialize() -> Value {
    json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {
            "tools": {"listChanged": false},
            "resources": {"listChanged": false}
        },
        "serverInfo": {"name": "fake-http", "version": "1.0.0"}
    })
}

fn fake_tools() -> Value {
    json!({
        "tools": [
            {
                "name": "echo",
                "description": "Echo the value",
                "inputSchema": {
                    "type": "object",
                    "properties": {"value": {"type": "string"}}
                }
            },
            {
                "name": "media",
                "description": "Return media",
                "inputSchema": {"type": "object", "properties": {}}
            }
        ]
    })
}

async fn fake_mcp_post(AxumJson(body): AxumJson<Value>) -> Response {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let method = body
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let result = match method {
        "initialize" => Some(fake_initialize()),
        "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
        "tools/list" => Some(fake_tools()),
        "tools/call" => {
            let name = body
                .get("params")
                .and_then(|params| params.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name == "echo" {
                Some(json!({"content": [{"type": "text", "text": "echo-ok"}]}))
            } else {
                Some(json!({
                    "content": [
                        {"type": "text", "text": "part"},
                        {"type": "image", "data": "aW1n", "mimeType": "image/png"}
                    ],
                    "structuredContent": {"answer": 42}
                }))
            }
        }
        "resources/list" => Some(json!({
            "resources": [{"name": "r1", "uri": "file:///r1", "mimeType": "text/plain"}]
        })),
        "resources/templates/list" => Some(json!({"resourceTemplates": []})),
        other => {
            return AxumJson(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("unknown {other}")}
            }))
            .into_response();
        }
    };
    AxumJson(json!({"jsonrpc": "2.0", "id": id, "result": result.unwrap()})).into_response()
}

async fn spawn_fake_mcp() -> String {
    let app = Router::new().route("/mcp", post(fake_mcp_post));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}/mcp")
}

fn remote_manager(url: &str, config_dir: &Path) -> McpManager {
    let config = McpConfig::from_value(&json!({
        "servers": {"remote": {"type": "remote", "url": url}}
    }))
    .unwrap();
    McpManager::new(config, std::env::temp_dir()).with_config_dir(config_dir)
}

#[tokio::test]
async fn mcp_connect_disconnect_204_status_and_catalogs() {
    let workspace = tempfile::tempdir().unwrap();
    let url = spawn_fake_mcp().await;
    let state = state(workspace.path()).with_mcp(Arc::new(remote_manager(&url, workspace.path())));
    let mut events = state.bus().subscribe();
    let app = router(state);

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/experimental/mcp/remote/connect",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none(), "204 carries no body");

    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("status event")
        .expect("event");
    assert_eq!(event.kind, "mcp.status.changed");
    assert_eq!(event.data["server"], "remote");
    assert_eq!(event.data["status"], "connected");

    let (status, value) = call(&app, Method::GET, "/api/mcp", None).await;
    assert_eq!(status, StatusCode::OK);
    let row = &value.unwrap()["data"][0];
    assert_eq!(row["status"], "connected");
    assert_eq!(row["tools"], 2);
    assert_eq!(row["resources"], 1);

    let (status, value) = call(&app, Method::GET, "/api/mcp/resource", None).await;
    assert_eq!(status, StatusCode::OK);
    let data = &value.unwrap()["data"];
    let resources = data["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0]["server"], "remote");
    assert_eq!(resources[0]["uri"], "file:///r1");
    assert_eq!(data["templates"].as_array().unwrap().len(), 0);

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/experimental/mcp/remote/disconnect",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("status event")
        .expect("event");
    assert_eq!(event.data["status"], "disconnected");
}

#[tokio::test]
async fn mcp_adapter_schema_execute_and_provider_snapshot() {
    let url = spawn_fake_mcp().await;
    let manager = Arc::new(remote_manager(&url, std::env::temp_dir().as_path()));
    manager.connect("remote").await.unwrap();
    let snapshot = manager.connected_tools();
    assert_eq!(snapshot.len(), 2);
    let provider = McpToolProvider::new(Arc::clone(&manager));
    let session: oc_types::SessionInfo = serde_json::from_value(json!({
        "id": "ses_test",
        "projectID": "prj_test",
        "cost": 0.0,
        "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
        "time": {"created": 0, "updated": 0},
        "location": {"directory": "."}
    }))
    .unwrap();
    assert_eq!(
        oc_runtime::ToolProvider::tools(&provider, &session, "build").len(),
        2
    );

    let (client, tool) = snapshot
        .iter()
        .find(|(_, tool)| tool.name == "echo")
        .cloned()
        .unwrap();
    let adapter = McpToolAdapter::new(Arc::clone(&client), tool);
    assert_eq!(adapter.name(), "remote_echo");
    assert_eq!(adapter.description(), "Echo the value");
    assert_eq!(
        adapter.schema()["properties"]["expectation"]["type"],
        "object"
    );
    let tool_context = ToolContext {
        session_id: "ses_test".to_owned(),
        directory: PathBuf::from("."),
        cwd: PathBuf::from("."),
        cancel: None,
    };
    let result = adapter
        .execute(
            &tool_context,
            json!({
                "value": "hi",
                "expectation": {"why": "test", "expect_seconds": 5, "intervene_after_seconds": 10}
            }),
        )
        .await
        .unwrap();
    assert_eq!(result.output.as_deref(), Some("echo-ok"));
    assert!(
        result.content.is_none(),
        "text-only results use the default"
    );

    let (client, tool) = snapshot
        .iter()
        .find(|(_, tool)| tool.name == "media")
        .cloned()
        .unwrap();
    let adapter = McpToolAdapter::new(client, tool);
    let result = adapter.execute(&tool_context, json!({})).await.unwrap();
    let content = result.content.expect("media content");
    assert_eq!(content[0], json!({"type": "text", "text": "part"}));
    assert_eq!(
        content[1],
        json!({
            "type": "file",
            "uri": "data:image/png;base64,aW1n",
            "mime": "image/png"
        })
    );
    assert_eq!(result.metadata.unwrap()["structured"]["answer"], 42);
    assert_eq!(result.output.as_deref(), Some("part"));

    manager.disconnect("remote").await;
    assert!(manager.connected_tools().is_empty());
}

fn supervisor(workspace: &std::path::Path) -> LspSupervisor {
    let config = LspConfig {
        enabled: true,
        install: false,
        max_servers: 2,
        servers: vec![LspServerConfig {
            id: "test-lsp".to_owned(),
            name: "Test LSP".to_owned(),
            command: vec!["definitely-not-spawned-lsp".to_owned()],
            extensions: vec![".rs".to_owned()],
            disabled: false,
            env: std::collections::BTreeMap::new(),
            initialization: None,
        }],
    };
    LspSupervisor::with_options(
        config,
        workspace,
        SupervisorOptions {
            start_sweeper: false,
            ..SupervisorOptions::default()
        },
    )
}

#[tokio::test]
async fn lsp_without_supervisor_is_503() {
    let workspace = tempfile::tempdir().unwrap();
    let app = router(state(workspace.path()));
    let (status, _) = call(&app, Method::GET, "/api/lsp", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _) = call(&app, Method::GET, "/api/lsp/diagnostics?path=x", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _) = call(&app, Method::POST, "/api/lsp/reload", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn lsp_reports_configured_servers_cold_and_reloads() {
    let workspace = tempfile::tempdir().unwrap();
    let supervisor = supervisor(workspace.path());
    let app = router(state(workspace.path()).with_lsp(Arc::new(supervisor)));

    let (status, value) = call(&app, Method::GET, "/api/lsp", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = value.unwrap()["data"].as_array().unwrap().clone();
    assert_eq!(
        rows.len(),
        1,
        "only configured servers are listed: {rows:?}"
    );
    assert_eq!(rows[0]["id"], "test-lsp");
    assert_eq!(rows[0]["name"], "Test LSP");
    assert_eq!(rows[0]["status"], "cold");
    let expected = std::fs::canonicalize(workspace.path()).unwrap();
    assert_eq!(
        rows[0]["root"].as_str().unwrap(),
        expected.to_string_lossy()
    );

    let (status, body) = call(&app, Method::POST, "/api/lsp/reload", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none());
}

#[tokio::test]
async fn lsp_diagnostics_validation() {
    let workspace = tempfile::tempdir().unwrap();
    let supervisor = supervisor(workspace.path());
    let app = router(state(workspace.path()).with_lsp(Arc::new(supervisor)));

    let (status, _) = call(&app, Method::GET, "/api/lsp/diagnostics", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = call(
        &app,
        Method::GET,
        "/api/lsp/diagnostics?path=missing.rs",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let unopened = workspace.path().join("unopened.rs");
    std::fs::write(&unopened, "fn main() {}\n").unwrap();
    let (status, _) = call(
        &app,
        Method::GET,
        "/api/lsp/diagnostics?path=unopened.rs",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("outside.rs");
    std::fs::write(&outside_file, "fn main() {}\n").unwrap();
    let uri = format!(
        "/api/lsp/diagnostics?path={}",
        outside_file.to_string_lossy().replace('\\', "%5C")
    );
    let (status, _) = call(&app, Method::GET, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn lsp_tool_adapters_expose_upstream_schemas() {
    let workspace = tempfile::tempdir().unwrap();
    let supervisor = Arc::new(supervisor(workspace.path()));
    let lsp = LspTool::new(Arc::clone(&supervisor));
    assert_eq!(lsp.name(), "lsp");
    let schema = lsp.schema();
    assert_eq!(schema["required"], json!(["operation"]));
    assert_eq!(schema["properties"]["line"]["minimum"], 1);
    let operations = schema["properties"]["operation"]["enum"]
        .as_array()
        .unwrap();
    assert!(operations.iter().any(|value| value == "goToDefinition"));

    let diagnostics = LspDiagnosticsTool::new(supervisor);
    assert_eq!(diagnostics.name(), "lsp_diagnostics");
    assert_eq!(diagnostics.schema()["required"], json!(["path"]));
}

#[tokio::test]
async fn plugin_inventory_without_config_is_empty() {
    let workspace = tempfile::tempdir().unwrap();
    let app = router(state(workspace.path()));
    let (status, value) = call(&app, Method::GET, "/api/plugin", None).await;
    assert_eq!(status, StatusCode::OK);
    let value = value.unwrap();
    assert_eq!(value["data"].as_array().unwrap().len(), 0);
    assert!(value["location"]["directory"].is_string());

    let (status, _) = call(&app, Method::POST, "/api/rpc/demo/method", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, value) = call(
        &app,
        Method::POST,
        "/api/plugin/check",
        Some(json!({"target": "demo"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value.unwrap()["data"].as_array().unwrap().len(), 0);

    let (status, _) = call(&app, Method::GET, "/api/plugin/gui?id=demo", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app, Method::GET, "/api/plugin/gui", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/plugin/update",
        Some(json!({"targets": ["some-package"]})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/plugin/update",
        Some(json!({"targets": []})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn configured_plugins_without_host_fail_closed() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("opencode.json"),
        r#"{"plugins":["some-package"]}"#,
    )
    .unwrap();
    let home = workspace.path().to_string_lossy().into_owned();
    let mut env = HashMap::new();
    env.insert("HOME".to_owned(), home.clone());
    env.insert("USERPROFILE".to_owned(), home.clone());
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        workspace.path().join("xdg").to_string_lossy().into_owned(),
    );
    env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());
    let config = Arc::new(
        ConfigService::load(LoadOptions {
            directory: workspace.path().to_path_buf(),
            env,
        })
        .unwrap(),
    );
    let app = router(state(workspace.path()).with_config(config));

    let (status, value) = call(&app, Method::GET, "/api/plugin", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(value.unwrap()["message"], "plugin host not attached");

    let (status, _) = call(&app, Method::POST, "/api/rpc/demo/method", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, _) = call(&app, Method::POST, "/api/plugin/check", Some(json!({}))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

struct DemoPlugin;

impl Plugin for DemoPlugin {
    fn id(&self) -> &'static str {
        "demo"
    }
}

struct DemoRpc;

impl Plugin for DemoRpc {
    fn id(&self) -> &'static str {
        "demo-rpc"
    }

    fn rpc(&self) -> Option<&dyn RpcNamespace> {
        Some(self)
    }
}

impl RpcNamespace for DemoRpc {
    fn namespace(&self) -> &'static str {
        "demo"
    }

    fn invoke(&self, method: &str, input: Value) -> Result<Value, RpcError> {
        match method {
            "echo" => Ok(input),
            "reject" => Err(RpcError::InvalidInput {
                message: "bad input".to_owned(),
            }),
            other => Err(RpcError::MethodNotFound {
                namespace: "demo".to_owned(),
                method: other.to_owned(),
            }),
        }
    }
}

#[tokio::test]
async fn attached_plugin_registry_lists_checks_and_guis() {
    let workspace = tempfile::tempdir().unwrap();
    let registry = Arc::new(PluginRegistry::new());
    registry.register(Tier::BuiltinPre, DemoPlugin);
    registry.register(Tier::Sdk, DemoRpc);
    let app = router(state(workspace.path()).with_plugins(registry));

    let (status, value) = call(&app, Method::GET, "/api/plugin", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = value.unwrap()["data"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], "demo");

    let (status, value) = call(
        &app,
        Method::POST,
        "/api/plugin/check",
        Some(json!({"target": "demo"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = value.unwrap()["data"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "target filters the inventory");
    assert_eq!(rows[0]["id"], "demo");

    let (status, _) = call(&app, Method::GET, "/api/plugin/gui?id=demo", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rpc_dispatch_maps_errors_and_returns_output() {
    let workspace = tempfile::tempdir().unwrap();
    let registry = Arc::new(PluginRegistry::new());
    registry.register(Tier::Sdk, DemoRpc);
    let app = router(state(workspace.path()).with_plugins(registry));

    let (status, value) = call(
        &app,
        Method::POST,
        "/api/rpc/demo/echo",
        Some(json!({"input": {"hello": "world"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value.unwrap(), json!({"output": {"hello": "world"}}));

    let (status, value) = call(&app, Method::POST, "/api/rpc/demo/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(value.unwrap()["type"], "rpc.method_not_found");

    let (status, value) = call(&app, Method::POST, "/api/rpc/unknown/echo", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(value.unwrap()["type"], "rpc.unavailable");

    let (status, value) = call(&app, Method::POST, "/api/rpc/demo/reject", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(value.unwrap()["type"], "rpc.invalid_input");
}

#[cfg(unix)]
fn write_fake_lsp_script(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let lsp_script = dir.join("fake-lsp.py");
    std::fs::write(
        &lsp_script,
        r#"#!/usr/bin/env python3
import json, sys

def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        s = line.decode("ascii", errors="ignore").strip()
        if not s:
            break
        if ":" in s:
            k, v = s.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    length = int(headers.get("content-length", "0"))
    body = sys.stdin.buffer.read(length)
    return json.loads(body.decode("utf-8"))

def send_msg(obj):
    raw = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(raw)}\r\n\r\n".encode("ascii") + raw)
    sys.stdout.buffer.flush()

while True:
    msg = read_msg()
    if msg is None:
        break
    method = msg.get("method")
    rid = msg.get("id")
    params = msg.get("params") or {}
    if method == "initialize":
        send_msg({"jsonrpc": "2.0", "id": rid, "result": {
            "capabilities": {
                "definitionProvider": True,
                "referencesProvider": True,
                "callHierarchyProvider": True
            }
        }})
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        uri = params["textDocument"]["uri"]
        send_msg({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
            "uri": uri,
            "diagnostics": [{
                "range": {"start": {"line": 0, "character": 28}, "end": {"line": 0, "character": 38}},
                "severity": 1,
                "message": "unresolved name `broken_sym`"
            }]
        }})
    elif method == "textDocument/prepareCallHierarchy":
        uri = params["textDocument"]["uri"]
        send_msg({"jsonrpc": "2.0", "id": rid, "result": [{
            "name": "helper_fn",
            "kind": 12,
            "uri": uri,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 40}},
            "selectionRange": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 16}}
        }]})
    elif method == "callHierarchy/incomingCalls":
        uri = params["item"]["uri"]
        send_msg({"jsonrpc": "2.0", "id": rid, "result": [{
            "from": {
                "name": "lsp_live_caller",
                "kind": 12,
                "uri": uri,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 40}},
                "selectionRange": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 16}}
            },
            "fromRanges": []
        }]})
    elif method == "textDocument/diagnostic":
        send_msg({"jsonrpc": "2.0", "id": rid, "result": {"kind": "full", "items": []}})
    elif method == "shutdown":
        send_msg({"jsonrpc": "2.0", "id": rid, "result": None})
    elif method == "exit":
        break
    elif rid is not None:
        send_msg({"jsonrpc": "2.0", "id": rid, "result": None})
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&lsp_script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&lsp_script, perms).unwrap();
    lsp_script
}

#[cfg(unix)]
#[tokio::test]
async fn lsp_edit_diagnostics_hook_and_semantic_oracle_bridge() {
    use oc_runtime::EditDiagnosticsHook;

    let workspace = tempfile::tempdir().unwrap();
    let canon_root = std::fs::canonicalize(workspace.path()).unwrap();
    let src_dir = canon_root.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("lib.rs"),
        "pub fn helper_fn() -> i32 { broken_sym }\n",
    )
    .unwrap();

    let lsp_script = write_fake_lsp_script(&canon_root);

    let cfg = LspConfig {
        enabled: true,
        servers: vec![LspServerConfig {
            id: "fake-rust".to_owned(),
            name: "Fake Rust".to_owned(),
            command: vec![lsp_script.to_string_lossy().into_owned()],
            extensions: vec![".rs".to_owned()],
            disabled: false,
            env: std::collections::BTreeMap::new(),
            initialization: None,
        }],
        ..LspConfig::default()
    };
    let supervisor = Arc::new(LspSupervisor::with_options(
        cfg,
        canon_root.clone(),
        SupervisorOptions {
            start_sweeper: false,
            diagnostics_wait: Duration::from_millis(500),
            ..SupervisorOptions::default()
        },
    ));

    let hook = LspEditDiagnosticsHook::new(Arc::clone(&supervisor));
    let diag = hook
        .after_edit(&canon_root, &["src/lib.rs".to_owned()])
        .await;
    assert_eq!(diag.error_count, 1);
    let formatted = diag.formatted.expect("formatted diagnostics");
    assert!(formatted.contains("<lsp_diagnostics>"));
    assert!(formatted.contains("unresolved name `broken_sym`"));
    assert!(diag.metadata.is_some());

    let oracle = Arc::new(LspSemanticOracle::new(Arc::clone(&supervisor)));
    let ctx = ToolContext {
        session_id: "ses_lsp_bridge".to_owned(),
        directory: canon_root.clone(),
        cwd: canon_root,
        cancel: None,
    };
    let out = oc_tools::code_map::with_semantic_oracle(Some(oracle), async {
        oc_tools::CodeMapTool
            .execute(
                &ctx,
                json!({
                    "operation": "impact",
                    "query": "helper_fn",
                    "precision": "lsp"
                }),
            )
            .await
    })
    .await
    .expect("code_map impact with live LspSemanticOracle");
    let text = out.output.expect("output");
    assert!(
        text.contains("<lsp_precision") && text.contains("lsp_live_caller"),
        "expected LSP incoming call in code_map output: {text}"
    );
    assert!(
        text.contains("unresolved name `broken_sym`"),
        "expected live LSP diagnostic overlaid in <lsp_precision>: {text}"
    );

    supervisor.shutdown_all().await;
}
