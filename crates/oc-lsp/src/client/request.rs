//! Request correlation, notifications and the per-request timeout.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::error::LspError;
use crate::rpc::{self, RequestId, RpcError};

use super::json::without_null_params;
use super::{Inner, LspClient};

impl LspClient {
    /// Send a request and await its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let timeout = self.inner.request_timeout;
        self.inner.request_timeout(method, params, timeout).await
    }

    /// Send a notification.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.inner.touch();
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let message = without_null_params(message);
        self.inner.write(&message).await?;
        Ok(())
    }
}

impl Inner {
    pub(super) async fn write(&self, message: &Value) -> Result<(), RpcError> {
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(RpcError::Closed);
        };
        rpc::write_message(stdin, message).await
    }

    pub(super) async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().insert(id.clone(), sender);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let message = without_null_params(message);
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.touch();
        let written = self.write(&message).await;
        if let Err(error) = written {
            self.pending.lock().remove(&id);
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(error.into());
        }
        let outcome = tokio::time::timeout(timeout, receiver).await;
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.touch();
        match outcome {
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(LspError::Timeout {
                    method: method.to_owned(),
                    millis: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                })
            }
            Ok(Err(_)) => {
                self.pending.lock().remove(&id);
                Err(RpcError::Closed.into())
            }
            Ok(Ok(Err(error))) => Err(error.into()),
            Ok(Ok(Ok(result))) => Ok(result),
        }
    }

    pub(super) fn fail_pending(&self) {
        let pending = std::mem::take(&mut *self.pending.lock());
        for (_, sender) in pending {
            let _ = sender.send(Err(RpcError::Closed));
        }
    }

    pub(super) async fn terminate_child(&self) {
        let mut guard = self.child.lock().await;
        if let Some(child) = guard.as_mut()
            && tokio::time::timeout(Duration::from_secs(1), child.wait())
                .await
                .is_err()
        {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
        }
        guard.take();
    }
}
