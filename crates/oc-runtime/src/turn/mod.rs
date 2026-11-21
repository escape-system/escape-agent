//! The turn loop: prompt promotion, streaming, tools, compaction and settlement.
//!
//! Durable message rows are written before the events that describe them. The
//! assistant message is appended once per step and rewritten at text end, tool
//! call, tool result and step end; deltas are never persisted.

use std::sync::Arc;

use oc_types::payload::{SessionInboxDelivered, SessionStatusUpdated};
use oc_types::{MessageId, SessionStatus};
use serde_json::Value;

use crate::runtime::{ActorHandle, CancelToken, Runtime, RuntimeError, TerminalState};

mod assistant;
pub(crate) mod budget;
mod compaction;
mod finalize;
mod permissions;
mod pressure;
mod repetition;
mod run;
mod session;
mod settle;
mod stall;
mod stats;
mod stream;
mod tools;
mod types;
mod verify;

pub(crate) use pressure::ContextPressure;
pub(crate) use tools::report_tool_progress;
pub(crate) use types::{PendingToolCall, ToolOutcome};

use stats::TurnStats;
use types::ExecutionOutcome;

/// `session.execution.step_extended` — published once per granted extension.
///
/// Runtime-local on purpose: `oc-types` owns the upstream wire catalog and this
/// event is our observability for a turn that outlived `agent.steps`, so it
/// must not pretend to be an upstream payload. `steps` is the size of this
/// grant, `totalSteps` the cap after it, `grantIndex` the 1-based grant ordinal
/// and `reason` the policy rule that granted it. See `docs/24-step-budget.md`.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionExecutionStepExtended {
    #[serde(rename = "sessionID")]
    session_id: String,
    steps: usize,
    total_steps: usize,
    grant_index: usize,
    reason: &'static str,
}

impl oc_types::payload::EventPayload for SessionExecutionStepExtended {
    const TYPE: &'static str = "session.execution.step_extended";
    const DURABLE: bool = true;
    const VERSION: u32 = 1;
}

/// Promotes one inbox row unless the host already wrote the message; a row
/// that already exists is consumed as a doorbell and reported as the sentinel
/// `Some(Value::Null)`.
async fn begin_execution(
    runtime: &Runtime,
    session: &str,
    directory: &str,
    inbox_id: &str,
    handle: &Arc<ActorHandle>,
) -> Result<bool, RuntimeError> {
    // Some hosts materialise the user row themselves before notifying. When the
    // row exists, the inbox entry is only a doorbell: consume it instead of
    // failing on the primary-key conflict.
    let promoted = promote_inbox_row(&runtime.inner.config.store, session, inbox_id).await?;
    if promoted.is_none() {
        return Ok(false);
    }
    runtime
        .emit(
            session,
            directory,
            SessionInboxDelivered {
                session_id: Runtime::session_id(session),
                inbox_id: MessageId::from_wire(inbox_id.to_owned()),
            },
        )
        .await?;
    runtime
        .inner
        .config
        .store
        .mark_session_busy(session, oc_types::now_millis())
        .await?;
    // From this commit onward the session is durably busy: every exit from this
    // function must publish the terminal pair, including the actor failure path
    // that fires when an `Err` escapes the drain loop.
    handle.terminal.begin(true);
    runtime
        .emit(
            session,
            directory,
            SessionStatusUpdated {
                session_id: Runtime::session_id(session),
                status: SessionStatus::Busy,
            },
        )
        .await?;
    runtime
        .emit(
            session,
            directory,
            oc_types::payload::SessionExecutionStarted {
                session_id: Runtime::session_id(session),
            },
        )
        .await?;
    Ok(true)
}

async fn promote_inbox_row(
    store: &oc_store::Store,
    session: &str,
    id: &str,
) -> Result<Option<Value>, RuntimeError> {
    if store.get_message(session, id).await?.is_some() {
        store.cancel_inbox(session, id).await?;
        Ok(Some(Value::Null))
    } else {
        match store.promote_inbox(session, id).await {
            Ok(promoted) => Ok(promoted),
            Err(error) => {
                if store.get_message(session, id).await?.is_some() {
                    store.cancel_inbox(session, id).await?;
                    Ok(Some(Value::Null))
                } else {
                    Err(error.into())
                }
            }
        }
    }
}

pub(crate) async fn run_execution(
    runtime: &Runtime,
    session: &str,
    item: &Value,
    cancel: &Arc<CancelToken>,
    handle: &Arc<ActorHandle>,
) -> Result<(), RuntimeError> {
    let info_value = runtime
        .inner
        .config
        .store
        .get_session(session)
        .await?
        .ok_or_else(|| RuntimeError::SessionNotFound(session.to_owned()))?;
    let info = Runtime::parse_session(&info_value)?;
    let directory = info.location.directory.clone();
    let Some(inbox_id) = item.get("id").and_then(Value::as_str) else {
        return Ok(());
    };

    if !begin_execution(runtime, session, &directory, inbox_id, handle).await? {
        return Ok(());
    }

    let mut turn = match runtime.resolve_turn(&info, &directory).await {
        Ok(turn) => turn,
        Err(error) => {
            runtime
                .settle(
                    session,
                    &directory,
                    ExecutionOutcome::Failed(error.structured()),
                    &handle.terminal,
                )
                .await?;
            return Ok(());
        }
    };
    if let Err(error) = runtime
        .refresh_instructions(session, &mut turn, &info, handle)
        .await
    {
        runtime
            .settle(
                session,
                &directory,
                ExecutionOutcome::Failed(error.structured()),
                &handle.terminal,
            )
            .await?;
        return Ok(());
    }

    let mut stats = TurnStats::default();
    let outcome = runtime
        .run_steps(session, &mut turn, cancel, &mut stats)
        .await;
    // Clear the live step counter here, not inside `run_steps`: the loop body
    // has many `?` early returns, so any epilogue inside it is bypassable.
    // This is the single point every `run_steps` exit flows through.
    runtime.reset_step_budget(session);
    let outcome = outcome?;
    // A step-limited turn still answered on its final, tool-less request; it is
    // treated like a success for memory/scribe, but settles with the qualifier.
    let answered = matches!(
        outcome,
        ExecutionOutcome::Succeeded | ExecutionOutcome::StepLimited { .. }
    );
    if answered {
        runtime.touch_memory(&turn.memory_ids).await;
    }
    runtime
        .settle(session, &directory, outcome, &handle.terminal)
        .await?;
    if answered {
        runtime.spawn_scribe(session, &stats).await;
    }
    Ok(())
}

impl Runtime {
    /// Terminal repair after an error escaped the drain loop.
    ///
    /// The turn died before `settle` could run, so two things are missing: the
    /// assistant/tool rows the step left open, and the terminal event pair.
    /// Repair the rows first (durable), then publish `session.idle` and
    /// `session.execution.failed` — in that order, matching [`settle`] — so a
    /// consumer never sees a terminal execution event for a still-busy session.
    ///
    /// Idempotent with a settle that already ran: the shared [`TerminalState`]
    /// guard suppresses duplicate `session.idle` / `execution.*` publications
    /// and a fully settled turn has no open rows to repair.
    pub(crate) async fn finalize_actor_failure(
        &self,
        session: &str,
        error: &RuntimeError,
        terminal: &TerminalState,
    ) {
        let directory = match self.inner.config.store.get_session(session).await {
            Ok(Some(value)) => value
                .pointer("/location/directory")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            Ok(None) => String::new(),
            Err(lookup) => {
                tracing::warn!(session = %session, error = %lookup, "actor failure: session lookup failed");
                String::new()
            }
        };
        if let Err(repair) = self.repair_open_turn_rows(session).await {
            tracing::error!(
                session = %session,
                error = %repair,
                "actor failure: could not repair the rows left open by the stopped turn"
            );
        }
        if !terminal.is_owed() {
            return;
        }
        self.note_observer_execution(session, true).await;
        self.clear_background_request(session);
        let structured = error.structured();
        let now = oc_types::now_millis();
        if let Err(idle) = self
            .publish_idle_terminal(
                session,
                &directory,
                now,
                "failed",
                "failed",
                Some(&structured),
                None,
                terminal,
            )
            .await
        {
            // No committed idle row means the busy flag may still be set and no
            // terminal can be published ahead of it.
            tracing::error!(session = %session, error = %idle, "actor failure: session.idle was not published");
            return;
        }
        if let Err(failed) = self
            .publish_outcome_terminal(
                session,
                &directory,
                ExecutionOutcome::Failed(structured),
                terminal,
            )
            .await
        {
            tracing::error!(session = %session, error = %failed, "actor failure: session.execution.failed was not published");
        }
    }
}
