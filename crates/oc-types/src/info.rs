//! `GET /api/info` response.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    pub urls: Vec<String>,
    pub paths: ServerPaths,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ServerCapabilities>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerPaths {
    pub tmp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(rename = "persistentPty", skip_serializing_if = "Option::is_none")]
    pub persistent_pty: Option<bool>,
}
