//! Request correlation: request id allocation, the pending map, and response deadlines.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::oneshot;

use super::protocol::{self, Exchange};
use super::register::millis;
use super::{Inner, SidecarStatus};
use crate::rpc::RpcError;

impl Inner {
    pub(super) async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Exchange, RpcError> {
        match &*self.status.lock() {
            SidecarStatus::Failed(reason) => {
                return Err(RpcError::Unavailable {
                    message: reason.clone(),
                });
            }
            SidecarStatus::Stopped => {
                return Err(RpcError::Unavailable {
                    message: "sidecar stopped".to_string(),
                });
            }
            SidecarStatus::Starting | SidecarStatus::Ready => {}
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().insert(id, sender);
        let frame = protocol::request_frame(id, method, params.as_ref());
        if self.outbound.send(frame).is_err() {
            self.pending.lock().remove(&id);
            return Err(RpcError::Unavailable {
                message: "sidecar output closed".to_string(),
            });
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => {
                self.pending.lock().remove(&id);
                Err(RpcError::Unavailable {
                    message: "sidecar closed the request".to_string(),
                })
            }
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(RpcError::Timeout {
                    method: method.to_string(),
                    timeout_ms: millis(timeout),
                })
            }
        }
    }
}
