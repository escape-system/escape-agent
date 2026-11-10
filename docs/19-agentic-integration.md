# 19 — Agentic integration plan (memory, observer, subagents)

This is the bridge from landed crates to a working agentic loop. It is the
source of truth for the next wave; update it when the wiring lands.

## Landed pieces

| Piece | API |
|---|---|
| `oc-config` | `instructions()`, `skills()`, `skill(id)`, `commands()`, `command(name)`, `agents()`, `evaluate()` |
| `oc-memory` | `MemoryService::{retrieve,write,forget,list}`, `MemoryTools::registry()`, `scribe_prompt/parse/run_scribe` |
| `oc-observer` | `ExpectationRegistry`, `ObserverEngine::{observe_start,observe_settle,note_host_event,tick,check_now}`, `GoalSnapshot/GoalPolicy`, `ObserverModel`, `RepairExecutor`, `ObserverNotifier`, `GoalStore`, `RepairLog` |
| `oc-subagents` | `Policy::{spawn,can_message,can_write,resolve_model}`, `SpawnBudget`, `SiblingBudget`, `TreeLookup` |
| `oc-runtime` | `Runtime::{notify,interrupt,wait_idle,active}`, turn loop, `EventSink`, `PermissionHandler`, `ModelClient` |

## 1. Memory in the runtime

`RuntimeConfig` gains:

```rust
pub memory: Option<Arc<oc_memory::MemoryService>>,
pub scribe: Option<Arc<dyn oc_memory::ScribeModel>>,
```

Prompt build (`prompt.rs`):

1. Query scopes: `global`, `project` (session `project_id`), `agent` (agent id),
   plus `session` for a subagent's own notes. Never cross-project.
2. Query text: last user text (trim 400) + active goal objective + loaded skill
   ids.
3. `MemoryService::retrieve` → `MemoryBlock`; add
   `InstructionSource::new(SourceKind::Memory, "oc/memory", block.text)`.
4. `ConfigService::instructions()` → one source per file
   (`SourceKind::Discovery`, key `discovery:<path>`), outermost first.
5. `ConfigService::skills()` → filter `description.is_some()`,
   `autoinvoke != Some(false)`, permission `skill:<id> != deny`; render
   `<available_skills>` sorted by id (`SourceKind::Skills`, key
   `core/skill-guidance`).
6. `InstructionSet` already computes deltas; publish
   `session.instructions.updated` on change, before the next provider call.
7. At turn settle: `memory.touch(&block.ids)` once; if
   `scribe` is configured and the turn had tools or a goal, run
   `run_scribe` off the turn and persist candidates with `source:"scribe"`.

Tools: merge `MemoryTools::registry()` into the per-session registry. Permission
action `memory:write` for write/forget; `memory_search`/`memory_list` use
`read`.

Invariants: memory is cache-stable per result set; no per-step splices; a
retrieval failure is logged and the turn continues without a memory block.

## 2. Observer in the runtime

`RuntimeConfig` stays unchanged; the observer is attached after construction:

```rust
use oc_observer::{ObserverConfig, ObserverEngine, SystemClock};
use oc_runtime::{ObserverBinding, RepairChannel, Runtime, StoreGoalStore};

let (executor, repairs) = RepairChannel::new();
let engine = Arc::new(ObserverEngine::new(
    ObserverConfig::default(),
    Arc::new(SystemClock),
    observer_model,   // Arc<dyn ObserverModel>, implemented over oc-llm by the host
    executor,         // feeds RepairCommand into the runtime worker
    notifier,         // host publishes observer.* / injects audit rows
));
let runtime = Runtime::new(config)
    .with_observer(ObserverBinding {
        engine: Arc::clone(&engine),
        goals: Arc::new(StoreGoalStore::new(Arc::clone(&store))),
        repairs,
    });
runtime.start().await?;          // starts the repair worker
// The server also passes `engine` to POST /observer/check -> check_now.
```

`StoreGoalStore` is the built-in `GoalStore` adapter: it maps goal rows to
`GoalSnapshot` (`total` = criteria, `done` = evidence), and saves status,
rounds, stalls, progress and evidence through `update_goal`/`set_goal`.
The host `ObserverNotifier` owns audit persistence (repair log, `goal.updated`).

Lifecycle:

1. On tool dispatch: `ExpectationRegistry::open(FlightStrip)` with the
   expectation from the tool input (`why`, clocks), `tool_call_id`, session,
   agent, depth, `parent_call: None`, `retryable` for read/glob/grep/lsp/
   digest/roster/memory_search.
2. On tool settle: `observe_settle(id, state, outcome)` (`ok`, the tool error
   message, `cancelled`).
3. On execution failure or interrupt: `note_host_event`.
4. `Runtime::notify` starts one ticker per **root** session; it ticks every
   `observer.tick_ms` and calls `tick(root, tree, goal)` with:
   - tree rows from `session_node`/`children`, BFS-capped at 100,
   - root context (agent/model/state) from `session_node`,
   - goal snapshot from `GoalStore`.
   Tick only invokes the LLM when the engine returns a run (deadline, error
   streak, stall, host failure, goal change); rate limits/budgets are inside.
5. `ObserverVerdict` execution:
   - `nudge` → runtime `synthetic` inbox item on target
     (`metadata.observer=true`, `metadata.target`), then notify target/root.
   - `inspect` → **gap**: no `digest` API yet; the runtime logs a warning and
     creates no digest rather than faking one.
   - `repair` → `RepairChannel` sends a `RepairCommand` to the runtime worker:
     `interrupt`, `compact_context` (compaction inbox row), `switch_model`
     (store patch + `session.model.selected`), `repost` (synthetic). Audit is
     the host notifier's job; the worker records nothing twice.
   - `goal_*` → the host notifier persists the new snapshot via `GoalStore`.
6. Repair budget exhaustion → `observer.escalated`; no further auto repairs.
7. Observer model failure → `observer.unavailable`, fail open, retry after
   5 min.

Known typed gaps:

- `inspect` (digest): the `digest` tool is landed in `oc-runtime`, but the
  observer does not call it yet;
- `retry_tool` has no runtime command (the executor returns a typed error);
- `cancel_tool` cannot cancel one tool call (the runtime cancels whole
  executions) and logs a typed no-op;
- `restart_subagent` interrupts the child but cannot re-spawn it until the
  subagent spawn API exists.

The observer never executes tools directly and never reads tool arguments or
outputs.

## 3. Goals

`Store` additions (next store wave):

```rust
pub async fn set_goal(...), get_goal(root_id), update_goal(...), list_goals(...)
```

`GoalStore` adapter persists `GoalSnapshot` + evidence. Prompt block is an
instruction source `oc/goal` (cache-stable per goal version). `goal_set` fails
if an active goal exists. `goal_complete` asks the observer to judge; only the
observer sets `done`, with evidence ids that resolve to real messages/tool
calls.

## 4. Subagents, siblings and lanes

`Store` additions:

```rust
pub async fn session_node(id) -> Option<SessionNodeRow>; // root/depth/role/spawn/child_count
pub async fn children(id) -> Vec<SessionNodeRow>;
pub async fn increment_child_count(parent, delta);
```

Tools (implemented in `oc_runtime::subagent`, built per session in the turn
loop and merged after the memory tools):

- `subagent {agent, description, prompt, model?, sessionID?, background?,
  role?, spawn?}`: `Policy::spawn` → `SpawnBudget::try_spawn` →
  `Policy::resolve_model` → create child session (`parent_id`, depth, role,
  root) → enqueue prompt → `Runtime::notify` → foreground waits for idle and
  returns **only** `<subagent sessionID=… state=…>final text</subagent>`.
- `talk {to, kind, text}`: `Policy::can_message`; enqueue a `peer` inbox item;
  publish `subagent.peer.message`. Peer messages are not user messages.
- `roster`: `Store::children` + siblings + parent; ids/status/titles only.
- `session_write {session_id, text}`: `Policy::can_write`; enqueue
  `synthetic`/peer with `metadata.from`.
- `digest {session_id?, tool_call_id?, question?}`: `ScribeModel` summarizes
  ≤300 tokens; cache by content hash; rate limit per parent.
- `lane`: `LaneService` (below).

Lane service:

- Command classifier (`shell-words`) maps `cargo build/test/clippy/nextest`,
  `npm/pnpm/bun test`, `go test`, `pytest`, `dotnet test`, `gradle`, `mvn` to
  `cargo:<workspace>` etc.
- Named lanes with semaphores (cargo 1, node 1, network 4, gpu 1, default 8).
- `shell` intercepts classified commands, returns a ticket, streams progress;
  `lane.queue/order/status/cancel/notify` for the coordinator. `lane.queue`
  takes an optional `tree_rev` and returns `{id, subscribed}`; identical
  submissions on a live ticket coalesce.
- Boot restores lanes: queued tickets replay in enqueue order, `running` ones
  become failed with a restart reason, and ids are never reused.
- Safety is the semaphore, not the model. `steward` agent is a template.

## 5. Events (additive)

`observer.repaired`, `observer.escalated`, `observer.unavailable`,
`observer.advised` (durable, v1, from the host-side compaction adviser),
`goal.updated`, `memory.updated`, `subagent.peer.message`,
`lane.queued|started|finished|cancelled`. Payloads are documented in
`docs/02-protocol.md` (extensions).

## 6. Order of work

1. `RuntimeMemory` (prompt sources + memory tools + touch/scribe). No schema
   change.
2. `StoreGoals` (goal/observation/repair tables API) + `RuntimeObserver`.
3. `StoreSubagents` (node/children/child_count) + subagent tools + sibling
   delivery.
4. `LaneService` + `shell` interception + `steward` agent template + UI lane
   pill.
5. UI pages for memory/goals/observer/subagents/lanes.

Each step ends with a real HTTP/turn test: no step is "done" because the crate
compiles.
