//! End-to-end tests against the `fake-lsp` helper binary.
//!
//! No network, no installed language servers: every client is the helper built
//! by Cargo (`CARGO_BIN_EXE_fake-lsp`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oc_lsp::{
    LspConfig, LspDiagnosticsEvent, LspError, LspEvent, LspSupervisor, LspToolInput,
    SupervisorOptions,
};
use serde_json::json;

const FAKE_LSP: &str = env!("CARGO_BIN_EXE_fake-lsp");

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    trace: PathBuf,
    marker: PathBuf,
    file: PathBuf,
    mode: &'static str,
}

impl Fixture {
    fn new(mode: &'static str) -> Self {
        let directory = tempfile::tempdir().expect("temp dir");
        let root = std::fs::canonicalize(directory.path()).expect("canonical root");
        let file = root.join("sample.fake");
        std::fs::write(&file, "fn fake_symbol() {}\nlet other = 1;\n").expect("write fixture");
        let trace = root.join("fake-trace.log");
        let marker = root.join("fake-exit.marker");
        Self {
            _directory: directory,
            root,
            trace,
            marker,
            file,
            mode,
        }
    }

    fn options() -> SupervisorOptions {
        SupervisorOptions {
            max_servers: 4,
            slot_budget: 6,
            init_timeout: Duration::from_secs(5),
            idle_ttl: Duration::from_secs(600),
            sweep_interval: Duration::from_secs(60),
            backoff: [
                Duration::from_millis(120),
                Duration::from_millis(240),
                Duration::from_millis(480),
            ],
            request_timeout: Duration::from_secs(5),
            diagnostics_wait: Duration::from_secs(3),
            start_sweeper: false,
        }
    }

    fn config(&self) -> LspConfig {
        LspConfig::from_config(&json!({
            "lsp": {
                "servers": {
                    "fake": {
                        "command": [FAKE_LSP],
                        "extensions": [".fake"],
                        "env": {
                            "FAKE_LSP_MODE": self.mode,
                            "FAKE_LSP_TRACE": self.trace.to_string_lossy(),
                            "FAKE_LSP_EXIT_MARKER": self.marker.to_string_lossy()
                        },
                        "initialization": { "fake": { "enabled": true } }
                    }
                }
            },
            "lsp_install": false
        }))
    }

    fn supervisor(&self) -> LspSupervisor {
        LspSupervisor::with_options(self.config(), &self.root, Self::options())
    }

    fn trace(&self) -> String {
        std::fs::read_to_string(&self.trace).unwrap_or_default()
    }

    async fn wait_trace(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let text = self.trace();
            if text.contains(needle) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "trace did not contain {needle:?}; trace:\n{text}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn tool_input(operation: &str, file: &Path) -> LspToolInput {
    LspToolInput::from_value(json!({
        "operation": operation,
        "filePath": file.to_string_lossy(),
        "line": 1,
        "character": 1
    }))
    .expect("tool input")
}

#[tokio::test]
async fn initialize_handshake_answers_server_requests() {
    let fixture = Fixture::new("normal");
    let supervisor = fixture.supervisor();
    let client = supervisor
        .client_for_file(&fixture.file)
        .await
        .expect("client spawns");

    assert!(client.capabilities()["hoverProvider"].as_bool().unwrap());
    assert!(client.supports_pull_diagnostics());

    let trace = fixture
        .wait_trace("response folders", Duration::from_secs(5))
        .await;
    for expected in [
        "notification initialized",
        "notification workspace/didChangeConfiguration",
        "send workspace/configuration",
        "response configuration result=[{\"enabled\":true}]",
        "response progress",
        "response register",
    ] {
        assert!(trace.contains(expected), "missing {expected:?} in\n{trace}");
    }

    let statuses = supervisor.statuses().await;
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].id, "fake");
    assert_eq!(statuses[0].status, "connected");
    assert_eq!(statuses[0].root, fixture.root.to_string_lossy());

    supervisor.shutdown_all().await;
    assert!(client.is_exited(), "child must be reaped");
    let marker = std::fs::read_to_string(&fixture.marker).expect("exit marker");
    assert_eq!(
        marker.trim().parse::<u32>().expect("pid in marker"),
        client.pid().expect("child pid")
    );
}

#[tokio::test]
async fn diagnostics_push_and_pull_are_cached_and_emitted() {
    let fixture = Fixture::new("normal");
    let supervisor = fixture.supervisor();
    let mut events = supervisor.subscribe();

    let client = supervisor
        .client_for_file(&fixture.file)
        .await
        .expect("client spawns");
    assert!(client.supports_pull_diagnostics());

    let cached = client
        .touch_file(&fixture.file, true)
        .await
        .expect("touch")
        .expect("diagnostics");
    assert_eq!(cached.version, Some(0));
    assert!(!cached.errors().is_empty(), "severity-1 error expected");
    assert!(cached.merged().len() >= 2);

    let event = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(LspEvent::Diagnostics(LspDiagnosticsEvent { server_id, path })) =
                events.recv().await
            {
                return (server_id, path);
            }
        }
    })
    .await
    .expect("diagnostics event");
    assert_eq!(event.0, "fake");
    assert_eq!(
        PathBuf::from(event.1),
        std::fs::canonicalize(&fixture.file).unwrap()
    );

    // A second touch sends didChange with an incremented version.
    let second = client
        .touch_file(&fixture.file, true)
        .await
        .expect("touch")
        .expect("diagnostics");
    assert_eq!(second.version, Some(1));
    let trace = fixture
        .wait_trace("\"version\":1", Duration::from_secs(3))
        .await;
    assert!(trace.contains("request textDocument/diagnostic"));

    let diagnostics = supervisor
        .execute_diagnostics_tool(&fixture.file)
        .await
        .expect("diagnostics tool");
    assert!(
        diagnostics
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );

    supervisor.shutdown_all().await;
    assert!(client.is_exited(), "child must be reaped");
}

#[tokio::test]
async fn navigation_operations_return_json() {
    let fixture = Fixture::new("normal");
    let supervisor = fixture.supervisor();

    let hover = supervisor
        .execute_tool(&tool_input("hover", &fixture.file))
        .await
        .expect("hover");
    assert_eq!(hover["contents"]["value"], "hover 0:0");

    let definition = supervisor
        .execute_tool(&tool_input("goToDefinition", &fixture.file))
        .await
        .expect("definition");
    let locations = definition.as_array().expect("location array");
    assert_eq!(locations.len(), 1);
    assert!(
        locations[0]["uri"]
            .as_str()
            .unwrap()
            .starts_with("file:///")
    );

    let references = supervisor
        .execute_tool(&tool_input("findReferences", &fixture.file))
        .await
        .expect("references");
    assert_eq!(references.as_array().unwrap().len(), 2);

    let symbols = supervisor
        .execute_tool(&tool_input("documentSymbol", &fixture.file))
        .await
        .expect("document symbols");
    assert_eq!(symbols.as_array().unwrap().len(), 1);
    assert_eq!(symbols[0]["name"], "fake_symbol");

    let implementation = supervisor
        .execute_tool(&tool_input("goToImplementation", &fixture.file))
        .await
        .expect("implementation");
    assert_eq!(implementation.as_array().unwrap().len(), 1);

    let prepared = supervisor
        .execute_tool(&tool_input("prepareCallHierarchy", &fixture.file))
        .await
        .expect("prepare");
    assert_eq!(prepared.as_array().unwrap().len(), 1);

    let incoming = supervisor
        .execute_tool(&tool_input("incomingCalls", &fixture.file))
        .await
        .expect("incoming");
    assert_eq!(incoming.as_array().unwrap().len(), 1);

    let outgoing = supervisor
        .execute_tool(&tool_input("outgoingCalls", &fixture.file))
        .await
        .expect("outgoing");
    assert_eq!(outgoing.as_array().unwrap().len(), 1);

    let workspace = supervisor
        .execute_tool(
            &LspToolInput::from_value(json!({
                "operation": "workspaceSymbol",
                "filePath": fixture.file.to_string_lossy(),
                "query": "abc"
            }))
            .unwrap(),
        )
        .await
        .expect("workspace symbols");
    assert_eq!(workspace["truncated"], true);
    assert_eq!(
        workspace["symbols"].as_array().unwrap().len(),
        oc_lsp::WORKSPACE_SYMBOL_LIMIT
    );
    assert_eq!(workspace["symbols"][0]["name"], "abc_0");

    supervisor.shutdown_all().await;
    assert!(fixture.marker.exists(), "clean exit marker expected");
}

#[tokio::test]
async fn missing_server_and_unsupported_operations_have_exact_errors() {
    let fixture = Fixture::new("normal");
    let supervisor = fixture.supervisor();
    let unknown = fixture.root.join("notes.zzz");
    std::fs::write(&unknown, "hello").unwrap();

    let error = supervisor
        .execute_tool(&tool_input("hover", &unknown))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("no LSP server available for file: {}", unknown.display())
    );

    let error = supervisor
        .execute_tool(&tool_input("rename", &fixture.file))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "unsupported operation: rename");

    let config = LspConfig::from_config(&json!({"lsp": false}));
    let disabled = LspSupervisor::with_options(config, &fixture.root, Fixture::options());
    let error = disabled
        .execute_tool(&tool_input("hover", &fixture.file))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "LSP is disabled");
}

#[tokio::test]
async fn crashed_server_is_in_backoff_before_retrying() {
    let fixture = Fixture::new("crash");
    let supervisor = fixture.supervisor();

    let first = supervisor.client_for_file(&fixture.file).await;
    assert!(first.is_err(), "crashing server must fail");
    assert!(
        !matches!(first, Err(LspError::Backoff { .. })),
        "first failure is the spawn failure, not backoff"
    );

    let second = supervisor.client_for_file(&fixture.file).await;
    match second {
        Err(LspError::Backoff { failures, .. }) => assert_eq!(failures, 1),
        other => panic!("expected backoff, got {other:?}"),
    }

    // After the short backoff the server is attempted again.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let third = supervisor.client_for_file(&fixture.file).await;
    assert!(third.is_err());
    assert!(
        !matches!(third, Err(LspError::Backoff { .. })),
        "backoff expired, a fresh attempt should run"
    );

    let statuses = supervisor.statuses().await;
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].status, "error");

    supervisor.shutdown_all().await;
}

#[tokio::test]
async fn shutdown_reaps_a_server_that_ignores_exit() {
    let fixture = Fixture::new("stubborn-exit");
    let supervisor = fixture.supervisor();
    let client = supervisor
        .client_for_file(&fixture.file)
        .await
        .expect("client spawns");

    let started = Instant::now();
    supervisor.shutdown_all().await;
    assert!(client.is_exited(), "killed child must be reaped");
    assert!(
        !fixture.marker.exists(),
        "server ignoring exit must not write the clean-exit marker"
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "shutdown must not hang"
    );
}

#[tokio::test]
async fn initialize_timeout_is_reported_and_child_is_killed() {
    let fixture = Fixture::new("hang");
    let mut options = Fixture::options();
    options.init_timeout = Duration::from_millis(200);
    let supervisor = LspSupervisor::with_options(fixture.config(), &fixture.root, options);

    let error = supervisor.client_for_file(&fixture.file).await.unwrap_err();
    assert!(
        matches!(error, LspError::Timeout { .. }),
        "expected initialize timeout, got {error}"
    );

    let statuses = supervisor.statuses().await;
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].status, "error");
    supervisor.shutdown_all().await;
}

#[tokio::test]
async fn concurrent_callers_share_one_spawn() {
    let fixture = Fixture::new("normal");
    let supervisor = fixture.supervisor();

    let (first, second) = tokio::join!(
        supervisor.client_for_file(&fixture.file),
        supervisor.client_for_file(&fixture.file)
    );
    let first = first.expect("first client");
    let second = second.expect("second client");
    assert_eq!(first.pid(), second.pid(), "one child per key");

    let trace = fixture
        .wait_trace("notification initialized", Duration::from_secs(5))
        .await;
    assert_eq!(
        trace.matches("request initialize ").count(),
        1,
        "initialize must be sent once; trace:\n{trace}"
    );

    supervisor.shutdown_all().await;
    assert!(first.is_exited());
}

#[tokio::test]
async fn large_files_are_not_synchronized() {
    let fixture = Fixture::new("normal");
    let supervisor = fixture.supervisor();
    let client = supervisor
        .client_for_file(&fixture.file)
        .await
        .expect("client spawns");

    let large = fixture.root.join("big.fake");
    std::fs::write(&large, vec![b'x'; 2 * 1024 * 1024 + 16]).expect("large file");
    let result = client.touch_file(&large, false).await.expect("touch");
    assert!(result.is_none(), "large files are skipped");

    let trace = fixture.trace();
    assert!(
        !trace.contains("big.fake"),
        "no didOpen for oversized files; trace:\n{trace}"
    );

    supervisor.shutdown_all().await;
    assert!(client.is_exited(), "child must be reaped after shutdown");
}

#[tokio::test]
async fn idle_clients_are_swept() {
    let fixture = Fixture::new("normal");
    let mut options = Fixture::options();
    options.idle_ttl = Duration::from_millis(100);
    options.sweep_interval = Duration::from_millis(25);
    options.start_sweeper = true;
    let supervisor = LspSupervisor::with_options(fixture.config(), &fixture.root, options);

    let client = supervisor
        .client_for_file(&fixture.file)
        .await
        .expect("client spawns");
    assert_eq!(supervisor.statuses().await.len(), 1);

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        supervisor.statuses().await.is_empty(),
        "idle client should be evicted"
    );
    assert!(client.is_exited(), "swept child must be reaped");
}
