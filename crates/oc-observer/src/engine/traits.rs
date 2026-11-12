//! Host-supplied boundaries the loop calls: the model, the repair executor and
//! the notice sink. Time comes from [`crate::clock::Clock`].

use std::pin::Pin;

use crate::config::ObserverConfig;
use crate::error::ObserverError;
use crate::verdict::ObserverVerdict;

use super::notice::RepairOutcome;

/// A boxed, sendable future used by the host-supplied traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The observer's model call.
pub trait ObserverModel: Send + Sync {
    /// Judge one rendered input document. The returned text is strict JSON
    /// parsed by [`crate::parse_verdict`].
    fn judge<'a>(
        &'a self,
        input: &'a str,
        cfg: &'a ObserverConfig,
    ) -> BoxFuture<'a, Result<String, ObserverError>>;
}

/// Host-side execution of a validated repair command.
pub trait RepairExecutor: Send + Sync {
    /// Apply the repair (re-run a tool, restart a child, compact, …).
    fn apply<'a>(
        &'a self,
        verdict: &'a ObserverVerdict,
    ) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>>;
}

/// Notice sink. The server maps these to `observer.*` events and synthetic
/// messages; a test records them in a vector.
pub trait ObserverNotifier: Send + Sync {
    /// Publish one notice.
    fn notify(&self, event: super::notice::ObserverNotice);
}
