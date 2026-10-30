//! Shutdown, forced kill, last-use tracking and stderr retention.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::uri::path_to_uri;

use super::{Inner, LspClient};

impl LspClient {
    /// `shutdown` → `exit` → 1 s → kill.
    pub async fn shutdown(&self) {
        if self.inner.shutdown_started.swap(true, Ordering::AcqRel) {
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                while !self.is_exited() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            return;
        }
        let open_files: Vec<PathBuf> = self
            .inner
            .open_files
            .lock()
            .drain()
            .map(|(path, _)| path)
            .collect();
        for path in open_files {
            let uri = path_to_uri(&path);
            let _ = self
                .notify(
                    "textDocument/didClose",
                    json!({ "textDocument": { "uri": uri } }),
                )
                .await;
        }
        let _ = self
            .inner
            .request_timeout("shutdown", Value::Null, Duration::from_secs(2))
            .await;
        let _ = self.notify("exit", Value::Null).await;
        // EOF on stdin is the polite signal when the server ignores `exit`.
        self.inner.stdin.lock().await.take();
        self.inner.terminate_child().await;
        self.inner.fail_pending();
        self.inner.diagnostics.lock().clear();
        self.inner.exited.store(true, Ordering::Release);
    }

    /// Kill without the polite sequence (initialize failure, supervisor drop).
    pub async fn force_kill(&self) {
        self.inner.stdin.lock().await.take();
        self.inner.terminate_child().await;
        self.inner.fail_pending();
        self.inner.exited.store(true, Ordering::Release);
    }
}

impl Inner {
    pub(super) fn touch(&self) {
        *self.last_used.lock() = Instant::now();
    }

    pub(super) fn append_stderr(&self, bytes: &[u8]) {
        const KEEP: usize = 8 * 1024;
        let mut buffer = self.stderr.lock();
        buffer.push_str(&String::from_utf8_lossy(bytes));
        if buffer.len() > KEEP {
            let mut cut = buffer.len() - KEEP;
            while cut < buffer.len() && !buffer.is_char_boundary(cut) {
                cut += 1;
            }
            buffer.drain(..cut);
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Best-effort synchronous kill; `kill_on_drop` covers the async path.
        if let Ok(mut guard) = self.child.try_lock()
            && let Some(child) = guard.as_mut()
        {
            let _ = child.start_kill();
        }
    }
}
