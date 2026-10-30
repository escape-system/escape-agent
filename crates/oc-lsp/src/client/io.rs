//! Reader and stderr loops plus diagnostics debouncing.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout};

use crate::rpc::FrameReader;
use crate::{LspDiagnosticsEvent, LspEvent};

use super::Inner;
use super::types::DIAGNOSTICS_DEBOUNCE;

pub(super) fn schedule_debounce(inner: Arc<Inner>) {
    if inner.debouncing.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(DIAGNOSTICS_DEBOUNCE).await;
        let paths: Vec<PathBuf> = inner.debounce_pending.lock().drain().collect();
        inner.debouncing.store(false, Ordering::Release);
        for path in paths {
            let _ = inner
                .events
                .send(LspEvent::Diagnostics(LspDiagnosticsEvent {
                    server_id: inner.server_id.clone(),
                    path: path.to_string_lossy().into_owned(),
                }));
        }
        if !inner.debounce_pending.lock().is_empty() {
            schedule_debounce(inner);
        }
    });
}

pub(super) async fn read_loop(inner: Arc<Inner>, mut reader: FrameReader<BufReader<ChildStdout>>) {
    loop {
        match reader.read_message().await {
            Ok(Some(message)) => inner.dispatch(message).await,
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(server = %inner.server_id, error = %error, "LSP reader stopping");
                break;
            }
        }
    }
    inner.fail_pending();
    let mut guard = inner.child.lock().await;
    if let Some(child) = guard.as_mut()
        && tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
    {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    }
    guard.take();
    drop(guard);
    inner.exited.store(true, Ordering::Release);
}

pub(super) async fn stderr_loop(inner: Arc<Inner>, mut stderr: ChildStderr) {
    let mut buffer = [0_u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => inner.append_stderr(&buffer[..read]),
        }
    }
}
