//! Priority-queue lane scheduler with semaphore-enforced concurrency.

use std::{
    collections::{BinaryHeap, HashMap},
    fmt,
    future::Future,
    hash::Hash,
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
    error::{LaneError, RestoreError},
    types::{
        EnqueueResult, LaneStatus, NewTicket, RunnerResult, Ticket, TicketId, TicketRecord,
        TicketState,
    },
};

/// A boxed runner future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Source of persisted tickets for [`LaneService::restore`].
///
/// `oc-lanes` has no store dependency: the host adapts its `lane_ticket` table
/// to this trait (see `oc_store::Store::lane_tickets_all` and the CLI wiring).
pub trait LaneTicketSource: Send + Sync {
    /// Loads every persisted ticket, newest first; ordering is not significant.
    fn load(&self) -> BoxFuture<'_, Result<Vec<TicketRecord>, RestoreError>>;
}

/// Persisted tickets are replayed in their original enqueue order: ascending
/// `queued_ms`, then ascending numeric id suffix.
fn replay_order(left: &TicketRecord, right: &TicketRecord) -> std::cmp::Ordering {
    left.queued_ms
        .cmp(&right.queued_ms)
        .then_with(|| id_suffix(&left.id).cmp(&id_suffix(&right.id)))
        .then_with(|| left.id.cmp(&right.id))
}

/// Numeric part of a `tkt_<n>` id; `None` for ids that do not parse.
fn id_suffix(id: &str) -> Option<u64> {
    id.strip_prefix("tkt_")?.parse().ok()
}

/// Builds the in-memory ticket for one persisted row.
///
/// A `running` row belongs to a process that no longer exists, so it is
/// replayed as [`TicketState::Failed`] with a restart reason. `queued` rows
/// stay queued and are returned as runnable. Terminal rows are read into
/// memory as history and are never re-published.
///
/// The tuple is `(ticket, runnable, publish)`: `runnable` rows are dispatched,
/// and `publish` marks rows whose state actually changed at restore (queued
/// rows re-emitted as queued, running rows converted to failed). A terminal
/// row has `publish == false`, so a boot never writes history back to the
/// store and cannot erase `finished`/`output_path`.
fn restored_ticket(record: TicketRecord, now_ms: i64) -> (Ticket, bool, bool) {
    let TicketRecord {
        id,
        lane,
        command,
        cwd,
        priority,
        owner_session,
        state,
        queued_ms,
        started_ms,
        finished_ms,
        output_path,
    } = record;
    let (state, finished_ms, runnable, publish) = match state {
        TicketState::Queued => (TicketState::Queued, finished_ms, true, true),
        TicketState::Running => (
            TicketState::Failed {
                exit: -1,
                message: "lane runner was interrupted by a restart".to_owned(),
            },
            Some(now_ms),
            false,
            true,
        ),
        terminal => (terminal, finished_ms, false, false),
    };
    (
        Ticket {
            id,
            lane,
            command,
            cwd,
            priority,
            owner_session,
            state,
            queued_ms,
            started_ms,
            finished_ms,
            output_path,
            tree_rev: None,
            subscribers: Vec::new(),
        },
        runnable,
        publish,
    )
}

/// Finds the active ticket that `key` coalesces onto, registering `session` as
/// a subscriber. `None` means the caller must create a new ticket.
fn coalesced_target(
    state: &mut State,
    key: Option<&CoalesceKey>,
    owner_session: Option<&str>,
) -> Option<Ticket> {
    let key = key?;
    let id = state
        .tickets
        .iter()
        .find(|(_, entry)| {
            entry.coalesce.as_ref() == Some(key) && !entry.ticket.state.is_terminal()
        })
        .map(|(id, _)| id.clone())?;
    let entry = state.tickets.get_mut(&id)?;
    if let Some(session) = owner_session
        && !entry
            .ticket
            .subscribers
            .iter()
            .any(|known| known == session)
    {
        entry.ticket.subscribers.push(session.to_owned());
    }
    Some(entry.ticket.clone())
}

/// Builds the coalescing key. `None` when the caller supplied no usable
/// `tree_rev`, which always creates a distinct ticket.
fn coalesce_key(
    lane: &str,
    command: &str,
    cwd: &std::path::Path,
    tree_rev: Option<&str>,
) -> Option<CoalesceKey> {
    let tree_rev = tree_rev.map(str::trim).filter(|rev| !rev.is_empty())?;
    Some(CoalesceKey {
        lane: lane.to_owned(),
        command: normalize_command(command),
        cwd: cwd.to_path_buf(),
        tree_rev: tree_rev.to_owned(),
    })
}

/// Coalescing key: `(lane, normalized command, cwd, tree_rev)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CoalesceKey {
    lane: String,
    command: String,
    cwd: std::path::PathBuf,
    tree_rev: String,
}

/// Normalizes a command for coalescing only.
///
/// Trims, collapses runs of spaces/tabs into one space and strips a trailing
/// `;`. It never reorders flags, rewrites arguments or merges different
/// commands: two commands coalesce only when their tokens are identical after
/// this normalization.
fn normalize_command(command: &str) -> String {
    let mut normalized = String::with_capacity(command.len());
    for character in command.chars() {
        if character == ' ' || character == '\t' {
            if !normalized.is_empty() && !normalized.ends_with(' ') {
                normalized.push(' ');
            }
        } else {
            normalized.push(character);
        }
    }
    let trimmed = normalized.trim();
    let without_semicolon = trimmed.strip_suffix(';').unwrap_or(trimmed).trim_end();
    without_semicolon.to_owned()
}

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

    /// Rebuilds the in-memory queue from persisted tickets.
    ///
    /// - `queued` rows are re-enqueued with their original `lane`, `command`,
    ///   `cwd`, `priority` and owner, in their original enqueue order;
    /// - `running` rows are marked [`TicketState::Failed`] with a message that
    ///   names the restart, because the process that owned them is gone, and
    ///   that failure is published;
    /// - terminal rows are only read, never re-published: they are rehydrated
    ///   into memory (with `finished_ms` and `output_path` intact) but no
    ///   lifecycle event is emitted for them, so a boot cannot rewrite the
    ///   persisted history row.
    ///
    /// # Id invariant
    ///
    /// The counter holds the highest id issued or seen, seeded from the
    /// maximum numeric `tkt_` suffix in the store. [`Inner::allocate_id`]
    /// pre-increments, so the next id is always `max seen + 1`: restoring
    /// `tkt_2` and `tkt_4` yields `tkt_5` next, and no persisted id is ever
    /// reissued. Ids that do not parse (`tkt_abc`, `lan_1`) are skipped for the
    /// counter but preserved for the row, and any id still live is skipped too.
    ///
    /// Restoring an empty store yields a fresh counter and a ready service, so
    /// the call is idempotent and safe on every boot.
    pub async fn restore(
        config: LaneConfig,
        runner: Arc<dyn LaneRunner>,
        events: Arc<dyn LaneEvents>,
        clock: Arc<dyn Clock>,
        source: Arc<dyn LaneTicketSource>,
    ) -> Result<Self, RestoreError> {
        let mut records = source.load().await?;
        records.sort_by(replay_order);
        let next_id = records
            .iter()
            .filter_map(|record| id_suffix(&record.id))
            .max()
            .unwrap_or(0);
        let service = Self::assemble(config, runner, events, clock, next_id);
        service.replay(records).await?;
        Ok(service)
    }

    async fn replay(&self, records: Vec<TicketRecord>) -> Result<(), RestoreError> {
        let mut restored = Vec::new();
        {
            let mut state = self.inner.state.lock().await;
            for record in records {
                if state.tickets.contains_key(&record.id) {
                    continue;
                }
                let (ticket, runnable, publish) =
                    restored_ticket(record, self.inner.clock.now_ms());
                let id = ticket.id.clone();
                let sequence = state.sequence;
                state.sequence += 1;
                let cancel = CancellationToken::new();
                // Only a runnable ticket needs a cancellation token; a
                // restored terminal row must not leak one into the table.
                if runnable {
                    self.inner.tokens.insert(&id, cancel.clone());
                }
                let (sender, _receiver) = watch::channel(ticket.clone());
                state.tickets.insert(
                    id.clone(),
                    TicketEntry {
                        ticket: ticket.clone(),
                        sender,
                        cancel,
                        sequence,
                        coalesce: None,
                    },
                );
                if runnable {
                    let lane_name = ticket.lane.clone();
                    let concurrency = self.inner.config.concurrency_for(&lane_name);
                    let lane_state = state
                        .lanes
                        .entry(lane_name)
                        .or_insert_with(|| LaneState::new(concurrency));
                    lane_state.queue.push(Queued {
                        priority: ticket.priority,
                        sequence,
                        id,
                    });
                }
                if publish {
                    restored.push(ticket);
                }
            }
        }
        // Only rows whose state changed at restore are published. Terminal rows
        // are read into memory and never re-published: a boot must not rewrite
        // history, and the CLI adapter persists every published ticket.
        for ticket in restored {
            self.inner.events.changed(&ticket);
        }
        self.inner.resume_dispatchers().await;
        Ok(())
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
    ///
    /// # Coalescing
    ///
    /// When the caller supplies [`NewTicket::tree_rev`] (`Some(non-empty)`),
    /// the key `(lane, normalized command, cwd, tree_rev)` identifies the work.
    /// If a ticket with that key is still `queued` or `running`, no new ticket
    /// is created: the caller receives that ticket id with
    /// [`EnqueueResult::subscribed`] set and, when the caller names a session,
    /// is appended to the ticket's subscriber list. Coalescing is decided under
    /// the same lock that would insert the ticket, so two concurrent callers
    /// cannot both create one.
    ///
    /// `tree_rev` is mandatory for coalescing on purpose. Two `cargo test`
    /// runs started from different tree states must never share a result, so a
    /// ticket without a tree revision (or with a different one) is always a
    /// distinct ticket.
    ///
    /// The dedupe state is in memory; a restart drops coalescing but never
    /// drops a run, because queued rows are replayed by [`Self::restore`].
    pub async fn enqueue(&self, lane: String, ticket: NewTicket) -> Result<TicketId, LaneError> {
        self.enqueue_ticket(lane, ticket)
            .await
            .map(|result| result.id)
    }

    /// [`Self::enqueue`] with the coalescing outcome attached.
    ///
    /// A caller that only needs an id uses [`Self::enqueue`]; a caller that
    /// wants to know whether it joined an existing run checks
    /// [`EnqueueResult::subscribed`].
    pub async fn enqueue_detailed(
        &self,
        lane: String,
        ticket: NewTicket,
    ) -> Result<EnqueueResult, LaneError> {
        self.enqueue_ticket(lane, ticket).await
    }

    async fn enqueue_ticket(
        &self,
        lane: String,
        ticket: NewTicket,
    ) -> Result<EnqueueResult, LaneError> {
        if lane.trim().is_empty() {
            return Err(LaneError::EmptyLane);
        }
        let NewTicket {
            command,
            cwd,
            priority,
            owner_session,
            label: _,
            tree_rev,
        } = ticket;
        let key = coalesce_key(&lane, &command, &cwd, tree_rev.as_deref());
        let now = self.inner.clock.now_ms();
        let (ticket, lane_name, start, subscribed) = {
            let mut state = self.inner.state.lock().await;
            if state.shutdown {
                return Err(LaneError::Shutdown);
            }
            if let Some(existing) =
                coalesced_target(&mut state, key.as_ref(), owner_session.as_deref())
            {
                return Ok(EnqueueResult {
                    id: existing.id,
                    subscribed: true,
                });
            }
            let ticket = NewTicket {
                command,
                cwd,
                priority,
                owner_session,
                label: None,
                tree_rev,
            };
            let (ticket, start) = self.inner.insert(&mut state, &lane, ticket, key, now)?;
            (ticket, lane, start, false)
        };
        // The token is visible before the ticket can be dispatched.
        self.inner.events.changed(&ticket);
        if start {
            tokio::spawn(self.inner.clone().dispatch(lane_name));
        } else if let Some(notify) = self.inner.notify_for(&lane_name).await {
            notify.notify_one();
        }
        Ok(EnqueueResult {
            id: ticket.id,
            subscribed,
        })
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

    /// Removes one session's interest in a coalesced ticket.
    ///
    /// If `session` is the ticket's owner, the owner slot is cleared too. The
    /// run stays alive while an owner or at least one subscriber remains, so a
    /// subscriber that times out never cancels work someone else is waiting
    /// for. Only when nobody is left is the run cancelled exactly like
    /// [`Self::cancel`] (queued rows leave the queue, running rows get their
    /// token cancelled) and `false` is returned.
    ///
    /// Returns whether the run is still alive. A terminal ticket returns
    /// `false` and is never reopened.
    ///
    /// Runtime-internal: no HTTP route exposes this.
    pub async fn unsubscribe(&self, id: &str, session: &str) -> Result<bool, LaneError> {
        let cancelled = {
            let mut state = self.inner.state.lock().await;
            let State { tickets, lanes, .. } = &mut *state;
            let Some(entry) = tickets.get_mut(id) else {
                return Err(LaneError::UnknownTicket(id.to_owned()));
            };
            if entry.ticket.state.is_terminal() {
                return Ok(false);
            }
            entry.ticket.subscribers.retain(|known| known != session);
            if entry.ticket.owner_session.as_deref() == Some(session) {
                entry.ticket.owner_session = None;
            }
            if entry.ticket.owner_session.is_some() || !entry.ticket.subscribers.is_empty() {
                return Ok(true);
            }
            let now = self.inner.clock.now_ms();
            if matches!(entry.ticket.state, TicketState::Queued)
                && let Some(lane) = lanes.get_mut(&entry.ticket.lane)
            {
                lane.queue.retain(|queued| queued.id != id);
                lane.notify.notify_one();
            }
            entry.ticket.state = TicketState::Cancelled;
            entry.ticket.finished_ms = Some(now);
            let ticket = entry.ticket.clone();
            entry.sender.send_replace(ticket.clone());
            (ticket, entry.cancel.clone())
        };
        cancelled.1.cancel();
        self.inner.events.changed(&cancelled.0);
        Ok(false)
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
    /// Allocates a `tkt_<n>` id that is not in the live map.
    ///
    /// The counter is bumped monotonically; an id that already exists (for
    /// example a restored row with a non-standard id) is skipped instead of
    /// overwriting the row in memory or in the store. Callers hold the state
    /// lock, so the check and the insert cannot interleave.
    fn allocate_id(&self, state: &State) -> String {
        loop {
            let id = format!("tkt_{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
            if !state.tickets.contains_key(&id) {
                return id;
            }
        }
    }

    /// Inserts one queued ticket and pushes it into its lane queue. Returns the
    /// snapshot to publish and whether the lane dispatcher must be spawned.
    ///
    /// Callers hold the state lock, so the queue check, id allocation and
    /// insert are one atomic step: no other submission can interleave between
    /// the coalescing miss and the insert.
    fn insert(
        &self,
        state: &mut State,
        lane: &str,
        ticket: NewTicket,
        coalesce: Option<CoalesceKey>,
        now_ms: i64,
    ) -> Result<(Ticket, bool), LaneError> {
        let NewTicket {
            command,
            cwd,
            priority,
            owner_session,
            label: _,
            tree_rev,
        } = ticket;
        let concurrency = self.config.concurrency_for(lane);
        let queued_len = state
            .lanes
            .entry(lane.to_owned())
            .or_insert_with(|| LaneState::new(concurrency))
            .queue
            .len();
        if self.config.max_queue > 0 && queued_len >= self.config.max_queue {
            return Err(LaneError::QueueFull {
                lane: lane.to_owned(),
                max: self.config.max_queue,
            });
        }
        let id = self.allocate_id(state);
        let sequence = state.sequence;
        state.sequence += 1;
        let cancel = CancellationToken::new();
        self.tokens.insert(&id, cancel.clone());
        let ticket = Ticket {
            id: id.clone(),
            lane: lane.to_owned(),
            command,
            cwd,
            priority,
            owner_session,
            state: TicketState::Queued,
            queued_ms: now_ms,
            started_ms: None,
            finished_ms: None,
            output_path: None,
            tree_rev,
            subscribers: Vec::new(),
        };
        let (sender, _receiver) = watch::channel(ticket.clone());
        state.tickets.insert(
            id.clone(),
            TicketEntry {
                ticket: ticket.clone(),
                sender,
                cancel,
                sequence,
                coalesce,
            },
        );
        let lane_state = state.lanes.get_mut(lane).expect("lane just created");
        lane_state.queue.push(Queued {
            priority,
            sequence,
            id,
        });
        let start = !lane_state.dispatcher_started;
        if start {
            lane_state.dispatcher_started = true;
        }
        Ok((ticket, start))
    }

    async fn notify_for(&self, lane: &str) -> Option<Arc<Notify>> {
        self.state
            .lock()
            .await
            .lanes
            .get(lane)
            .map(|lane| lane.notify.clone())
    }

    /// Starts one dispatcher per lane that has queued or running tickets after
    /// a restore. Lanes without work stay dormant, exactly like a fresh boot.
    async fn resume_dispatchers(self: &Arc<Self>) {
        let lanes: Vec<String> = {
            let mut state = self.state.lock().await;
            let names: Vec<String> = state
                .lanes
                .iter()
                .filter(|(_, lane)| !lane.queue.is_empty() || lane.active > 0)
                .map(|(name, _)| name.clone())
                .collect();
            for name in &names {
                if let Some(lane) = state.lanes.get_mut(name) {
                    lane.dispatcher_started = true;
                }
            }
            names
        };
        for lane in lanes {
            tokio::spawn(Arc::clone(self).dispatch(lane));
        }
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
    /// Coalescing key, present only when the caller supplied a `tree_rev`.
    coalesce: Option<CoalesceKey>,
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
