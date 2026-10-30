//! Native plugin trait and ordered registry.
//!
//! # Activation order
//!
//! Dispatch follows upstream activation order, independent of registration
//! calls:
//!
//! `builtin pre` → `sdk` → `instance` → `packages` → `builtin post`
//!
//! Within a tier, plugins dispatch in registration order. A duplicate id keeps
//! the first registration and marks the rest `failed` (`reason: "duplicate
//! id"`). The same rule protects RPC namespaces: the first plugin to claim a
//! namespace owns it.
//!
//! Hooks are sequential and see earlier mutations. Only
//! [`crate::hook::Hook::ToolExecuteBefore`] may fail
//! and abort the call; errors from other hooks are logged and ignored. A panic
//! in plugin code is caught, the plugin is disabled, and dispatch continues.

use std::any::Any;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};
use serde_json::Value;

use crate::domain::Domain;
use crate::error::PluginError;
use crate::hook::{Hook, HookEvent, TransformDoc};
use crate::info::{PluginFeatures, PluginInfo, PluginSource, PluginState};
use crate::rpc::{RpcError, RpcNamespace};

/// A plugin compiled into the server. No dynamic loading, no `dlopen`.
pub trait Plugin: Send + Sync {
    /// Stable id (package name, builtin id, …). Unique per registry.
    fn id(&self) -> &str;

    /// Where the plugin came from; defaults to `builtin`.
    fn source(&self) -> PluginSource {
        PluginSource::Builtin
    }

    /// Entrypoints the plugin exposes; defaults to `server`.
    fn features(&self) -> PluginFeatures {
        PluginFeatures::server()
    }

    /// Transform one document. Runs for every transform, so a plugin that does
    /// not care about `domain` must return without touching `doc`.
    fn transform(&self, _domain: Domain, _doc: &mut TransformDoc) {}

    /// Observe or mutate one hook event. Only `tool.execute.before` may return
    /// an error; the runtime rejects the tool call when it does.
    fn hook(&self, _hook: Hook, _event: &mut HookEvent) -> Result<(), PluginError> {
        Ok(())
    }

    /// Optional RPC namespace owned by this plugin.
    fn rpc(&self) -> Option<&dyn RpcNamespace> {
        None
    }
}

/// Activation tier. Dispatch order is the variant order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// First-party plugins that must see config before anyone else.
    BuiltinPre,
    /// Plugins shipped by an SDK integration.
    Sdk,
    /// The one plugin instance named by the session/config.
    Instance,
    /// Packages loaded from `plugins` config and the plugin directories.
    Package,
    /// First-party plugins that must see the final document.
    BuiltinPost,
}

impl Tier {
    /// Short name used in logs and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BuiltinPre => "builtin-pre",
            Self::Sdk => "sdk",
            Self::Instance => "instance",
            Self::Package => "package",
            Self::BuiltinPost => "builtin-post",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

struct Entry {
    seq: u64,
    tier: Tier,
    id: String,
    plugin: Arc<dyn Plugin>,
    source: PluginSource,
    features: PluginFeatures,
    state: Mutex<PluginState>,
}

impl Entry {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            id: self.id.clone(),
            source: self.source.clone(),
            features: self.features,
            state: self.state.lock().clone(),
        }
    }

    fn is_active(&self) -> bool {
        self.state.lock().is_active()
    }
}

/// Ordered registry of native plugins.
pub struct PluginRegistry {
    entries: RwLock<Vec<Arc<Entry>>>,
    guards: RwLock<std::collections::BTreeSet<String>>,
    next_seq: AtomicU64,
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for PluginRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginRegistry")
            .field(
                "plugins",
                &self
                    .entries
                    .read()
                    .iter()
                    .map(|entry| (entry.tier, entry.id.as_str()))
                    .collect::<Vec<_>>(),
            )
            .field("guards", &self.guards.read().clone())
            .field("next_seq", &self.next_seq.load(Ordering::Relaxed))
            .finish()
    }
}

impl PluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            guards: RwLock::new(std::collections::BTreeSet::new()),
            next_seq: AtomicU64::new(0),
        }
    }

    /// Register a plugin. Returns its inventory row, including the `failed`
    /// state when the id or RPC namespace is already taken.
    pub fn register<P>(&self, tier: Tier, plugin: P) -> PluginInfo
    where
        P: Plugin + 'static,
    {
        self.register_arc(tier, Arc::new(plugin))
    }

    /// Register an already shared plugin.
    pub fn register_arc(&self, tier: Tier, plugin: Arc<dyn Plugin>) -> PluginInfo {
        let id = plugin.id().to_string();
        let source = plugin.source();
        let features = plugin.features();
        let state = self.initial_state(&id, &plugin);
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(Entry {
            seq,
            tier,
            id: id.clone(),
            plugin,
            source: source.clone(),
            features,
            state: Mutex::new(state.clone()),
        });
        {
            let mut entries = self.entries.write();
            let position = entries.partition_point(|other| (other.tier, other.seq) <= (tier, seq));
            entries.insert(position, entry);
        }
        if !state.is_active() {
            tracing::warn!(target: "oc_plugin", plugin = %id, reason = state.error().unwrap_or(""), "plugin registered as failed");
        }
        PluginInfo {
            id,
            source,
            features,
            state,
        }
    }

    fn initial_state(&self, id: &str, plugin: &Arc<dyn Plugin>) -> PluginState {
        if id.trim().is_empty() {
            return PluginState::failed("missing plugin id");
        }
        let entries = self.entries.read();
        if entries.iter().any(|entry| entry.id == id) {
            return PluginState::failed("duplicate id");
        }
        if let Some(namespace) = plugin.rpc().map(|rpc| rpc.namespace().to_string()) {
            if namespace.trim().is_empty() {
                return PluginState::failed("missing rpc namespace");
            }
            let taken = entries.iter().any(|entry| {
                entry.is_active()
                    && entry
                        .plugin
                        .rpc()
                        .is_some_and(|other| other.namespace() == namespace)
            });
            if taken {
                return PluginState::failed(format!("duplicate rpc namespace: {namespace}"));
            }
        }
        PluginState::Active
    }

    /// Run every active plugin's transform, in tier order.
    pub fn transform(&self, domain: Domain, doc: &mut TransformDoc) {
        for entry in self.snapshot() {
            if !entry.is_active() {
                continue;
            }
            match catch_unwind(AssertUnwindSafe(|| entry.plugin.transform(domain, doc))) {
                Ok(()) => {}
                Err(payload) => {
                    let reason =
                        format!("panic in transform {domain}: {}", panic_message(&*payload));
                    Self::disable(&entry, &reason);
                }
            }
        }
    }

    /// Run every active plugin's hook, in tier order.
    ///
    /// Only `tool.execute.before` may return an error; it aborts dispatch and is
    /// returned to the caller. Other hook errors are logged and ignored so one
    /// plugin cannot break the runtime.
    pub fn hook(&self, hook: Hook, event: &mut HookEvent) -> Result<(), PluginError> {
        for entry in self.snapshot() {
            if !entry.is_active() {
                continue;
            }
            match catch_unwind(AssertUnwindSafe(|| entry.plugin.hook(hook, event))) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    if hook.can_fail() {
                        return Err(error);
                    }
                    tracing::warn!(
                        target: "oc_plugin",
                        plugin = %entry.id,
                        hook = %hook,
                        error = %error,
                        "ignoring infallible hook error"
                    );
                }
                Err(payload) => {
                    let reason = format!("panic in hook {hook}: {}", panic_message(&*payload));
                    Self::disable(&entry, &reason);
                    if hook.can_fail() {
                        return Err(PluginError::HookFailed {
                            plugin: entry.id.clone(),
                            hook,
                            message: reason,
                            metadata: None,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Dispatch an RPC call to the namespace owner.
    pub fn invoke(&self, namespace: &str, method: &str, input: Value) -> Result<Value, RpcError> {
        let entry = self
            .snapshot()
            .into_iter()
            .find(|entry| {
                entry.is_active()
                    && entry
                        .plugin
                        .rpc()
                        .is_some_and(|rpc| rpc.namespace() == namespace)
            })
            .ok_or_else(|| RpcError::NamespaceNotFound {
                namespace: namespace.to_string(),
            })?;
        let rpc = entry
            .plugin
            .rpc()
            .ok_or_else(|| RpcError::NamespaceNotFound {
                namespace: namespace.to_string(),
            })?;
        let input_for_panic = input.clone();
        match catch_unwind(AssertUnwindSafe(|| rpc.invoke(method, input))) {
            Ok(result) => result,
            Err(payload) => {
                let reason = format!(
                    "panic in rpc {namespace}.{method}: {}",
                    panic_message(&*payload)
                );
                Self::disable(&entry, &reason);
                Err(RpcError::Failed {
                    message: reason,
                    data: Some(input_for_panic),
                })
            }
        }
    }

    /// Whether an active plugin serves `namespace`.
    #[must_use]
    pub fn has_namespace(&self, namespace: &str) -> bool {
        self.snapshot().iter().any(|entry| {
            entry.is_active()
                && entry
                    .plugin
                    .rpc()
                    .is_some_and(|rpc| rpc.namespace() == namespace)
        })
    }

    /// Namespaces owned by active plugins, in dispatch order.
    #[must_use]
    pub fn namespaces(&self) -> Vec<String> {
        self.snapshot()
            .iter()
            .filter(|entry| entry.is_active())
            .filter_map(|entry| entry.plugin.rpc().map(|rpc| rpc.namespace().to_string()))
            .collect()
    }

    /// Disable a plugin by id. Returns whether a row changed.
    pub fn set_state(&self, id: &str, state: PluginState) -> bool {
        let entries = self.snapshot();
        let Some(entry) = entries.into_iter().find(|entry| entry.id == id) else {
            return false;
        };
        let mut current = entry.state.lock();
        if *current == state {
            return false;
        }
        *current = state;
        true
    }

    /// Guard an id against removal.
    pub fn guard(&self, id: impl Into<String>) {
        self.guards.write().insert(id.into());
    }

    /// Remove an explicit guard. Implicit guards (`opencode.provider.*`) stay.
    pub fn unguard(&self, id: &str) {
        self.guards.write().remove(id);
    }

    /// Whether `id` is guarded, implicitly or explicitly.
    #[must_use]
    pub fn is_guarded(&self, id: &str) -> bool {
        is_guarded_id(id) || self.guards.read().contains(id)
    }

    /// Remove every entry with `id`. Guarded ids return
    /// [`PluginError::Guarded`].
    pub fn remove(&self, id: &str) -> Result<bool, PluginError> {
        if self.is_guarded(id) {
            return Err(PluginError::Guarded { id: id.to_string() });
        }
        let mut entries = self.entries.write();
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        Ok(before != entries.len())
    }

    /// Inventory rows, in dispatch order.
    #[must_use]
    pub fn inventory(&self) -> Vec<PluginInfo> {
        self.snapshot().iter().map(|entry| entry.info()).collect()
    }

    /// Inventory row for one id.
    #[must_use]
    pub fn info(&self, id: &str) -> Option<PluginInfo> {
        self.snapshot()
            .into_iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.info())
    }

    /// Whether the registry has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// Number of entries, including failed ones.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    fn snapshot(&self) -> Vec<Arc<Entry>> {
        self.entries.read().clone()
    }

    fn disable(entry: &Entry, reason: &str) {
        *entry.state.lock() = PluginState::failed(reason);
        tracing::warn!(
            target: "oc_plugin",
            plugin = %entry.id,
            reason,
            "plugin disabled"
        );
    }
}

/// Ids the runtime always protects from removal.
///
/// `opencode.provider.*` plugins carry provider policy; removing them would
/// silently change provider behaviour.
#[must_use]
pub fn is_guarded_id(id: &str) -> bool {
    id.starts_with("opencode.provider.")
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}
