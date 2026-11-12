# 24 — Step budget extension

A subagent at 49 of 50 steps with its tests unrun loses the whole turn; asking
the model or the observer for more steps would add a provider round trip in the
stop path. The decision is instead deterministic, in memory, and never an LLM
call: the runtime extends the budget only when the turn shows visible progress.

## Current behaviour

- `DEFAULT_STEPS = 50` (`crates/oc-runtime/src/turn/session.rs:21`); `agent.steps`
  overrides it, floored at 1 (`crates/oc-runtime/src/turn/session.rs:147-152`).
- `run_steps` keeps the live budget in `cap`, initialised from `turn.max_steps`
  (`crates/oc-runtime/src/turn/mod.rs:307`). `final_call` is `step_number >= cap`
  (`crates/oc-runtime/src/turn/mod.rs:382`), so the limit request goes out with
  tools disabled (`crates/oc-runtime/src/turn/stream.rs:113`) and carries
  `MAX_STEPS_PROMPT` (`crates/oc-runtime/src/turn/mod.rs:397`); tool defs stay so
  the prompt cache survives (`docs/04`).
- The terminal branch (`crates/oc-runtime/src/turn/mod.rs:507-509`) returns
  `ExecutionOutcome::StepLimited { steps: turn.max_steps }`
  (`crates/oc-runtime/src/turn/mod.rs:555-557`). Settling stores `step_limit` with
  the wire terminal `succeeded` (`crates/oc-runtime/src/turn/mod.rs:683`), records
  `step limit reached after N steps` (`crates/oc-runtime/src/turn/mod.rs:695`;
  grammar in `crates/oc-types/src/payload.rs:56-62`) and publishes
  `session.execution.step_limit {sessionID, steps}` after it
  (`crates/oc-runtime/src/turn/mod.rs:725-735`).

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

Runtime-private constants, no config surface: `EXTENSION_STEPS = 15`,
`MAX_EXTENSIONS = 2`, `MIN_CAP_FOR_EXTENSION = 10` (caller-enforced floor),
`THRASH_LIMIT = 3`, `WINDOW = 5`, `CEILING_FACTOR = 2` (grant <= initial cap * 2).

`budget::evaluate` (`crates/oc-runtime/src/turn/budget.rs:68`) is a pure function
of the observations, `tool_calls`, the grants made and the initial cap; first
match wins and every rule fails closed:

| Rule | Condition | Result |
|---|---|---|
| 1 | `grants.len() >= MAX_EXTENSIONS` | deny "extension limit reached" |
| 2 | `granted_so_far + EXTENSION_STEPS > initial_cap * CEILING_FACTOR` | deny "extension ceiling reached" |
| 3 | `tool_calls == 0` | deny "no tool calls in this turn" |
| 4 | a `(name, message)` error pair seen `>= THRASH_LIMIT` times | deny "repeated identical tool failure" |
| 5 | no `Ok` among the last `WINDOW` observations | deny "no recent successful tool call" |
| 6 | otherwise | grant 15, "recent tool progress" |

A grant raises `cap`, appends to `grants` and continues the loop with tools
re-enabled (`crates/oc-runtime/src/turn/mod.rs:517-549`).

## Durability and replay

`cap` is a `run_steps` local (`crates/oc-runtime/src/turn/mod.rs:307`), never
persisted, so a grant cannot be replayed: a restart rebuilds the budget from
`agent.steps`. The grant event is durable and goes out through `emit`, which
appends the payload before broadcasting
(`crates/oc-runtime/src/runtime/mod.rs:554-589`; store append, then
`sink.publish` at `:587`), after `finish_step` committed
the step's row (`crates/oc-runtime/src/turn/mod.rs:466`). The ceiling is enforced
twice: rule 2 bounds the total, rule 1 the count.

## Failure modes

| Situation | Behaviour |
|---|---|
| evaluator denies | unchanged `StepLimited` path, byte-identical to today: same qualifier and event; the deny reason is not published |
| `granted == 0` | filtered out (`crates/oc-runtime/src/turn/mod.rs:524`), so deny path |
| cap below `MIN_CAP_FOR_EXTENSION` | `evaluate` is never called; caps 2 and 5 keep the legacy path |
| no tool observations | rule 3 or 5 denies; the turn ends as before |

## Tests

- `budget.rs` unit tests: `grants_on_recent_progress`,
  `denies_thrashing_on_identical_failures`, `denies_when_no_tool_calls`,
  `denies_when_window_has_no_success`, `denies_at_extension_limit`,
  `denies_when_ceiling_would_be_exceeded`, `caller_enforces_min_cap_floor`.
- `crates/oc-runtime/tests/runtime/turn_budget.rs` (registered at
  `crates/oc-runtime/tests/runtime.rs:32`): `small_cap_is_never_extended` (:43),
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

- **Observer step context.** `RootContext.step` is hardcoded to `0`
  (`crates/oc-runtime/src/observer/tick.rs:137`) and `HostFlag::StepEvent` is
  never raised (`crates/oc-observer/src/engine/notice.rs:165`).
- **Goal evidence mapping.** `done` counts evidence entries
  (`crates/oc-runtime/src/goal_store.rs:88`) and pairs them with criteria by
  position (`crates/oc-runtime/src/goal.rs:77-87`,
  `ui/vendor/opencode/packages/app/src/session/agent-panels/goal-chip.tsx:43`).
- **Observer-granted budget.** Adding an `ObserverAction` variant breaks no
  compilation (the only exhaustive match is
  `crates/oc-observer/src/engine/actions.rs:20`) and no UI (the observer audit
  response, `crates/oc-server/src/data/observer.rs:62`, has no action field).
