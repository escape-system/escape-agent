//! Step-budget reconstruction from the durable event log.
//!
//! `session.execution.step_extended` is `DURABLE = true`, so the effective cap
//! must be a projection of committed state rather than of process memory: a
//! process that dies mid-turn leaves the grant event behind and no terminal, and
//! the runtime that boots next has to derive the same cap from what it persisted.
//! These are the scenario tests for `crate::turn::step_grants`; the arithmetic is
//! unit-tested there.
//!
//! The "crash" is modelled the way `recovery.rs` models it: the durable prefix a
//! killed process leaves (an open execution plus its grant events and **no**
//! terminal) is written through the store, then a second `Runtime` over the same
//! SQLite file runs the resumed turn. Assertions are on observable behaviour —
//! the effective cap the turn ended at, the number of provider calls it was
//! allowed, and the number of durable grant events — never on source text.
//!
//! Every fixture uses the **default** `step_budget` policy (`extension_steps` 15,
//! `max_extensions` 2, `min_cap` 10, `ceiling_factor` 2), because a
//! `step_budget` section of `OPENCODE_CONFIG_CONTENT` does not currently survive
//! configuration normalization: `step_budget` is listed as handled
//! (`crates/oc-config/src/normalize/keys.rs:68`) but `normalize/root.rs` never
//! copies it and the unknown-key loop skips handled keys, so
//! `BudgetPolicy::from_merged` only ever sees the defaults in a live runtime.
//! That defect belongs to `oc-config`, which this change does not own; the unit
//! tests in `budget.rs` cover the parsing from a raw JSON value directly.

use std::sync::Arc;
use std::time::Duration;

use oc_runtime::Runtime;
use oc_store::{NewEvent, StoredEvent};
use serde_json::{Value, json};

use crate::support::*;

const GRANT: &str = "session.execution.step_extended";
/// Default cap for these fixtures (`agents.engineer.steps`).
const INITIAL: u64 = 10;
/// Default `step_budget.extension_steps`.
const EXTENSION: u64 = 15;
/// Default ceiling: `INITIAL * step_budget.ceiling_factor`.
const CEILING: u64 = 20;

/// `n` successful `read` tool steps.
fn reads(n: usize) -> Vec<Script> {
    (0..n)
        .map(|index| {
            tool_script(
                &format!("call_{index}"),
                json!({"path": "hello.txt", "expectation": expectation()}),
            )
        })
        .collect()
}

/// `n` failing `read` tool steps: every one produces the identical error, so the
/// deterministic policy never sees recent progress and refuses to extend.
fn failing_reads(n: usize) -> Vec<Script> {
    (0..n)
        .map(|index| {
            tool_script(
                &format!("call_{index}"),
                json!({"path": "missing.txt", "expectation": expectation()}),
            )
        })
        .collect()
}

/// The durable prefix a process killed at the step boundary leaves: an open
/// execution (`session.execution.started`) and nothing that closes it.
async fn open_execution(harness: &Harness) {
    harness
        .store
        .append_event(NewEvent {
            id: "evt_d4_started".to_owned(),
            aggregate_id: harness.session.clone(),
            kind: "session.execution.started".to_owned(),
            data: json!({ "sessionID": harness.session }),
            durable: true,
            version: 1,
            created: oc_types::now_millis(),
        })
        .await
        .expect("started event");
}

/// One durable `session.execution.step_extended` exactly as `run_steps` emits it.
async fn record_grant(harness: &Harness, index: u64, steps: u64, total: u64) {
    harness
        .store
        .append_event(NewEvent {
            id: format!("evt_d4_grant_{index}"),
            aggregate_id: harness.session.clone(),
            kind: GRANT.to_owned(),
            data: json!({
                "sessionID": harness.session,
                "steps": steps,
                "totalSteps": total,
                "grantIndex": index,
                "reason": "recent tool progress"
            }),
            durable: true,
            version: 1,
            created: oc_types::now_millis(),
        })
        .await
        .expect("grant event");
}

/// Every durable event of the session, oldest first.
async fn stored_events(harness: &Harness) -> Vec<StoredEvent> {
    let mut after = 0_i64;
    let mut all = Vec::new();
    loop {
        let page = harness
            .store
            .events_after(&harness.session, after, 1_000)
            .await
            .expect("event log");
        let Some(last) = page.last() else {
            return all;
        };
        after = last.seq;
        all.extend(page);
    }
}

async fn stored_grants(harness: &Harness) -> Vec<Value> {
    stored_events(harness)
        .await
        .into_iter()
        .filter(|event| event.kind == GRANT)
        .map(|event| event.data)
        .collect()
}

/// Enqueue one prompt and wait for the runtime under test to finish the turn.
async fn prompt_on(harness: &Harness, runtime: &Runtime, text: &str) {
    harness.enqueue(text).await;
    runtime.notify(&harness.session).await.expect("notify");
    runtime
        .wait_idle(&harness.session, Duration::from_secs(60))
        .await
        .expect("idle");
}

/// The `steps` of the single `session.execution.step_limit` the turn published.
fn step_limit(harness: &Harness) -> u64 {
    let limits = harness.sink.data("session.execution.step_limit");
    assert_eq!(
        limits.len(),
        1,
        "the exhausted budget is qualified exactly once: {limits:?}"
    );
    limits[0]["steps"].as_u64().expect("steps is a number")
}

/// The stored outcome of the session row.
async fn outcome(harness: &Harness) -> String {
    harness
        .store
        .get_session(&harness.session)
        .await
        .expect("session row")
        .expect("session exists")["outcome"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// D4-1 — a grant recorded before the crash still budgets the resumed turn.
///
/// One 10-step grant is durable with no terminal, so the open window replays to
/// `INITIAL + 10 = 20`. The resumed turn makes no progress (every read fails
/// identically), so it cannot earn a replacement grant: it must run 20 steps and
/// stop at 20. A runtime that rebuilt the budget from `agent.steps` would stop at
/// 10.
#[tokio::test]
async fn a_grant_recorded_before_the_crash_still_budgets_the_resumed_turn() {
    let mut scripts = failing_reads(usize::try_from(INITIAL + 10).expect("fits"));
    scripts.push(text_script("still here"));
    let harness = Harness::new(
        scripts,
        Arc::new(AllowAll),
        json!({"agents": {"engineer": {"steps": INITIAL}}}),
        None,
    )
    .await;
    open_execution(&harness).await;
    record_grant(&harness, 1, 10, INITIAL + 10).await;

    let runtime = harness.restart();
    runtime.start().await.expect("start after crash");
    assert!(
        !harness.sink.has("session.execution.interrupted"),
        "the prefix models a crash that left no terminal, so the window is still open"
    );
    prompt_on(&harness, &runtime, "keep going").await;

    assert_eq!(
        step_limit(&harness),
        INITIAL + 10,
        "the recorded grant survived the restart"
    );
    assert_eq!(
        harness.model.calls(),
        usize::try_from(INITIAL + 10 + 1).expect("fits"),
        "20 inherited tool-enabled steps plus the tool-less final call"
    );
    assert_eq!(
        stored_grants(&harness).await.len(),
        1,
        "replaying the log must not append a second copy of the recorded grant"
    );
    assert_eq!(outcome(&harness).await, "step_limit");
}

/// D4-2 — a restart must not resurrect budget past the logged ceiling.
///
/// Two 10-step grants are durable, which spends both the grant count
/// (`max_extensions` 2) and the ceiling (20) exactly, so the cap replays to 30.
/// The resumed turn *would* qualify for an automatic extension (every read
/// succeeds) but must still be refused, and must not be reset to `agent.steps`.
#[tokio::test]
async fn a_restart_does_not_resurrect_budget_past_the_logged_ceiling() {
    let mut scripts = reads(usize::try_from(INITIAL + CEILING).expect("fits"));
    scripts.push(text_script("spent"));
    let harness = Harness::new(
        scripts,
        Arc::new(AllowAll),
        json!({"agents": {"engineer": {"steps": INITIAL}}}),
        None,
    )
    .await;
    open_execution(&harness).await;
    record_grant(&harness, 1, 10, INITIAL + 10).await;
    record_grant(&harness, 2, 10, INITIAL + CEILING).await;

    let runtime = harness.restart();
    runtime.start().await.expect("start after crash");
    prompt_on(&harness, &runtime, "spend it").await;

    assert_eq!(
        stored_grants(&harness).await.len(),
        2,
        "a spent ceiling must not buy a third grant"
    );
    assert_eq!(
        step_limit(&harness),
        INITIAL + CEILING,
        "the logged grants are inherited and no extension is granted above them"
    );
    assert_eq!(
        harness.model.calls(),
        usize::try_from(INITIAL + CEILING + 1).expect("fits"),
        "30 inherited steps plus the final call; a reset to `agent.steps` would be 11"
    );
}

/// D4-3 — the durable event is emitted exactly once per grant.
///
/// One 5-step grant is durable (cap 15) and the resumed turn shows real progress,
/// so it earns the one remaining grant of 15 (5 + 15 is the ceiling exactly). The
/// log must then hold exactly two grant events with ordinals 1 and 2: the
/// recorded grant is neither duplicated by the replay nor renumbered by the
/// resumed turn.
#[tokio::test]
async fn a_recorded_grant_is_not_re_emitted_when_the_turn_resumes() {
    let mut scripts = reads(usize::try_from(INITIAL + CEILING).expect("fits"));
    scripts.push(text_script("done"));
    let harness = Harness::new(
        scripts,
        Arc::new(AllowAll),
        json!({"agents": {"engineer": {"steps": INITIAL}}}),
        None,
    )
    .await;
    open_execution(&harness).await;
    record_grant(&harness, 1, 5, INITIAL + 5).await;

    let runtime = harness.restart();
    runtime.start().await.expect("start after crash");
    prompt_on(&harness, &runtime, "keep going").await;

    let grants = stored_grants(&harness).await;
    assert_eq!(
        grants.len(),
        2,
        "one recorded grant plus one new one, each exactly once: {grants:?}"
    );
    let indexes: Vec<u64> = grants
        .iter()
        .map(|grant| grant["grantIndex"].as_u64().expect("grantIndex"))
        .collect();
    assert_eq!(
        indexes,
        vec![1, 2],
        "the recorded grant keeps its ordinal; the resumed turn continues it"
    );
    let totals: Vec<u64> = grants
        .iter()
        .map(|grant| grant["totalSteps"].as_u64().expect("totalSteps"))
        .collect();
    assert_eq!(
        totals,
        vec![INITIAL + 5, INITIAL + CEILING],
        "the recorded total is unchanged and the new one is 15 + 15"
    );
    assert_eq!(
        grants[1]["steps"], EXTENSION,
        "the resumed turn granted the default extension size"
    );
    assert_eq!(
        step_limit(&harness),
        INITIAL + CEILING,
        "the turn ends at the ceiling: 10 initial + 5 + 15 granted"
    );
    assert_eq!(
        harness.model.calls(),
        usize::try_from(INITIAL + CEILING + 1).expect("fits"),
        "30 tool steps plus the final call"
    );
}

/// Negative control for over-inheritance: a settled turn closes the window.
///
/// Turn 1 uses its one reachable grant (cap 10 -> 25) and settles, publishing
/// `session.execution.succeeded`. Turn 2, after a restart, must start from
/// `agent.steps` again: it ends at 10 and adds no grant. This guard passes before
/// and after the replay change; it exists so a replay that inherits a *closed*
/// window is caught.
#[tokio::test]
async fn a_settled_turn_does_not_lend_its_grant_to_the_next_turn() {
    let mut scripts = reads(usize::try_from(INITIAL + EXTENSION).expect("fits"));
    scripts.push(text_script("all done"));
    // Turn 2: identical failures, so the policy would deny an extension anyway.
    scripts.extend(failing_reads(usize::try_from(INITIAL).expect("fits")));
    scripts.push(text_script("nothing to read"));
    let harness = Harness::new(
        scripts,
        Arc::new(AllowAll),
        json!({"agents": {"engineer": {"steps": INITIAL}}}),
        None,
    )
    .await;

    harness.prompt("start").await;
    assert_eq!(
        stored_grants(&harness).await.len(),
        1,
        "turn 1 was extended once"
    );
    assert_eq!(
        harness.sink.count("session.execution.succeeded"),
        1,
        "turn 1 settled, so its window is closed"
    );
    assert_eq!(
        harness.model.calls(),
        usize::try_from(INITIAL + EXTENSION + 1).expect("fits"),
        "10 initial + 15 granted + 1 final call"
    );
    harness.runtime.shutdown().await;

    let runtime = harness.restart();
    runtime.start().await.expect("restart");
    harness.sink.events.lock().expect("events").clear();
    prompt_on(&harness, &runtime, "again").await;

    assert_eq!(
        step_limit(&harness),
        INITIAL,
        "a settled turn's grant must not be inherited by the next turn"
    );
    assert_eq!(
        stored_grants(&harness).await.len(),
        1,
        "the next turn earned no extension and duplicated none"
    );
    assert_eq!(
        harness.model.calls(),
        usize::try_from(INITIAL + EXTENSION + 1 + INITIAL + 1).expect("fits"),
        "26 calls for turn 1 plus 11 for turn 2"
    );
}
