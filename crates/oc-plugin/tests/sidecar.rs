//! JS sidecar bridge against the fake sidecar binary.
//!
//! No Bun/Node, network or npm is required: the bridge spawns the
//! `oc-plugin-fake-sidecar` test double, which speaks the same newline-framed
//! JSON-RPC 2.0 protocol.

use std::ffi::OsString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use oc_plugin::{
    Domain, Hook, HostServices, PluginError, PluginState, RpcError, Sidecar, SidecarConfig,
    SidecarError, SidecarStatus, detect_runtime_in,
};
use serde_json::{Value, json};

const FAKE_SIDECAR: &str = env!("CARGO_BIN_EXE_oc-plugin-fake-sidecar");

fn config(mode: &str) -> SidecarConfig {
    SidecarConfig::new(FAKE_SIDECAR)
        .with_env("FAKE_PLUGIN_SIDECAR_MODE", mode)
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5))
        .with_shutdown_grace(Duration::from_millis(500))
}

async fn start(mode: &str) -> Sidecar {
    Sidecar::start(config(mode), vec!["fake.pkg".to_string()])
        .await
        .unwrap_or_else(|error| panic!("sidecar failed to start in mode {mode}: {error}"))
}

async fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    predicate()
}

#[tokio::test]
async fn handshake_registers_plugins_and_logs() {
    let sidecar = start("ok").await;
    assert_eq!(sidecar.status(), SidecarStatus::Ready);
    assert!(sidecar.is_ready());

    let entries = sidecar.plugin_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "fake.pkg");
    assert!(entries[0].state.is_active());
    assert!(entries[0].hooks.contains(&Hook::ToolExecuteBefore));
    assert!(entries[0].transforms.contains(&Domain::Agent));
    assert!(entries[0].features.rpc);

    assert!(sidecar.has_hook(Hook::ToolExecuteBefore));
    assert!(sidecar.has_transform(Domain::Agent));
    assert!(!sidecar.has_transform(Domain::Vcs));
    assert!(sidecar.has_rpc());

    assert!(sidecar.logs().iter().any(|log| log.level == "info"));

    let inventory = sidecar.inventory();
    assert_eq!(inventory.len(), 1);
    assert!(inventory[0].is_active());
    assert_eq!(inventory[0].id, "fake.pkg");

    sidecar.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsupported_feature_is_reported_in_inventory() {
    let sidecar = start("unsupported").await;
    let inventory = sidecar.inventory();
    assert_eq!(inventory.len(), 1);
    assert_eq!(
        inventory[0].state,
        PluginState::failed("unsupported feature: aisdk.sdk")
    );
    assert!(!sidecar.has_hook(Hook::ToolExecuteBefore));
    sidecar.shutdown().await.unwrap();
}

#[tokio::test]
async fn hook_mutation_round_trips() {
    let sidecar = start("ok").await;
    let mut event = json!({"tool": "read", "input": {"path": "a.txt"}});
    sidecar
        .hook(Hook::ToolExecuteBefore, &mut event)
        .await
        .unwrap();
    assert_eq!(event["sidecar"], "fake");
    assert_eq!(event["input"]["path"], "a.txt");
    sidecar.shutdown().await.unwrap();
}

#[tokio::test]
async fn tool_before_failure_is_typed() {
    let sidecar = start("ok").await;
    let mut event = json!({"tool": "deny"});
    let error = sidecar
        .hook(Hook::ToolExecuteBefore, &mut event)
        .await
        .unwrap_err();
    match error {
        PluginError::HookFailed {
            plugin,
            hook,
            message,
            metadata,
        } => {
            assert_eq!(plugin, "fake.pkg");
            assert_eq!(hook, Hook::ToolExecuteBefore);
            assert_eq!(message, "blocked by fake plugin");
            assert_eq!(metadata, Some(json!({"policy": "deny"})));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    sidecar.shutdown().await.unwrap();
}

#[tokio::test]
async fn infallible_hook_failure_is_ignored() {
    let sidecar = start("ok").await;
    let mut event = json!({"sessionID": "ses_1", "deny": true});
    sidecar.hook(Hook::SessionPrompt, &mut event).await.unwrap();
    assert_eq!(event["sidecar"], Value::Null);
    sidecar.shutdown().await.unwrap();
}

#[tokio::test]
async fn transform_mutates_document() {
    let sidecar = start("ok").await;
    let mut doc = json!({"agents": [], "x": 1});
    sidecar.transform(Domain::Agent, &mut doc).await.unwrap();
    assert_eq!(doc["sidecar"], "fake");
    assert_eq!(doc["x"], 1);
    sidecar.shutdown().await.unwrap();
}

#[tokio::test]
async fn transform_failure_disables_plugin() {
    let sidecar = start("ok").await;
    let mut doc = json!({"fail": true});
    let error = sidecar
        .transform(Domain::Agent, &mut doc)
        .await
        .unwrap_err();
    match error {
        PluginError::TransformFailed {
            plugin,
            domain,
            message,
        } => {
            assert_eq!(plugin, "fake.pkg");
            assert_eq!(domain, Domain::Agent);
            assert_eq!(message, "transform rejected");
        }
        other => panic!("unexpected error: {other:?}"),
    }
    let inventory = sidecar.inventory();
    assert_eq!(
        inventory[0].state,
        PluginState::failed("transform rejected")
    );
    assert!(!sidecar.has_transform(Domain::Agent));
    sidecar.shutdown().await.unwrap();
}

#[tokio::test]
async fn rpc_round_trips() {
    let sidecar = start("ok").await;
    let output = sidecar
        .invoke("fake.ns", "echo", json!({"a": 1}))
        .await
        .unwrap();
    assert_eq!(output["namespace"], "fake.ns");
    assert_eq!(output["method"], "echo");
    assert_eq!(output["input"], json!({"a": 1}));
    sidecar.shutdown().await.unwrap();
}

#[tokio::test]
async fn crash_before_handshake_reports_failure() {
    let error = Sidecar::start(config("crash"), vec!["fake.pkg".to_string()])
        .await
        .unwrap_err();
    match error {
        SidecarError::Crashed { reason } => assert!(reason.contains("exited"), "{reason}"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn crash_after_handshake_is_detected() {
    let sidecar = start("crash_after_ready").await;
    assert!(
        wait_until(|| matches!(sidecar.status(), SidecarStatus::Failed(_))).await,
        "sidecar did not report the crash: {}",
        sidecar.status()
    );
    let mut event = json!({"tool": "read"});
    let error = sidecar
        .hook(Hook::ToolExecuteBefore, &mut event)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PluginError::Sidecar(RpcError::Unavailable { .. })
    ));
    assert_eq!(
        sidecar.inventory()[0].state,
        PluginState::failed("sidecar unavailable")
    );
}

#[tokio::test]
async fn startup_timeout_is_enforced() {
    let started = Instant::now();
    let error = Sidecar::start(
        config("hang").with_timeouts(Duration::from_millis(200), Duration::from_secs(5)),
        vec!["fake.pkg".to_string()],
    )
    .await
    .unwrap_err();
    assert!(matches!(error, SidecarError::StartupTimeout { .. }));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn request_timeout_is_typed() {
    let sidecar = Sidecar::start(
        config("hang_after_ready")
            .with_timeouts(Duration::from_secs(5), Duration::from_millis(200)),
        vec!["fake.pkg".to_string()],
    )
    .await
    .unwrap();
    let mut event = json!({"tool": "read"});
    let error = sidecar
        .hook(Hook::ToolExecuteBefore, &mut event)
        .await
        .unwrap_err();
    match error {
        PluginError::Sidecar(RpcError::Timeout { timeout_ms, .. }) => {
            assert_eq!(timeout_ms, 200);
        }
        other => panic!("unexpected error: {other:?}"),
    }
    sidecar.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_frame_is_tolerated_once() {
    let sidecar = start("malformed_once").await;
    assert_eq!(sidecar.protocol_errors(), 1);
    assert!(sidecar.is_ready());
    sidecar.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_frames_fail_the_sidecar() {
    let error = Sidecar::start(config("malformed"), vec!["fake.pkg".to_string()])
        .await
        .unwrap_err();
    match error {
        SidecarError::Crashed { reason } => assert!(reason.contains("malformed"), "{reason}"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn oversized_frame_fails_the_sidecar() {
    let error = Sidecar::start(config("oversize"), vec!["fake.pkg".to_string()])
        .await
        .unwrap_err();
    match error {
        SidecarError::Crashed { reason } => {
            assert!(reason.contains("exceeds 8 MiB"), "{reason}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn shutdown_stops_the_process() {
    let sidecar = start("ok").await;
    sidecar.shutdown().await.unwrap();
    assert_eq!(sidecar.status(), SidecarStatus::Stopped);
    let mut event = json!({"tool": "read"});
    let error = sidecar
        .hook(Hook::ToolExecuteBefore, &mut event)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PluginError::Sidecar(RpcError::Unavailable { .. })
    ));
    assert_eq!(
        sidecar.inventory()[0].state,
        PluginState::failed("sidecar unavailable")
    );
    // Shutting down twice is a no-op.
    sidecar.shutdown().await.unwrap();
}

#[derive(Default)]
struct Recorder {
    calls: AtomicUsize,
    last: Mutex<Option<(String, String, Value)>>,
}

#[async_trait]
impl HostServices for Recorder {
    async fn callback(
        &self,
        request_id: &str,
        method: &str,
        args: &Value,
    ) -> Result<Value, RpcError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() =
            Some((request_id.to_string(), method.to_string(), args.clone()));
        Ok(json!({"echo": "ok"}))
    }
}

#[tokio::test]
async fn callback_reaches_host_services() {
    let recorder = Arc::new(Recorder::default());
    let sidecar = Sidecar::start_with_host(
        config("callback"),
        vec!["fake.pkg".to_string()],
        Some(recorder.clone()),
    )
    .await
    .unwrap();

    assert!(
        wait_until(|| {
            sidecar
                .logs()
                .iter()
                .any(|log| log.message == "callback.ok")
        })
        .await,
        "callback was not answered: {:?}",
        sidecar.logs()
    );

    assert_eq!(recorder.calls.load(Ordering::SeqCst), 1);
    let (request_id, method, args) = recorder.last.lock().unwrap().clone().unwrap();
    assert_eq!(request_id, "r1");
    assert_eq!(method, "session.read");
    assert_eq!(args, json!({"sessionID": "ses_fake"}));
    sidecar.shutdown().await.unwrap();
}

#[test]
fn runtime_detection_is_path_based() {
    let empty = std::env::temp_dir().join("oc-plugin-missing-runtime-path");
    assert!(detect_runtime_in(&OsString::from(empty)).is_none());
    let path = std::env::var_os("PATH").unwrap_or_default();
    // Any found runtime must actually exist on disk.
    if let Some(runtime) = detect_runtime_in(&path) {
        assert!(runtime.is_file());
    }
}

#[test]
fn config_reads_plugin_host_command() {
    let config = SidecarConfig::from_config(&json!({
        "plugin_host": {"command": ["bun", "--smol"], "cwd": "/tmp/host"}
    }));
    assert_eq!(config.command.as_deref(), Some("bun"));
    assert_eq!(config.args, vec!["--smol".to_string()]);
    assert_eq!(config.cwd.unwrap().to_string_lossy(), "/tmp/host");
    assert_eq!(config.startup_timeout, Duration::from_secs(5));
    assert_eq!(config.request_timeout, Duration::from_secs(5));

    let string_form = SidecarConfig::from_config(&json!({
        "plugin_host": {"command": "node", "args": ["shim.js"]}
    }));
    assert_eq!(string_form.command.as_deref(), Some("node"));
    assert_eq!(string_form.args, vec!["shim.js".to_string()]);

    let absent = SidecarConfig::from_config(&json!({}));
    assert!(absent.command.is_none());
}
