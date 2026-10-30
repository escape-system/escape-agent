//! `SidecarConfig`: runtime command discovery and deadline configuration.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// Spawn and deadline configuration for the sidecar.
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    /// Explicit runtime command (`plugin_host.command`). `None` discovers
    /// `bun`, then `node`, on PATH.
    pub command: Option<String>,
    /// Extra arguments passed to the runtime before the sidecar script.
    pub args: Vec<String>,
    /// Working directory of the sidecar.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Load/handshake deadline. Default 5 s.
    pub startup_timeout: Duration,
    /// Per-request deadline, including callbacks into host services. Default 5 s.
    pub request_timeout: Duration,
    /// Grace period between `plugin.shutdown` and the kill. Default 2 s.
    pub shutdown_grace: Duration,
}

impl Default for SidecarConfig {
    fn default() -> Self {
        Self {
            command: None,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            shutdown_grace: Duration::from_secs(2),
        }
    }
}

impl SidecarConfig {
    /// Config with an explicit runtime command.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: Some(command.into()),
            ..Self::default()
        }
    }

    /// Read `plugin_host` from the merged config document.
    ///
    /// `plugin_host.command` is a string, or an array whose first element is the
    /// command and whose remainder are arguments. `plugin_host.args`,
    /// `plugin_host.cwd` are accepted too. Unrecognised members are ignored so a
    /// newer config never breaks startup.
    #[must_use]
    pub fn from_config(config: &Value) -> Self {
        let mut out = Self::default();
        let Some(host) = config.get("plugin_host").and_then(Value::as_object) else {
            return out;
        };
        match host.get("command") {
            Some(Value::String(command)) if !command.trim().is_empty() => {
                out.command = Some(command.clone());
            }
            Some(Value::Array(items)) => {
                let mut parts = items.iter().filter_map(Value::as_str);
                if let Some(command) = parts.next() {
                    out.command = Some(command.to_string());
                    out.args = parts.map(str::to_string).collect();
                }
            }
            _ => {}
        }
        if let Some(args) = host.get("args").and_then(Value::as_array) {
            out.args = args
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        if let Some(cwd) = host.get("cwd").and_then(Value::as_str) {
            out.cwd = Some(PathBuf::from(cwd));
        }
        out
    }

    /// Append runtime arguments.
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.args.extend(args);
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Add one environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Override the startup/request deadlines (tests use short ones).
    #[must_use]
    pub fn with_timeouts(mut self, startup: Duration, request: Duration) -> Self {
        self.startup_timeout = startup;
        self.request_timeout = request;
        self
    }

    /// Override the shutdown grace period.
    #[must_use]
    pub fn with_shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }
}
