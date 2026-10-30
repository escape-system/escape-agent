//! [`EventSink`] over the server's SSE broadcast channel.
//!
//! The runtime persists durable events itself and only needs somewhere to
//! publish the finished envelope; this sink forwards it unchanged. It never
//! appends to the store.

use std::sync::Arc;

use oc_runtime::EventSink;
use oc_types::EventEnvelope;
use tokio::sync::broadcast;

use crate::AppState;

/// Publishes runtime events to every SSE subscriber.
#[derive(Debug, Clone)]
pub struct BusSink {
    tx: broadcast::Sender<Arc<EventEnvelope>>,
}

impl BusSink {
    #[must_use]
    pub fn new(tx: broadcast::Sender<Arc<EventEnvelope>>) -> Self {
        Self { tx }
    }

    /// Build a sink that shares an existing server state's bus.
    #[must_use]
    pub fn from_state(state: &AppState) -> Self {
        Self::new(state.bus().clone())
    }

    #[must_use]
    pub fn bus(&self) -> &broadcast::Sender<Arc<EventEnvelope>> {
        &self.tx
    }
}

impl EventSink for BusSink {
    fn publish(&self, event: EventEnvelope) {
        // A send error only means no subscriber is connected.
        let _ = self.tx.send(Arc::new(event));
    }
}
