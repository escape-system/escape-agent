//! Transport abstraction shared by the stdio and streamable-HTTP clients.

pub mod http;
pub mod stdio;

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{McpError, McpResult, RpcError};

/// Handles messages initiated by the MCP server.
#[async_trait]
pub trait TransportEventHandler: Send + Sync + std::fmt::Debug {
    /// Handles a server request and returns the JSON result (or a JSON-RPC error).
    async fn on_request(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError>;

    /// Handles a server notification.
    async fn on_notification(&self, method: &str, params: Option<Value>);

    /// The transport closed or the server exited.
    async fn on_close(&self, reason: String);

    /// A non-fatal transport error (malformed frame, background stream failure).
    fn on_error(&self, message: String);
}

/// A bidirectional MCP transport.
#[async_trait]
pub trait Transport: Send + Sync + std::fmt::Debug {
    /// Sends a request and waits for its response.
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> McpResult<Value>;

    /// Sends a notification (fire and forget, but surfaces write errors).
    async fn notify(&self, method: &str, params: Option<Value>) -> McpResult<()>;

    /// Installs the handler for server-initiated traffic.
    fn set_event_handler(&self, handler: std::sync::Arc<dyn TransportEventHandler>);

    /// Protocol revision echoed in the `MCP-Protocol-Version` header.
    fn set_protocol_version(&self, _version: &str) {}

    /// Session id minted by the server, when the transport has one.
    fn session_id(&self) -> Option<String> {
        None
    }

    /// Child process id for stdio servers.
    fn pid(&self) -> Option<u32> {
        None
    }

    /// Gracefully closes the transport and releases its resources.
    async fn shutdown(&self);
}

/// Creates fresh transports for reconnect-after-session-expiry.
#[async_trait]
pub trait TransportFactory: Send + Sync + std::fmt::Debug {
    /// Opens a new transport. Must not reuse any state from a previous session.
    async fn create(&self) -> McpResult<std::sync::Arc<dyn Transport>>;
}

/// Runs a request with the upstream transient-retry policy: 250 ms exponential
/// backoff, two extra attempts, `retry-after` honored, bounded by the deadline.
pub(crate) async fn with_retries<F, Fut>(
    deadline: tokio::time::Instant,
    timeout: Duration,
    mut run: F,
) -> McpResult<Value>
where
    F: FnMut(Duration) -> Fut,
    Fut: Future<Output = McpResult<Value>>,
{
    let mut attempt = 0_u32;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(McpError::Timeout(timeout));
        }
        match run(remaining).await {
            Ok(value) => return Ok(value),
            Err(error) if error.is_transient() && attempt < 2 => {
                let backoff = Duration::from_millis(250 * 2_u64.pow(attempt));
                let delay = error
                    .retry_after()
                    .map_or(backoff, |after| after.max(backoff));
                if tokio::time::Instant::now() + delay >= deadline {
                    return Err(error);
                }
                tracing::warn!(?delay, %error, "retrying MCP HTTP request");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Parses a `retry-after` header value in seconds.
#[must_use]
pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let value = value?.trim();
    value.parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = attempts.clone();
        let value = with_retries(
            tokio::time::Instant::now() + Duration::from_secs(5),
            Duration::from_secs(5),
            move |_| {
                let seen = seen.clone();
                async move {
                    let attempt = seen.fetch_add(1, Ordering::SeqCst);
                    if attempt < 2 {
                        Err(McpError::Http {
                            status: 503,
                            body: String::new(),
                            retry_after: Some(Duration::from_millis(1)),
                        })
                    } else {
                        Ok(serde_json::json!({"ok": true}))
                    }
                }
            },
        )
        .await
        .expect("eventually succeeds");
        assert_eq!(value["ok"], true);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_501_or_4xx() {
        for status in [501, 400, 403] {
            let attempts = Arc::new(AtomicU32::new(0));
            let seen = attempts.clone();
            let error = with_retries(
                tokio::time::Instant::now() + Duration::from_secs(5),
                Duration::from_secs(5),
                move |_| {
                    let seen = seen.clone();
                    async move {
                        seen.fetch_add(1, Ordering::SeqCst);
                        Err::<Value, _>(McpError::Http {
                            status,
                            body: String::new(),
                            retry_after: None,
                        })
                    }
                },
            )
            .await
            .expect_err("status is fatal");
            assert!(matches!(error, McpError::Http { status: s, .. } if s == status));
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn retry_after_seconds() {
        assert_eq!(parse_retry_after(Some("3")), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after(Some("bogus")), None);
        assert_eq!(parse_retry_after(None), None);
    }
}
