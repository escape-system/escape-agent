//! Priority-queue lane scheduler with semaphore-enforced concurrency.

mod coalesce;
mod dispatch;
mod enqueue;
mod lifecycle;
mod restore;
mod tickets;
mod tokens;
mod traits;

use std::{
    collections::{BinaryHeap, HashMap},
    fmt,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use oc_observer::Clock;
use tokio::sync::{Mutex, Notify, Semaphore, watch};
use tokio_util::sync::CancellationToken;

use crate::{config::LaneConfig, error::LaneError, types::Ticket};

use coalesce::CoalesceKey;

pub use tokens::CancellationTokens;
pub use traits::{BoxFuture, LaneEvents, LaneRunner, LaneTicketSource};

/// Build-lane scheduler.
///
/// One dispatcher task per lane acquires a semaphore permit before popping the
/// highest-priority queued ticket, so concurrency is enforced in Rust and does
/// not depend on model or tool cooperation. Equal priorities are FIFO.
#[derive(Clone)]
pub struct LaneService {
    inner: Arc<Inner>,
}

impl fmt::Debug for LaneService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaneService")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

impl LaneService {
    /// Builds a service. A Tokio runtime is required for [`Self::enqueue`].
    pub fn new(
        config: LaneConfig,
        runner: Arc<dyn LaneRunner>,
        events: Arc<dyn LaneEvents>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::assemble(config, runner, events, clock, 0)
    }

    fn assemble(
        config: LaneConfig,
        runner: Arc<dyn LaneRunner>,
        events: Arc<dyn LaneEvents>,
        clock: Arc<dyn Clock>,
        next_id: u64,
    ) -> Self {
        let mut lanes = HashMap::new();
        for lane in config.lanes.keys() {
            lanes.insert(lane.clone(), LaneState::new(config.concurrency_for(lane)));
        }
        Self {
            inner: Arc::new(Inner {
                config,
                runner,
                events,
                clock,
                tokens: CancellationTokens::default(),
                next_id: AtomicU64::new(next_id),
                state: Mutex::new(State {
                    shutdown: false,
                    sequence: 0,
                    tickets: HashMap::new(),
                    lanes,
                }),
            }),
        }
    }

    /// Immutable configuration.
    pub fn config(&self) -> &LaneConfig {
        &self.inner.config
    }

    /// Shared cancellation-token table for runners.
    pub fn tokens(&self) -> CancellationTokens {
        self.inner.tokens.clone()
    }
}

pub(super) fn timeout_error(id: &str, timeout: Duration) -> LaneError {
    LaneError::Timeout {
        id: id.to_owned(),
        timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
    }
}

struct Inner {
    config: LaneConfig,
    runner: Arc<dyn LaneRunner>,
    events: Arc<dyn LaneEvents>,
    clock: Arc<dyn Clock>,
    tokens: CancellationTokens,
    next_id: AtomicU64,
    state: Mutex<State>,
}

struct State {
    shutdown: bool,
    sequence: u64,
    tickets: HashMap<String, TicketEntry>,
    lanes: HashMap<String, LaneState>,
}

struct TicketEntry {
    ticket: Ticket,
    sender: watch::Sender<Ticket>,
    cancel: CancellationToken,
    sequence: u64,
    /// Coalescing key, present only when the caller supplied a `tree_rev`.
    coalesce: Option<CoalesceKey>,
}

struct LaneState {
    semaphore: Arc<Semaphore>,
    notify: Arc<Notify>,
    queue: BinaryHeap<Queued>,
    active: usize,
    /// Whether a dispatcher task for this lane has been started.
    ///
    /// **Invariant: at most one dispatcher task exists per lane.** The idle
    /// path in `Inner::dispatch` parks on `notify` while still holding the
    /// semaphore permit it acquired before its first pop, and reuses that
    /// permit for the next ticket. That is only safe because no second
    /// dispatcher can exist: on a `concurrency = 1` lane a second dispatcher
    /// would block forever in `acquire_owned()` on the permit the parked one
    /// holds, the parked one would never be woken to release it, and the lane
    /// would deadlock. Reset this latch after startup and you get exactly that.
    ///
    /// Lifecycle: `false` in `LaneState::new`; set by
    /// `LaneState::claim_dispatcher` before the dispatcher task is spawned and
    /// by `Inner::resume_dispatchers` for restored lanes. It is a one-way
    /// latch: nothing ever clears it again, so a lane never gets a second
    /// dispatcher.
    dispatcher_started: bool,
}

impl LaneState {
    fn new(concurrency: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(concurrency.max(1))),
            notify: Arc::new(Notify::new()),
            queue: BinaryHeap::new(),
            active: 0,
            dispatcher_started: false,
        }
    }

    /// Claims the lane's dispatcher slot.
    ///
    /// Returns `true` when the caller must spawn a dispatcher, and `false`
    /// when one was already started (the caller then only has to wake it).
    /// The latch is set in the same step that reports the claim, so one lane
    /// can never report two spawns. Callers hold the state lock.
    fn claim_dispatcher(&mut self) -> bool {
        if self.dispatcher_started {
            return false;
        }
        self.dispatcher_started = true;
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Queued {
    priority: i32,
    sequence: u64,
    id: String,
}

impl Ord for Queued {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.sequence.cmp(&self.sequence))
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
