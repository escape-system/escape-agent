//! Initialize handshake and the static client capability blob.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::LspServerConfig;
use crate::error::LspError;

use super::{Inner, LspClient};

impl LspClient {
    pub(super) async fn initialize(
        &self,
        server: &LspServerConfig,
        init_timeout: Duration,
    ) -> Result<(), LspError> {
        let mut params = json!({
            "processId": std::process::id(),
            "clientInfo": {
                "name": "opencode",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "rootUri": self.inner.root_uri,
            "rootPath": self.inner.root.to_string_lossy(),
            "workspaceFolders": [self.workspace_folder()],
            "capabilities": client_capabilities(),
            "trace": "off",
        });
        if let Some(initialization) = &server.initialization {
            params["initializationOptions"] = initialization.clone();
        }

        let result = self
            .inner
            .request_timeout("initialize", params, init_timeout)
            .await
            .map_err(|error| match error {
                LspError::Timeout { .. } => LspError::Timeout {
                    method: "initialize".into(),
                    millis: u64::try_from(init_timeout.as_millis()).unwrap_or(u64::MAX),
                },
                other => other,
            })?;

        let mut capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
        if capabilities.is_null() {
            capabilities = json!({});
        }
        let pull = capabilities
            .get("diagnosticProvider")
            .is_some_and(|value| !matches!(value, Value::Null | Value::Bool(false)));
        self.inner.pull_supported.store(pull, Ordering::Release);
        *self.inner.capabilities.lock() = capabilities;

        self.notify("initialized", json!({})).await?;
        if let Some(initialization) = &server.initialization {
            self.notify(
                "workspace/didChangeConfiguration",
                json!({ "settings": initialization }),
            )
            .await?;
        }
        Ok(())
    }

    fn workspace_folder(&self) -> Value {
        self.inner.workspace_folder()
    }
}

impl Inner {
    pub(super) fn workspace_folder(&self) -> Value {
        let name = self.root.file_name().map_or_else(
            || self.root.to_string_lossy().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        json!({ "uri": self.root_uri, "name": name })
    }
}

fn client_capabilities() -> Value {
    json!({
        "workspace": {
            "applyEdit": false,
            "workspaceEdit": { "documentChanges": false },
            "configuration": true,
            "workspaceFolders": true,
            "didChangeConfiguration": { "dynamicRegistration": true },
            "didChangeWatchedFiles": { "dynamicRegistration": true },
            "symbol": { "dynamicRegistration": false },
            "executeCommand": { "dynamicRegistration": false }
        },
        "textDocument": {
            "synchronization": {
                "dynamicRegistration": false,
                "willSave": false,
                "willSaveWaitUntil": false,
                "didSave": false
            },
            "publishDiagnostics": {
                "relatedInformation": true,
                "versionSupport": true,
                "codeDescriptionSupport": true,
                "dataSupport": true,
                "tagSupport": { "valueSet": [1, 2] }
            },
            "diagnostic": { "dynamicRegistration": false, "relatedDocumentSupport": false },
            "hover": {
                "dynamicRegistration": false,
                "contentFormat": ["markdown", "plaintext"]
            },
            "definition": { "dynamicRegistration": false, "linkSupport": true },
            "declaration": { "dynamicRegistration": false, "linkSupport": true },
            "references": { "dynamicRegistration": false },
            "implementation": { "dynamicRegistration": false, "linkSupport": true },
            "documentSymbol": {
                "dynamicRegistration": false,
                "hierarchicalDocumentSymbolSupport": true,
                "symbolKind": { "valueSet": [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26] }
            },
            "callHierarchy": { "dynamicRegistration": false }
        },
        "window": { "workDoneProgress": true },
        "general": {
            "positionEncodings": ["utf-16"],
            "staleRequestSupport": { "cancel": true, "retryOnContentModified": [] }
        }
    })
}
