# 29 — Step budget extension

A subagent at 49 of 50 steps with its tests unrun loses the whole turn; deciding
whether to extend it by calling a model *in the stop path* would add a provider
round trip exactly when the turn is ending. The decision is instead
deterministic, in memory, and never an LLM call: the runtime extends the budget
only when the turn shows visible progress. The agent can still *ask* early
through the observer path (see *Early warning and budget requests*), but the
grant itself is decided by the deterministic policy below.

## Current behaviour

- `DEFAULT_STEPS = 50` (`crates/oc-runtime/src/turn/session.rs:21`); `agent.steps`
  overrides it, floored at 1 (`crates/oc-runtime/src/turn/session.rs:147-152`).
- `run_steps` keeps the live budget in `cap`, initialised from `turn.max_steps`
  (`crates/oc-runtime/src/turn/mod.rs:314`). `final_call` is `step_number >= cap`
  (`crates/oc-runtime/src/turn/mod.rs:394`), so the limit request goes out with
  tools disabled (`crates/oc-runtime/src/turn/stream.rs:113`) and carries
  `MAX_STEPS_PROMPT` (`crates/oc-runtime/src/turn/mod.rs:409`); tool defs stay so
  the prompt cache survives (`docs/04`).
- The terminal branch returns `ExecutionOutcome::StepLimited { steps:
  turn.max_steps }` (`crates/oc-runtime/src/turn/mod.rs:567`). Settling stores
  `step_limit` with the wire terminal `succeeded`
  (`crates/oc-runtime/src/turn/mod.rs:695`), records
  `step limit reached after N steps` (`crates/oc-runtime/src/turn/mod.rs:707`;
  grammar in `crates/oc-types/src/payload.rs:56-62`) and publishes
  `session.execution.step_limit {sessionID, steps}` after it
  (`crates/oc-runtime/src/turn/mod.rs:737-746`).
- The observer sees the real step index and is woken on step-limit:
  `run_steps` mirrors `step_number` into the session actor (`Runtime::set_step`)
  and `observer_pass` renders it as `RootContext.step`
  (`crates/oc-runtime/src/observer/tick.rs:139`), resetting to `0` when the turn
  finalizes; a step-limited settle then raises the interrupted host flag
  (`note_observer_execution(session, false)`,
  `crates/oc-runtime/src/turn/mod.rs:759`), so the next tick evaluates the
  now-idle root and reads `step limit reached after N steps` from its tree row.
  That flag is a wake marker only: `note_host_event` sets `pending_wake` and
  `last_activity_ms` in the engine's in-memory `RootState`
  (`crates/oc-observer/src/engine/run.rs:75-78`) and persists nothing, and
  `user_stopped` — the only terminal state — is reserved for
  `ExecutionInterruptedByUser`. The stored outcome stays `step_limit` with the
  wire terminal `succeeded`.

## Decision

1. The observer never sees tool outputs: it works from terse flight strips and
   "never sees tool arguments or outputs" (`docs/08-goals-observer.md:3-6`), and
   the only outcome text crossing that boundary is capped at 60 chars
   (`crates/oc-observer/src/registry.rs:24`). Thrash detection needs the full
   `(name, message)` history, so the evidence stays in the runtime.
2. Calling the observer from the step loop would await while engine state is
   mutex-guarded: `with_root` runs its closure under `self.state.lock()`
   (`crates/oc-observer/src/engine/run.rs:342-349`). Awaiting an engine call
   inside `run_steps` has no precedent and would park the loop behind a lock.

## Policy

The extension policy is deterministic and configurable through the `step_budget`
object in the merged config. Every key has a default equal to the legacy
constant, so a config that does not set `step_budget` keeps the same behaviour:

| Config key | Default | Meaning |
|---|---|---|
| `step_budget.extension_steps` | `15` | steps granted by one extension |
| `step_budget.max_extensions` | `2` | maximum grants per turn |
| `step_budget.min_cap` | `10` | caller-enforced floor; below it evaluation is skipped |
| `step_budget.thrash_limit` | `3` | repeats of one `(name, message)` error that deny |
| `step_budget.window` | `5` | trailing observations checked for recent success |
| `step_budget.ceiling_factor` | `2` | total granted <= initial cap * factor |

`budget::evaluate_with_policy` (`crates/oc-runtime/src/turn/budget.rs`) is a pure
function of the observations, `tool_calls`, the grants made and the initial cap;
first match wins and every rule fails closed. `budget::evaluate` delegates to
the default policy:

| Rule | Condition | Result |
|---|---|---|
| 1 | `grants.len() >= max_extensions` | deny "extension limit reached" |
| 2 | `granted_so_far + extension_steps > initial_cap * ceiling_factor` | deny "extension ceiling reached" |
| 3 | `tool_calls == 0` | deny "no tool calls in this turn" |
| 4 | a `(name, message)` error pair seen `>= thrash_limit` times in the recent history | deny "repeated identical tool failure" |
| 5 | no `Ok` among the last `window` observations | deny "no recent successful tool call" |
| 6 | otherwise | grant `extension_steps`, "recent tool progress" |

Thrash detection is bounded to the most recent `window * 2` observations. A
deep, sequential run of *distinct* failures (a failing test, a fix, another
failing test) followed by a recent success is therefore not thrashing: rule 4
only matches an identical `(name, message)` pair repeated, and rule 5 needs one
success in the trailing window. Such a turn can be granted an extension.
Repeated identical failures and turns with no recent success still fail closed.

A grant raises `cap`, appends to `grants` and continues the loop with tools
re-enabled (`crates/oc-runtime/src/turn/mod.rs:529-561`).

## Early warning and budget requests

At `cap - step_number <= 5` the step prompt carries an early warning telling the
agent it may request an extension through the observer path
(`goal_update`/observer). The warning text lives in
`crates/oc-runtime/src/prompt/mod.rs` (`STEP_BUDGET_WARNING`). It is advisory:
the deterministic policy above remains the authority and no provider call is
made merely to ask for steps.

The request channel reuses the existing `ObserverAction::Repair`; no new
`ObserverAction` variant is introduced. The observer emits a repair with
`kind` = `RepairKind::RequestBudgetExtension` (serde
`request_budget_extension`), so the request inherits the existing repair
reservation, audit row and notification channel instead of new wire shapes.
Before `run_steps` enters the tool-less `final_call` at `step_number >= cap`, it
checks both pending observer grants (`take_granted_steps` from `extra_steps`)
and deterministic auto-extension (`budget::evaluate_with_policy`), so an active
turn with recent progress extends seamlessly without dropping tools on step
`cap`. The runtime evaluator still decides whether `cap` actually rises; an
observer request cannot bypass the thrash, ceiling or grant-count rules.

When no grant is available the terminal path is unchanged: the final call at
`step_number >= cap` still disables tools and carries `MAX_STEPS_PROMPT`, which
now notes that the deterministic policy was consulted and declined without
promising a retry. `step_limit` is stored with the wire terminal `succeeded`
and `session.execution.step_limit {sessionID, steps}` is published only after
the step row and outcome are committed. The stored outcome, event grammar and
wire shape are unchanged.

## Compaction guidance

The guidance emitted when an agent is asked to compact — including the
overflow-retry prompt — tells the model to preserve the selected model/variant,
tool state (pending and cancelled calls, lanes, diagnostics), open goals
(id/status/criteria/next step), and the exact `keepID` boundary and threshold
context that made compaction necessary. This keeps the context the budget
decision depends on from being silently dropped across a compaction.

## Durability and replay

`session.execution.step_extended` is the durable projection of the grant, and
`cap` is a pure function of the log at `run_steps` entry — not a process-local
counter. `step_grants::replay` re-derives `{steps, cap, dropped}` from the
committed `session.execution.step_extended` rows
(`crates/oc-runtime/src/turn/step_grants.rs:70`, paging `Store::events_after`
1000 events at a time, `crates/oc-store/src/event.rs:109`), and `run_steps`
seeds both the cap and the grant list from it
(`crates/oc-runtime/src/turn/run.rs:102-115`). The grant event goes out through
`emit`, which appends the payload before broadcasting
(`crates/oc-runtime/src/runtime/mod.rs:549-584`; store append, then
`sink.publish` at `:582`), after `finish_step` committed the step's row
(`crates/oc-runtime/src/turn/run.rs:429`; `stream.rs:938-954`).

**The window.** The log holds every grant the session ever recorded, so replay
honours only grants in the *open* execution window: the events after the
session's last durable terminal
(`session.execution.{succeeded,failed,interrupted}`,
`crates/oc-runtime/src/turn/step_grants.rs:46-50`). A settled turn closes the
window, so the next turn starts at `agent.steps` and inherits nothing; a process
killed mid-turn never published a terminal, so its grants stay live and are
honoured on resume. `session.execution.started` deliberately does **not** open
the window: the turn loop publishes it before `run_steps` is called
(`crates/oc-runtime/src/turn/mod.rs:109-127`, `:209`), so keying on it would
erase exactly the grants a resumed execution must inherit.

**No double-grant.** Pre-seeding `grants` from the log (`run.rs:115`) makes
recorded grants count against `max_extensions` and the ceiling and keeps
`grantIndex` continuous, so a resumed turn cannot grant a second time for an
extension the previous process already recorded.

**Clamp.** Recorded grants are re-applied under the *current* `BudgetPolicy`
(`crates/oc-runtime/src/turn/step_grants.rs:127`), so a configuration that shrank
between the grant and the replay narrows the budget instead of widening it;
dropped grants are `tracing::warn`-ed (`run.rs:103-110`), never silent. The
ceiling is enforced twice: rule 2 bounds the total, rule 1 the count.

### Tests (replay landed 2026-11-23)

`crates/oc-runtime/tests/runtime/step_grants.rs` (registered at
`crates/oc-runtime/tests/runtime.rs:30-31`), all behavioural — asserted on the
cap the turn ended at, the provider-call count and the number of durable grant
events:

- `a_grant_recorded_before_the_crash_still_budgets_the_resumed_turn` (:179)
- `a_restart_does_not_resurrect_budget_past_the_logged_ceiling` (:225)
- `a_recorded_grant_is_not_re_emitted_when_the_turn_resumes` (:268)
- `a_settled_turn_does_not_lend_its_grant_to_the_next_turn` (:333) — the negative
  control for the window rule.

The arithmetic of the clamp lives in the `step_grants.rs` unit tests. Cost is
one forward log scan per `run_steps` entry (one turn, not one step), bounded by
the session log because `oc-store` has no reverse `seq` query.

## Failure modes

| Situation | Behaviour |
|---|---|
| evaluator denies | unchanged `StepLimited` path, byte-identical to today: same qualifier and event; the deny reason is not published |
| `granted == 0` | filtered out (`crates/oc-runtime/src/turn/mod.rs:536`), so deny path |
| cap below `step_budget.min_cap` | policy evaluation is skipped; caps 2 and 5 keep the legacy path |
| no tool observations | rule 3 or 5 denies; the turn ends as before |

## Tests

- `budget.rs` unit tests: `grants_on_recent_progress`,
  `denies_thrashing_on_identical_failures`, `denies_when_no_tool_calls`,
  `denies_when_window_has_no_success`, `denies_at_extension_limit`,
  `denies_when_ceiling_would_be_exceeded`, `caller_enforces_min_cap_floor`.
- `crates/oc-runtime/tests/runtime/turn_budget.rs` (registered at
  `crates/oc-runtime/tests/runtime.rs:33`): `small_cap_is_never_extended` (:43),
  `progress_extensions_continue_the_turn` (:101), `thrashing_is_not_extended`
  (:202) and `extension_is_bounded` (:274). At cap 10 the ceiling binds before
  `MAX_EXTENSIONS`, so one 15-step grant fits and the next evaluation denies
  "extension ceiling reached".
- Legacy pins in `crates/oc-runtime/tests/runtime/turn_core.rs`: `:266`
  (`max_steps_makes_one_final_tool_less_call`), the step-limit cases at `:455`,
  `:562`, `:601`, `:648`, `:723`; in `subagents.rs`, `:1205`
  (`subagent_step_limit_is_reported_to_the_parent`) and its negative control at
  `:1278` (`subagent_that_stops_on_its_own_has_no_step_limit_marker`).

## Out of scope

- **Per-criterion evidence attribution.** The goal snapshot reports `done` as
  `total` only once the goal itself is accepted `done`, never as an evidence
  count (`crates/oc-runtime/src/goal_store.rs:86-103`), and `render_goal` emits
  one `Evidence:` line instead of pairing ids with criteria by position
  (`crates/oc-runtime/src/goal.rs:77-85`). The UI chip prefers the wire
  `done`/`total` and only falls back to evidence length for legacy rows
  (`ui/vendor/opencode/packages/app/src/session/agent-panels/goal-chip.tsx:28-32`).
  Attributing a specific evidence id to a specific criterion would need a new
  column and is intentionally out of scope.
- **Host wiring: one gap open, one wired.** The step-boundary call site is
  wired: `run_steps` evaluates `budget::evaluate_with_policy` before treating a
  step as `final_call` (`crates/oc-runtime/src/turn/run.rs:255-288`), consumes
  observer grants at the loop top (`run.rs:140-168`), and publishes the live cap
  for the observer (`run.rs:174-175` → `Runtime::set_step_budget`,
  `crates/oc-runtime/src/runtime/state.rs:39`), which `observer_pass` maps to
  `RootContext.step_cap` / `steps_remaining`
  (`crates/oc-runtime/src/observer/tick.rs:134-142`,
  `crates/oc-observer/src/engine/snapshot.rs:74-76`).
  **Still open (not fixed as of 2026-11-23, fix in flight in `oc-config`):** the
  `step_budget.*` overrides in the policy table above never reach a live runtime.
  `step_budget` is listed as handled
  (`crates/oc-config/src/normalize/keys.rs:68`) and therefore *dropped* by the
  unknown-key loop (`crates/oc-config/src/normalize/root.rs:107-112`), but the
  `copy_object` list (`normalize/root.rs:93-103`) does not include it, so the
  normalized document never carries the section and
  `BudgetPolicy::from_merged` (`crates/oc-runtime/src/turn/budget.rs:45-69`)
  always sees the defaults. Every override here is inert until that one line
  lands; only the `budget.rs` unit test
  (`merged_config_reads_step_budget_overrides`, `budget.rs:431`) exercises the
  parser, from a raw JSON value. `crates/oc-runtime/tests/runtime/step_grants.rs`
  documents the same caveat and pins its fixtures to the defaults.
  This doc intentionally does not add an
  `ObserverAction` variant: `ObserverAction` and its dispatch are exhaustive in
  files outside the budget owner's scope, so the request channel is the
  existing `RepairKind::RequestBudgetExtension` on `ObserverAction::Repair`.
  Wire shapes are unchanged; the generated wire types in
  `../opencode/packages/client/src/promise/generated/types.ts` and
  `../opencode/packages/protocol/openapi.json` win if prose here ever conflicts.
