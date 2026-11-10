use std::time::Duration;

use oc_store::{Delivery, NewInbox};
use oc_tools::{ToolError, ToolResult};
use oc_types::MessageId;
use oc_types::payload::parse_step_limit;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::runtime::Runtime;

use super::protocol::{
    after_prompt, assistant_failure, child_state, envelope, envelope_reason, final_assistant_text,
};
use super::settings::SubagentSettings;
use super::tree::{runtime_error, store_error};

/// Poll interval while a foreground `subagent` call waits for its child.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Grace allowed for an interrupted child to settle before a timeout report is
/// delivered to the parent.
const TIMEOUT_SETTLE_GRACE: Duration = Duration::from_millis(500);

/// Poll ticks a queued prompt may stay unpromoted before the child is
/// considered dead (2 s at [`POLL_INTERVAL`]).
const CHILD_START_GRACE_TICKS: usize = 20;

/// Poll the child until this prompt's turn has produced a final answer.
///
/// Completion requires the prompt marker to exist **and** either a non-empty
/// final assistant text while idle, or a durable failure/cancel outcome. An
/// idle session before the marker is delivered (or before any assistant
/// message exists) keeps polling until the start grace expires: it is the
/// actor's pre-execution state, not a completed turn.
///
/// Error paths that bypass [`crate::turn`]'s settle step leave no idle row, so
/// the child session's existence, an errored assistant message and a prompt
/// that was never promoted are all treated as terminal failures.
///
/// `detach_on` is the session whose `POST /background` request ends the wait:
/// when it is set, one pending request is consumed and the wait returns
/// `("detached", "")` **without** touching the child. Watchers pass `None`, so
/// a detach request can never shorten a background delivery.
async fn wait_child(
    runtime: &Runtime,
    child: &str,
    prompt_id: &str,
    timeout: Option<Duration>,
    detach_on: Option<&str>,
) -> Result<(String, String), ToolError> {
    let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    let store = &runtime.inner.config.store;
    let mut observed_activity = false;
    let mut unstarted_ticks = 0usize;
    loop {
        if runtime.inner.shutdown.is_cancelled() {
            return Err(runtime_error(&crate::RuntimeError::Shutdown));
        }
        if let Some(session) = detach_on
            && runtime.has_background_request(session)
        {
            return Ok(("detached".to_owned(), String::new()));
        }
        let Some(session) = store
            .get_session(child)
            .await
            .map_err(|error| store_error(&error))?
        else {
            return Ok(("failed".to_owned(), "session deleted".to_owned()));
        };
        // The durable terminal outcome of the child's last settled turn. It is
        // written before the idle message, so it is the first source to trust.
        let durable = session
            .get("outcome")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let idle = runtime
            .is_idle(child)
            .await
            .map_err(|error| runtime_error(&error))?;
        let messages = runtime
            .list_all_messages(child)
            .await
            .map_err(|error| runtime_error(&error))?;
        let (started, tail) = after_prompt(&messages, prompt_id);
        let has_assistant = tail
            .iter()
            .any(|message| message.get("type").and_then(Value::as_str) == Some("assistant"));
        if started && !idle {
            observed_activity = true;
        }
        if !started && messages.is_empty() && idle {
            unstarted_ticks += 1;
            if unstarted_ticks >= CHILD_START_GRACE_TICKS {
                return Ok(("failed".to_owned(), "child never started".to_owned()));
            }
        } else {
            unstarted_ticks = 0;
        }
        if idle && started {
            let state = child_state(tail, durable.as_deref());
            if let Some(state @ ("failed" | "cancelled")) = state {
                let text = match final_assistant_text(tail).trim() {
                    "" => format!("subagent {state}"),
                    text => text.to_owned(),
                };
                return Ok((state.to_owned(), text));
            }
            if let Some((state, text)) = assistant_failure(tail) {
                return Ok((state, text));
            }
            if has_assistant {
                let text = final_assistant_text(tail);
                if text.trim().is_empty() {
                    return Ok((
                        "failed".to_owned(),
                        "subagent finished without final text".to_owned(),
                    ));
                }
                // Assistant text alone is not a result: only a turn the child
                // settled as `succeeded` may be reported as completed. A child
                // that went idle without settling (the actor died, or an error
                // path bypassed `turn::settle`) did not deliver.
                if state == Some("completed") {
                    return Ok(("completed".to_owned(), text));
                }
                return Ok((
                    "failed".to_owned(),
                    "subagent ended without a settled outcome".to_owned(),
                ));
            }
            if observed_activity {
                return Ok((
                    "failed".to_owned(),
                    "subagent ended without a final message".to_owned(),
                ));
            }
        }
        if let Some(deadline) = deadline
            && tokio::time::Instant::now() >= deadline
        {
            return Ok(("running".to_owned(), String::new()));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

pub(super) fn spawn_watcher(
    runtime: Runtime,
    parent: String,
    child: String,
    agent: String,
    description: String,
    prompt_id: String,
    timeout: Duration,
) {
    let should_watch = {
        let mut state = runtime.inner.subagents.lock().expect("subagent state");
        state
            .active_watchers
            .insert((parent.clone(), child.clone()))
    };
    if !should_watch {
        tracing::debug!(
            parent = %parent,
            child = %child,
            "watcher already active for child; skipping duplicate watcher"
        );
        return;
    }

    tokio::spawn(async move {
        let outcome = match wait_child(&runtime, &child, &prompt_id, Some(timeout), None).await {
            Ok((state, _)) if state == "running" => {
                interrupt_timed_out(&runtime, &child, &prompt_id).await;
                timeout_failure(timeout)
            }
            Ok((state, text)) => (state, text, None),
            Err(error) => {
                let reason = error.message.clone();
                ("failed".to_owned(), reason.clone(), Some(reason))
            }
        };
        {
            let mut state = runtime.inner.subagents.lock().expect("subagent state");
            state
                .active_watchers
                .remove(&(parent.clone(), child.clone()));
        }
        deliver_subagent_result(&runtime, &parent, &child, &agent, &description, outcome).await;
    });
}

/// Reports a timeout whose budget was already consumed by the caller.
///
/// The foreground path interrupts the child itself, so the watcher only gives
/// the actor a bounded settle window and then delivers the single timeout
/// failure. It never waits another full timeout.
fn spawn_timeout_watcher(
    runtime: Runtime,
    parent: String,
    child: String,
    agent: String,
    description: String,
    prompt_id: String,
    timeout: Duration,
) {
    let should_watch = {
        let mut state = runtime.inner.subagents.lock().expect("subagent state");
        state
            .active_watchers
            .insert((parent.clone(), child.clone()))
    };
    if !should_watch {
        return;
    }
    tokio::spawn(async move {
        let _ = wait_child(
            &runtime,
            &child,
            &prompt_id,
            Some(TIMEOUT_SETTLE_GRACE),
            None,
        )
        .await;
        {
            let mut state = runtime.inner.subagents.lock().expect("subagent state");
            state
                .active_watchers
                .remove(&(parent.clone(), child.clone()));
        }
        deliver_subagent_result(
            &runtime,
            &parent,
            &child,
            &agent,
            &description,
            timeout_failure(timeout),
        )
        .await;
    });
}

/// Interrupts `child` when the parent's execution token is cancelled. It runs
/// detached from the tool future, which the turn loop drops on cancellation.
fn spawn_cancel_guard(
    runtime: &Runtime,
    child: &str,
    prompt_id: &str,
    token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let runtime = runtime.clone();
    let child = child.to_owned();
    let prompt_id = prompt_id.to_owned();
    tokio::spawn(async move {
        token.cancelled().await;
        interrupt_timed_out(&runtime, &child, &prompt_id).await;
    })
}

/// Cancels a child that exhausted its timeout so its actor settles and any
/// pending permission ask is dropped, then waits a bounded settle grace.
async fn interrupt_timed_out(runtime: &Runtime, child: &str, prompt_id: &str) {
    match runtime.interrupt(child).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(child = %child, "timed-out subagent had no running execution to interrupt");
        }
        Err(error) => {
            tracing::warn!(child = %child, error = %error, "timed-out subagent could not be interrupted");
        }
    }
    let _ = wait_child(runtime, child, prompt_id, Some(TIMEOUT_SETTLE_GRACE), None).await;
}

fn timeout_failure(timeout: Duration) -> (String, String, Option<String>) {
    let reason = format!("timeout after {}s", timeout.as_secs());
    ("failed".to_owned(), reason.clone(), Some(reason))
}

/// Mirrors a terminal state decided by the parent-side wait onto the child's
/// durable session row.
///
/// [`wait_child`] can conclude a child failed without the child's own turn ever
/// recording that: a prompt that never started, a child that produced no final
/// text, or a timeout. Without this write the parent's tool result says
/// `failed` while `session_v2.idle_outcome` still says `succeeded`, so the
/// durable record disagrees with the delivered outcome.
///
/// Idempotent: when the child already recorded the same terminal outcome the
/// row is left untouched, so this never rewrites `time_idle` for a child that
/// settled itself.
async fn persist_child_outcome(runtime: &Runtime, child: &str, state: &str) {
    let outcome = match state {
        "failed" => "failed",
        "cancelled" => "interrupted",
        _ => return,
    };
    let store = &runtime.inner.config.store;
    match store.get_session(child).await {
        Ok(Some(session)) => {
            // The child already recorded a failure-ish terminal state that
            // agrees with the parent-side view: do not rewrite it.
            if let Some("failed" | "interrupted") = session.get("outcome").and_then(Value::as_str) {
                return;
            }
            // `succeeded`, `step_limit` (a completed turn with a qualifier) or
            // no outcome at all contradicts the failure the parent is about to
            // deliver; that is the divergence to repair.
        }
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(
                child = %child,
                error = %error,
                "subagent outcome could not be read back from the child session"
            );
            return;
        }
    }
    if let Err(error) = store
        .mark_session_idle(child, oc_types::now_millis(), outcome)
        .await
    {
        tracing::warn!(
            child = %child,
            error = %error,
            "subagent outcome was not persisted on the child session"
        );
    }
}

/// Appends the step-budget qualifier to a completed child's envelope and
/// records it in the parent-facing metadata.
///
/// `steps == None` leaves both untouched, so every other delivery path stays
/// byte-identical. The state stays `completed`: the child did answer, and the
/// qualifier is the honest caveat on that answer.
pub(crate) fn apply_step_limit(body: &str, metadata: &mut Value, steps: Option<u64>) -> String {
    let Some(steps) = steps else {
        return body.to_owned();
    };
    metadata["stepLimit"] = json!(true);
    metadata["steps"] = json!(steps);
    format!("{body}\n[step budget exhausted after {steps} steps; result may be incomplete]")
}

/// Steps consumed by the child's last settled turn when it stopped at its step
/// budget.
///
/// The marker is durable in two places: `session_v2.idle_reason` (the reason
/// [`super::protocol`] parses) and the child's own
/// `session.execution.step_limit` event, which the runtime publishes after the
/// terminal execution event. The event is authoritative for a `succeeded`
/// turn, because `idle_reason` is internal and the store surfaces it only for
/// interrupted and step-limited rows.
async fn child_step_limit(runtime: &Runtime, child: &str) -> Option<u64> {
    let store = &runtime.inner.config.store;
    if let Ok(Some(node)) = store.session_node(child).await
        && let Some(steps) = node.idle_reason.as_deref().and_then(parse_step_limit)
    {
        return Some(steps);
    }
    let mut after = 0;
    let mut marker: Option<(i64, u64)> = None;
    let mut terminal = 0;
    loop {
        let events = store.events_after(child, after, 1_000).await.ok()?;
        let Some(last) = events.last() else {
            break;
        };
        after = last.seq;
        for event in &events {
            match event.kind.as_str() {
                "session.execution.succeeded"
                | "session.execution.failed"
                | "session.execution.interrupted" => terminal = event.seq,
                "session.execution.step_limit" => {
                    if let Some(steps) = event.data.get("steps").and_then(Value::as_u64) {
                        marker = Some((event.seq, steps));
                    }
                }
                _ => {}
            }
        }
        if events.len() < 1_000 {
            break;
        }
    }
    // A marker from an older turn is stale: only one after the newest
    // terminal execution event describes the child's last settled turn.
    marker
        .filter(|(seq, _)| *seq > terminal)
        .map(|(_, steps)| steps)
}

pub(crate) async fn deliver_subagent_result(
    runtime: &Runtime,
    parent: &str,
    child: &str,
    agent: &str,
    description: &str,
    (state, text, reason): (String, String, Option<String>),
) {
    persist_child_outcome(runtime, child, &state).await;
    let text = if text.trim().is_empty() {
        "subagent failed".to_owned()
    } else {
        text
    };
    let mut body = match &reason {
        Some(reason) => envelope_reason(&state, child, Some(description), reason, &text),
        None => envelope(&state, child, Some(description), &text),
    };
    let mut metadata = json!({
        "source": "subagent",
        "childID": child,
        "agent": agent,
        "state": state,
    });
    if let Some(reason) = &reason {
        metadata["reason"] = json!(reason);
    }
    let steps = match &reason {
        Some(reason) => parse_step_limit(reason),
        None => None,
    }
    .or(match state.as_str() {
        "completed" => child_step_limit(runtime, child).await,
        _ => None,
    });
    body = apply_step_limit(&body, &mut metadata, steps);
    let payload = json!({ "text": body, "metadata": metadata });
    let item = NewInbox {
        id: MessageId::new().into_inner(),
        session_id: parent.to_owned(),
        kind: "synthetic".to_owned(),
        payload,
        delivery: Delivery::Steer,
    };
    if let Err(error) = runtime.enqueue_and_emit(item).await {
        tracing::warn!(parent = %parent, error = %error, "subagent completion was not delivered");
        return;
    }
    if let Err(error) = runtime.notify(parent).await {
        tracing::warn!(parent = %parent, error = %error, "subagent completion did not wake the parent");
    }
}

pub(super) async fn enqueue_prompt(
    runtime: &Runtime,
    session: &str,
    prompt: &str,
    parent: &str,
) -> Result<String, ToolError> {
    let id = MessageId::new().into_inner();
    runtime
        .enqueue_and_emit(NewInbox {
            id: id.clone(),
            session_id: session.to_owned(),
            kind: "user".to_owned(),
            payload: json!({
                "text": prompt,
                "metadata": { "source": "subagent", "parentID": parent },
            }),
            delivery: Delivery::Steer,
        })
        .await
        .map_err(|error| runtime_error(&error))?;
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_foreground(
    runtime: &Runtime,
    parent: &str,
    child: &str,
    agent: &str,
    description: &str,
    prompt_id: &str,
    settings: &SubagentSettings,
    cancel: Option<&CancellationToken>,
) -> Result<ToolResult, ToolError> {
    // The turn loop drops the tool future as soon as the execution is
    // cancelled; this detached guard is what stops the child in that case, so
    // an interrupted parent cannot leave an orphan burning tokens.
    let guard = cancel.map(|token| spawn_cancel_guard(runtime, child, prompt_id, token.clone()));
    let outcome = wait_child(
        runtime,
        child,
        prompt_id,
        Some(settings.timeout),
        Some(parent),
    )
    .await;
    if let Some(guard) = guard {
        guard.abort();
    }
    let (state, text) = outcome?;
    let running = json!({ "sessionID": child, "status": "running" });
    if state == "detached" {
        // `POST /background` arrived while this turn waited on the child: hand
        // the wait to a watcher so the turn can continue. The child is not
        // interrupted; its final text is delivered as a synthetic message.
        spawn_watcher(
            runtime.clone(),
            parent.to_owned(),
            child.to_owned(),
            agent.to_owned(),
            description.to_owned(),
            prompt_id.to_owned(),
            settings.timeout,
        );
        return Ok(ToolResult::text(running.to_string()).with_metadata(running));
    }
    if state == "running" {
        // The child consumed its full budget: cancel it now so it settles and
        // any pending permission ask is dropped, then report the timeout with
        // a short settle window instead of waiting another full timeout.
        match runtime.interrupt(child).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(child = %child, "timed-out subagent had no running execution to interrupt");
            }
            Err(error) => {
                tracing::warn!(child = %child, error = %error, "timed-out subagent could not be interrupted");
            }
        }
        spawn_timeout_watcher(
            runtime.clone(),
            parent.to_owned(),
            child.to_owned(),
            agent.to_owned(),
            description.to_owned(),
            prompt_id.to_owned(),
            settings.timeout,
        );
        // The timeout is a FAILURE for the caller: returning `Ok` here would be
        // reported as `session.tool.success`, and the parent model would plan on
        // a child that was just cancelled. The child id is in both the message
        // and the metadata so the model can still tell which child timed out.
        let error = ToolError::new(format!(
            "subagent {child} timed out after {}s",
            settings.timeout.as_secs()
        ))
        .with_metadata(json!({
            "sessionID": child,
            "status": "failed",
            "reason": format!("timeout after {}s", settings.timeout.as_secs()),
        }));
        return Err(error);
    }
    persist_child_outcome(runtime, child, &state).await;
    let body = envelope(&state, child, None, &text);
    if matches!(state.as_str(), "failed" | "cancelled") {
        // A child that failed or was interrupted is not a result the parent may
        // plan on: returning `Ok` here would be reported as
        // `session.tool.success` even though the child never produced one. The
        // envelope (with the child id and its final text) is the message, so the
        // model still sees what the child said before it failed.
        let error = ToolError::new(body).with_metadata(json!({
            "sessionID": child,
            "status": state,
        }));
        return Err(error);
    }
    Ok(ToolResult::text(body).with_metadata(json!({ "sessionID": child, "status": state })))
}

#[cfg(test)]
mod tests {
    use super::apply_step_limit;
    use serde_json::json;

    #[test]
    fn step_limit_qualifies_a_completed_envelope() {
        let mut metadata = json!({"state": "completed"});
        let body = apply_step_limit(
            "<subagent state=\"completed\">x</subagent>",
            &mut metadata,
            Some(7),
        );
        assert_eq!(
            metadata,
            json!({"state": "completed", "stepLimit": true, "steps": 7})
        );
        assert!(
            body.ends_with("\n[step budget exhausted after 7 steps; result may be incomplete]")
        );
    }

    #[test]
    fn no_step_limit_leaves_the_envelope_untouched() {
        let mut metadata = json!({"state": "completed"});
        assert_eq!(apply_step_limit("x", &mut metadata, None), "x");
        assert_eq!(metadata, json!({"state": "completed"}));
    }
}
