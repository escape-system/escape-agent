//! Diagnostics waiting, caching and close-file cleanup.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::diagnostics::CachedDiagnostics;
use crate::error::LspError;
use crate::position::Position;
use crate::uri::path_to_uri;

use super::types::DIAGNOSTICS_DEBOUNCE;
use super::{Inner, LspClient};

impl LspClient {
    /// Wait for a diagnostics update for `path` (3 s budget by default).
    pub async fn wait_diagnostics(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Option<CachedDiagnostics> {
        self.inner.wait_for_diagnostics(path, 0, timeout).await
    }

    /// Cached diagnostics without waiting.
    pub fn cached_diagnostics(&self, path: &Path) -> Option<CachedDiagnostics> {
        self.inner.diagnostics.lock().get(path)
    }

    /// Wire position for a one-based user position, using the file's text.
    pub async fn wire_position(
        &self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Position, LspError> {
        let text = tokio::fs::read_to_string(path).await.unwrap_or_default();
        let (position, _) = crate::position::user_position_to_offset(&text, line, character);
        Ok(position)
    }

    /// Send `textDocument/didClose` and drop cached diagnostics for `path`.
    pub async fn close_file(&self, path: &Path) {
        let canonical = tokio::fs::canonicalize(path)
            .await
            .unwrap_or_else(|_| path.to_path_buf());
        let was_open = self.inner.open_files.lock().remove(&canonical).is_some();
        if was_open {
            let uri = path_to_uri(&canonical);
            let _ = self
                .notify(
                    "textDocument/didClose",
                    json!({ "textDocument": { "uri": uri } }),
                )
                .await;
        }
        self.inner.diagnostics.lock().remove(&canonical);
    }
}

impl Inner {
    pub(super) fn diagnostics_sequence(&self) -> u64 {
        *self.diag_seq.borrow()
    }

    pub(super) fn bump_diagnostics(&self) {
        self.diag_seq.send_modify(|sequence| {
            *sequence = sequence.wrapping_add(1);
        });
    }

    pub(super) fn apply_pull(&self, path: &Path, result: &Value) {
        if let Some(kind) = result.get("kind").and_then(Value::as_str)
            && kind != "full"
        {
            return;
        }
        let items = match result.get("items").and_then(Value::as_array) {
            Some(items) => items.clone(),
            None => match result.as_array() {
                Some(items) => items.clone(),
                None => return,
            },
        };
        let result_id = result
            .get("resultId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.diagnostics.lock().put_pull(path, items, result_id);
        self.bump_diagnostics();
    }

    pub(super) async fn wait_for_diagnostics(
        &self,
        path: &Path,
        baseline: u64,
        timeout: Duration,
    ) -> Option<CachedDiagnostics> {
        let mut receiver = self.diag_seq.subscribe();
        let deadline = Instant::now() + timeout;
        let debounce = DIAGNOSTICS_DEBOUNCE.min(timeout);
        tokio::time::sleep(debounce).await;
        let mut seen = baseline;
        loop {
            let sequence = self.diagnostics_sequence();
            if sequence > seen {
                seen = sequence;
                if let Some(entry) = self.diagnostics.lock().peek(path) {
                    return Some(entry);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.diagnostics.lock().peek(path);
            }
            if tokio::time::timeout(remaining, receiver.changed())
                .await
                .is_err()
            {
                return self.diagnostics.lock().peek(path);
            }
        }
    }
}
