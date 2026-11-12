//! Cancellation and shutdown: the two paths that end tickets from outside the
//! runner.

use crate::error::LaneError;
use crate::types::TicketState;

impl super::LaneService {
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

use super::State;
