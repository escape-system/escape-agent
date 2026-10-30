//! Schedule runner: admits due schedule prompts, records firings and wakes sessions.

use anyhow::Context;
use oc_server::AppState;
use oc_store::{
    Delivery, NewInbox, ScheduleAction, ScheduleOutcome, ScheduleWhen, Store, StoreError,
};
use oc_types::{
    EventEnvelope, EventLocation, EventPayload, InboxItem, MessageId, SessionId,
    SessionInboxEnqueued, now_millis,
};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio::time::{Duration, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

/// How often the runner looks for due schedules.
const SCHEDULE_TICK: Duration = Duration::from_secs(1);
/// How many due schedules one tick admits.
const SCHEDULE_BATCH: usize = 16;
/// A firing this far past its due time is recorded as late.
const SCHEDULE_LATE_AFTER_MS: i64 = 60_000;

/// Start the schedule runner once the runtime owns the store.
///
/// Returns `None` when the server was built without a runtime or store: there
/// is then nothing to admit prompts into, so no task is spawned.
pub(crate) fn spawn_schedule_runner(
    state: AppState,
    shutdown: CancellationToken,
) -> Option<JoinHandle<()>> {
    let store = state.store()?.clone();
    state.runtime()?;
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SCHEDULE_TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let now = now_millis();
            let due = match store.due_schedules(now, SCHEDULE_BATCH).await {
                Ok(due) => due,
                Err(error) => {
                    tracing::warn!(%error, "schedule poll failed");
                    continue;
                }
            };
            for schedule in due {
                if shutdown.is_cancelled() {
                    break;
                }
                if let Err(error) = fire_schedule(&state, &schedule, now).await {
                    tracing::warn!(%error, "schedule failed to fire");
                }
            }
        }
    }))
}

/// Admit one due schedule's prompt, record the firing, then wake its session.
///
/// The inbox id is derived from the schedule id and run number so a crash
/// between admission and the firing record replays as a conflict instead of a
/// duplicate prompt. The firing is recorded before `notify`, so the next tick
/// cannot fire the same due time again.
#[allow(clippy::too_many_lines)] // one linear admit-record-wake sequence
async fn fire_schedule(state: &AppState, schedule: &Value, now: i64) -> anyhow::Result<()> {
    let store = state.store().context("schedule runner has no store")?;
    let Some(id) = schedule.get("id").and_then(Value::as_str) else {
        anyhow::bail!("due schedule has no id");
    };
    let due = schedule
        .get("nextDueAt")
        .and_then(Value::as_i64)
        .unwrap_or(now);
    let run_count = schedule
        .get("runCount")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1;
    let when: ScheduleWhen =
        match serde_json::from_value(schedule.get("when").cloned().unwrap_or(Value::Null)) {
            Ok(when) => when,
            Err(error) => {
                fail_schedule(
                    store,
                    id,
                    None,
                    run_count,
                    &format!("invalid when: {error}"),
                )
                .await;
                return Ok(());
            }
        };
    let next = match when.next_after(now, due) {
        Ok(next) => next,
        Err(error) => {
            fail_schedule(
                store,
                id,
                None,
                run_count,
                &format!("invalid when: {error}"),
            )
            .await;
            return Ok(());
        }
    };

    let Some(session_id) = schedule.get("sessionID").and_then(Value::as_str) else {
        fail_schedule(store, id, next, run_count, "schedule has no session").await;
        return Ok(());
    };
    let Some(session) = store.get_session(session_id).await? else {
        fail_schedule(store, id, next, run_count, "session no longer exists").await;
        return Ok(());
    };
    if session_archived(&session) {
        fail_schedule(store, id, next, run_count, "session is archived").await;
        return Ok(());
    }
    let prompt = match action_prompt(schedule.get("action")) {
        Ok(prompt) => prompt,
        Err(error) => {
            fail_schedule(store, id, next, run_count, &error).await;
            return Ok(());
        }
    };

    let outcome = schedule_outcome(due, now);
    let message_id = format!("msg_{id}_{run_count}");
    let payload = json!({ "text": prompt, "time": { "created": now } });

    // The prompt is admitted as a `queue` inbox row only. The runtime promotes
    // it when a turn settles, so a message must not be written here: an early
    // row would be visible to (and run alongside) a turn already in flight.
    match store
        .enqueue(NewInbox {
            id: message_id.clone(),
            session_id: session_id.to_owned(),
            kind: "user".to_owned(),
            payload,
            delivery: Delivery::Queue,
        })
        .await
    {
        Ok(item) => {
            let label = outcome.as_str();
            if let Err(error) = publish_inbox_enqueued(state, session_id, &message_id, &item).await
            {
                tracing::warn!(%error, "could not publish schedule inbox event");
            }
            publish_schedule_fired(state, &session, session_id, id, label);
        }
        // An earlier tick admitted this run but did not record it; finish the
        // record and wake the session instead of adding a second prompt.
        Err(StoreError::Conflict(_)) => {
            tracing::debug!(schedule = %id, "scheduled prompt already admitted");
        }
        Err(error) => return Err(error.into()),
    }

    match store
        .mark_schedule_fired(id, outcome, next, run_count)
        .await
        .with_context(|| format!("record schedule {id}"))
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            tracing::warn!(
                schedule = %id,
                "schedule vanished before its firing was recorded"
            );
        }
        Err(error) => return Err(error),
    }
    if let Some(runtime) = state.runtime()
        && let Err(error) = runtime.notify(session_id).await
    {
        tracing::warn!(session = %session_id, %error, "could not wake scheduled session");
    }
    Ok(())
}

/// Record a failed firing so the schedule advances (or finishes) without
/// retrying the same due time on every tick.
async fn fail_schedule(store: &Store, id: &str, next: Option<i64>, run_count: i64, reason: &str) {
    tracing::warn!(schedule = %id, %reason, "schedule could not fire");
    if let Err(error) = store
        .mark_schedule_fired(id, ScheduleOutcome::Failed, next, run_count)
        .await
    {
        tracing::warn!(schedule = %id, %error, "could not record failed schedule");
    }
}

/// The prompt text of a stored prompt action.
fn action_prompt(action: Option<&Value>) -> Result<String, String> {
    let action = action.ok_or_else(|| "schedule action is missing".to_owned())?;
    let action: ScheduleAction = serde_json::from_value(action.clone())
        .map_err(|error| format!("invalid schedule action: {error}"))?;
    match action {
        ScheduleAction::Prompt { prompt } if prompt.is_empty() => {
            Err("schedule prompt is empty".to_owned())
        }
        ScheduleAction::Prompt { prompt } => Ok(prompt),
    }
}

fn schedule_outcome(due: i64, now: i64) -> ScheduleOutcome {
    if now - due >= SCHEDULE_LATE_AFTER_MS {
        ScheduleOutcome::Late
    } else {
        ScheduleOutcome::Fired
    }
}

fn session_archived(session: &Value) -> bool {
    session
        .pointer("/time/archived")
        .is_some_and(|value| !value.is_null())
}

/// Persist the `session.inbox.enqueued` event for an admitted prompt.
async fn publish_inbox_enqueued(
    state: &AppState,
    session_id: &str,
    inbox_id: &str,
    item: &Value,
) -> anyhow::Result<()> {
    let item: InboxItem =
        serde_json::from_value(item.clone()).context("inbox row is not a wire item")?;
    let payload = SessionInboxEnqueued {
        session_id: SessionId::from_wire(session_id),
        inbox_id: MessageId::from_wire(inbox_id),
        item,
    };
    oc_server::data::events::publish_durable(
        state,
        session_id,
        SessionInboxEnqueued::TYPE,
        payload.data(),
        SessionInboxEnqueued::VERSION,
    )
    .await
    .map_err(|error| anyhow::anyhow!("publish inbox event: {}", error.0.body()))?;
    Ok(())
}

/// Publish the ephemeral `schedule.fired` event for a schedule that fired.
fn publish_schedule_fired(
    state: &AppState,
    session: &Value,
    session_id: &str,
    id: &str,
    outcome: &'static str,
) {
    let mut event = EventEnvelope::now(
        "schedule.fired",
        json!({
            "scheduleID": id,
            "sessionID": session_id,
            "outcome": outcome,
        }),
    );
    event.location = session
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .map(|directory| EventLocation {
            directory: directory.to_owned(),
        });
    state.publish(event);
}

#[cfg(test)]
mod tests {
    use oc_store::ScheduleOutcome;
    use serde_json::json;

    use super::{action_prompt, schedule_outcome};

    #[test]
    fn action_prompt_decodes_stored_prompt_actions() {
        let stored = json!({ "type": "prompt", "prompt": "Run the nightly audit" });
        assert_eq!(
            action_prompt(Some(&stored)).expect("prompt action"),
            "Run the nightly audit"
        );
    }

    #[test]
    fn action_prompt_rejects_missing_and_foreign_actions() {
        assert!(action_prompt(None).is_err());
        assert!(action_prompt(Some(&json!({ "type": "prompt" }))).is_err());
        assert!(action_prompt(Some(&json!({ "type": "prompt", "prompt": "" }))).is_err());
        assert!(action_prompt(Some(&json!({ "type": "command", "prompt": "hi" }))).is_err());
    }

    #[test]
    fn schedule_outcome_is_late_after_a_minute() {
        assert!(matches!(
            schedule_outcome(1_000, 1_000),
            ScheduleOutcome::Fired
        ));
        assert!(matches!(
            schedule_outcome(1_000, 60_999),
            ScheduleOutcome::Fired
        ));
        assert!(matches!(
            schedule_outcome(1_000, 61_000),
            ScheduleOutcome::Late
        ));
    }
}
