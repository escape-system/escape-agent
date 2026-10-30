//! MCP server configuration, matching the upstream `Mcp` schema keys.
//!
//! Timeouts are milliseconds, exactly as the upstream schema documents them.
//! [`McpConfig::from_value`] accepts the merged `mcp` config document and
//! returns typed errors instead of panicking on malformed entries.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{McpError, McpResult};

/// Default startup timeout.
pub const DEFAULT_STARTUP_MS: u64 = 30_000;
/// Default catalog (list) timeout.
pub const DEFAULT_CATALOG_MS: u64 = 30_000;
/// Default execution timeout: 12 hours.
pub const DEFAULT_EXECUTION_MS: u64 = 12 * 60 * 60 * 1_000;
/// Protocol revision spoken by the default (`legacy`) handshake.
pub const LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";
/// Newest revision this client understands as a pinned protocol.
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

/// `mcp.timeout` in milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<u64>,
}

impl TimeoutConfig {
    /// Startup timeout with defaults applied.
    #[must_use]
    pub fn startup(&self) -> Duration {
        Duration::from_millis(self.startup.unwrap_or(DEFAULT_STARTUP_MS))
    }

    /// Catalog timeout with defaults applied.
    #[must_use]
    pub fn catalog(&self) -> Duration {
        Duration::from_millis(self.catalog.unwrap_or(DEFAULT_CATALOG_MS))
    }

    /// Execution timeout with defaults applied.
    #[must_use]
    pub fn execution(&self) -> Duration {
        Duration::from_millis(self.execution.unwrap_or(DEFAULT_EXECUTION_MS))
    }

    /// Overlays `other` on `self`, preferring the values in `other`.
    #[must_use]
    pub fn merged_with(&self, other: &Self) -> Self {
        Self {
            startup: other.startup.or(self.startup),
            catalog: other.catalog.or(self.catalog),
            execution: other.execution.or(self.execution),
        }
    }

    fn validate(&self, path: &str) -> McpResult<()> {
        for (key, value) in [
            ("startup", self.startup),
            ("catalog", self.catalog),
            ("execution", self.execution),
        ] {
            if value == Some(0) {
                return Err(McpError::Config(format!(
                    "{path}.{key} must be a positive number of milliseconds"
                )));
            }
        }
        Ok(())
    }
}

/// MCP protocol negotiation mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    /// Initialize handshake with revisions up to `2025-11-25`.
    #[default]
    #[serde(rename = "legacy")]
    Legacy,
    /// Probe `2026-07-28`, fall back to the legacy handshake.
    #[serde(rename = "auto")]
    Auto,
    /// Require revision `2026-07-28`.
    #[serde(rename = "2026-07-28")]
    V2026_07_28,
}

impl Protocol {
    /// Version sent by the first initialize attempt.
    #[must_use]
    pub fn initial_version(self) -> &'static str {
        match self {
            Self::Legacy => LEGACY_PROTOCOL_VERSION,
            Self::Auto | Self::V2026_07_28 => MODERN_PROTOCOL_VERSION,
        }
    }

    /// Whether a failed modern probe may fall back to the legacy handshake.
    #[must_use]
    pub fn allows_legacy_fallback(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// OAuth options for a remote server (`oauth: { ... }`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_server_metadata_url: Option<String>,
}

impl OAuthConfig {
    fn validate(&self, path: &str) -> McpResult<()> {
        if let Some(port) = self.callback_port
            && port == 0
        {
            return Err(McpError::Config(format!(
                "{path}.callback_port must be between 1 and 65535"
            )));
        }
        Ok(())
    }
}

/// `oauth` is either an object or the literal `false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OAuthSetting {
    Enabled(OAuthConfig),
    Disabled(bool),
}

impl OAuthSetting {
    /// Returns the configuration, or `None` when OAuth is disabled.
    #[must_use]
    pub fn enabled(&self) -> Option<&OAuthConfig> {
        match self {
            Self::Enabled(config) => Some(config),
            Self::Disabled(_) => None,
        }
    }

    fn validate(&self, path: &str) -> McpResult<()> {
        match self {
            Self::Enabled(config) => config.validate(path),
            Self::Disabled(false) => Ok(()),
            Self::Disabled(true) => Err(McpError::Config(format!(
                "{path} must be an object or false"
            ))),
        }
    }
}

/// A stdio MCP server (`type: "local"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalConfig {
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codemode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<TimeoutConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
}

impl LocalConfig {
    fn validate(&self, path: &str) -> McpResult<()> {
        if self.command.is_empty() || self.command[0].trim().is_empty() {
            return Err(McpError::Config(format!(
                "{path}.command must contain at least one element"
            )));
        }
        if let Some(timeout) = &self.timeout {
            timeout.validate(&format!("{path}.timeout"))?;
        }
        Ok(())
    }
}

/// A remote MCP server (`type: "remote"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub url: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthSetting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codemode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<TimeoutConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
}

impl RemoteConfig {
    fn validate(&self, path: &str) -> McpResult<()> {
        let url = url::Url::parse(&self.url)
            .map_err(|error| McpError::Config(format!("{path}.url is invalid: {error}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(McpError::Config(format!(
                "{path}.url must use http or https"
            )));
        }
        if let Some(timeout) = &self.timeout {
            timeout.validate(&format!("{path}.timeout"))?;
        }
        if let Some(oauth) = &self.oauth {
            oauth.validate(&format!("{path}.oauth"))?;
        }
        Ok(())
    }
}

/// A configured MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    Local(LocalConfig),
    Remote(RemoteConfig),
}

impl McpServerConfig {
    /// Validates and converts one server entry.
    pub fn from_value(path: &str, value: &Value) -> McpResult<Self> {
        let config: Self = serde_json::from_value(value.clone())
            .map_err(|error| McpError::Config(format!("{path}: {error}")))?;
        config.validate(path)?;
        Ok(config)
    }

    /// Parses the `mcp.servers` object.
    pub fn parse_servers(value: &Value) -> McpResult<BTreeMap<String, Self>> {
        let Some(object) = value.as_object() else {
            return Err(McpError::Config("mcp.servers must be an object".to_owned()));
        };
        let mut servers = BTreeMap::new();
        for (name, entry) in object {
            let path = format!("mcp.servers.{name}");
            servers.insert(name.clone(), Self::from_value(&path, entry)?);
        }
        Ok(servers)
    }

    /// Validates this entry.
    pub fn validate(&self, path: &str) -> McpResult<()> {
        match self {
            Self::Local(config) => config.validate(path),
            Self::Remote(config) => config.validate(path),
        }
    }

    /// Whether the server is disabled by configuration.
    #[must_use]
    pub fn disabled(&self) -> bool {
        match self {
            Self::Local(config) => config.disabled.unwrap_or(false),
            Self::Remote(config) => config.disabled.unwrap_or(false),
        }
    }

    /// Whether tools are exposed through Code Mode (defaults to true upstream).
    #[must_use]
    pub fn codemode(&self) -> bool {
        match self {
            Self::Local(config) => config.codemode.unwrap_or(true),
            Self::Remote(config) => config.codemode.unwrap_or(true),
        }
    }

    /// Server-local timeout overrides.
    #[must_use]
    pub fn timeout(&self) -> TimeoutConfig {
        match self {
            Self::Local(config) => config.timeout.clone().unwrap_or_default(),
            Self::Remote(config) => config.timeout.clone().unwrap_or_default(),
        }
    }

    /// Protocol negotiation mode (defaults to legacy).
    #[must_use]
    pub fn protocol(&self) -> Protocol {
        match self {
            Self::Local(config) => config.protocol.unwrap_or_default(),
            Self::Remote(config) => config.protocol.unwrap_or_default(),
        }
    }
}

/// Parsed `mcp` config document: global timeout plus servers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<TimeoutConfig>,
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

impl McpConfig {
    /// Parses the merged `mcp` document. Missing sections yield an empty config.
    pub fn from_value(value: &Value) -> McpResult<Self> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let Some(object) = value.as_object() else {
            return Err(McpError::Config("mcp must be an object".to_owned()));
        };
        let timeout = match object.get("timeout") {
            Some(value) => Some(
                serde_json::from_value::<TimeoutConfig>(value.clone())
                    .map_err(|error| McpError::Config(format!("mcp.timeout: {error}")))?,
            ),
            None => None,
        };
        if let Some(timeout) = &timeout {
            timeout.validate("mcp.timeout")?;
        }
        let servers = match object.get("servers") {
            Some(value) => McpServerConfig::parse_servers(value)?,
            None => BTreeMap::new(),
        };
        Ok(Self { timeout, servers })
    }

    /// Effective timeouts for one server: global values under server overrides.
    #[must_use]
    pub fn timeout_for(&self, name: &str) -> TimeoutConfig {
        let mut resolved = self.timeout.clone().unwrap_or_default();
        if let Some(server) = self.servers.get(name) {
            resolved = resolved.merged_with(&server.timeout());
        }
        resolved
    }
}

/// Expands `{env:VAR}` and `{file:path}` placeholders in config string maps.
///
/// Errors are returned for unset environment variables and unreadable files so
/// a typo never silently sends an empty header.
pub fn expand_placeholders<S: std::hash::BuildHasher>(
    map: &BTreeMap<String, String>,
    environment: &HashMap<String, String, S>,
) -> McpResult<BTreeMap<String, String>> {
    let mut expanded = BTreeMap::new();
    for (key, value) in map {
        expanded.insert(key.clone(), expand_value(value, environment)?);
    }
    Ok(expanded)
}

fn expand_value<S: std::hash::BuildHasher>(
    value: &str,
    environment: &HashMap<String, String, S>,
) -> McpResult<String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            output.push_str(&rest[start..]);
            return Ok(output);
        };
        let token = &after[..end];
        if let Some(name) = token.strip_prefix("env:") {
            let Some(found) = environment.get(name) else {
                return Err(McpError::Config(format!(
                    "environment variable {name} is not set"
                )));
            };
            output.push_str(found);
        } else if let Some(path) = token.strip_prefix("file:") {
            let content = std::fs::read_to_string(path).map_err(|error| {
                McpError::Config(format!(
                    "unable to read {path} for config substitution: {error}"
                ))
            })?;
            output.push_str(content.trim_end_matches(['\r', '\n']));
        } else {
            output.push('{');
            output.push_str(token);
            output.push('}');
        }
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_and_local() {
        let value = serde_json::json!({
            "timeout": {"startup": 1000, "catalog": 1000, "execution": 2000},
            "servers": {
                "github": {
                    "type": "remote",
                    "url": "https://mcp.example.com/mcp",
                    "headers": {"Authorization": "Bearer x"},
                    "oauth": {"client_id": "abc", "scope": "repo"},
                    "protocol": "auto",
                    "codemode": true
                },
                "fs": {
                    "type": "local",
                    "command": ["npx", "-y", "server"],
                    "cwd": ".",
                    "environment": {"NODE_OPTIONS": "--max-old-space-size=512"},
                    "timeout": {"startup": 60, "execution": 600}
                }
            }
        });
        let config = McpConfig::from_value(&value).expect("config");
        assert_eq!(config.servers.len(), 2);
        let github = config.servers.get("github").expect("github");
        assert!(matches!(github, McpServerConfig::Remote(_)));
        assert_eq!(github.protocol(), Protocol::Auto);
        assert_eq!(
            config.timeout_for("github").startup(),
            Duration::from_millis(1000)
        );
        assert_eq!(
            config.timeout_for("github").execution(),
            Duration::from_millis(2000)
        );
        let fs = config.servers.get("fs").expect("fs");
        assert_eq!(fs.timeout().startup(), Duration::from_millis(60));
        assert_eq!(
            config.timeout_for("fs").catalog(),
            Duration::from_millis(1000)
        );
        assert_eq!(
            config.timeout_for("fs").execution(),
            Duration::from_millis(600)
        );
        assert!(fs.codemode());
    }

    #[test]
    fn oauth_false_disables() {
        let value = serde_json::json!({
            "servers": {"x": {"type": "remote", "url": "https://example.com/mcp", "oauth": false}}
        });
        let config = McpConfig::from_value(&value).expect("config");
        let server = config.servers.get("x").expect("x");
        match server {
            McpServerConfig::Remote(remote) => {
                assert!(matches!(remote.oauth, Some(OAuthSetting::Disabled(false))));
                assert!(
                    remote
                        .oauth
                        .as_ref()
                        .and_then(OAuthSetting::enabled)
                        .is_none()
                );
            }
            McpServerConfig::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn rejects_bad_entries() {
        let cases = [
            serde_json::json!({"servers": {"a": {"type": "remote", "url": "ftp://x"}}}),
            serde_json::json!({"servers": {"a": {"type": "local", "command": []}}}),
            serde_json::json!({"servers": {"a": {"type": "weird"}}}),
            serde_json::json!({"servers": {"a": {"type": "remote", "url": "https://x", "oauth": true}}}),
            serde_json::json!({"timeout": {"startup": 0}}),
        ];
        for case in cases {
            assert!(McpConfig::from_value(&case).is_err(), "accepted {case}");
        }
    }

    #[test]
    fn disabled_bool_is_honored() {
        let value = serde_json::json!({
            "servers": {"off": {"type": "local", "command": ["x"], "disabled": true}}
        });
        let config = McpConfig::from_value(&value).expect("config");
        assert!(config.servers.get("off").expect("off").disabled());
    }

    #[test]
    fn expands_env_and_leaves_unknown_tokens() {
        let mut environment = HashMap::new();
        environment.insert("TOKEN".to_owned(), "secret".to_owned());
        let map = BTreeMap::from([
            ("Authorization".to_owned(), "Bearer {env:TOKEN}".to_owned()),
            ("X-Literal".to_owned(), "{not-a-token}".to_owned()),
        ]);
        let expanded = expand_placeholders(&map, &environment).expect("expand");
        assert_eq!(expanded["Authorization"], "Bearer secret");
        assert_eq!(expanded["X-Literal"], "{not-a-token}");
        assert!(
            expand_placeholders(
                &BTreeMap::from([("A".to_owned(), "{env:MISSING}".to_owned())]),
                &environment
            )
            .is_err()
        );
    }
}
