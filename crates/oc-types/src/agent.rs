//! `Agent.Info` wire object.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::permission::PermissionRule;
use crate::session::ModelRef;

/// `Agent.Info.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Subagent,
    Primary,
    All,
}

/// `Provider.Request`: per-agent provider overrides.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ProviderRequest {
    #[serde(default)]
    pub settings: Map<String, Value>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Map<String, Value>,
}

/// Exact wire `Agent.Info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    pub request: ProviderRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub mode: AgentMode,
    pub hidden: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    pub permissions: Vec<PermissionRule>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_info_round_trips() {
        let fixture = serde_json::json!({
            "id": "build",
            "name": "build",
            "model": {"id": "claude-sonnet-4", "providerID": "anthropic"},
            "request": {"settings": {}, "headers": {}, "body": {}},
            "system": "You are a builder.",
            "description": "Builds things",
            "mode": "primary",
            "hidden": false,
            "color": "#ff0000",
            "steps": 20,
            "permissions": [{"action": "*", "resource": "*", "effect": "allow"}]
        });
        let parsed: AgentInfo = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(parsed.mode, AgentMode::Primary);
        assert_eq!(serde_json::to_value(&parsed).unwrap(), fixture);
        assert!(!serde_json::to_string(&parsed).unwrap().contains("null"));
    }

    #[test]
    fn minimal_agent_omits_optionals() {
        let fixture = serde_json::json!({
            "id": "general",
            "name": "general",
            "request": {"settings": {}, "headers": {}, "body": {}},
            "mode": "all",
            "hidden": true,
            "permissions": []
        });
        let parsed: AgentInfo = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), fixture);
    }
}
