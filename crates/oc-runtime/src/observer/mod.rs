//! Runtime wiring for `oc-observer`.
//!
//! The runtime feeds flight strips to an [`ObserverEngine`] and hosts the two
//! long-lived tasks the engine needs:
//!
//! - one ticker per root session, started on the first [`Runtime::notify`],
//!   which builds the tree/goal inputs and applies non-repair verdicts;
//! - one repair worker, started by [`Runtime::start`], which drains
//!   [`RepairCommand`]s produced by the host [`RepairExecutor`].
//!
//! The observer never sees tool arguments or output. Only [`FlightStrip`]
//! fields cross the boundary.

mod host;
mod repair;
mod repair_worker;
mod strips;
mod tick;

pub use host::ObserverBinding;
pub(crate) use host::ObserverHost;
pub use repair::{RepairChannel, RepairCommand};
pub(crate) use strips::{flight_strip, strip_result};

/// Upper bound on tree rows fed to one observer tick.
const MAX_TREE_ROWS: usize = 100;
/// Floor for the ticker interval so a zero config cannot spin.
const MIN_TICK_MS: u64 = 10;
