//! Shared cancellation-token table keyed by ticket id.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use tokio_util::sync::CancellationToken;

use super::LaneService;

/// Shared cancellation-token table keyed by ticket id.
///
/// Cheap to clone; a runner can hold one handle and resolve
/// [`LaneService::cancellation_token`] without an `Arc<LaneService>` cycle.
#[derive(Debug, Clone, Default)]
pub struct CancellationTokens {
    inner: Arc<StdMutex<HashMap<String, CancellationToken>>>,
}

impl CancellationTokens {
    /// Returns the token for a live ticket, if any.
    pub fn get(&self, id: &str) -> Option<CancellationToken> {
        self.lock().get(id).cloned()
    }

    /// Returns whether the ticket's token has been cancelled.
    pub fn is_cancelled(&self, id: &str) -> bool {
        self.get(id).is_some_and(|token| token.is_cancelled())
    }

    pub(super) fn insert(&self, id: &str, token: CancellationToken) {
        self.lock().insert(id.to_owned(), token);
    }

    pub(super) fn remove(&self, id: &str) {
        self.lock().remove(id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancellationToken>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl LaneService {
    /// Looks up the cancellation token for a ticket.
    pub async fn cancellation_token(&self, id: &str) -> Option<CancellationToken> {
        if !self.inner.state.lock().await.tickets.contains_key(id) {
            return None;
        }
        self.inner.tokens.get(id)
    }
}
