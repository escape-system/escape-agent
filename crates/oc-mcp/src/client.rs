//! MCP client lifecycle: handshake, catalogs, calls, status and reconnect.

mod accessors;
mod catalog;
mod handler;
mod handshake;
mod request;
mod session;
mod types;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use serde_json::Value;

use crate::config::{Protocol, TimeoutConfig};
use crate::tool::McpTool;
use crate::transport::{Transport, TransportFactory};

use types::{ChangeHandler, StatusHandler};

pub use types::{
    ClientInfo, ClientOptions, ElicitationHandler, McpChange, McpServerInfo, McpStatus,
    McpStatusEvent, ServerInfo,
};

#[derive(Debug, Default)]
struct State {
    status: Option<McpStatus>,
    server_info: Option<ServerInfo>,
    instructions: Option<String>,
    protocol_version: Option<String>,
    capabilities: Value,
    tools: Vec<McpTool>,
    prompts: Vec<Value>,
    resources: Vec<Value>,
    templates: Vec<Value>,
}

impl State {
    fn clear_catalog(&mut self) {
        self.tools.clear();
        self.prompts.clear();
        self.resources.clear();
        self.templates.clear();
    }
}

struct Inner {
    server: String,
    directory: PathBuf,
    client_info: ClientInfo,
    protocol: Protocol,
    timeouts: TimeoutConfig,
    elicitation: Option<Arc<dyn ElicitationHandler>>,
    transport: RwLock<Arc<dyn Transport>>,
    factory: Option<Arc<dyn TransportFactory>>,
    state: Mutex<State>,
    status_handler: RwLock<Option<StatusHandler>>,
    change_handler: RwLock<Option<ChangeHandler>>,
    reconnect_lock: tokio::sync::Mutex<()>,
    generation: AtomicU64,
    closed: AtomicBool,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("server", &self.server)
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Inner {
    fn current_transport(&self) -> Arc<dyn Transport> {
        self.transport
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_status(&self, status: McpStatus) {
        lock(&self.state).status = Some(status.clone());
        let handler = self
            .status_handler
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(handler) = handler {
            handler(McpStatusEvent {
                server: self.server.clone(),
                status,
            });
        }
    }

    fn emit_change(&self, change: McpChange) {
        let handler = self
            .change_handler
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(handler) = handler {
            handler(change);
        }
    }
}

#[derive(Debug)]
struct InitResult {
    protocol_version: String,
    capabilities: Value,
    server_info: Option<ServerInfo>,
    instructions: Option<String>,
}

/// A connected MCP server client.
#[derive(Debug, Clone)]
pub struct McpClient {
    inner: Arc<Inner>,
}
