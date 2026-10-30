//! Priority-queue lane scheduler with semaphore-enforced concurrency.

use std::{
    collections::{BinaryHeap, HashMap},
    fmt,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::FutureExt;
use oc_observer::Clock;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    config::LaneConfig,
    error::LaneError,
    types::{LaneStatus, NewTicket, RunnerResult, Ticket, TicketId, TicketState},
};

/// A boxed runner future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Executes one ticket.
///
/// The runner receives the running ticket and may look up the cancellation
/// token with the ticket id through [`LaneService::cancellation_token`] (or a
/// [`CancellationTokens`] handle captured at construction). A panic is caught
/// by the service and becomes [`TicketState::Failed`].
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

    fn insert(&self, id: &str, token: CancellationToken) {
        self.lock().insert(id.to_owned(), token);
    }

    fn remove(&self, id: &str) {
        self.lock().remove(id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancellationToken>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

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
                next_id: AtomicU64::new(0),
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

    /// Looks up the cancellation token for a ticket.
    pub async fn cancellation_token(&self, id: &str) -> Option<CancellationToken> {
        if !self.inner.state.lock().await.tickets.contains_key(id) {
            return None;
        }
        self.inner.tokens.get(id)
    }

    /// Queues a ticket and starts the lane's dispatcher on first use.
    ///
    /// `max_queue` counts queued tickets only; running tickets do not consume
    /// the limit.
    pub async fn enqueue(&self, lane: String, ticket: NewTicket) -> Result<TicketId, LaneError> {
        if lane.trim().is_empty() {
            return Err(LaneError::EmptyLane);
        }
        let NewTicket {
            command,
            cwd,
            priority,
            owner_session,
            label: _,
        } = ticket;
        let id = format!(
            "tkt_{}",
            self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1
        );
        let now = self.inner.clock.now_ms();
        let cancel = CancellationToken::new();
        let (queued, lane_name, start) = {
            let mut state = self.inner.state.lock().await;
            if state.shutdown {
                return Err(LaneError::Shutdown);
            }
            let concurrency = self.inner.config.concurrency_for(&lane);
            let queued_len = state
                .lanes
                .entry(lane.clone())
                .or_insert_with(|| LaneState::new(concurrency))
                .queue
                .len();
            if self.inner.config.max_queue > 0 && queued_len >= self.inner.config.max_queue {
                return Err(LaneError::QueueFull {
                    lane,
                    max: self.inner.config.max_queue,
                });
            }
            let sequence = state.sequence;
            state.sequence += 1;
            self.inner.tokens.insert(&id, cancel.clone());
            let ticket = Ticket {
                id: id.clone(),
                lane: lane.clone(),
                command,
                cwd,
                priority,
                owner_session,
                state: TicketState::Queued,
                queued_ms: now,
                started_ms: None,
                finished_ms: None,
                output_path: None,
            };
            let (sender, _receiver) = watch::channel(ticket.clone());
            state.tickets.insert(
                id.clone(),
                TicketEntry {
                    ticket: ticket.clone(),
                    sender,
                    cancel: cancel.clone(),
                    sequence,
                },
            );
            let start = {
                let lane_state = state.lanes.get_mut(&lane).expect("lane just created");
                lane_state.queue.push(Queued {
                    priority,
                    sequence,
                    id: id.clone(),
                });
                let start = !lane_state.dispatcher_started;
                lane_state.dispatcher_started = true;
                start
            };
            (ticket, lane, start)
        };
        // The token is visible before the ticket can be dispatched.
        self.inner.events.changed(&queued);
        if start {
            tokio::spawn(self.inner.clone().dispatch(lane_name));
        } else if let Some(notify) = self.inner.notify_for(&lane_name).await {
            notify.notify_one();
        }
        Ok(id)
    }

    /// Returns lane status. `Some(lane)` always returns one row for that lane,
    /// even when it has never been used; `None` returns all known lanes sorted
    /// by name.
    pub async fn status(&self, lane: Option<&str>) -> Vec<LaneStatus> {
        let state = self.inner.state.lock().await;
        let names: Vec<String> = if let Some(name) = lane {
            vec![name.to_owned()]
        } else {
            let mut names: Vec<String> = state.lanes.keys().cloned().collect();
            names.sort();
            names
        };
        names
            .into_iter()
            .map(|name| {
                let lane = state.lanes.get(&name);
                LaneStatus {
                    concurrency: self.inner.config.concurrency_for(&name),
                    running: lane.map_or(0, |lane| lane.active),
                    queued: lane.map_or(0, |lane| lane.queue.len()),
                    lane: name,
                }
            })
            .collect()
    }

    /// Returns a snapshot of one ticket.
    pub async fn ticket(&self, id: &str) -> Option<Ticket> {
        self.inner
            .state
            .lock()
            .await
            .tickets
            .get(id)
            .map(|entry| entry.ticket.clone())
    }

    /// Cancels a queued ticket (removes it) or a running ticket (cancels its
    /// token). Returns `false` for an already terminal ticket.
    pub async fn cancel(&self, id: &str) -> Result<bool, LaneError> {
        let (changed, token) = {
            let mut state = self.inner.state.lock().await;
            let now = self.inner.clock.now_ms();
            let State { tickets, lanes, .. } = &mut *state;
            let Some(entry) = tickets.get_mut(id) else {
                return Err(LaneError::UnknownTicket(id.to_owned()));
            };
            match entry.ticket.state {
                TicketState::Queued => {
                    let lane_name = entry.ticket.lane.clone();
                    if let Some(lane) = lanes.get_mut(&lane_name) {
                        lane.queue.retain(|queued| queued.id != id);
                        lane.notify.notify_one();
                    }
                    entry.ticket.state = TicketState::Cancelled;
                    entry.ticket.finished_ms = Some(now);
                    let ticket = entry.ticket.clone();
                    entry.sender.send_replace(ticket.clone());
                    (ticket, entry.cancel.clone())
                }
                TicketState::Running => {
                    entry.ticket.state = TicketState::Cancelled;
                    entry.ticket.finished_ms = Some(now);
                    let ticket = entry.ticket.clone();
                    entry.sender.send_replace(ticket.clone());
                    (ticket, entry.cancel.clone())
                }
                TicketState::Succeeded { .. }
                | TicketState::Failed { .. }
                | TicketState::Cancelled => return Ok(false),
            }
        };
        token.cancel();
        self.inner.events.changed(&changed);
        Ok(true)
    }

    /// Reorders a queued ticket. Running and terminal tickets are rejected.
    pub async fn reorder(&self, id: &str, priority: i32) -> Result<(), LaneError> {
        let changed = {
            let mut state = self.inner.state.lock().await;
            let Some(entry) = state.tickets.get_mut(id) else {
                return Err(LaneError::UnknownTicket(id.to_owned()));
            };
            if !matches!(entry.ticket.state, TicketState::Queued) {
                return Err(LaneError::NotQueued { id: id.to_owned() });
            }
            let lane = entry.ticket.lane.clone();
            let sequence = entry.sequence;
            entry.ticket.priority = priority;
            let ticket = entry.ticket.clone();
            entry.sender.send_replace(ticket.clone());
            if let Some(lane_state) = state.lanes.get_mut(&lane) {
                lane_state.queue.retain(|queued| queued.id != id);
                lane_state.queue.push(Queued {
                    priority,
                    sequence,
                    id: id.to_owned(),
                });
                lane_state.notify.notify_one();
            }
            ticket
        };
        self.inner.events.changed(&changed);
        Ok(())
    }

    /// Waits for a ticket to become terminal.
    ///
    /// The wait uses the ticket's watch channel; it does not poll. A timeout
    /// returns [`LaneError::Timeout`].
    pub async fn wait(&self, id: &str, timeout: Duration) -> Result<Ticket, LaneError> {
        let mut receiver = {
            let state = self.inner.state.lock().await;
            let Some(entry) = state.tickets.get(id) else {
                return Err(LaneError::UnknownTicket(id.to_owned()));
            };
            entry.sender.subscribe()
        };
        let started = tokio::time::Instant::now();
        loop {
            let ticket = receiver.borrow_and_update().clone();
            if ticket.state.is_terminal() {
                return Ok(ticket);
            }
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return Err(timeout_error(id, timeout));
            };
            if remaining.is_zero() {
                return Err(timeout_error(id, timeout));
            }
            match tokio::time::timeout(remaining, receiver.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Ok(receiver.borrow().clone()),
                Err(_) => return Err(timeout_error(id, timeout)),
            }
        }
    }

    /// Cancels every queued and running ticket and stops all dispatchers.
    ///
    /// Subsequent [`Self::enqueue`] calls fail with [`LaneError::Shutdown`].
    pub async fn shutdown(&self) {
        let changed = {
            let mut state = self.inner.state.lock().await;
            if state.shutdown {
                return;
            }
            state.shutdown = true;
            let now = self.inner.clock.now_ms();
            for lane in state.lanes.values_mut() {
                lane.queue.clear();
                lane.semaphore.close();
                lane.notify.notify_one();
            }
            let mut changed = Vec::new();
            for entry in state.tickets.values_mut() {
                if matches!(
                    entry.ticket.state,
                    TicketState::Queued | TicketState::Running
                ) {
                    entry.ticket.state = TicketState::Cancelled;
                    entry.ticket.finished_ms = Some(now);
                    entry.cancel.cancel();
                    entry.sender.send_replace(entry.ticket.clone());
                    changed.push(entry.ticket.clone());
                }
            }
            changed
        };
        for ticket in changed {
            self.inner.events.changed(&ticket);
        }
    }
}

fn timeout_error(id: &str, timeout: Duration) -> LaneError {
    LaneError::Timeout {
        id: id.to_owned(),
        timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
    }
}

impl Inner {
    async fn notify_for(&self, lane: &str) -> Option<Arc<Notify>> {
        self.state
            .lock()
            .await
            .lanes
            .get(lane)
            .map(|lane| lane.notify.clone())
    }

    async fn dispatch(self: Arc<Self>, lane_name: String) {
        loop {
            let (semaphore, notify) = {
                let state = self.state.lock().await;
                if state.shutdown {
                    return;
                }
                let Some(lane) = state.lanes.get(&lane_name) else {
                    return;
                };
                (lane.semaphore.clone(), lane.notify.clone())
            };
            let Ok(permit) = semaphore.acquire_owned().await else {
                return;
            };
            let ticket = loop {
                let mut state = self.state.lock().await;
                if state.shutdown {
                    return;
                }
                let now = self.clock.now_ms();
                let queued = {
                    let Some(lane) = state.lanes.get_mut(&lane_name) else {
                        return;
                    };
                    lane.queue.pop()
                };
                let Some(queued) = queued else {
                    drop(state);
                    notify.notified().await;
                    continue;
                };
                let ticket = {
                    let Some(entry) = state.tickets.get_mut(&queued.id) else {
                        continue;
                    };
                    if !matches!(entry.ticket.state, TicketState::Queued) {
                        continue;
                    }
                    entry.ticket.state = TicketState::Running;
                    entry.ticket.started_ms = Some(now);
                    let ticket = entry.ticket.clone();
                    entry.sender.send_replace(ticket.clone());
                    ticket
                };
                if let Some(lane) = state.lanes.get_mut(&lane_name) {
                    lane.active += 1;
                }
                break ticket;
            };
            self.events.changed(&ticket);
            let runner = self.runner.clone();
            let ticket_for_run = Arc::new(ticket);
            let ticket_id = ticket_for_run.id.clone();
            let finish_lane = lane_name.clone();
            let this = self.clone();
            tokio::spawn(async move {
                let run = async { runner.run(&ticket_for_run).await };
                let result = AssertUnwindSafe(run)
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| RunnerResult::failure(-1, "lane runner panicked"));
                this.finish(&finish_lane, &ticket_id, result, permit).await;
            });
        }
    }

    async fn finish(
        &self,
        lane_name: &str,
        id: &str,
        result: RunnerResult,
        _permit: OwnedSemaphorePermit,
    ) {
        self.tokens.remove(id);
        let RunnerResult {
            exit,
            message,
            output_path,
        } = result;
        let changed = {
            let mut state = self.state.lock().await;
            if let Some(lane) = state.lanes.get_mut(lane_name) {
                lane.active = lane.active.saturating_sub(1);
            }
            let now = self.clock.now_ms();
            let Some(entry) = state.tickets.get_mut(id) else {
                return;
            };
            if !matches!(entry.ticket.state, TicketState::Running) {
                return;
            }
            entry.ticket.state = if exit == 0 {
                TicketState::Succeeded { exit: 0 }
            } else {
                TicketState::Failed {
                    exit,
                    message: message
                        .unwrap_or_else(|| format!("command exited with status {exit}")),
                }
            };
            entry.ticket.output_path = output_path;
            entry.ticket.finished_ms = Some(now);
            let ticket = entry.ticket.clone();
            entry.sender.send_replace(ticket.clone());
            ticket
        };
        self.events.changed(&changed);
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
}

struct LaneState {
    semaphore: Arc<Semaphore>,
    notify: Arc<Notify>,
    queue: BinaryHeap<Queued>,
    active: usize,
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
