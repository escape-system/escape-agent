//! Event streaming for a terminal turn: deltas, tool progress, settlement.
//!
//! The live bus is a bounded ring buffer, so it can drop events
//! ([`broadcast::error::RecvError::Lagged`]) or close
//! ([`broadcast::error::RecvError::Closed`]). Neither may be swallowed: the
//! terminal event is what ends the turn, so a dropped terminal would hang the
//! CLI forever and a closed bus would fall through to a success that never
//! happened. Both faults resynchronise from the durable event log instead, and
//! a turn that cannot be confirmed ends in a typed error rather than `Ok(())`.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use oc_server::data::events::from_stored;
use oc_store::Store;
use oc_types::EventEnvelope;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep_until};

use super::TerminalHost;
use super::permission::{PermissionPrompt, decide_permission_reply};

/// Durable events that close a turn.
///
/// This is the same set the print loop returns on. It must stay identical: a
/// resync that accepted a broader set would judge a live turn settled — an
/// automatic compaction, for example, publishes `session.compaction.ended`
/// in the middle of a turn that is still running.
const TERMINALS: [&str; 3] = [
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
];

/// How long a resynchronisation may take to reach a terminal event.
///
/// It bounds recovery after a bus fault, not a healthy turn: a turn that never
/// loses an event is still allowed to run for as long as it needs.
const RESYNC_BUDGET: Duration = Duration::from_secs(300);

/// Page size of the log scan; the store caps a page at 1000 events.
const PAGE: usize = 1_000;

/// The way the live bus failed to deliver the rest of a turn.
#[derive(Debug, Clone, Copy)]
enum BusFault {
    /// The receiver fell behind and the bus dropped this many events.
    Lagged(u64),
    /// The bus closed; nothing more will ever arrive on it.
    Closed,
}

impl fmt::Display for BusFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lagged(skipped) => {
                write!(formatter, "the event bus dropped {skipped} event(s)")
            }
            Self::Closed => formatter.write_str("the event bus closed"),
        }
    }
}

/// A turn the stream could not confirm, named by cause.
///
/// Returned instead of `Ok(())`: an unconfirmed terminal is not a success.
#[derive(Debug)]
pub(super) enum StreamFault {
    /// The bus closed and no terminal event is durable.
    BusClosed {
        session: String,
        last_seq: i64,
        /// Stored outcome of the last turn, when the session row says it already
        /// settled — which makes the missing terminal a log anomaly, not a hang.
        settled: Option<String>,
    },
    /// A resync never reached a terminal event within [`RESYNC_BUDGET`].
    ResyncBudget {
        session: String,
        last_seq: i64,
        settled: Option<String>,
        resyncs: u32,
    },
}

/// `None` when the row has no recorded outcome or a newer `time.busy`.
fn settled_outcome(session: Option<&Value>) -> Option<String> {
    let session = session?;
    let outcome = session.get("outcome").and_then(Value::as_str)?;
    let busy = session
        .pointer("/time/busy")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let idle = session
        .pointer("/time/idle")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    (busy <= idle).then(|| outcome.to_owned())
}

fn settled_clause(settled: Option<&str>) -> String {
    settled.map_or_else(String::new, |outcome| {
        format!(
            "; the session row already recorded a settled turn ({outcome}), so its terminal event \
             is missing from the log"
        )
    })
}

impl fmt::Display for StreamFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BusClosed {
                session,
                last_seq,
                settled,
            } => write!(
                formatter,
                "{session}: the event bus closed after durable sequence {last_seq} with no terminal \
                 execution event{}",
                settled_clause(settled.as_deref())
            ),
            Self::ResyncBudget {
                session,
                last_seq,
                settled,
                resyncs,
            } => write!(
                formatter,
                "{session}: no terminal execution event arrived within {}s of resynchronising \
                 ({resyncs} resync attempt(s), last durable sequence {last_seq}){}",
                RESYNC_BUDGET.as_secs(),
                settled_clause(settled.as_deref())
            ),
        }
    }
}

impl std::error::Error for StreamFault {}

/// What to do after the live bus misbehaved.
#[derive(Debug)]
enum Recovery {
    /// Durable envelopes to consume before touching the bus again, oldest first.
    /// They go through the same print loop as live events, so a replayed
    /// terminal settles the turn exactly like a delivered one.
    Replay(VecDeque<Arc<EventEnvelope>>),
    /// Nothing durable can end this turn; stop with the cause.
    Stop(StreamFault),
}

/// Durable state a faulted stream resynchronises from.
struct Resync<'a> {
    store: &'a Store,
    directory: &'a str,
    session_id: &'a str,
}

impl Resync<'_> {
    /// The durable envelopes after `cursor`, oldest first.
    ///
    /// The cursor is the sequence of the last envelope this stream handled, so
    /// the replay contains exactly the window the bus dropped and never
    /// re-publishes an event that was already printed.
    async fn replay(&self, cursor: i64) -> anyhow::Result<VecDeque<Arc<EventEnvelope>>> {
        let mut queued = VecDeque::new();
        let mut after = cursor;
        loop {
            let batch = self
                .store
                .events_after(self.session_id, after, PAGE)
                .await
                .with_context(|| format!("read the durable log of {}", self.session_id))?;
            let Some(last) = batch.last() else {
                break;
            };
            after = last.seq;
            let count = batch.len();
            queued.extend(
                batch
                    .into_iter()
                    .map(|event| Arc::new(from_stored(event, self.directory))),
            );
            if count < PAGE {
                break;
            }
        }
        Ok(queued)
    }

    /// The stored outcome of the last turn when the session row says it already
    /// settled.
    async fn settled(&self) -> anyhow::Result<Option<String>> {
        let session = self
            .store
            .get_session(self.session_id)
            .await
            .with_context(|| format!("read session {}", self.session_id))?;
        Ok(settled_outcome(session.as_ref()))
    }

    /// Decide whether a fault can still be recovered from durable state.
    ///
    /// `deadline` is fixed at the first fault, so repeated faults share one
    /// bounded window instead of extending it.
    async fn recover(
        &self,
        cursor: i64,
        fault: BusFault,
        deadline: Instant,
        resyncs: u32,
    ) -> anyhow::Result<Recovery> {
        let queued = self.replay(cursor).await?;
        if queued.iter().any(|event| is_terminal(&event.kind)) {
            return Ok(Recovery::Replay(queued));
        }
        let last_seq = queued
            .back()
            .and_then(|event| event.durable.as_ref())
            .map_or(cursor, |durable| durable.seq);
        match fault {
            // Nothing will ever arrive on a closed bus, so an unconfirmed turn
            // is an error now rather than a wait.
            BusFault::Closed => Ok(Recovery::Stop(StreamFault::BusClosed {
                session: self.session_id.to_owned(),
                last_seq,
                settled: self.settled().await?,
            })),
            // The bus is alive; drain what the log still holds and keep
            // consuming, but not past the shared window.
            BusFault::Lagged(_) if Instant::now() >= deadline => {
                Ok(Recovery::Stop(StreamFault::ResyncBudget {
                    session: self.session_id.to_owned(),
                    last_seq,
                    settled: self.settled().await?,
                    resyncs,
                }))
            }
            BusFault::Lagged(_) => Ok(Recovery::Replay(queued)),
        }
    }
}

fn is_terminal(kind: &str) -> bool {
    TERMINALS.contains(&kind)
}

#[allow(clippy::too_many_lines)]
pub(super) async fn stream_turn(
    host: &TerminalHost,
    session_id: &str,
    rx: &mut broadcast::Receiver<Arc<EventEnvelope>>,
    json_mode: bool,
    prompt_mode: &mut PermissionPrompt<'_>,
) -> anyhow::Result<()> {
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut in_reasoning = false;
    let mut printed_text = false;
    // Envelopes replayed from the durable log are consumed before the bus is
    // touched again, through the same arms as live events.
    let mut pending: VecDeque<Arc<EventEnvelope>> = VecDeque::new();
    // Sequences of the updates the stream already handled. Only events of this
    // session advance it: `seq` is per aggregate, so another session's event
    // would move the cursor into a hole.
    let mut cursor = 0_i64;
    let mut bus_closed = false;
    let mut deadline: Option<Instant> = None;
    let mut resyncs = 0_u32;
    let directory = host.state.directory().to_string_lossy().into_owned();
    let resync = Resync {
        store: host.services.store.as_ref(),
        directory: &directory,
        session_id,
    };

    loop {
        let envelope = match pending.pop_front() {
            Some(envelope) => envelope,
            // Every `Closed` fault ends the stream inside the branch below, so
            // this is unreachable defense in depth: never `Ok(())` here.
            None if bus_closed => {
                return Err(anyhow::Error::new(StreamFault::BusClosed {
                    session: session_id.to_owned(),
                    last_seq: cursor,
                    settled: resync.settled().await?,
                }));
            }
            None => {
                let wait_until = deadline.unwrap_or_else(|| Instant::now() + RESYNC_BUDGET);
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        let _ = host.runtime.interrupt(session_id).await;
                        eprintln!("\n\x1b[33m[interrupted]\x1b[0m");
                        bail!("execution interrupted by user");
                    }
                    recv = rx.recv() => match recv {
                        Ok(envelope) => envelope,
                        Err(error) => {
                            let fault = match error {
                                broadcast::error::RecvError::Lagged(skipped) => BusFault::Lagged(skipped),
                                broadcast::error::RecvError::Closed => {
                                    bus_closed = true;
                                    BusFault::Closed
                                }
                            };
                            let spent = *deadline.get_or_insert_with(|| Instant::now() + RESYNC_BUDGET);
                            resyncs = resyncs.saturating_add(1);
                            eprintln!(
                                "\x1b[2m[resync] {fault}; reading the durable log from sequence {cursor}\x1b[0m"
                            );
                            match resync.recover(cursor, fault, spent, resyncs).await? {
                                Recovery::Replay(queued) => {
                                    pending = queued;
                                    continue;
                                }
                                Recovery::Stop(cause) => return Err(anyhow::Error::new(cause)),
                            }
                        }
                    },
                    () = sleep_until(wait_until), if deadline.is_some() => {
                        return Err(anyhow::Error::new(StreamFault::ResyncBudget {
                            session: session_id.to_owned(),
                            last_seq: cursor,
                            settled: resync.settled().await?,
                            resyncs,
                        }));
                    }
                }
            }
        };

        if let Some(durable) = &envelope.durable
            && durable.aggregate_id == session_id
        {
            cursor = cursor.max(durable.seq);
        }

        if envelope.kind == "permission.asked" {
            let ask_session = envelope
                .data
                .get("sessionID")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let request_id = envelope
                .data
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !ask_session.is_empty() && !request_id.is_empty() {
                let reply = decide_permission_reply(
                    host,
                    session_id,
                    &envelope.data,
                    json_mode,
                    prompt_mode,
                )
                .await?;
                let _ = host.broker.reply(ask_session, request_id, reply, None);
            }
            continue;
        }

        let event_session = envelope
            .data
            .get("sessionID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event_session != session_id {
            continue;
        }

        if json_mode {
            if let Ok(line) = serde_json::to_string(envelope.as_ref()) {
                println!("{line}");
            }
            match envelope.kind.as_str() {
                "session.execution.succeeded" => return Ok(()),
                "session.execution.failed" => {
                    let message = envelope
                        .data
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("execution failed");
                    bail!("{message}");
                }
                "session.execution.interrupted" => bail!("execution interrupted"),
                _ => continue,
            }
        }

        match envelope.kind.as_str() {
            "session.reasoning.delta" => {
                if let Some(delta) = envelope.data.get("delta").and_then(Value::as_str) {
                    if !in_reasoning {
                        eprint!("\x1b[2m");
                        in_reasoning = true;
                    }
                    eprint!("{delta}");
                    let _ = std::io::stderr().flush();
                }
            }
            "session.reasoning.ended" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                    in_reasoning = false;
                }
            }
            "session.tool.input.started" => {
                if let (Some(id), Some(name)) = (
                    envelope.data.get("id").and_then(Value::as_str),
                    envelope.data.get("name").and_then(Value::as_str),
                ) {
                    tool_names.insert(id.to_owned(), name.to_owned());
                }
            }
            "session.tool.called" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                    in_reasoning = false;
                }
                let id = envelope
                    .data
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = tool_names.get(id).map_or("tool", String::as_str);
                let summary = summarize_tool_input(envelope.data.get("input"));
                eprintln!("\x1b[36m▸ {name}\x1b[0m \x1b[2m{summary}\x1b[0m");
            }
            "session.tool.failed" => {
                let err = envelope
                    .data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("tool failed");
                eprintln!("\x1b[31m✗ {err}\x1b[0m");
            }
            "session.text.delta" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                    in_reasoning = false;
                }
                if let Some(delta) = envelope.data.get("delta").and_then(Value::as_str) {
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                    printed_text = true;
                }
            }
            "session.text.ended" => {
                if printed_text {
                    println!();
                    printed_text = false;
                }
            }
            "session.execution.succeeded" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                }
                if printed_text {
                    println!();
                }
                return Ok(());
            }
            "session.execution.failed" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                }
                let message = envelope
                    .data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("execution failed");
                bail!("{message}");
            }
            "session.execution.interrupted" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                }
                bail!("execution interrupted");
            }
            _ => {}
        }
    }
}

fn summarize_tool_input(input: Option<&Value>) -> String {
    let Some(obj) = input.and_then(Value::as_object) else {
        return String::new();
    };
    for key in ["path", "command", "query", "url", "pattern", "id"] {
        if let Some(val) = obj.get(key).and_then(Value::as_str) {
            let single_line = val.lines().next().unwrap_or_default().trim();
            if single_line.chars().count() > 80 {
                let end = single_line
                    .char_indices()
                    .nth(80)
                    .map_or(single_line.len(), |(i, _)| i);
                return format!("{}…", &single_line[..end]);
            }
            return single_line.to_owned();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use oc_store::{NewEvent, NewSession};
    use oc_types::EventId;

    use super::*;

    const SESSION: &str = "ses_streamresync01";

    /// A store holding one session, so `events_after` and `get_session` work.
    async fn store() -> Store {
        let store = Store::open_memory().await.expect("in-memory store");
        store
            .create_session(NewSession {
                id: SESSION.to_owned(),
                parent_id: None,
                project_id: oc_types::project_id_from_directory("/tmp/stream-resync")
                    .as_str()
                    .to_owned(),
                directory: "/tmp/stream-resync".to_owned(),
                title: None,
                agent: None,
                model: None,
                metadata: None,
                permissions: None,
                time_created: oc_types::now_millis(),
            })
            .await
            .expect("create session");
        store
    }

    /// Appends a durable event of `kind` and returns its dense sequence.
    async fn append(store: &Store, kind: &str) -> i64 {
        store
            .append_event(NewEvent {
                id: EventId::new().as_str().to_owned(),
                aggregate_id: SESSION.to_owned(),
                kind: kind.to_owned(),
                data: serde_json::json!({ "sessionID": SESSION }),
                durable: true,
                version: 1,
                created: oc_types::now_millis(),
            })
            .await
            .expect("append event")
    }

    /// The live bus overflows: the receiver observes `Lagged`, and the dropped
    /// terminal is recovered from the durable log rather than lost.
    ///
    /// A `capacity` of 1 with two published events is enough: the second one
    /// overwrites the first in the ring, so the receiver observably lags.
    #[tokio::test]
    async fn a_lagged_bus_resynchronises_and_replays_the_dropped_terminal() {
        let store = store().await;
        let (bus, mut rx) = broadcast::channel::<Arc<EventEnvelope>>(1);
        let dropped = append(&store, "session.text.ended").await;
        let terminal = append(&store, "session.execution.succeeded").await;
        for envelope in [
            EventEnvelope::now(
                "session.text.ended",
                serde_json::json!({ "sessionID": SESSION }),
            ),
            EventEnvelope::now(
                "session.execution.succeeded",
                serde_json::json!({ "sessionID": SESSION }),
            ),
        ] {
            bus.send(Arc::new(envelope)).expect("a receiver is alive");
        }

        // The bus really did drop the first event; without this the test would
        // prove nothing.
        let fault = match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(skipped)) => BusFault::Lagged(skipped),
            Ok(envelope) => panic!("expected to lag, got {envelope:?}"),
            Err(broadcast::error::RecvError::Closed) => panic!("expected a lag, not a close"),
        };
        assert_eq!(dropped + 1, terminal, "sequences are dense");

        let resync = Resync {
            store: &store,
            directory: "/tmp/stream-resync",
            session_id: SESSION,
        };
        // Cursor 0: nothing was handled yet, so the whole turn is replayed —
        // including the terminal the bus dropped.
        let recovery = resync
            .recover(0, fault, Instant::now() + RESYNC_BUDGET, 1)
            .await
            .expect("resync");
        let Recovery::Replay(queued) = recovery else {
            panic!("a lag must replay from the durable log, not stop");
        };
        let kinds: Vec<&str> = queued.iter().map(|event| event.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["session.text.ended", "session.execution.succeeded"],
            "the replay contains the dropped terminal"
        );
        let seqs: Vec<i64> = queued
            .iter()
            .filter_map(|event| event.durable.as_ref().map(|durable| durable.seq))
            .collect();
        assert_eq!(seqs, [dropped, terminal], "replayed in sequence order");
        assert_eq!(
            from_stored(
                store
                    .events_after(SESSION, 0, 10)
                    .await
                    .expect("read log")
                    .remove(1),
                "/tmp/stream-resync"
            )
            .kind,
            queued[1].kind,
            "the replay is the envelope the live bus would have delivered"
        );
    }

    /// A cursor already past the terminal replays nothing and does not invent a
    /// success: the caller keeps consuming the live bus.
    #[tokio::test]
    async fn a_caught_up_lag_replays_nothing_and_does_not_settle_the_turn() {
        let store = store().await;
        append(&store, "session.text.ended").await;
        let terminal = append(&store, "session.execution.succeeded").await;
        let resync = Resync {
            store: &store,
            directory: "/tmp/stream-resync",
            session_id: SESSION,
        };
        let recovery = resync
            .recover(
                terminal,
                BusFault::Lagged(3),
                Instant::now() + RESYNC_BUDGET,
                1,
            )
            .await
            .expect("resync");
        let Recovery::Replay(queued) = recovery else {
            panic!("a live bus keeps consuming after replaying what the log holds");
        };
        assert!(
            queued.is_empty(),
            "nothing durable follows the cursor, so nothing is replayed: {queued:?}"
        );
    }

    /// `Closed` with no terminal event is an error naming the cause — never the
    /// `Ok(())` the old `break` fell through to.
    #[tokio::test]
    async fn a_closed_bus_without_a_terminal_event_is_a_typed_error() {
        let store = store().await;
        append(&store, "session.text.ended").await;
        let resync = Resync {
            store: &store,
            directory: "/tmp/stream-resync",
            session_id: SESSION,
        };
        let recovery = resync
            .recover(0, BusFault::Closed, Instant::now() + RESYNC_BUDGET, 1)
            .await
            .expect("resync");
        let Recovery::Stop(StreamFault::BusClosed {
            session,
            last_seq,
            settled,
        }) = recovery
        else {
            panic!("a closed bus with no terminal must stop with a typed error");
        };
        assert_eq!(session, SESSION);
        assert_eq!(last_seq, 1, "it names how far the log was read");
        assert_eq!(
            settled, None,
            "an unconfirmed turn reports no settled outcome"
        );
        let error = anyhow::Error::new(StreamFault::BusClosed {
            session,
            last_seq,
            settled,
        });
        assert_eq!(
            error.to_string(),
            format!(
                "{SESSION}: the event bus closed after durable sequence 1 with no terminal execution \
                 event"
            ),
            "the message names the cause instead of reporting success"
        );
    }

    /// The budget is shared by every fault, so a stream that keeps lagging
    /// gives up with a typed error instead of looping forever.
    #[tokio::test]
    async fn an_exhausted_resync_budget_stops_with_a_typed_error() {
        let store = store().await;
        append(&store, "session.text.ended").await;
        let resync = Resync {
            store: &store,
            directory: "/tmp/stream-resync",
            session_id: SESSION,
        };
        let recovery = resync
            .recover(0, BusFault::Lagged(7), Instant::now(), 4)
            .await
            .expect("resync");
        let Recovery::Stop(fault) = recovery else {
            panic!("an expired budget must stop, not keep waiting");
        };
        let message = fault.to_string();
        assert!(
            message.contains("no terminal execution event arrived within"),
            "the error explains the wait it exhausted: {message}"
        );
        assert!(
            message.contains("4 resync attempt(s)"),
            "the error counts the attempts: {message}"
        );
    }

    /// A session row that already recorded a settled turn explains a missing
    /// terminal instead of hiding it: the error distinguishes a log anomaly
    /// from a turn that is still running.
    #[tokio::test]
    async fn a_settled_session_row_explains_the_missing_terminal() {
        let store = store().await;
        let now = oc_types::now_millis();
        store
            .mark_session_busy(SESSION, now)
            .await
            .expect("mark busy");
        store
            .mark_session_idle_with_reason(SESSION, now + 1, "succeeded", None)
            .await
            .expect("mark idle");
        let resync = Resync {
            store: &store,
            directory: "/tmp/stream-resync",
            session_id: SESSION,
        };
        let session = store.get_session(SESSION).await.expect("session");
        assert_eq!(
            settled_outcome(session.as_ref()).as_deref(),
            Some("succeeded"),
            "an idle row reports its outcome"
        );
        let Recovery::Stop(fault) = resync
            .recover(0, BusFault::Closed, Instant::now() + RESYNC_BUDGET, 1)
            .await
            .expect("resync")
        else {
            panic!("a closed bus without a terminal event must stop");
        };
        assert!(
            fault
                .to_string()
                .contains("already recorded a settled turn"),
            "the error names the missing terminal: {fault}"
        );
    }

    /// A busy session is not settled: the row must never be read as a terminal.
    #[tokio::test]
    async fn a_busy_session_row_is_not_treated_as_settled() {
        let store = store().await;
        let now = oc_types::now_millis();
        store
            .mark_session_idle_with_reason(SESSION, now, "succeeded", None)
            .await
            .expect("mark idle");
        store
            .mark_session_busy(SESSION, now + 1)
            .await
            .expect("mark busy");
        let session = store.get_session(SESSION).await.expect("session");
        assert_eq!(
            settled_outcome(session.as_ref()),
            None,
            "a newer time_busy means the turn is still running"
        );
    }

    /// The resync accepts exactly the terminals the print loop returns on. A
    /// mid-turn `session.compaction.ended` must not end the wait, or an
    /// auto-compacting turn would be reported as finished while still running.
    #[test]
    fn only_execution_events_close_a_turn() {
        for kind in TERMINALS {
            assert!(is_terminal(kind), "{kind} closes a turn");
        }
        for kind in [
            "session.compaction.ended",
            "session.compaction.started",
            "session.idle",
            "session.status",
            "session.text.ended",
        ] {
            assert!(!is_terminal(kind), "{kind} does not close a turn");
        }
    }
}
