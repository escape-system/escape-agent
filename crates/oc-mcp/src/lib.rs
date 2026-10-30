//! MCP client for opencode-rust.
//!
//! Implements the JSON-RPC core, stdio and streamable-HTTP transports, the
//! client lifecycle (initialize, catalogs, tools, resources, prompts, ping,
//! shutdown), tool name/schema/result mapping, and the OAuth building blocks.
//!
//! See `docs/09-mcp.md`. WebSocket "attached" transports are hosted by the
//! caller: feed frames into a [`transport::Transport`] implementation and use
//! [`client::McpClient::connect_transport`].

pub mod client;
pub mod config;
pub mod error;
pub mod oauth;
pub mod protocol;
pub mod tool;
pub mod transport;

pub use client::{
    ClientInfo, ClientOptions, ElicitationHandler, McpChange, McpClient, McpServerInfo, McpStatus,
    McpStatusEvent, ServerInfo,
};
pub use config::{
    DEFAULT_CATALOG_MS, DEFAULT_EXECUTION_MS, DEFAULT_STARTUP_MS, LEGACY_PROTOCOL_VERSION,
    LocalConfig, MODERN_PROTOCOL_VERSION, McpConfig, McpServerConfig, OAuthConfig, OAuthSetting,
    Protocol, RemoteConfig, TimeoutConfig, expand_placeholders,
};
pub use error::{McpError, McpResult, RpcError};
pub use tool::{
    MAX_RESULT_MEDIA_BYTES, MAX_RESULT_TEXT_BYTES, McpContent, McpTool, McpToolOutput,
    effective_name, map_call_result, namespace, patch_expectation, sanitize, strip_expectation,
};
