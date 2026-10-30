//! RPC dispatch to the sidecar: load, reload, dispose, hooks, transforms, invoke, shutdown.

use serde_json::{Value, json};
use tracing::warn;

use super::protocol::Exchange;
use super::register::{millis, rpc_error_from_object};
use super::{Sidecar, SidecarError, SidecarStatus, protocol};
use crate::domain::Domain;
use crate::error::PluginError;
use crate::hook::{Hook, HookEvent, TransformDoc};
use crate::info::PluginState;
use crate::rpc::{RpcError, RpcErrorObject};

impl Sidecar {
    /// Load plugin targets. Used by the initial handshake; a supervisor that
    /// reuses a warm sidecar calls [`Sidecar::reload`] instead.
    ///
    /// # Errors
    ///
    /// See [`Sidecar::start`].
    pub async fn load(&self, targets: &[String]) -> Result<(), SidecarError> {
        let params = json!({ "targets": targets });
        match self
            .inner
            .send_request(
                protocol::METHOD_LOAD,
                Some(params),
                self.inner.config.startup_timeout,
            )
            .await
        {
            Ok(Exchange::Ok(_)) => {
                let mut status = self.inner.status.lock();
                if matches!(*status, SidecarStatus::Starting) {
                    *status = SidecarStatus::Ready;
                }
                Ok(())
            }
            Ok(Exchange::Error(error)) => Err(SidecarError::Load {
                code: error.code,
                message: error.message,
            }),
            Err(RpcError::Timeout { .. }) => Err(SidecarError::StartupTimeout {
                timeout_ms: millis(self.inner.config.startup_timeout),
            }),
            Err(error) => Err(SidecarError::Crashed {
                reason: error.to_string(),
            }),
        }
    }

    /// Reload the given plugin ids (hot reload keeps the previous generation
    /// when the sidecar rejects the new one).
    ///
    /// # Errors
    ///
    /// Transport failures and sidecar rejections.
    pub async fn reload(&self, ids: &[String]) -> Result<(), RpcError> {
        let params = json!({ "ids": ids });
        match self
            .inner
            .send_request(
                protocol::METHOD_RELOAD,
                Some(params),
                self.inner.config.request_timeout,
            )
            .await?
        {
            Exchange::Ok(_) => Ok(()),
            Exchange::Error(error) => Err(rpc_error_from_object(error, "reload")),
        }
    }

    /// Dispose one plugin.
    ///
    /// # Errors
    ///
    /// Transport failures and sidecar rejections.
    pub async fn dispose(&self, id: &str) -> Result<(), RpcError> {
        let params = json!({ "id": id });
        match self
            .inner
            .send_request(
                protocol::METHOD_DISPOSE,
                Some(params),
                self.inner.config.request_timeout,
            )
            .await?
        {
            Exchange::Ok(_) => Ok(()),
            Exchange::Error(error) => Err(rpc_error_from_object(error, "dispose")),
        }
    }

    /// Dispatch a hook through the sidecar.
    ///
    /// Infallible hooks swallow sidecar errors; `tool.execute.before` maps them
    /// to [`PluginError::HookFailed`] so the runtime rejects the tool call.
    ///
    /// # Errors
    ///
    /// Transport failures ([`PluginError::Sidecar`]) and, for
    /// `tool.execute.before`, a plugin rejection.
    pub async fn hook(&self, hook: Hook, event: &mut HookEvent) -> Result<(), PluginError> {
        if !self.has_hook(hook) {
            return Ok(());
        }
        let params = json!({ "hook": hook.as_str(), "event": &*event });
        match self
            .inner
            .send_request(
                protocol::METHOD_HOOK,
                Some(params),
                self.inner.config.request_timeout,
            )
            .await
        {
            Ok(Exchange::Ok(value)) => {
                if !value.is_null() {
                    *event = value;
                }
                Ok(())
            }
            Ok(Exchange::Error(error)) => {
                if hook.can_fail() {
                    let plugin = error
                        .data
                        .as_ref()
                        .and_then(|data| data.get("plugin"))
                        .and_then(Value::as_str)
                        .unwrap_or("sidecar")
                        .to_string();
                    let metadata = error
                        .data
                        .as_ref()
                        .and_then(|data| data.get("metadata"))
                        .cloned();
                    Err(PluginError::HookFailed {
                        plugin,
                        hook,
                        message: error.message,
                        metadata,
                    })
                } else {
                    warn!(
                        target: "oc_plugin::sidecar",
                        hook = %hook,
                        error = %error.message,
                        "ignoring infallible hook error"
                    );
                    Ok(())
                }
            }
            Err(error) => Err(PluginError::Sidecar(error)),
        }
    }

    /// Dispatch a transform through the sidecar. A failure disables every
    /// plugin that declared the domain (or the one the sidecar named) and
    /// returns [`PluginError::TransformFailed`].
    ///
    /// # Errors
    ///
    /// Transport failures and transform rejections.
    pub async fn transform(
        &self,
        domain: Domain,
        doc: &mut TransformDoc,
    ) -> Result<(), PluginError> {
        if !self.has_transform(domain) {
            return Ok(());
        }
        let params = json!({ "domain": domain.as_str(), "doc": &*doc });
        match self
            .inner
            .send_request(
                protocol::METHOD_TRANSFORM,
                Some(params),
                self.inner.config.request_timeout,
            )
            .await
        {
            Ok(Exchange::Ok(value)) => {
                if !value.is_null() {
                    *doc = value;
                }
                Ok(())
            }
            Ok(Exchange::Error(error)) => {
                let plugin = self.disable_transformers(domain, &error);
                Err(PluginError::TransformFailed {
                    plugin,
                    domain,
                    message: error.message,
                })
            }
            Err(error) => Err(PluginError::Sidecar(error)),
        }
    }

    /// Dispatch an RPC call to the sidecar.
    ///
    /// # Errors
    ///
    /// [`RpcError::NamespaceNotFound`] when no active plugin exposes RPC, plus
    /// the sidecar's typed error mapping.
    pub async fn invoke(
        &self,
        namespace: &str,
        method: &str,
        input: Value,
    ) -> Result<Value, RpcError> {
        if !self.has_rpc() {
            return Err(RpcError::NamespaceNotFound {
                namespace: namespace.to_string(),
            });
        }
        let params = json!({ "namespace": namespace, "method": method, "input": input });
        match self
            .inner
            .send_request(
                protocol::METHOD_RPC,
                Some(params),
                self.inner.config.request_timeout,
            )
            .await?
        {
            Exchange::Ok(value) => Ok(value),
            Exchange::Error(error) => Err(rpc_error_from_object(error, method)),
        }
    }

    /// Graceful shutdown: `plugin.shutdown`, wait the grace period, then kill
    /// the process tree. Safe to call more than once.
    ///
    /// # Errors
    ///
    /// Currently infallible; the signature is kept so a future implementation
    /// can report a failure to reap the child.
    pub async fn shutdown(&self) -> Result<(), SidecarError> {
        if matches!(self.status(), SidecarStatus::Stopped) {
            return Ok(());
        }
        if self.status().is_ready() {
            let _ = self
                .inner
                .send_request(
                    protocol::METHOD_SHUTDOWN,
                    None,
                    self.inner.config.shutdown_grace,
                )
                .await;
        }
        self.inner.stop().await;
        Ok(())
    }

    fn disable_transformers(&self, domain: Domain, error: &RpcErrorObject) -> String {
        let named = error
            .data
            .as_ref()
            .and_then(|data| data.get("plugin"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut plugins = self.inner.plugins.lock();
        let mut disabled = Vec::new();
        for plugin in plugins.iter_mut() {
            if !plugin.state.is_active() || !plugin.transforms.contains(&domain) {
                continue;
            }
            if let Some(named) = &named
                && plugin.id != *named
            {
                continue;
            }
            plugin.state = PluginState::failed(error.message.clone());
            disabled.push(plugin.id.clone());
        }
        if disabled.is_empty() {
            named.unwrap_or_else(|| "sidecar".to_string())
        } else {
            disabled.join(",")
        }
    }
}
