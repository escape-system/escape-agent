//! Ticket submission, coalescing onto live runs, and insertion into the
//! in-memory queue.

use crate::error::LaneError;
use crate::types::{EnqueueResult, NewTicket, Ticket, TicketId, TicketState};

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

use super::coalesce::{coalesce_key, coalesced_target};
use super::{Inner, LaneState, Queued, State, TicketEntry};

impl Inner {
    /// Allocates a `tkt_<n>` id that is not in the live map.
    ///
    /// The counter is bumped monotonically; an id that already exists (for
    /// example a restored row with a non-standard id) is skipped instead of
    /// overwriting the row in memory or in the store. Callers hold the state
    /// lock, so the check and the insert cannot interleave.
    pub(super) fn allocate_id(&self, state: &State) -> String {
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
    pub(super) fn insert(
        &self,
        state: &mut State,
        lane: &str,
        ticket: NewTicket,
        coalesce: Option<super::CoalesceKey>,
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
        // Exactly one dispatcher exists per lane, and it is spawned at most
        // once: the latch is set by the claim that reports the spawn.
        let start = lane_state.claim_dispatcher();
        Ok((ticket, start))
    }

    pub(super) async fn notify_for(&self, lane: &str) -> Option<Arc<Notify>> {
        self.state
            .lock()
            .await
            .lanes
            .get(lane)
            .map(|lane| lane.notify.clone())
    }
}

impl super::LaneService {
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
}
