//! Lane dispatcher lifecycle and terminal ticket publication.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures_util::FutureExt;
use tokio::sync::OwnedSemaphorePermit;

use crate::types::{RunnerResult, TicketState};

impl super::Inner {
    /// Starts one dispatcher per lane that has queued or running tickets after
    /// a restore. Lanes without work stay dormant, exactly like a fresh boot.
    pub(super) async fn resume_dispatchers(self: &Arc<Self>) {
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

    pub(super) async fn dispatch(self: Arc<Self>, lane_name: String) {
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

    pub(super) async fn finish(
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
