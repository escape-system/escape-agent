//! The additive config surface: `Config.Entry` documents and `Config.Patch`.
//!
//! The full config model lives in `oc-config`; these are the wire shapes the
//! server needs to serve `GET /api/config` and accept
//! `PATCH /api/experimental/config`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `Config.Entry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ConfigEntry {
    #[serde(rename = "document")]
    Document(ConfigDocument),
    #[serde(rename = "directory")]
    Directory(ConfigDirectory),
}

/// `Config.Document`. `info` is the raw document, `null`s preserved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigDocument {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub info: Value,
}

/// `Config.Directory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigDirectory {
    pub path: String,
}

/// `Config.Patch` extended with this project's additive top-level keys.
///
/// Every key is omitted when unset, matching the "optional fields are omitted,
/// never `null`" wire rule; setting a key to `Value::Null` explicitly clears it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ConfigPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagents: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observer: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<Value>,
    #[serde(rename = "lsp_install", skip_serializing_if = "Option::is_none")]
    pub lsp_install: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lanes: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_document_entry_uses_type_discriminator() {
        let entry: ConfigEntry = serde_json::from_value(serde_json::json!({
            "type": "document",
            "path": "/work/opencode.json",
            "info": {"shell": "/bin/bash"}
        }))
        .unwrap();
        let ConfigEntry::Document(document) = &entry else {
            panic!("expected document");
        };
        assert_eq!(document.path.as_deref(), Some("/work/opencode.json"));
        assert_eq!(
            serde_json::to_value(&entry).unwrap()["type"],
            Value::String("document".to_owned())
        );
    }

    #[test]
    fn config_directory_entry_round_trips() {
        let fixture = serde_json::json!({"type": "directory", "path": "/work/.opencode"});
        let entry: ConfigEntry = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&entry).unwrap(), fixture);
    }

    #[test]
    fn patch_omits_unset_keys() {
        let patch = ConfigPatch {
            shell: Some("pwsh".to_owned()),
            ..ConfigPatch::default()
        };
        let text = serde_json::to_string(&patch).unwrap();
        assert_eq!(text, r#"{"shell":"pwsh"}"#);
        assert!(!text.contains("null"));
    }

    #[test]
    fn patch_round_trips_additive_keys() {
        let fixture = serde_json::json!({
            "shell": "bash",
            "lanes": {"cargo": 1, "default": 8},
            "lsp_install": false,
            "events": {"persist": true}
        });
        let patch: ConfigPatch = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&patch).unwrap(), fixture);
    }
}
