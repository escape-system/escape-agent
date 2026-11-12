//! Per-ticket reads, priority reordering, subscription release and waiting.

use std::time::Duration;

use crate::error::LaneError;
use crate::types::{LaneStatus, Ticket, TicketState};

use super::{Queued, timeout_error};

impl super::LaneService {
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
}

use super::State;
