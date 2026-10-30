//! MCP session lifecycle: connect, transport attach, reconnect and shutdown.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use url::Url;

use crate::config::{McpServerConfig, expand_placeholders};
use crate::error::{McpError, McpResult};
use crate::transport::http::{HttpFactory, HttpOptions, HttpTransport};
use crate::transport::stdio::{StdioFactory, StdioOptions, StdioTransport};
use crate::transport::{Transport, TransportFactory};

use super::handler::ClientHandler;
use super::handshake::{apply_init, handshake, host_environment};
use super::lock;
use super::{ClientOptions, Inner, McpClient, McpStatus, State};

impl McpClient {
    /// Connects using a configured server entry.
    ///
    /// Disabled servers are rejected; callers represent them with
    /// [`McpStatus::Disabled`].
    pub async fn connect(
        name: &str,
        config: &McpServerConfig,
        options: ClientOptions,
    ) -> McpResult<Self> {
        if config.disabled() {
            return Err(McpError::NotConnected {
                server: name.to_owned(),
                reason: "disabled".to_owned(),
            });
        }
        let timeouts = options.timeouts.merged_with(&config.timeout());
        let (transport, factory): (Arc<dyn Transport>, Option<Arc<dyn TransportFactory>>) =
            match config {
                McpServerConfig::Local(local) => {
                    let cwd = match &local.cwd {
                        Some(cwd) if Path::new(cwd).is_absolute() => PathBuf::from(cwd),
                        Some(cwd) => options.directory.join(cwd),
                        None => options.directory.clone(),
                    };
                    let environment = expand_placeholders(&local.environment, &host_environment())?;
                    let stdio = StdioOptions {
                        server: name.to_owned(),
                        command: local.command.clone(),
                        cwd,
                        environment,
                    };
                    let transport = StdioTransport::spawn(stdio.clone())?;
                    (transport, Some(Arc::new(StdioFactory::new(stdio))))
                }
                McpServerConfig::Remote(remote) => {
                    let url = Url::parse(&remote.url)
                        .map_err(|error| McpError::Config(format!("invalid MCP URL: {error}")))?;
                    let headers = expand_placeholders(&remote.headers, &host_environment())?;
                    let http = HttpOptions {
                        server: name.to_owned(),
                        url,
                        headers,
                        protocol_version: None,
                    };
                    let transport = HttpTransport::new(http.clone())?;
                    (transport, Some(Arc::new(HttpFactory::new(http))))
                }
            };
        let options = ClientOptions {
            timeouts,
            protocol: config.protocol(),
            ..options
        };
        Self::connect_transport(name, transport, factory, options).await
    }

    /// Connects over an already-open transport (used by tests and attached
    /// transports). `factory`, when present, enables one reconnect on session
    /// expiry.
    pub async fn connect_transport(
        name: &str,
        transport: Arc<dyn Transport>,
        factory: Option<Arc<dyn TransportFactory>>,
        options: ClientOptions,
    ) -> McpResult<Self> {
        let inner = Arc::new(Inner {
            server: name.to_owned(),
            directory: options.directory.clone(),
            client_info: options.client_info.clone(),
            protocol: options.protocol,
            timeouts: options.timeouts.clone(),
            elicitation: options.elicitation.clone(),
            transport: RwLock::new(transport.clone()),
            factory,
            state: Mutex::new(State::default()),
            status_handler: RwLock::new(options.status_handler.clone()),
            change_handler: RwLock::new(options.change_handler.clone()),
            reconnect_lock: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        let client = Self {
            inner: inner.clone(),
        };
        inner.set_status(McpStatus::Pending);
        transport.set_event_handler(Arc::new(ClientHandler::new(&inner, 1)));
        let result = async {
            let init = handshake(&inner, &transport).await?;
            transport.set_protocol_version(&init.protocol_version);
            apply_init(&inner, init);
            transport.notify("notifications/initialized", None).await?;
            client.refresh_tools().await?;
            inner.set_status(McpStatus::Connected);
            Ok(())
        }
        .await;
        match result {
            Ok(()) => Ok(client),
            Err(error) => {
                let status = match &error {
                    McpError::NeedsAuth { message, .. } => McpStatus::NeedsAuth {
                        error: message.clone(),
                    },
                    other => McpStatus::Failed {
                        error: other.to_string(),
                    },
                };
                inner.set_status(status);
                transport.shutdown().await;
                Err(error)
            }
        }
    }

    pub(super) async fn reconnect(&self, observed_generation: u64) -> McpResult<()> {
        let _guard = self.inner.reconnect_lock.lock().await;
        if self.inner.generation.load(Ordering::SeqCst) != observed_generation {
            return Ok(());
        }
        let Some(factory) = self.inner.factory.clone() else {
            self.inner.set_status(McpStatus::Failed {
                error: "MCP session expired and the transport cannot reconnect".to_owned(),
            });
            return Err(McpError::NotConnected {
                server: self.inner.server.clone(),
                reason: "session expired".to_owned(),
            });
        };
        let previous = self.inner.current_transport();
        let transport = factory.create().await?;
        let generation = observed_generation + 1;
        transport.set_event_handler(Arc::new(ClientHandler::new(&self.inner, generation)));
        let init = match handshake(&self.inner, &transport).await {
            Ok(init) => init,
            Err(error) => {
                transport.shutdown().await;
                self.inner.set_status(McpStatus::Failed {
                    error: error.to_string(),
                });
                return Err(error);
            }
        };
        transport.set_protocol_version(&init.protocol_version);
        apply_init(&self.inner, init);
        if let Err(error) = transport.notify("notifications/initialized", None).await {
            transport.shutdown().await;
            self.inner.set_status(McpStatus::Failed {
                error: error.to_string(),
            });
            return Err(error);
        }
        *self
            .inner
            .transport
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = transport;
        self.inner.generation.store(generation, Ordering::SeqCst);
        tokio::spawn(async move {
            previous.shutdown().await;
        });
        self.inner.set_status(McpStatus::Connected);
        Ok(())
    }

    /// Closes the transport and marks the client disabled.
    pub async fn shutdown(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.current_transport().shutdown().await;
        lock(&self.inner.state).clear_catalog();
        self.inner.set_status(McpStatus::Disabled);
    }
}
