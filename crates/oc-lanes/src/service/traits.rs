//! Host-supplied boundaries the scheduler calls: the runner, the event sink and
//! the persisted-ticket source.

use std::future::Future;
use std::pin::Pin;

use crate::error::RestoreError;
use crate::types::{RunnerResult, Ticket, TicketRecord};

/// A boxed runner future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Source of persisted tickets for [`crate::LaneService::restore`].
///
/// `oc-lanes` has no store dependency: the host adapts its `lane_ticket` table
/// to this trait (see `oc_store::Store::lane_tickets_all` and the CLI wiring).
pub trait LaneTicketSource: Send + Sync {
    /// Loads every persisted ticket, newest first; ordering is not significant.
    fn load(&self) -> BoxFuture<'_, Result<Vec<TicketRecord>, RestoreError>>;
}

/// Executes one ticket.
///
/// The runner receives the running ticket and may look up the cancellation
/// token with the ticket id through [`crate::LaneService::cancellation_token`]
/// (or a [`crate::CancellationTokens`] handle captured at construction). A
/// panic is caught by the service and becomes [`crate::TicketState::Failed`].
pub trait LaneRunner: Send + Sync {
    /// Runs the command. A non-zero [`RunnerResult::exit`] marks the ticket
    /// failed.
    fn run<'a>(&'a self, ticket: &'a Ticket) -> BoxFuture<'a, RunnerResult>;
}

/// Receives lifecycle changes: queued, started, finished and cancelled.
///
/// Calls are made without holding the service lock, so an implementation may
/// call back into the service. The callback must be quick; it runs on the
/// dispatcher or on the enqueueing task.
pub trait LaneEvents: Send + Sync {
    /// One ticket changed state (or priority).
    fn changed(&self, ticket: &Ticket);
}
