//! Sidecar process transport: spawn, stdio loops, frame reading, and lifecycle transitions.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::register::handle_incoming;
use super::{
    HostServices, Inner, MAX_LOGS, MAX_PROTOCOL_ERRORS, Sidecar, SidecarConfig, SidecarError,
    SidecarLog, SidecarStatus, detect_runtime, protocol,
};
use crate::rpc::RpcError;

impl Inner {
    pub(super) fn push_log(&self, log: SidecarLog) {
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

    pub(super) fn protocol_error(self: &Arc<Self>, message: &str) {
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

    pub(super) async fn stop(&self) {
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
