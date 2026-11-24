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
///
/// # Receipt contract
///
/// The future must not resolve until the host knows whether the repair was
/// performed:
///
/// - [`RepairOutcome::applied`] `= true` means the repair happened. Only then
///   does the engine publish `observer.repaired` (and the host write the
///   `observer_repair` audit row).
/// - `applied = false` means the host refused (unknown model, nothing to
///   cancel, unimplemented kind, …). [`RepairOutcome::detail`] carries the
///   reason. The engine publishes `ObserverNotice::RepairFailed` instead.
/// - `Err(ObserverError::Executor)` means the attempt failed or cannot be
///   implemented; the reason is published as a failure.
///
/// The engine bounds the wait with
/// [`crate::ObserverConfig::repair_ack_timeout_ms`]; a future that has not
/// resolved by then is treated as **unconfirmed**, which is a failure. A host
/// that queues the command and resolves immediately is therefore no longer
/// able to publish a success it cannot back up.
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
