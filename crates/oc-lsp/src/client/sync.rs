//! Document synchronization and pull diagnostics.

use std::path::Path;

use serde_json::{Value, json};

use crate::diagnostics::CachedDiagnostics;
use crate::error::LspError;
use crate::language::language_id;
use crate::uri::path_to_uri;

use super::LspClient;
use super::types::MAX_FILE_BYTES;

impl LspClient {
    /// Synchronize a file with the server.
    ///
    /// The first touch sends `workspace/didChangeWatchedFiles` (created) and
    /// `textDocument/didOpen`; later touches send (changed) plus a full-text
    /// `didChange`. When `wait` is true, returns diagnostics after the
    /// documented debounce/wait budget.
    pub async fn touch_file(
        &self,
        path: &Path,
        wait: bool,
    ) -> Result<Option<CachedDiagnostics>, LspError> {
        let canonical = tokio::fs::canonicalize(path).await?;
        if !canonical.starts_with(&self.inner.root) {
            return Err(LspError::OutsideRoot(canonical));
        }
        let metadata = tokio::fs::metadata(&canonical).await?;
        if metadata.len() > MAX_FILE_BYTES {
            tracing::debug!(
                server = %self.inner.server_id,
                path = %canonical.display(),
                bytes = metadata.len(),
                "skipping LSP sync for a large file"
            );
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&tokio::fs::read(&canonical).await?).into_owned();
        let uri = path_to_uri(&canonical);
        let baseline = self.inner.diagnostics_sequence();

        self.sync_document(&canonical, &uri, &text).await?;
        self.pull_diagnostics(&canonical, &uri).await;

        if wait {
            Ok(self
                .inner
                .wait_for_diagnostics(&canonical, baseline, self.inner.diagnostics_wait)
                .await)
        } else {
            Ok(self.inner.diagnostics.lock().peek(&canonical))
        }
    }

    async fn sync_document(&self, canonical: &Path, uri: &str, text: &str) -> Result<(), LspError> {
        let version = {
            let mut open = self.inner.open_files.lock();
            if let Some(version) = open.get_mut(canonical) {
                *version = version.saturating_add(1);
                Some(*version)
            } else {
                open.insert(canonical.to_path_buf(), 0);
                None
            }
        };

        self.inner.touch();
        match version {
            None => {
                self.notify(
                    "workspace/didChangeWatchedFiles",
                    json!({ "changes": [{ "uri": uri, "type": 1 }] }),
                )
                .await?;
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id(canonical),
                            "version": 0,
                            "text": text,
                        }
                    }),
                )
                .await?;
            }
            Some(version) => {
                self.notify(
                    "workspace/didChangeWatchedFiles",
                    json!({ "changes": [{ "uri": uri, "type": 2 }] }),
                )
                .await?;
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }],
                    }),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn pull_diagnostics(&self, canonical: &Path, uri: &str) {
        if !self.supports_pull_diagnostics() {
            return;
        }
        let previous = self
            .inner
            .diagnostics
            .lock()
            .peek(canonical)
            .and_then(|entry| entry.result_id);
        let mut params = json!({ "textDocument": { "uri": uri } });
        if let Some(previous) = previous {
            params["previousResultId"] = Value::String(previous);
        }
        match self
            .inner
            .request_timeout(
                "textDocument/diagnostic",
                params,
                self.inner.request_timeout,
            )
            .await
        {
            Ok(result) => self.inner.apply_pull(canonical, &result),
            Err(error) => tracing::debug!(
                server = %self.inner.server_id,
                error = %error,
                "pull diagnostics failed; falling back to push"
            ),
        }
    }
}
