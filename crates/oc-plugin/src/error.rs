//! Errors surfaced by the plugin host.

use serde_json::Value;
use thiserror::Error;

use crate::domain::Domain;
use crate::hook::Hook;
use crate::rpc::RpcError;

/// A name that is not part of a closed set (hook or transform domain).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown {kind}: {name}")]
pub struct UnknownName {
    /// The closed set the name was looked up in.
    pub kind: &'static str,
    /// The rejected name.
    pub name: String,
}

/// Failure of a native plugin or of the JS sidecar while serving a plugin call.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum PluginError {
    /// `tool.execute.before` rejected the call. This is the only hook that may
    /// fail; the runtime must reject the tool call with this error.
    #[error("plugin {plugin} rejected {hook}: {message}")]
    HookFailed {
        plugin: String,
        hook: Hook,
        message: String,
        metadata: Option<Value>,
    },
    /// A transform failed; the offending plugin was disabled and its scope closed.
    #[error("plugin {plugin} failed transforming {domain}: {message}")]
    TransformFailed {
        plugin: String,
        domain: Domain,
        message: String,
    },
    /// Guarded ids (`opencode.provider.*`, policy plugins) cannot be removed.
    #[error("plugin {id} is guarded and cannot be removed")]
    Guarded { id: String },
    /// Transport or dispatch failure of the JS sidecar.
    #[error(transparent)]
    Sidecar(#[from] RpcError),
}
