//! Replaying persisted tickets into the in-memory queue at boot.

use crate::config::LaneConfig;
use crate::error::RestoreError;
use crate::types::{Ticket, TicketRecord, TicketState};

use std::sync::Arc;

use oc_observer::Clock;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{
    LaneEvents, LaneRunner, LaneService, LaneState, LaneTicketSource, Queued, TicketEntry,
};

/// Persisted tickets are replayed in their original enqueue order: ascending
/// `queued_ms`, then ascending numeric id suffix.
fn replay_order(left: &TicketRecord, right: &TicketRecord) -> std::cmp::Ordering {
    left.queued_ms
        .cmp(&right.queued_ms)
        .then_with(|| id_suffix(&left.id).cmp(&id_suffix(&right.id)))
        .then_with(|| left.id.cmp(&right.id))
}

/// Numeric part of a `tkt_<n>` id; `None` for ids that do not parse.
pub(super) fn id_suffix(id: &str) -> Option<u64> {
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

impl LaneService {
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
}
