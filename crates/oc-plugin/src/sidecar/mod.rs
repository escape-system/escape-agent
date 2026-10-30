//! JS plugin sidecar bridge.
//!
//! One sidecar per server process, spawned lazily by the caller (the server
//! starts it only when `plugins` is non-empty or a plugin directory exists).
//! Transport is newline-framed JSON-RPC 2.0 over the child's stdin/stdout;
//! frames and envelopes are documented in [`protocol`].
//!
//! Lifecycle:
//!
//! - spawn (`plugin_host.command`, else `bun`, else `node`) → `load` with the
//!   configured targets, bounded by [`SidecarConfig::startup_timeout`] (5 s by
//!   default);
//! - every request (`hook`, `transform`, `rpc`, `reload`, `dispose`) is bounded
//!   by [`SidecarConfig::request_timeout`] (5 s by default);
//! - reader EOF or a write error marks the sidecar failed, fails pending
//!   requests and kills the process tree;
//! - `shutdown` sends `plugin.shutdown`, waits [`SidecarConfig::shutdown_grace`]
//!   (2 s) and then kills the process tree.
//!
//! The 90 MB RSS budget and crash-restart backoff from `docs/10-plugins.md` are
//! owned by the supervisor that calls this bridge; the bridge only detects and
//! reports the crash. No live Effect/Bun objects cross the boundary.

pub mod protocol;
mod runtime;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::domain::Domain;
use crate::error::PluginError;
use crate::hook::{Hook, HookEvent, TransformDoc};
use crate::info::{PluginFeatures, PluginInfo, PluginSource, PluginState};
use crate::rpc::{RpcError, RpcErrorObject};

pub use runtime::{detect_runtime, detect_runtime_in};

use protocol::{CallbackParams, Exchange, Incoming, LogParams, RegisterParams, UpdateParams};

/// Malformed frames tolerated before the sidecar is considered failed.
const MAX_PROTOCOL_ERRORS: u32 = 3;
/// Log lines retained for the inventory/debug endpoint.
const MAX_LOGS: usize = 256;

/// Spawn and deadline configuration for the sidecar.
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    /// Explicit runtime command (`plugin_host.command`). `None` discovers
    /// `bun`, then `node`, on PATH.
    pub command: Option<String>,
    /// Extra arguments passed to the runtime before the sidecar script.
    pub args: Vec<String>,
    /// Working directory of the sidecar.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Load/handshake deadline. Default 5 s.
    pub startup_timeout: Duration,
    /// Per-request deadline, including callbacks into host services. Default 5 s.
    pub request_timeout: Duration,
    /// Grace period between `plugin.shutdown` and the kill. Default 2 s.
    pub shutdown_grace: Duration,
}

impl Default for SidecarConfig {
    fn default() -> Self {
        Self {
            command: None,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            shutdown_grace: Duration::from_secs(2),
        }
    }
}

impl SidecarConfig {
    /// Config with an explicit runtime command.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: Some(command.into()),
            ..Self::default()
        }
    }

    /// Read `plugin_host` from the merged config document.
    ///
    /// `plugin_host.command` is a string, or an array whose first element is the
    /// command and whose remainder are arguments. `plugin_host.args`,
    /// `plugin_host.cwd` are accepted too. Unrecognised members are ignored so a
    /// newer config never breaks startup.
    #[must_use]
    pub fn from_config(config: &Value) -> Self {
        let mut out = Self::default();
        let Some(host) = config.get("plugin_host").and_then(Value::as_object) else {
            return out;
        };
        match host.get("command") {
            Some(Value::String(command)) if !command.trim().is_empty() => {
                out.command = Some(command.clone());
            }
            Some(Value::Array(items)) => {
                let mut parts = items.iter().filter_map(Value::as_str);
                if let Some(command) = parts.next() {
                    out.command = Some(command.to_string());
                    out.args = parts.map(str::to_string).collect();
                }
            }
            _ => {}
        }
        if let Some(args) = host.get("args").and_then(Value::as_array) {
            out.args = args
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        if let Some(cwd) = host.get("cwd").and_then(Value::as_str) {
            out.cwd = Some(PathBuf::from(cwd));
        }
        out
    }

    /// Append runtime arguments.
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.args.extend(args);
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Add one environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Override the startup/request deadlines (tests use short ones).
    #[must_use]
    pub fn with_timeouts(mut self, startup: Duration, request: Duration) -> Self {
        self.startup_timeout = startup;
        self.request_timeout = request;
        self
    }

    /// Override the shutdown grace period.
    #[must_use]
    pub fn with_shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }
}

/// Sidecar lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarStatus {
    /// Spawned; `load` handshake in flight.
    Starting,
    /// Handshake completed; requests are served.
    Ready,
    /// Unusable, with the reason (crash, malformed protocol, spawn failure).
    Failed(String),
    /// Shut down on purpose.
    Stopped,
}

impl SidecarStatus {
    /// Whether requests can be served.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Failure reason, if failed.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Failed(reason) => Some(reason),
            _ => None,
        }
    }
}

impl fmt::Display for SidecarStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Starting => formatter.write_str("starting"),
            Self::Ready => formatter.write_str("ready"),
            Self::Failed(reason) => write!(formatter, "failed: {reason}"),
            Self::Stopped => formatter.write_str("stopped"),
        }
    }
}

/// One `log` notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarLog {
    /// `debug`, `info`, `warn` or `error`.
    pub level: String,
    /// Message text.
    pub message: String,
    /// Owning plugin, when the sidecar reported one.
    pub plugin: Option<String>,
}

/// A plugin announced by the sidecar `register` notification.
#[derive(Debug, Clone, PartialEq)]
pub struct SidecarPlugin {
    /// Plugin id.
    pub id: String,
    /// Declared entrypoints.
    pub features: PluginFeatures,
    /// Supported hooks.
    pub hooks: Vec<Hook>,
    /// Supported transform domains.
    pub transforms: Vec<Domain>,
    /// Current state (`active`, or `failed` with the reason).
    pub state: PluginState,
    /// Source reported by the sidecar.
    pub source: PluginSource,
}

/// Host services a plugin may call while a hook runs.
///
/// The sidecar correlates calls with `requestId`; the bridge enforces the
/// request deadline and returns a JSON-RPC error on timeout or absence.
#[async_trait]
pub trait HostServices: Send + Sync {
    /// Handle one callback (session read, permission, storage, event subscribe).
    async fn callback(
        &self,
        request_id: &str,
        method: &str,
        args: &Value,
    ) -> Result<Value, RpcError>;
}

struct Inner {
    config: SidecarConfig,
    command: String,
    status: Mutex<SidecarStatus>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Exchange, RpcError>>>>,
    outbound: mpsc::UnboundedSender<String>,
    child: tokio::sync::Mutex<Option<Child>>,
    plugins: Mutex<Vec<SidecarPlugin>>,
    logs: Mutex<VecDeque<SidecarLog>>,
    host: RwLock<Option<Arc<dyn HostServices>>>,
    protocol_errors: AtomicU32,
}

impl Inner {
    fn push_log(&self, log: SidecarLog) {
        let mut logs = self.logs.lock();
        if logs.len() >= MAX_LOGS {
            logs.pop_front();
        }
        logs.push_back(log);
    }

    fn drain_pending(&self, error: &RpcError) {
        let senders: Vec<_> = self
            .pending
            .lock()
            .drain()
            .map(|(_, sender)| sender)
            .collect();
        for sender in senders {
            let _ = sender.send(Err(error.clone()));
        }
    }

    fn fail(self: &Arc<Self>, reason: &str) {
        {
            let mut status = self.status.lock();
            if matches!(*status, SidecarStatus::Failed(_) | SidecarStatus::Stopped) {
                return;
            }
            *status = SidecarStatus::Failed(reason.to_string());
        }
        warn!(target: "oc_plugin::sidecar", reason, "plugin sidecar failed");
        self.drain_pending(&RpcError::Unavailable {
            message: reason.to_string(),
        });
        let inner = Arc::clone(self);
        tokio::spawn(async move { inner.kill_child().await });
    }

    fn protocol_error(self: &Arc<Self>, message: &str) {
        let count = self.protocol_errors.fetch_add(1, Ordering::Relaxed) + 1;
        warn!(
            target: "oc_plugin::sidecar",
            message,
            count,
            "malformed sidecar frame"
        );
        if count >= MAX_PROTOCOL_ERRORS {
            self.fail(&format!("malformed frame: {message}"));
        }
    }

    async fn on_eof(self: &Arc<Self>) {
        if matches!(*self.status.lock(), SidecarStatus::Stopped) {
            return;
        }
        let code = {
            let mut guard = self.child.lock().await;
            guard
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
                .and_then(|status| status.code())
        };
        let reason = code.map_or_else(
            || "sidecar exited unexpectedly".to_string(),
            |code| format!("sidecar exited unexpectedly with code {code}"),
        );
        self.fail(&reason);
    }

    async fn kill_child(&self) {
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            let _ = tokio::time::timeout(Duration::from_secs(2), child.kill()).await;
        }
    }

    async fn stop(&self) {
        {
            *self.status.lock() = SidecarStatus::Stopped;
        }
        self.drain_pending(&RpcError::Unavailable {
            message: "sidecar stopped".to_string(),
        });
        let grace = self.config.shutdown_grace;
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.start_kill();
            if tokio::time::timeout(grace, child.wait()).await.is_err() {
                let _ = tokio::time::timeout(Duration::from_secs(2), child.kill()).await;
            }
        }
    }

    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Exchange, RpcError> {
        match &*self.status.lock() {
            SidecarStatus::Failed(reason) => {
                return Err(RpcError::Unavailable {
                    message: reason.clone(),
                });
            }
            SidecarStatus::Stopped => {
                return Err(RpcError::Unavailable {
                    message: "sidecar stopped".to_string(),
                });
            }
            SidecarStatus::Starting | SidecarStatus::Ready => {}
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().insert(id, sender);
        let frame = protocol::request_frame(id, method, params.as_ref());
        if self.outbound.send(frame).is_err() {
            self.pending.lock().remove(&id);
            return Err(RpcError::Unavailable {
                message: "sidecar output closed".to_string(),
            });
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => {
                self.pending.lock().remove(&id);
                Err(RpcError::Unavailable {
                    message: "sidecar closed the request".to_string(),
                })
            }
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(RpcError::Timeout {
                    method: method.to_string(),
                    timeout_ms: millis(timeout),
                })
            }
        }
    }
}

/// Handle to the JS plugin sidecar.
pub struct Sidecar {
    inner: Arc<Inner>,
}

impl fmt::Debug for Sidecar {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Sidecar")
            .field("command", &self.inner.command)
            .field("status", &self.status())
            .field("plugins", &self.inner.plugins.lock().len())
            .finish()
    }
}

impl Sidecar {
    /// Spawn the sidecar and complete the `load` handshake.
    ///
    /// # Errors
    ///
    /// [`SidecarError::RuntimeUnavailable`] when no command is configured and
    /// neither `bun` nor `node` exists, [`SidecarError::Spawn`] when the process
    /// cannot start, [`SidecarError::StartupTimeout`] when the handshake
    /// exceeds the deadline, [`SidecarError::Crashed`] when the process dies or
    /// floods malformed frames, [`SidecarError::Load`] when the sidecar rejects
    /// the targets.
    pub async fn start(config: SidecarConfig, targets: Vec<String>) -> Result<Self, SidecarError> {
        Self::start_with_host(config, targets, None).await
    }

    /// Like [`Sidecar::start`], but installs the host service before the
    /// handshake so plugins may call back while loading.
    ///
    /// # Errors
    ///
    /// See [`Sidecar::start`].
    pub async fn start_with_host(
        config: SidecarConfig,
        targets: Vec<String>,
        host: Option<Arc<dyn HostServices>>,
    ) -> Result<Self, SidecarError> {
        let command = match config.command.as_deref() {
            Some(command) if !command.trim().is_empty() => PathBuf::from(command),
            _ => detect_runtime().ok_or(SidecarError::RuntimeUnavailable)?,
        };
        let mut builder = Command::new(&command);
        builder
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            builder.current_dir(cwd);
        }
        for (key, value) in &config.env {
            builder.env(key, value);
        }
        let mut child = builder.spawn().map_err(|error| SidecarError::Spawn {
            command: command.display().to_string(),
            message: error.to_string(),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| SidecarError::Spawn {
            command: command.display().to_string(),
            message: "stdin pipe unavailable".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| SidecarError::Spawn {
            command: command.display().to_string(),
            message: "stdout pipe unavailable".to_string(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| SidecarError::Spawn {
            command: command.display().to_string(),
            message: "stderr pipe unavailable".to_string(),
        })?;

        let (outbound, outbound_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            config,
            command: command.display().to_string(),
            status: Mutex::new(SidecarStatus::Starting),
            next_id: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            outbound,
            child: tokio::sync::Mutex::new(Some(child)),
            plugins: Mutex::new(Vec::new()),
            logs: Mutex::new(VecDeque::new()),
            host: RwLock::new(host),
            protocol_errors: AtomicU32::new(0),
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(write_loop(stdin, outbound_rx, weak.clone()));
        tokio::spawn(stderr_loop(stderr));
        tokio::spawn(read_loop(stdout, weak));

        let sidecar = Self { inner };
        match sidecar.load(&targets).await {
            Ok(()) => {
                info!(
                    target: "oc_plugin::sidecar",
                    command = %sidecar.inner.command,
                    plugins = sidecar.plugin_entries().len(),
                    "plugin sidecar ready"
                );
                Ok(sidecar)
            }
            Err(error) => {
                sidecar.inner.kill_child().await;
                Err(error)
            }
        }
    }

    /// Runtime command used by this sidecar.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.inner.command
    }

    /// Current lifecycle state.
    #[must_use]
    pub fn status(&self) -> SidecarStatus {
        self.inner.status.lock().clone()
    }

    /// Whether the handshake completed and requests are served.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status().is_ready()
    }

    /// Registered plugins, in registration order.
    #[must_use]
    pub fn plugin_entries(&self) -> Vec<SidecarPlugin> {
        self.inner.plugins.lock().clone()
    }

    /// Log notifications received so far (bounded to 256).
    #[must_use]
    pub fn logs(&self) -> Vec<SidecarLog> {
        self.inner.logs.lock().iter().cloned().collect()
    }

    /// Number of malformed frames seen.
    #[must_use]
    pub fn protocol_errors(&self) -> u32 {
        self.inner.protocol_errors.load(Ordering::Relaxed)
    }

    /// Install the host service used by plugin callbacks. Replaces any previous
    /// one; callbacks are rejected until a host is installed.
    pub fn set_host(&self, host: Arc<dyn HostServices>) {
        *self.inner.host.write() = Some(host);
    }

    /// Whether any active plugin handles `hook`.
    #[must_use]
    pub fn has_hook(&self, hook: Hook) -> bool {
        self.inner
            .plugins
            .lock()
            .iter()
            .any(|plugin| plugin.state.is_active() && plugin.hooks.contains(&hook))
    }

    /// Whether any active plugin transforms `domain`.
    #[must_use]
    pub fn has_transform(&self, domain: Domain) -> bool {
        self.inner
            .plugins
            .lock()
            .iter()
            .any(|plugin| plugin.state.is_active() && plugin.transforms.contains(&domain))
    }

    /// Whether any active plugin exposes RPC.
    #[must_use]
    pub fn has_rpc(&self) -> bool {
        self.inner
            .plugins
            .lock()
            .iter()
            .any(|plugin| plugin.state.is_active() && plugin.features.rpc)
    }

    /// Inventory rows for the JS plugins.
    ///
    /// While the sidecar is failed or stopped, every row reports
    /// `failed (sidecar unavailable)` as required by the protocol.
    #[must_use]
    pub fn inventory(&self) -> Vec<PluginInfo> {
        let unavailable = !matches!(
            self.status(),
            SidecarStatus::Starting | SidecarStatus::Ready
        );
        self.inner
            .plugins
            .lock()
            .iter()
            .map(|plugin| PluginInfo {
                id: plugin.id.clone(),
                source: plugin.source.clone(),
                features: plugin.features,
                state: if unavailable {
                    PluginState::failed("sidecar unavailable")
                } else {
                    plugin.state.clone()
                },
            })
            .collect()
    }

    /// Load plugin targets. Used by the initial handshake; a supervisor that
    /// reuses a warm sidecar calls [`Sidecar::reload`] instead.
    ///
    /// # Errors
    ///
    /// See [`Sidecar::start`].
    pub async fn load(&self, targets: &[String]) -> Result<(), SidecarError> {
        let params = json!({ "targets": targets });
        match self
            .inner
            .send_request(
                protocol::METHOD_LOAD,
                Some(params),
                self.inner.config.startup_timeout,
            )
            .await
        {
            Ok(Exchange::Ok(_)) => {
                let mut status = self.inner.status.lock();
                if matches!(*status, SidecarStatus::Starting) {
                    *status = SidecarStatus::Ready;
                }
                Ok(())
            }
            Ok(Exchange::Error(error)) => Err(SidecarError::Load {
                code: error.code,
                message: error.message,
            }),
            Err(RpcError::Timeout { .. }) => Err(SidecarError::StartupTimeout {
                timeout_ms: millis(self.inner.config.startup_timeout),
            }),
            Err(error) => Err(SidecarError::Crashed {
                reason: error.to_string(),
            }),
        }
    }

    /// Reload the given plugin ids (hot reload keeps the previous generation
    /// when the sidecar rejects the new one).
    ///
    /// # Errors
    ///
    /// Transport failures and sidecar rejections.
    pub async fn reload(&self, ids: &[String]) -> Result<(), RpcError> {
        let params = json!({ "ids": ids });
        match self
            .inner
            .send_request(
                protocol::METHOD_RELOAD,
                Some(params),
                self.inner.config.request_timeout,
            )
            .await?
        {
            Exchange::Ok(_) => Ok(()),
            Exchange::Error(error) => Err(rpc_error_from_object(error, "reload")),
        }
    }

    /// Dispose one plugin.
    ///
    /// # Errors
    ///
    /// Transport failures and sidecar rejections.
    pub async fn dispose(&self, id: &str) -> Result<(), RpcError> {
        let params = json!({ "id": id });
        match self
            .inner
            .send_request(
                protocol::METHOD_DISPOSE,
                Some(params),
                self.inner.config.request_timeout,
            )
            .await?
        {
            Exchange::Ok(_) => Ok(()),
            Exchange::Error(error) => Err(rpc_error_from_object(error, "dispose")),
        }
    }

    /// Dispatch a hook through the sidecar.
    ///
    /// Infallible hooks swallow sidecar errors; `tool.execute.before` maps them
    /// to [`PluginError::HookFailed`] so the runtime rejects the tool call.
    ///
    /// # Errors
    ///
    /// Transport failures ([`PluginError::Sidecar`]) and, for
    /// `tool.execute.before`, a plugin rejection.
    pub async fn hook(&self, hook: Hook, event: &mut HookEvent) -> Result<(), PluginError> {
        if !self.has_hook(hook) {
            return Ok(());
        }
        let params = json!({ "hook": hook.as_str(), "event": &*event });
        match self
            .inner
            .send_request(
                protocol::METHOD_HOOK,
                Some(params),
                self.inner.config.request_timeout,
            )
            .await
        {
            Ok(Exchange::Ok(value)) => {
                if !value.is_null() {
                    *event = value;
                }
                Ok(())
            }
            Ok(Exchange::Error(error)) => {
                if hook.can_fail() {
                    let plugin = error
                        .data
                        .as_ref()
                        .and_then(|data| data.get("plugin"))
                        .and_then(Value::as_str)
                        .unwrap_or("sidecar")
                        .to_string();
                    let metadata = error
                        .data
                        .as_ref()
                        .and_then(|data| data.get("metadata"))
                        .cloned();
                    Err(PluginError::HookFailed {
                        plugin,
                        hook,
                        message: error.message,
                        metadata,
                    })
                } else {
                    warn!(
                        target: "oc_plugin::sidecar",
                        hook = %hook,
                        error = %error.message,
                        "ignoring infallible hook error"
                    );
                    Ok(())
                }
            }
            Err(error) => Err(PluginError::Sidecar(error)),
        }
    }

    /// Dispatch a transform through the sidecar. A failure disables every
    /// plugin that declared the domain (or the one the sidecar named) and
    /// returns [`PluginError::TransformFailed`].
    ///
    /// # Errors
    ///
    /// Transport failures and transform rejections.
    pub async fn transform(
        &self,
        domain: Domain,
        doc: &mut TransformDoc,
    ) -> Result<(), PluginError> {
        if !self.has_transform(domain) {
            return Ok(());
        }
        let params = json!({ "domain": domain.as_str(), "doc": &*doc });
        match self
            .inner
            .send_request(
                protocol::METHOD_TRANSFORM,
                Some(params),
                self.inner.config.request_timeout,
            )
            .await
        {
            Ok(Exchange::Ok(value)) => {
                if !value.is_null() {
                    *doc = value;
                }
                Ok(())
            }
            Ok(Exchange::Error(error)) => {
                let plugin = self.disable_transformers(domain, &error);
                Err(PluginError::TransformFailed {
                    plugin,
                    domain,
                    message: error.message,
                })
            }
            Err(error) => Err(PluginError::Sidecar(error)),
        }
    }

    /// Dispatch an RPC call to the sidecar.
    ///
    /// # Errors
    ///
    /// [`RpcError::NamespaceNotFound`] when no active plugin exposes RPC, plus
    /// the sidecar's typed error mapping.
    pub async fn invoke(
        &self,
        namespace: &str,
        method: &str,
        input: Value,
    ) -> Result<Value, RpcError> {
        if !self.has_rpc() {
            return Err(RpcError::NamespaceNotFound {
                namespace: namespace.to_string(),
            });
        }
        let params = json!({ "namespace": namespace, "method": method, "input": input });
        match self
            .inner
            .send_request(
                protocol::METHOD_RPC,
                Some(params),
                self.inner.config.request_timeout,
            )
            .await?
        {
            Exchange::Ok(value) => Ok(value),
            Exchange::Error(error) => Err(rpc_error_from_object(error, method)),
        }
    }

    /// Graceful shutdown: `plugin.shutdown`, wait the grace period, then kill
    /// the process tree. Safe to call more than once.
    ///
    /// # Errors
    ///
    /// Currently infallible; the signature is kept so a future implementation
    /// can report a failure to reap the child.
    pub async fn shutdown(&self) -> Result<(), SidecarError> {
        if matches!(self.status(), SidecarStatus::Stopped) {
            return Ok(());
        }
        if self.status().is_ready() {
            let _ = self
                .inner
                .send_request(
                    protocol::METHOD_SHUTDOWN,
                    None,
                    self.inner.config.shutdown_grace,
                )
                .await;
        }
        self.inner.stop().await;
        Ok(())
    }

    fn disable_transformers(&self, domain: Domain, error: &RpcErrorObject) -> String {
        let named = error
            .data
            .as_ref()
            .and_then(|data| data.get("plugin"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut plugins = self.inner.plugins.lock();
        let mut disabled = Vec::new();
        for plugin in plugins.iter_mut() {
            if !plugin.state.is_active() || !plugin.transforms.contains(&domain) {
                continue;
            }
            if let Some(named) = &named
                && plugin.id != *named
            {
                continue;
            }
            plugin.state = PluginState::failed(error.message.clone());
            disabled.push(plugin.id.clone());
        }
        if disabled.is_empty() {
            named.unwrap_or_else(|| "sidecar".to_string())
        } else {
            disabled.join(",")
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn rpc_error_from_object(error: RpcErrorObject, method: &str) -> RpcError {
    match error.code {
        -32601 => RpcError::MethodNotFound {
            namespace: String::new(),
            method: method.to_string(),
        },
        -32602 => RpcError::InvalidInput {
            message: error.message,
        },
        _ => RpcError::Failed {
            message: error.message,
            data: error.data,
        },
    }
}

async fn write_loop(
    mut stdin: ChildStdin,
    mut receiver: mpsc::UnboundedReceiver<String>,
    inner: Weak<Inner>,
) {
    while let Some(frame) = receiver.recv().await {
        let write = async {
            stdin.write_all(frame.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if write.await.is_err() {
            if let Some(inner) = inner.upgrade() {
                inner.fail("sidecar input closed");
            }
            break;
        }
    }
    let _ = stdin.shutdown().await;
}

async fn stderr_loop(stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        debug!(target: "oc_plugin::sidecar::stderr", %line);
    }
}

async fn read_loop(stdout: ChildStdout, inner: Weak<Inner>) {
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();
    loop {
        match read_frame(&mut reader, &mut buffer).await {
            Ok(0) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buffer);
                let line = text.trim_end_matches('\n').trim_end_matches('\r');
                if line.trim().is_empty() {
                    continue;
                }
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                match protocol::parse_incoming(line) {
                    Ok(incoming) => handle_incoming(&inner, incoming),
                    Err(error) => inner.protocol_error(&error.message),
                }
            }
            Err(error) => {
                if let Some(inner) = inner.upgrade() {
                    inner.fail(&format!("sidecar read error: {error}"));
                }
                break;
            }
        }
    }
    if let Some(inner) = inner.upgrade() {
        inner.on_eof().await;
    }
}

/// Read one newline-terminated frame without ever holding more than
/// [`protocol::MAX_FRAME_BYTES`] bytes. Returns the byte count, `0` at EOF.
async fn read_frame(
    reader: &mut BufReader<ChildStdout>,
    buffer: &mut Vec<u8>,
) -> std::io::Result<usize> {
    buffer.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(buffer.len());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if buffer.len() + take > protocol::MAX_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "malformed frame: exceeds 8 MiB",
            ));
        }
        buffer.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(buffer.len());
        }
    }
}

fn handle_incoming(inner: &Arc<Inner>, incoming: Incoming) {
    match incoming {
        Incoming::Response { id, outcome } => {
            if let Some(sender) = inner.pending.lock().remove(&id) {
                let _ = sender.send(Ok(outcome));
            } else {
                debug!(target: "oc_plugin::sidecar", id, "response for unknown request");
            }
        }
        Incoming::Notification { method, params } => match method.as_str() {
            protocol::NOTIFY_REGISTER => handle_register(inner, params),
            protocol::NOTIFY_UPDATE => handle_update(inner, params),
            protocol::NOTIFY_LOG => handle_log(inner, params),
            other => debug!(target: "oc_plugin::sidecar", method = other, "unknown notification"),
        },
        Incoming::Request { id, method, params } => {
            if method == protocol::METHOD_CALLBACK {
                spawn_callback(inner, id, params);
            } else {
                let error = RpcError::MethodNotFound {
                    namespace: "host".to_string(),
                    method: method.clone(),
                }
                .to_object();
                let _ = inner
                    .outbound
                    .send(protocol::error_response(id, &error).to_string());
            }
        }
    }
}

fn handle_register(inner: &Arc<Inner>, params: Value) {
    let parsed: RegisterParams = match serde_json::from_value(params) {
        Ok(parsed) => parsed,
        Err(error) => {
            inner.protocol_error(&format!("invalid register params: {error}"));
            return;
        }
    };
    let features = parsed.features.unwrap_or_else(PluginFeatures::server);
    let source = parsed.source.unwrap_or_else(|| PluginSource::Package {
        target: parsed.id.clone(),
        version: None,
        outdated: None,
        updating: None,
    });
    let mut state = PluginState::Active;
    let mut hooks = Vec::new();
    for name in &parsed.hooks {
        match name.parse::<Hook>() {
            Ok(hook) => hooks.push(hook),
            Err(_) => state = PluginState::failed(format!("unsupported feature: {name}")),
        }
    }
    let mut transforms = Vec::new();
    for name in &parsed.transforms {
        match name.parse::<Domain>() {
            Ok(domain) => transforms.push(domain),
            Err(_) => state = PluginState::failed(format!("unsupported feature: {name}")),
        }
    }
    let mut plugins = inner.plugins.lock();
    if plugins.iter().any(|plugin| plugin.id == parsed.id) {
        state = PluginState::failed("duplicate id");
    }
    plugins.push(SidecarPlugin {
        id: parsed.id,
        features,
        hooks,
        transforms,
        state,
        source,
    });
}

fn handle_update(inner: &Arc<Inner>, params: Value) {
    let parsed: UpdateParams = match serde_json::from_value(params) {
        Ok(parsed) => parsed,
        Err(error) => {
            inner.protocol_error(&format!("invalid update params: {error}"));
            return;
        }
    };
    let mut plugins = inner.plugins.lock();
    if let Some(plugin) = plugins.iter_mut().find(|plugin| plugin.id == parsed.id) {
        plugin.state = parsed.state;
    } else {
        debug!(
            target: "oc_plugin::sidecar",
            id = %parsed.id,
            "update for unknown plugin"
        );
    }
}

fn handle_log(inner: &Arc<Inner>, params: Value) {
    let parsed: LogParams = match serde_json::from_value(params) {
        Ok(parsed) => parsed,
        Err(error) => {
            inner.protocol_error(&format!("invalid log params: {error}"));
            return;
        }
    };
    match parsed.level.as_str() {
        "debug" => {
            debug!(target: "oc_plugin::sidecar", plugin = parsed.plugin.as_deref().unwrap_or(""), "{}", parsed.message);
        }
        "warn" => {
            warn!(target: "oc_plugin::sidecar", plugin = parsed.plugin.as_deref().unwrap_or(""), "{}", parsed.message);
        }
        "error" => {
            warn!(target: "oc_plugin::sidecar", plugin = parsed.plugin.as_deref().unwrap_or(""), level = "error", "{}", parsed.message);
        }
        _ => {
            info!(target: "oc_plugin::sidecar", plugin = parsed.plugin.as_deref().unwrap_or(""), "{}", parsed.message);
        }
    }
    inner.push_log(SidecarLog {
        level: parsed.level,
        message: parsed.message,
        plugin: parsed.plugin,
    });
}

fn spawn_callback(inner: &Arc<Inner>, id: Value, params: Value) {
    let inner = Arc::clone(inner);
    tokio::spawn(async move {
        let parsed: CallbackParams = match serde_json::from_value(params) {
            Ok(parsed) => parsed,
            Err(error) => {
                let object = RpcError::InvalidInput {
                    message: format!("invalid callback params: {error}"),
                }
                .to_object();
                let _ = inner
                    .outbound
                    .send(protocol::error_response(id, &object).to_string());
                return;
            }
        };
        let host = inner.host.read().clone();
        let response = match host {
            None => Err(RpcError::Unavailable {
                message: "no host service registered".to_string(),
            }),
            Some(host) => {
                let deadline = inner.config.request_timeout;
                match tokio::time::timeout(
                    deadline,
                    host.callback(&parsed.request_id, &parsed.method, &parsed.args),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(RpcError::Timeout {
                        method: parsed.method.clone(),
                        timeout_ms: millis(deadline),
                    }),
                }
            }
        };
        let frame = match response {
            Ok(value) => protocol::ok_response(id, value),
            Err(error) => protocol::error_response(id, &error.to_object()),
        };
        let _ = inner.outbound.send(frame.to_string());
    });
}

/// Failure to start or drive the sidecar.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SidecarError {
    /// Neither a configured command nor `bun`/`node` is available.
    #[error("no JS runtime found: install bun or node")]
    RuntimeUnavailable,
    /// The process could not be spawned.
    #[error("failed to spawn plugin sidecar {command}: {message}")]
    Spawn {
        /// Command that was attempted.
        command: String,
        /// OS error text.
        message: String,
    },
    /// The `load` handshake exceeded the startup deadline.
    #[error("plugin sidecar did not start within {timeout_ms} ms")]
    StartupTimeout {
        /// Deadline in milliseconds.
        timeout_ms: u64,
    },
    /// The process exited, or flooded malformed frames.
    #[error("plugin sidecar crashed: {reason}")]
    Crashed {
        /// Crash reason.
        reason: String,
    },
    /// The sidecar rejected the load request.
    #[error("plugin sidecar failed to load plugins ({code}): {message}")]
    Load {
        /// JSON-RPC code.
        code: i64,
        /// Error message.
        message: String,
    },
}
